//! Watcher implementation for the kqueue API
//!
//! The kqueue() system call provides a generic method of notifying the user
//! when an event happens or a condition holds, based on the results of small
//! pieces of kernel code termed filters.

use super::event::*;
use super::{Config, Error, EventHandler, RecursiveMode, Result, WatchMode, Watcher};
#[cfg(test)]
use crate::{BoundSender, bounded};
use crate::{ErrorKind, PathsMut, Receiver, Sender, TargetMode, unbounded};
use kqueue::{EventData, EventFilter, FilterFlag, Ident};
use rustc_hash::FxBuildHasher;
use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::env;
use std::fs::metadata;
use std::ops::Bound;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use walkdir::WalkDir;

const KQUEUE: mio::Token = mio::Token(0);
const MESSAGE: mio::Token = mio::Token(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootState {
    Missing,
    File,
    Directory,
}

/// A path the user watches. With `TargetMode::TrackPath` the path is tracked through every
/// ancestor: the ones that exist are watched, so that the root is reported removed when any of
/// them goes away, and created and watched again when it is reachable again. When watching it
/// again fails, the error is reported and the root stays missing until it is watched again.
#[derive(Clone, Copy, Debug)]
struct RootWatch {
    mode: WatchMode,
    state: RootState,
}

// The EventLoop will set up a mio::Poll and use it to wait for the following:
//
// -  messages telling it what to do
//
// -  events telling it that something has happened on one of the watched files.
struct EventLoop {
    running: bool,
    poll: mio::Poll,
    event_loop_waker: Arc<mio::Waker>,
    event_loop_tx: Sender<EventLoopMsg>,
    event_loop_rx: Receiver<EventLoopMsg>,
    kqueue: kqueue::Watcher,
    event_handler: Box<dyn EventHandler>,
    watches: HashMap<PathBuf, RootWatch, FxBuildHasher>,
    /// The watched paths, and whether each is only an ancestor of tracked roots.
    watch_handles: HashMap<PathBuf, bool, FxBuildHasher>,
    /// The watched paths in order, so that the handles below a path are a range; see
    /// [`EventLoop::handles_below`].
    handle_paths: BTreeSet<PathBuf>,
    /// How many tracked roots lie strictly below each path.
    ancestors: HashMap<PathBuf, usize, FxBuildHasher>,
    /// The ancestors of tracked roots last seen as directories; see
    /// [`EventLoop::check_ancestor`].
    present_ancestors: HashSet<PathBuf, FxBuildHasher>,
    /// The tracked roots found missing since the kevents were last registered; see
    /// [`EventLoop::register`].
    unarmed: Vec<PathBuf>,
    /// The ancestors found present again by an arm, looked at again once the kevents are
    /// registered; see [`EventLoop::note_ancestor_present`].
    reappeared: Vec<PathBuf>,
    /// Whether a handle was opened since the kevents were last registered.
    unregistered: bool,
    /// The tracked roots found present again, reported once their watches are registered.
    pending: Vec<Event>,
    follow_symlinks: bool,
}

/// Watcher implementation based on inotify
#[derive(Debug)]
pub struct KqueueWatcher {
    channel: Sender<EventLoopMsg>,
    waker: Arc<mio::Waker>,
}

enum EventLoopMsg {
    AddWatch(PathBuf, WatchMode, Sender<Result<()>>),
    AddWatchMultiple(Vec<(PathBuf, WatchMode)>, Sender<Result<()>>),
    RemoveWatch(PathBuf, Sender<Result<()>>),
    Shutdown,
    #[cfg(test)]
    GetWatchHandles(BoundSender<Vec<(PathBuf, bool)>>),
}

impl EventLoop {
    pub fn new(
        kqueue: kqueue::Watcher,
        event_handler: Box<dyn EventHandler>,
        follow_symlinks: bool,
    ) -> Result<Self> {
        let (event_loop_tx, event_loop_rx) = unbounded::<EventLoopMsg>();
        let poll = mio::Poll::new()?;

        let event_loop_waker = Arc::new(mio::Waker::new(poll.registry(), MESSAGE)?);

        let kqueue_fd = kqueue.as_raw_fd();
        let mut evented_kqueue = mio::unix::SourceFd(&kqueue_fd);
        poll.registry()
            .register(&mut evented_kqueue, KQUEUE, mio::Interest::READABLE)?;

        let event_loop = EventLoop {
            running: true,
            poll,
            event_loop_waker,
            event_loop_tx,
            event_loop_rx,
            kqueue,
            event_handler,
            watches: HashMap::default(),
            watch_handles: HashMap::default(),
            handle_paths: BTreeSet::new(),
            ancestors: HashMap::default(),
            present_ancestors: HashSet::default(),
            unarmed: Vec::new(),
            reappeared: Vec::new(),
            unregistered: false,
            pending: Vec::new(),
            follow_symlinks,
        };
        Ok(event_loop)
    }

    // Run the event loop.
    pub fn run(self) {
        let result = thread::Builder::new()
            .name("notify-rs kqueue loop".to_string())
            .spawn(|| self.event_loop_thread());
        if let Err(e) = result {
            tracing::error!(?e, "failed to start kqueue event loop thread");
        }
    }

    fn event_loop_thread(mut self) {
        let mut events = mio::Events::with_capacity(16);
        loop {
            // Wait for something to happen.
            match self.poll.poll(&mut events, None) {
                Err(ref e) if matches!(e.kind(), std::io::ErrorKind::Interrupted) => {
                    // System call was interrupted, we will retry
                    // TODO: Not covered by tests (to reproduce likely need to setup signal handlers)
                }
                Err(e) => panic!("poll failed: {e}"),
                Ok(()) => {}
            }

            // Process whatever happened.
            for event in &events {
                self.handle_event(event);
            }

            // Stop, if we're done.
            if !self.running {
                break;
            }
        }
    }

    // Handle a single event.
    fn handle_event(&mut self, event: &mio::event::Event) {
        match event.token() {
            MESSAGE => {
                // The channel is readable - handle messages.
                self.handle_messages();
            }
            KQUEUE => {
                // inotify has something to tell us.
                self.handle_kqueue();
            }
            _ => unreachable!(),
        }
    }

    fn handle_messages(&mut self) {
        while let Ok(msg) = self.event_loop_rx.try_recv() {
            match msg {
                EventLoopMsg::AddWatch(path, watch_mode, tx) => {
                    let result = tx.send(self.add_watch(path, watch_mode));
                    if let Err(e) = result {
                        tracing::error!(?e, "failed to send AddWatch result");
                    }
                }
                EventLoopMsg::AddWatchMultiple(paths, tx) => {
                    let result = tx.send(self.add_watch_multiple(paths));
                    if let Err(e) = result {
                        tracing::error!(?e, "failed to send AddWatchMultiple result");
                    }
                }
                EventLoopMsg::RemoveWatch(path, tx) => {
                    let result = tx.send(self.remove_watch(&path));
                    if let Err(e) = result {
                        tracing::error!(?e, "failed to send RemoveWatch result");
                    }
                }
                EventLoopMsg::Shutdown => {
                    self.running = false;
                    break;
                }
                #[cfg(test)]
                EventLoopMsg::GetWatchHandles(tx) => {
                    let handles = self
                        .watch_handles
                        .iter()
                        .map(|(path, chain)| (path.clone(), *chain))
                        .collect();
                    tx.send(handles).unwrap();
                }
            }
        }
    }

    /// Whether a root reports through the handle at `path`: the root itself, an entry of a
    /// directory root, or anything below a recursive root.
    fn is_watched_path(watches: &HashMap<PathBuf, RootWatch, FxBuildHasher>, path: &Path) -> bool {
        Self::reporting_roots(watches, path).next().is_some()
    }

    /// The roots that report through the handle at `path`; see [`Self::is_watched_path`].
    fn reporting_roots<'a>(
        watches: &'a HashMap<PathBuf, RootWatch, FxBuildHasher>,
        path: &'a Path,
    ) -> impl Iterator<Item = (&'a Path, &'a RootWatch)> {
        path.ancestors()
            .enumerate()
            .filter_map(move |(depth, ancestor)| {
                watches
                    .get(ancestor)
                    .filter(|root| depth <= 1 || root.mode.recursive_mode.is_recursive())
                    .map(|root| (ancestor, root))
            })
    }

    /// Whether a `NoTrack` root at or below the gone `path` reports through the handle at
    /// `handle`. Such a root follows the entity its handle is bound to wherever it went, so the
    /// handle stays with it. A `NoTrack` root above `path` does not keep it: the entity left it.
    fn is_kept_by_no_track_root(
        watches: &HashMap<PathBuf, RootWatch, FxBuildHasher>,
        path: &Path,
        handle: &Path,
    ) -> bool {
        Self::reporting_roots(watches, handle).any(|(root, watch)| {
            watch.mode.target_mode == TargetMode::NoTrack && root.starts_with(path)
        })
    }

    /// A `NoTrack` root ends with the entity it was bound to. It goes at once, so that the rest
    /// of the batch does not take a new entity at its path for it. The paths at or below it that
    /// an earlier event of the batch found new, in a new entity at its path, go too, unless
    /// another root reports through them: no root would hold their handles.
    fn forget_no_track_root(
        watches: &mut HashMap<PathBuf, RootWatch, FxBuildHasher>,
        add_watches: &mut HashMap<PathBuf, (bool, bool), FxBuildHasher>,
        path: &Path,
    ) {
        if watches
            .get(path)
            .is_some_and(|root| root.mode.target_mode == TargetMode::NoTrack)
        {
            watches.remove(path);
            add_watches.retain(|entry, _| {
                !entry.starts_with(path) || Self::is_watched_path(watches, entry)
            });
        }
    }

    /// The handles at or below `path`.
    fn handles_below<'a>(
        handle_paths: &'a BTreeSet<PathBuf>,
        path: &'a Path,
    ) -> impl Iterator<Item = &'a PathBuf> {
        handle_paths
            .range::<Path, _>((Bound::Included(path), Bound::Unbounded))
            .take_while(move |handle| handle.starts_with(path))
    }

    /// Whether `path` is a recursive root or lies below one.
    fn is_recursive_at(watches: &HashMap<PathBuf, RootWatch, FxBuildHasher>, path: &Path) -> bool {
        watches
            .iter()
            .any(|(root, watch)| watch.mode.recursive_mode.is_recursive() && path.starts_with(root))
    }

    /// `path` is gone: a root there is missing, and the roots below it are cut off.
    fn note_gone(
        watches: &mut HashMap<PathBuf, RootWatch, FxBuildHasher>,
        ancestors: &HashMap<PathBuf, usize, FxBuildHasher>,
        path: &Path,
        vanished: &mut Vec<PathBuf>,
    ) {
        if let Some(root) = watches.get_mut(path) {
            root.state = RootState::Missing;
        }
        if ancestors.contains_key(path) {
            vanished.push(path.to_path_buf());
        }
    }

    fn note_present(
        watches: &mut HashMap<PathBuf, RootWatch, FxBuildHasher>,
        path: &Path,
        is_dir: bool,
    ) {
        if let Some(root) = watches.get_mut(path) {
            root.state = if is_dir {
                RootState::Directory
            } else {
                RootState::File
            };
        }
    }

    #[expect(clippy::too_many_lines)]
    fn handle_kqueue(&mut self) {
        // The paths to watch, with whether recursively and whether they are directories. A path
        // found new twice in the batch is one entry, so it is reported once.
        let mut add_watches: HashMap<PathBuf, (bool, bool), FxBuildHasher> = HashMap::default();
        // The paths whose entity is gone or renamed: their handles are stale, and are dropped
        // once the batch is read.
        let mut stale: HashSet<PathBuf, FxBuildHasher> = HashSet::default();
        let mut rewalk = Vec::new();
        let mut vanished = Vec::new();
        let mut changed_dirs = Vec::new();

        while let Some(event) = self.kqueue.poll(None) {
            tracing::trace!(?event, "kqueue event received");

            match event {
                kqueue::Event {
                    data: EventData::Vnode(data),
                    ident: Ident::Filename(_, path),
                } => {
                    let path = PathBuf::from(path);
                    let mut evs = Vec::new();
                    match data {
                        /*
                        TODO: Differentiate folders and files
                        kqueue doesn't tell us if this was a file or a dir, so we
                        could only emulate this inotify behavior if we keep track of
                        all files and directories internally and then perform a
                        lookup.
                        */
                        kqueue::Vnode::Delete => {
                            stale.insert(path.clone());
                            Self::note_gone(
                                &mut self.watches,
                                &self.ancestors,
                                &path,
                                &mut vanished,
                            );
                            if Self::is_watched_path(&self.watches, &path) {
                                let remove_event = Event::new(EventKind::Remove(RemoveKind::Any))
                                    .add_path(path.clone());
                                evs.push(remove_event);
                            }
                            Self::forget_no_track_root(&mut self.watches, &mut add_watches, &path);
                            // delete event also happens when this file is overwritten by a rename
                            // in that case, emit a create event for the new file, if a root still
                            // reports it
                            if Self::is_watched_path(&self.watches, &path)
                                && let Ok(metadata) = path.metadata()
                            {
                                let is_dir = metadata.is_dir();
                                Self::note_present(&mut self.watches, &path, is_dir);
                                add_watches.insert(
                                    path.clone(),
                                    (Self::is_recursive_at(&self.watches, &path), is_dir),
                                );
                                tracing::trace!("overwrite detected: {}", path.display());
                                evs.push(
                                    Event::new(EventKind::Create(if is_dir {
                                        CreateKind::Folder
                                    } else if metadata.is_file() {
                                        CreateKind::File
                                    } else {
                                        CreateKind::Other
                                    }))
                                    .add_path(path),
                                );
                            }
                        }

                        // a write to a directory means that a new file was created in it, let's
                        // figure out which file this was
                        kqueue::Vnode::Write if path.is_dir() => {
                            if self.ancestors.contains_key(&path) {
                                changed_dirs.push(path.clone());
                            }
                            if self.watch_handles.get(&path) == Some(&true) {
                                // A chain handle watches for the ancestor below it only, which
                                // `check_chain` looks at: no root reports through its entries.
                                tracing::trace!("write to a chain ancestor: {}", path.display());
                            } else {
                                self.scan_directory(path, &stale, &mut add_watches, &mut evs);
                            }
                        }

                        // data was written to this file
                        kqueue::Vnode::Write => {
                            if Self::is_watched_path(&self.watches, &path) {
                                evs.push(
                                    Event::new(EventKind::Modify(ModifyKind::Data(
                                        DataChange::Any,
                                    )))
                                    .add_path(path),
                                );
                            }
                        }

                        /*
                        Extend and Truncate are just different names for the same
                        operation, extend is only used on FreeBSD, truncate everywhere
                        else
                        */
                        kqueue::Vnode::Extend | kqueue::Vnode::Truncate => {
                            if Self::is_watched_path(&self.watches, &path) {
                                evs.push(
                                    Event::new(EventKind::Modify(ModifyKind::Data(
                                        DataChange::Size,
                                    )))
                                    .add_path(path),
                                );
                            }
                        }

                        /*
                        this kevent has the same problem as the delete kevent. The
                        only way i can think of providing "better" event with more
                        information is to do the diff our self, while this maybe do
                        able of delete. In this case it would somewhat expensive to
                        keep track and compare ever peace of metadata for every file
                        */
                        kqueue::Vnode::Attrib => {
                            if Self::is_watched_path(&self.watches, &path) {
                                evs.push(
                                    Event::new(EventKind::Modify(ModifyKind::Metadata(
                                        MetadataKind::Any,
                                    )))
                                    .add_path(path),
                                );
                            }
                        }

                        /*
                        The link count on a file changed => subdirectory created or
                        delete.
                        */
                        kqueue::Vnode::Link => {
                            // As we currently don't have a solution that would allow us
                            // to only add/remove the new/delete directory and that dosn't include a
                            // possible race condition. On possible solution would be to
                            // create a `HashMap<PathBuf, Vec<PathBuf>>` which would
                            // include every directory and this content add the time of
                            // adding it to kqueue. While this should allow us to do the
                            // diff and only add/remove the files necessary. This would
                            // also introduce a race condition, where multiple files could
                            // all ready be remove from the directory, and we could get out
                            // of sync.
                            // So for now, until we find a better solution, let remove and
                            // readd the whole directory.
                            // This is a expensive operation, as we recursive through all
                            // subdirectories.
                            if Self::is_recursive_at(&self.watches, &path) {
                                rewalk.push(path.clone());
                                add_watches.insert(path.clone(), (true, true));
                            }
                            if Self::is_watched_path(&self.watches, &path) {
                                evs.push(
                                    Event::new(EventKind::Modify(ModifyKind::Any)).add_path(path),
                                );
                            }
                        }

                        // Kqueue not provide us with the information necessary to provide
                        // the new file name to the event.
                        kqueue::Vnode::Rename => {
                            stale.insert(path.clone());
                            Self::note_gone(
                                &mut self.watches,
                                &self.ancestors,
                                &path,
                                &mut vanished,
                            );
                            if Self::is_watched_path(&self.watches, &path) {
                                evs.push(
                                    Event::new(EventKind::Modify(ModifyKind::Name(
                                        RenameMode::Any,
                                    )))
                                    .add_path(path.clone()),
                                );
                            }
                            Self::forget_no_track_root(&mut self.watches, &mut add_watches, &path);
                        }

                        // Access to the file was revoked via revoke(2) or the underlying file system was unmounted.
                        kqueue::Vnode::Revoke => {
                            stale.insert(path.clone());
                            Self::note_gone(
                                &mut self.watches,
                                &self.ancestors,
                                &path,
                                &mut vanished,
                            );
                            if Self::is_watched_path(&self.watches, &path) {
                                evs.push(
                                    Event::new(EventKind::Remove(RemoveKind::Any))
                                        .add_path(path.clone()),
                                );
                            }
                            Self::forget_no_track_root(&mut self.watches, &mut add_watches, &path);
                        }

                        // On different BSD variants, different extra events may be present
                        _ => evs.push(Event::new(EventKind::Other)),
                    }
                    for ev in evs {
                        self.event_handler.handle_event(Ok(ev));
                    }
                }
                // as we don't add any other EVFILTER to kqueue we should never get here
                kqueue::Event { ident: _, data: _ } => unreachable!(),
            }
        }

        tracing::trace!(?add_watches, ?stale, "processing kqueue watch changes");

        for path in &stale {
            // The stale handle goes even when a new entity was found at the path already, which
            // `add_watches` covers.
            self.remove_single_watch(path).ok();
            self.remove_handles_below(path);
        }

        // A subdirectory was created or removed below a recursive root: the tree is walked again.
        for path in rewalk {
            self.remove_handles_below(&path);
        }

        // The ancestors are looked at before the new entries are watched: an ancestor that went
        // takes the handles below it along, which must not include fresh ones.
        vanished.sort_unstable();
        vanished.dedup();
        for path in &vanished {
            self.vanish_below(path);
        }
        // A tracked root replaced within the batch is there again, even when the scan of its
        // parent was read before its own event and took it for known.
        for path in &stale {
            self.rearm(path);
        }
        // So is an ancestor replaced within the batch.
        for path in &vanished {
            self.check_ancestor(path);
        }
        // A burst of writes to one directory is one change to look at.
        changed_dirs.sort_unstable();
        changed_dirs.dedup();
        for dir in changed_dirs {
            self.check_chain(&dir);
        }

        for (path, (is_recursive, is_dir)) in add_watches {
            if let Err(err) = self.add_maybe_recursive_watch(path.clone(), is_recursive, is_dir)
                && let ErrorKind::Io(err_kind) = err.kind
                && err_kind.kind() == std::io::ErrorKind::NotFound
                && err.paths.contains(&path)
            {
                // file was deleted before we could add the watch, emit a remove event
                self.event_handler.handle_event(Ok(
                    Event::new(EventKind::Remove(RemoveKind::Any)).add_path(path)
                ));
            }
        }

        if let Err(err) = self.register() {
            self.event_handler.handle_event(Err(err));
        }
    }

    /// A directory was written to: the entry that is new and watched is reported created and
    /// watched, or the write is reported for the directory when there is none. An entry whose
    /// handle is stale is new too: the entity was replaced within the batch.
    fn scan_directory(
        &mut self,
        path: PathBuf,
        stale: &HashSet<PathBuf, FxBuildHasher>,
        add_watches: &mut HashMap<PathBuf, (bool, bool), FxBuildHasher>,
        evs: &mut Vec<Event>,
    ) {
        // find which file is new in the directory by comparing it with our
        // list of known watches
        match std::fs::read_dir(&path) {
            Ok(dir) => {
                let files = dir
                    .filter_map(std::result::Result::ok)
                    .map(|f| f.path())
                    .filter(|f| !self.watch_handles.contains_key(f) || stale.contains(f));
                let mut found_new_file = false;
                for file in files {
                    found_new_file = true;
                    if add_watches.contains_key(&file) {
                        // Found by an earlier event of the batch.
                        continue;
                    }
                    tracing::trace!("new file detected: {}", file.display());

                    let metadata = file.metadata();
                    let is_dir = metadata.as_ref().is_ok_and(|m| m.is_dir());
                    if Self::is_watched_path(&self.watches, &file) {
                        // watch this new file
                        Self::note_present(&mut self.watches, &file, is_dir);
                        add_watches.insert(
                            file.clone(),
                            (Self::is_recursive_at(&self.watches, &file), is_dir),
                        );

                        evs.push(
                            Event::new(EventKind::Create(if is_dir {
                                CreateKind::Folder
                            } else if metadata.is_ok_and(|m| m.is_file()) {
                                CreateKind::File
                            } else {
                                CreateKind::Other
                            }))
                            .add_path(file),
                        );
                        break;
                    }
                }
                if !found_new_file && Self::is_watched_path(&self.watches, &path) {
                    evs.push(
                        Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any)))
                            .add_path(path),
                    );
                }
            }
            Err(err) => {
                self.event_handler.handle_event(Err(err.into()));
            }
        }
    }

    /// Registers the pending kevents. Then the roots found missing since the last registration
    /// are looked at again: one created after its stat but before the watch on its parent was
    /// registered produced no event, and would stay missing until the next one. So are the
    /// ancestors an arm found present again; see [`Self::recheck_reappeared`]. The roots found
    /// present are reported afterwards, so that whatever the user does to one on hearing of it
    /// is seen by its watch.
    fn register(&mut self) -> Result<()> {
        let result = self.register_kevents();
        for event in std::mem::take(&mut self.pending) {
            self.event_handler.handle_event(Ok(event));
        }
        result
    }

    /// Registers in rounds, until one opens no handle. The ancestors found present again are
    /// looked at in the round after the one that found them, so that the handles opened
    /// meanwhile are registered first.
    fn register_kevents(&mut self) -> Result<()> {
        loop {
            self.kqueue.watch()?;
            self.unregistered = false;
            for root in std::mem::take(&mut self.unarmed) {
                self.rearm(&root);
            }
            for ancestor in std::mem::take(&mut self.reappeared) {
                self.recheck_reappeared(&ancestor);
            }
            if !self.unregistered && self.reappeared.is_empty() {
                break;
            }
        }
        self.unarmed.clear();
        Ok(())
    }

    /// An ancestor an arm found present again is looked at once more, now that the handle on its
    /// parent is registered: it may have gone again after that arm or after
    /// [`Self::check_ancestor`] saw it, unseen by a handle opened in the same batch. The other
    /// roots below it are armed again if it is still there, and cut off if it is gone.
    fn recheck_reappeared(&mut self, ancestor: &Path) {
        if !self.ancestors.contains_key(ancestor) {
            // Unwatched meanwhile: no root is below it any more.
            return;
        }
        match dir_at(ancestor) {
            Ok(true) => self.rearm_below(ancestor),
            Ok(false) => self.vanish_below(ancestor),
            // Nothing is known about it: it stays as the arm found it.
            Err(err) => tracing::debug!(?err, "cannot stat ancestor: {}", ancestor.display()),
        }
    }

    /// An ancestor of tracked roots changed: its children that are ancestors too are looked at;
    /// see [`Self::check_ancestor`].
    fn check_chain(&mut self, dir: &Path) {
        let children: Vec<PathBuf> = self
            .ancestors
            .keys()
            .filter(|ancestor| ancestor.parent() == Some(dir))
            .cloned()
            .collect();
        for child in children {
            self.check_ancestor(&child);
        }
    }

    /// The roots below an ancestor that is gone are cut off, the ones below an ancestor that is
    /// back can be reached. Whether it was there is recorded apart from its handle: a scan may
    /// have watched it again in the batch that shows the change, and an ancestor that cannot be
    /// opened has none. A write that changed nothing costs a stat.
    fn check_ancestor(&mut self, ancestor: &Path) {
        let present = self.present_ancestors.contains(ancestor);
        match dir_at(ancestor) {
            Ok(true) if !present => self.rearm_below(ancestor),
            Ok(false) if present => self.vanish_below(ancestor),
            Ok(_) => {}
            // Nothing is known about it: it stays as it was.
            Err(err) => tracing::debug!(?err, "cannot stat ancestor: {}", ancestor.display()),
        }
    }

    /// The roots below `path` are out of reach: report the ones that were present, and drop the
    /// watches below, which sit on moved or deleted files.
    fn vanish_below(&mut self, path: &Path) {
        self.present_ancestors
            .retain(|ancestor| !ancestor.starts_with(path));
        for (root, watch) in &mut self.watches {
            if watch.mode.target_mode != TargetMode::TrackPath
                || watch.state == RootState::Missing
                || root.as_path() == path
                || !root.starts_with(path)
            {
                continue;
            }
            let kind = if watch.state == RootState::Directory {
                RemoveKind::Folder
            } else {
                RemoveKind::File
            };
            watch.state = RootState::Missing;
            self.event_handler.handle_event(Ok(
                Event::new(EventKind::Remove(kind)).add_path(root.clone())
            ));
        }
        self.remove_handles_below(path);
    }

    /// `path` was just found a directory again: watch the roots below it that can be reached now.
    fn rearm_below(&mut self, path: &Path) {
        self.present_ancestors.insert(path.to_path_buf());
        let roots: Vec<PathBuf> = self
            .watches
            .iter()
            .filter(|(root, watch)| {
                watch.mode.target_mode == TargetMode::TrackPath
                    && watch.state == RootState::Missing
                    && root.as_path() != path
                    && root.starts_with(path)
            })
            .map(|(root, _)| root.clone())
            .collect();
        for root in roots {
            self.rearm(&root);
        }
    }

    /// Arms a missing tracked root again, and reports it created when it is there now; the
    /// report waits for the registration of its watch, see [`Self::register`].
    fn rearm(&mut self, root: &Path) {
        let Some(watch) = self.watches.get(root) else {
            return;
        };
        if watch.mode.target_mode != TargetMode::TrackPath || watch.state != RootState::Missing {
            return;
        }
        let recursive_mode = watch.mode.recursive_mode;
        match self.arm_root_or_rollback(root, recursive_mode) {
            Ok(RootState::Missing) => {}
            Ok(state) => {
                if let Some(watch) = self.watches.get_mut(root) {
                    watch.state = state;
                }
                self.pending.push(created(root, state));
            }
            Err(error) => self.event_handler.handle_event(Err(error)),
        }
    }

    /// Drops the handles at or below `path`, which is gone, except the ones still in use: the
    /// handle of a root present at `path` itself, and the handles of the `NoTrack` roots at or
    /// below `path`; see [`Self::is_kept_by_no_track_root`].
    ///
    /// A handle kept for a `NoTrack` root stays under the path it was watched by. A `TrackPath`
    /// root that shares the path, as the same root or as its parent, and is armed again when the
    /// path comes back takes that handle for its own, so it keeps watching the moved entity
    /// rather than the new one: `NoTrack` and `TrackPath` roots that overlap below a moved
    /// directory are not supported.
    fn remove_handles_below(&mut self, path: &Path) {
        let present_root = self
            .watches
            .get(path)
            .is_some_and(|root| root.state != RootState::Missing);
        self.drop_handles_below(path, |this, handle| {
            (present_root && handle == path)
                || Self::is_kept_by_no_track_root(&this.watches, path, handle)
        });
    }

    /// Drops the handles at or below `path` that `keep` does not accept.
    fn drop_handles_below(&mut self, path: &Path, keep: impl Fn(&Self, &Path) -> bool) {
        let handles: Vec<PathBuf> = Self::handles_below(&self.handle_paths, path)
            .filter(|handle| !keep(self, handle))
            .cloned()
            .collect();
        for handle in handles {
            self.remove_single_watch(&handle).ok();
        }
    }

    #[tracing::instrument(level = "trace", skip(self))]
    fn add_watch(&mut self, path: PathBuf, watch_mode: WatchMode) -> Result<()> {
        let result = self.add_watch_inner(path, watch_mode);

        // Only make a single `kevent` syscall to add all the watches.
        self.register()?;

        result
    }

    #[tracing::instrument(level = "trace", skip(self))]
    fn add_watch_multiple(&mut self, paths: Vec<(PathBuf, WatchMode)>) -> Result<()> {
        let result = paths
            .into_iter()
            .try_for_each(|(path, watch_mode)| self.add_watch_inner(path, watch_mode));

        // Only make a single `kevent` syscall to add all the watches.
        self.register()?;

        result
    }

    /// The caller of this function must call `self.register()` afterwards to register the new watch.
    #[tracing::instrument(level = "trace", skip(self))]
    fn add_watch_inner(&mut self, path: PathBuf, watch_mode: WatchMode) -> Result<()> {
        if let Some(existing) = self.watches.get(&path).copied() {
            return self.upgrade_watch(path, existing, watch_mode);
        }

        if watch_mode.target_mode == TargetMode::TrackPath {
            // The ancestors are counted first, so that a failed arm is undone by forgetting them.
            self.track_ancestors(&path);
            let state = match self.arm_root_or_rollback(&path, watch_mode.recursive_mode) {
                Ok(state) => state,
                Err(err) => {
                    self.untrack_ancestors(&path);
                    return Err(err);
                }
            };
            self.watches.insert(
                path,
                RootWatch {
                    mode: watch_mode,
                    state,
                },
            );
            return Ok(());
        }

        let meta = metadata(&path).map_err(Error::io_watch)?;
        self.add_maybe_recursive_watch(
            path.clone(),
            // If the watch is not recursive, or if we determine (by stat'ing the path to get its
            // metadata) that the watched path is not a directory, add a single path watch.
            watch_mode.recursive_mode.is_recursive() && meta.is_dir(),
            meta.is_dir(),
        )?;
        let state = if meta.is_dir() {
            RootState::Directory
        } else {
            RootState::File
        };
        self.watches.insert(
            path,
            RootWatch {
                mode: watch_mode,
                state,
            },
        );

        Ok(())
    }

    /// A root watched again: its mode is upgraded, and a tracked root that is missing is armed
    /// again, so that a root left missing by a failed arm is recovered by watching it again.
    fn upgrade_watch(
        &mut self,
        path: PathBuf,
        existing: RootWatch,
        watch_mode: WatchMode,
    ) -> Result<()> {
        let need_upgrade_to_recursive = match existing.mode.recursive_mode {
            RecursiveMode::Recursive => false,
            RecursiveMode::NonRecursive => watch_mode.recursive_mode == RecursiveMode::Recursive,
        };
        let need_to_track = match existing.mode.target_mode {
            TargetMode::TrackPath => false,
            TargetMode::NoTrack => watch_mode.target_mode == TargetMode::TrackPath,
        };
        tracing::trace!(
            ?need_to_track,
            ?need_upgrade_to_recursive,
            "upgrading existing watch for path: {}",
            path.display()
        );
        let mut root = existing;
        if need_to_track {
            self.track_ancestors(&path);
            if let Err(err) = self.arm_chain(&path) {
                self.untrack_ancestors(&path);
                return Err(err);
            }
            // Recorded now, so that unwatching the root forgets the ancestors whatever follows.
            root.mode.target_mode = TargetMode::TrackPath;
            self.watches.insert(path.clone(), root);
        }
        if root.state == RootState::Missing {
            root.mode.upgrade_with(watch_mode);
            self.watches.insert(path.clone(), root);
            let state = self.arm_root_or_rollback(&path, root.mode.recursive_mode)?;
            if state != RootState::Missing {
                if let Some(watch) = self.watches.get_mut(&path) {
                    watch.state = state;
                }
                self.pending.push(created(&path, state));
            }
            return Ok(());
        }
        if need_upgrade_to_recursive && metadata(&path).map_err(Error::io)?.is_dir() {
            self.add_maybe_recursive_watch(path.clone(), true, true)?;
        }
        root.mode.upgrade_with(watch_mode);
        self.watches.insert(path, root);
        Ok(())
    }

    /// Arms a tracked root; see [`Self::arm_root`]. A root armed halfway would report events
    /// without having been reported created, so when arming fails, what it did to the handles at
    /// or below the root and on its ancestors is undone: the handles it added go, and the ones it
    /// turned from chain handles into full ones are chain handles again. The handles other roots
    /// hold there stay as they were.
    ///
    /// Of the ancestors, only the parent can be turned into a full handle. The handles at or
    /// below the root are noted before arming, which for a root that holds none yet, as most
    /// do, is an empty walk.
    fn arm_root_or_rollback(
        &mut self,
        root: &Path,
        recursive_mode: RecursiveMode,
    ) -> Result<RootState> {
        let parent = root.parent();
        let parent_chain = parent.and_then(|parent| self.watch_handles.get(parent).copied());
        // In the order of the handles, so that looking one up is a binary search.
        let before: Vec<(PathBuf, bool)> = Self::handles_below(&self.handle_paths, root)
            .filter_map(|handle| Some((handle.clone(), *self.watch_handles.get(handle)?)))
            .collect();
        let error = match self.arm_root(root, recursive_mode) {
            Ok(state) => return Ok(state),
            Err(error) => error,
        };
        let added: Vec<PathBuf> = Self::handles_below(&self.handle_paths, root)
            .filter(|handle| {
                before
                    .binary_search_by(|(known, _)| known.cmp(handle))
                    .is_err()
            })
            .cloned()
            .collect();
        tracing::debug!(
            ?error,
            "dropping the {} handles of a root that could not be armed: {}",
            added.len(),
            root.display()
        );
        for handle in added {
            self.remove_single_watch(&handle).ok();
        }
        let restore = parent.zip(parent_chain);
        for (handle, chain) in before
            .iter()
            .map(|(handle, chain)| (handle.as_path(), *chain))
            .chain(restore)
        {
            if let Some(existing) = self.watch_handles.get_mut(handle) {
                *existing = chain;
            }
        }
        Err(error)
    }

    /// Watches a tracked root: its ancestors, and the root itself if it exists. A root found
    /// missing is looked at again once the kevents are registered; see [`Self::register`].
    fn arm_root(&mut self, root: &Path, recursive_mode: RecursiveMode) -> Result<RootState> {
        if !self.arm_chain(root)? {
            self.unarmed.push(root.to_path_buf());
            return Ok(RootState::Missing);
        }
        let meta = match metadata(root).map_err(Error::io_watch) {
            Ok(meta) => meta,
            Err(err) if matches!(err.kind, ErrorKind::PathNotFound) => {
                self.unarmed.push(root.to_path_buf());
                return Ok(RootState::Missing);
            }
            Err(err) => return Err(err),
        };
        self.add_maybe_recursive_watch(
            root.to_path_buf(),
            recursive_mode.is_recursive() && meta.is_dir(),
            meta.is_dir(),
        )?;
        Ok(if meta.is_dir() {
            RootState::Directory
        } else {
            RootState::File
        })
    }

    /// Watches the ancestors of `root` that exist, from the top down, and records them present.
    /// Returns whether the parent exists; a root without a parent is watched directly.
    ///
    /// An ancestor whose handle cannot be opened, such as a directory that can be searched but
    /// not read, is recorded present all the same, so that its removal and its return are seen
    /// through its parent. What happens inside it is not seen: a root below it that is cut off
    /// there is reported removed through the handles below, but it is not armed again when the
    /// directory in between comes back, until the ancestor itself goes and comes back or the
    /// root is watched again.
    fn arm_chain(&mut self, root: &Path) -> Result<bool> {
        // Nearest first, so the parent is at depth 0; they are walked from the top down.
        let ancestors: Vec<&Path> = root.ancestors().skip(1).collect();
        for (depth, &ancestor) in ancestors.iter().enumerate().rev() {
            if !dir_at(ancestor).map_err(|err| Error::io(err).add_path(ancestor.to_path_buf()))? {
                return Ok(false);
            }
            self.note_ancestor_present(ancestor);
            let chain = depth > 0;
            if let Some(existing) = self.watch_handles.get_mut(ancestor) {
                // Most arms find the handle there: its path is only copied when it is new.
                *existing = *existing && chain;
            } else if let Err(e) = self.add_single_watch(ancestor.to_path_buf(), chain) {
                if !chain {
                    return Err(e);
                }
                tracing::debug!(?e, "cannot watch ancestor: {}", ancestor.display());
            }
        }
        Ok(true)
    }

    /// Records a tracked ancestor present. One that was recorded absent came back without
    /// [`Self::check_ancestor`] seeing it yet, and now it will not see it: it is looked at again
    /// once the kevents are registered, see [`Self::recheck_reappeared`].
    fn note_ancestor_present(&mut self, ancestor: &Path) {
        if self.present_ancestors.contains(ancestor) {
            return;
        }
        self.present_ancestors.insert(ancestor.to_path_buf());
        if self.ancestors.get(ancestor).is_some_and(|count| *count > 1) {
            self.reappeared.push(ancestor.to_path_buf());
        }
    }

    fn track_ancestors(&mut self, root: &Path) {
        for ancestor in root.ancestors().skip(1) {
            if let Some(count) = self.ancestors.get_mut(ancestor) {
                *count += 1;
            } else {
                self.ancestors.insert(ancestor.to_path_buf(), 1);
            }
        }
    }

    /// Forgets the ancestors of an unwatched root, dropping the watches nobody needs any more.
    fn untrack_ancestors(&mut self, root: &Path) {
        for ancestor in root.ancestors().skip(1) {
            let Some(count) = self.ancestors.get_mut(ancestor) else {
                continue;
            };
            *count -= 1;
            if *count > 0 {
                continue;
            }
            self.ancestors.remove(ancestor);
            self.present_ancestors.remove(ancestor);
            if !Self::is_watched_path(&self.watches, ancestor) {
                self.remove_single_watch(ancestor).ok();
            }
        }
    }

    /// Watches `path`: the tree below it when `is_recursive`, else a directory and its entries,
    /// else the path alone.
    ///
    /// A directory created in or moved into a non-recursive directory root is watched as a
    /// directory too, so its entries get handles that no root reports through, and that
    /// unwatching the root leaves open until the entries are removed or the watcher is dropped.
    /// This is an old limit of this backend.
    ///
    /// The caller of this function must call `self.kqueue.watch()` afterwards to register the new watch.
    #[tracing::instrument(level = "trace", skip(self))]
    fn add_maybe_recursive_watch(
        &mut self,
        path: PathBuf,
        is_recursive: bool,
        is_dir: bool,
    ) -> Result<()> {
        if is_recursive {
            for entry in WalkDir::new(&path).follow_links(self.follow_symlinks) {
                let entry = entry.map_err(map_walkdir_error)?;
                self.add_single_watch(entry.into_path(), false)?;
            }
        } else if is_dir {
            self.add_single_watch(path.clone(), false)?;
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.filter_map(std::result::Result::ok) {
                    self.add_single_watch(entry.path(), false)?;
                }
            }
        } else {
            self.add_single_watch(path, false)?;
        }
        Ok(())
    }

    /// Adds a single watch to the kqueue.
    ///
    /// The caller of this function must call `self.kqueue.watch()` afterwards to register the new watch.
    #[tracing::instrument(level = "trace", skip(self))]
    fn add_single_watch(&mut self, path: PathBuf, chain: bool) -> Result<()> {
        let entry = match self.watch_handles.entry(path) {
            Entry::Occupied(mut existing) => {
                tracing::trace!("watch handle already exists: {}", existing.key().display());
                let existing_chain = existing.get_mut();
                *existing_chain = *existing_chain && chain;
                return Ok(());
            }
            Entry::Vacant(entry) => entry,
        };

        let event_filter = EventFilter::EVFILT_VNODE;
        let filter_flags = FilterFlag::NOTE_DELETE
            | FilterFlag::NOTE_WRITE
            | FilterFlag::NOTE_EXTEND
            | FilterFlag::NOTE_ATTRIB
            | FilterFlag::NOTE_LINK
            | FilterFlag::NOTE_RENAME
            | FilterFlag::NOTE_REVOKE;

        tracing::trace!("adding kqueue watch: {}", entry.key().display());

        self.kqueue
            .add_filename(entry.key(), event_filter, filter_flags)
            .map_err(|e| Error::io(e).add_path(entry.key().clone()))?;
        self.handle_paths.insert(entry.key().clone());
        entry.insert(chain);
        self.unregistered = true;

        Ok(())
    }

    /// Unwatches a root, keeping every handle another root still reports through or needs as an
    /// ancestor. A missing root holds no handle of its own, so unwatching it only forgets it.
    #[tracing::instrument(level = "trace", skip(self))]
    fn remove_watch(&mut self, path: &Path) -> Result<()> {
        let Some(root) = self.watches.remove(path) else {
            return Err(Error::watch_not_found());
        };
        let result = if root.mode.recursive_mode.is_recursive() {
            self.release_tree(path);
            Ok(())
        } else {
            self.release_root(path)
        };
        if root.mode.target_mode == TargetMode::TrackPath {
            self.untrack_ancestors(path);
        }
        self.register()?;
        result
    }

    /// Drops the handles of an unwatched non-recursive root: its own and, for a directory, the
    /// ones of its entries.
    fn release_root(&mut self, path: &Path) -> Result<()> {
        // By the handles we hold, not by the disk: the directory may be gone already.
        let entries: Vec<PathBuf> = Self::handles_below(&self.handle_paths, path)
            .filter(|handle| handle.parent() == Some(path))
            .cloned()
            .collect();
        for entry in entries {
            self.release_handle(&entry).ok();
        }
        self.release_handle(path)
    }

    /// Drops the handles of an unwatched recursive root, one by one like
    /// [`Self::release_handle`]: the ones another root still reports through or needs as an
    /// ancestor stay.
    fn release_tree(&mut self, path: &Path) {
        let handles: Vec<PathBuf> = Self::handles_below(&self.handle_paths, path)
            .cloned()
            .collect();
        for handle in handles {
            self.release_handle(&handle).ok();
        }
    }

    /// Drops the handle at `path` unless a root still reports through it, or tracked roots lie
    /// below it: then it becomes a chain handle, unless a root is right below it.
    fn release_handle(&mut self, path: &Path) -> Result<()> {
        if Self::is_watched_path(&self.watches, path) {
            return Ok(());
        }
        if self.ancestors.contains_key(path) {
            if !self.is_parent_of_root(path)
                && let Some(chain) = self.watch_handles.get_mut(path)
            {
                *chain = true;
            }
            return Ok(());
        }
        self.remove_single_watch(path)
    }

    fn is_parent_of_root(&self, path: &Path) -> bool {
        self.watches.keys().any(|root| root.parent() == Some(path))
    }

    /// Removes a single watch from the kqueue. A path without a handle has nothing to remove.
    ///
    /// The caller of this function must call `self.register()` afterwards to unregister the old watch.
    #[tracing::instrument(level = "trace", skip(self))]
    fn remove_single_watch(&mut self, path: &Path) -> Result<()> {
        if self.watch_handles.remove(path).is_none() {
            return Ok(());
        }
        self.handle_paths.remove(path);
        tracing::trace!("removing kqueue watch: {}", path.display());

        self.kqueue
            .remove_filename(path, EventFilter::EVFILT_VNODE)
            .map_err(|e| Error::io(e).add_path(path.to_path_buf()))?;
        Ok(())
    }
}

/// Whether a directory is at `path`, following symlinks like the handles do. Only a missing
/// entry or one that is not a directory counts as absent; any other failure says nothing about
/// presence and is returned.
fn dir_at(path: &Path) -> std::io::Result<bool> {
    match metadata(path) {
        Ok(meta) => Ok(meta.is_dir()),
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

/// The event for a tracked root that is there.
fn created(root: &Path, state: RootState) -> Event {
    let kind = if state == RootState::Directory {
        CreateKind::Folder
    } else {
        CreateKind::File
    };
    Event::new(EventKind::Create(kind)).add_path(root.to_path_buf())
}

fn map_walkdir_error(e: walkdir::Error) -> Error {
    if e.io_error().is_some() {
        let path = e.path().map(|p| p.to_path_buf());
        // safe to unwrap otherwise we whouldn't be in this branch
        let mut err = Error::io(e.into_io_error().unwrap());
        if let Some(path) = path {
            err = err.add_path(path);
        }
        err
    } else {
        Error::generic(&e.to_string())
    }
}

struct KqueuePathsMut<'a> {
    inner: &'a mut KqueueWatcher,
    add_paths: Vec<(PathBuf, WatchMode)>,
}
impl<'a> KqueuePathsMut<'a> {
    fn new(watcher: &'a mut KqueueWatcher) -> Self {
        Self {
            inner: watcher,
            add_paths: Vec::new(),
        }
    }
}
impl PathsMut for KqueuePathsMut<'_> {
    #[tracing::instrument(level = "debug", skip(self))]
    fn add(&mut self, path: &Path, watch_mode: WatchMode) -> Result<()> {
        self.add_paths.push((path.to_owned(), watch_mode));
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn remove(&mut self, path: &Path) -> Result<()> {
        self.inner.unwatch_inner(path)
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn commit(self: Box<Self>) -> Result<()> {
        let paths = self.add_paths;
        self.inner.watch_multiple_inner(paths)
    }
}

impl KqueueWatcher {
    fn from_event_handler(
        event_handler: Box<dyn EventHandler>,
        follow_symlinks: bool,
    ) -> Result<Self> {
        let kqueue = kqueue::Watcher::new()?;
        let event_loop = EventLoop::new(kqueue, event_handler, follow_symlinks)?;
        let channel = event_loop.event_loop_tx.clone();
        let waker = Arc::clone(&event_loop.event_loop_waker);
        event_loop.run();
        Ok(KqueueWatcher { channel, waker })
    }

    fn watch_inner(&self, path: &Path, watch_mode: WatchMode) -> Result<()> {
        let pb = if path.is_absolute() {
            path.to_owned()
        } else {
            let p = env::current_dir().map_err(Error::io)?;
            p.join(path)
        };
        let (tx, rx) = unbounded();
        let msg = EventLoopMsg::AddWatch(pb, watch_mode, tx);

        self.channel
            .send(msg)
            .map_err(|e| Error::generic(&e.to_string()))?;
        self.waker
            .wake()
            .map_err(|e| Error::generic(&e.to_string()))?;
        rx.recv().unwrap()
    }

    fn watch_multiple_inner(&self, paths: Vec<(PathBuf, WatchMode)>) -> Result<()> {
        let pbs = paths
            .into_iter()
            .map(|(path, watch_mode)| {
                if path.is_absolute() {
                    Ok((path, watch_mode))
                } else {
                    let p = env::current_dir().map_err(Error::io)?;
                    Ok((p.join(path), watch_mode))
                }
            })
            .collect::<Result<Vec<(PathBuf, WatchMode)>>>()?;
        let (tx, rx) = unbounded();
        let msg = EventLoopMsg::AddWatchMultiple(pbs, tx);

        self.channel
            .send(msg)
            .map_err(|e| Error::generic(&e.to_string()))?;
        self.waker
            .wake()
            .map_err(|e| Error::generic(&e.to_string()))?;
        rx.recv()
            .unwrap()
            .map_err(|e| Error::generic(&e.to_string()))
    }

    fn unwatch_inner(&self, path: &Path) -> Result<()> {
        let pb = if path.is_absolute() {
            path.to_owned()
        } else {
            let p = env::current_dir().map_err(Error::io)?;
            p.join(path)
        };
        let (tx, rx) = unbounded();
        let msg = EventLoopMsg::RemoveWatch(pb, tx);

        self.channel
            .send(msg)
            .map_err(|e| Error::generic(&e.to_string()))?;
        self.waker
            .wake()
            .map_err(|e| Error::generic(&e.to_string()))?;
        rx.recv()
            .unwrap()
            .map_err(|e| Error::generic(&e.to_string()))
    }
}

impl Watcher for KqueueWatcher {
    /// Create a new watcher.
    #[tracing::instrument(level = "debug", skip(event_handler))]
    fn new<F: EventHandler>(event_handler: F, config: Config) -> Result<Self> {
        Self::from_event_handler(Box::new(event_handler), config.follow_symlinks())
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn watch(&mut self, path: &Path, watch_mode: WatchMode) -> Result<()> {
        self.watch_inner(path, watch_mode)
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn paths_mut<'me>(&'me mut self) -> Box<dyn PathsMut + 'me> {
        Box::new(KqueuePathsMut::new(self))
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn unwatch(&mut self, path: &Path) -> Result<()> {
        self.unwatch_inner(path)
    }

    fn kind() -> crate::WatcherKind {
        crate::WatcherKind::Kqueue
    }

    #[cfg(test)]
    /// The watches that report the watched paths, without the ancestors watched for their entries
    /// only; see [`KqueueWatcher::get_chain_handles`].
    fn get_watch_handles(&self) -> HashSet<std::path::PathBuf> {
        self.handles(|chain| !chain)
    }
}

#[cfg(test)]
impl KqueueWatcher {
    /// The ancestors of tracked paths, watched for their entries only.
    fn get_chain_handles(&self) -> HashSet<std::path::PathBuf> {
        self.handles(|chain| chain)
    }

    fn handles(&self, keep: impl Fn(bool) -> bool) -> HashSet<std::path::PathBuf> {
        let (tx, rx) = bounded(1);
        self.channel
            .send(EventLoopMsg::GetWatchHandles(tx))
            .unwrap();
        self.waker.wake().unwrap();
        rx.recv()
            .unwrap()
            .into_iter()
            .filter(|(_, chain)| keep(*chain))
            .map(|(path, _)| path)
            .collect()
    }
}

impl Drop for KqueueWatcher {
    fn drop(&mut self) {
        // we expect the event loop to live => unwrap must not panic
        self.channel.send(EventLoopMsg::Shutdown).unwrap();
        self.waker.wake().unwrap();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::test::{self, *};

    fn watcher() -> (TestWatcher<KqueueWatcher>, test::Receiver) {
        channel()
    }

    #[expect(clippy::print_stdout)]
    #[test]
    fn test_remove_recursive() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let path = PathBuf::from("src");

        let mut watcher = KqueueWatcher::new(|event| println!("{event:?}"), Config::default())?;
        watcher.watch(&path, WatchMode::recursive())?;
        let result = watcher.unwatch(&path);
        assert!(
            result.is_ok(),
            "unwatch yielded error: {}",
            result.unwrap_err()
        );
        Ok(())
    }

    #[test]
    fn create_file() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();
        watcher.watch_recursively(&tmpdir);

        let path = tmpdir.path().join("entry");
        std::fs::File::create_new(&path).expect("create");

        rx.wait_ordered_exact([
            expected(&path).modify_meta_any().optional(),
            expected(path.clone()).create_file(),
            // The close of the new file may land after its watch is registered.
            expected(&path).modify_meta_any().optional(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf(), path]),
        );
    }

    #[test]
    fn create_self_file() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");

        watcher.watch_nonrecursively(&path);

        std::fs::File::create_new(&path).expect("create");

        rx.wait_ordered_exact([
            expected(&path).modify_meta_any().optional(),
            expected(path.clone()).create_file(),
        ]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf(), path])
        );
    }

    #[test]
    fn create_self_file_no_track() {
        let tmpdir = testdir();
        let (mut watcher, _) = watcher();

        let path = tmpdir.path().join("entry");

        let result = watcher.watcher.watch(
            &path,
            WatchMode {
                recursive_mode: RecursiveMode::NonRecursive,
                target_mode: TargetMode::NoTrack,
            },
        );
        assert!(
            matches!(
                result,
                Err(Error {
                    paths: _,
                    kind: ErrorKind::PathNotFound
                })
            ),
            "{result:?}"
        );
    }

    #[test]
    fn create_self_file_nested() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry/nested");

        watcher.watch_nonrecursively(&path);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));
        assert!(watcher.watcher.get_chain_handles().contains(tmpdir.path()));

        std::fs::create_dir_all(path.parent().unwrap()).expect("create");
        std::fs::File::create_new(&path).expect("create");

        rx.wait_ordered([expected(&path).create_file()]);
        assert!(
            watcher
                .get_watch_handles()
                .is_superset(&HashSet::from([tmpdir.path().join("entry")]))
        );
    }

    #[test]
    fn track_path_reports_roots_when_an_ancestor_moves_away_and_back() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let lib = tmpdir.path().join("lib");
        let a = lib.join("a.js");
        let b = lib.join("sub").join("b.js");
        let moved = tmpdir.path().join("moved");
        std::fs::create_dir_all(lib.join("sub")).expect("create_dir_all");
        std::fs::write(&a, "1").expect("write");
        std::fs::write(&b, "1").expect("write");

        watcher.watch_nonrecursively(&a);
        watcher.watch_nonrecursively(&b);
        assert!(
            watcher
                .watcher
                .get_chain_handles()
                .is_superset(&HashSet::from([
                    tmpdir.parent_path_buf(),
                    tmpdir.to_path_buf()
                ]))
        );

        std::fs::rename(&lib, &moved).expect("rename away");
        rx.wait_unordered_exact([expected(&a).remove_file(), expected(&b).remove_file()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));

        std::fs::rename(&moved, &lib).expect("rename back");
        rx.wait_unordered_exact([expected(&a).create_file(), expected(&b).create_file()])
            .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([lib.clone(), lib.join("sub"), a.clone(), b])
        );

        std::fs::write(&a, "2").expect("write");
        expect_write(&rx, &a);
    }

    #[test]
    fn track_path_reports_a_directory_root_when_an_ancestor_is_removed() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let parent = tmpdir.path().join("parent");
        let child = parent.join("child");
        std::fs::create_dir_all(&child).expect("create_dir_all");

        watcher.watch_recursively(&child);
        std::fs::remove_dir_all(&parent).expect("remove_dir_all");
        rx.wait_unordered([expected(&child).remove_any()]);

        std::fs::create_dir_all(&child).expect("create_dir_all");
        rx.wait_unordered([expected(&child).create_folder()]);
        // The round trip waits for the watch on the new directory to be registered. A root found
        // by the scan of its parent is reported before that, which is an old limit of this
        // backend: a file created in it right on hearing of it can be missed.
        assert!(watcher.get_watch_handles().contains(&child));

        std::fs::File::create_new(child.join("file")).expect("create");
        rx.wait_unordered([expected(child.join("file")).create_file()]);
    }

    #[test]
    fn unwatch_drops_the_ancestor_watches() {
        let tmpdir = testdir();
        let (mut watcher, _rx) = watcher();

        let path = tmpdir.path().join("entry");
        std::fs::write(&path, "1").expect("write");

        watcher.watch_nonrecursively(&path);
        assert!(!watcher.watcher.get_chain_handles().is_empty());

        watcher.watcher.unwatch(&path).expect("unwatch");
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));
        assert_eq!(watcher.watcher.get_chain_handles(), HashSet::from([]));
    }

    #[test]
    fn write_file() {
        let tmpdir = testdir();

        let path = tmpdir.path().join("entry");
        std::fs::File::create_new(&path).expect("create");

        let (mut watcher, rx) = watcher();

        watcher.watch_recursively(&tmpdir);

        std::fs::write(&path, b"123").expect("write");

        expect_write(&rx, &path);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf(), path]),
        );
    }

    #[test]
    fn chmod_file() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        let file = std::fs::File::create_new(&path).expect("create");
        let mut permissions = file.metadata().expect("metadata").permissions();
        permissions.set_readonly(true);

        watcher.watch_recursively(&tmpdir);
        file.set_permissions(permissions).expect("set_permissions");

        rx.wait_ordered_exact([expected(&path).modify_meta_any()]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf(), path]),
        );
    }

    #[test]
    fn rename_file() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        std::fs::File::create_new(&path).expect("create");

        watcher.watch_recursively(&tmpdir);
        let new_path = tmpdir.path().join("renamed");

        std::fs::rename(&path, &new_path).expect("rename");

        rx.wait_ordered_exact([
            expected(&new_path).create_file(),
            expected(path).rename_any(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf(), new_path]),
        );
    }

    #[test]
    fn rename_self_file() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        std::fs::File::create_new(&path).expect("create");

        watcher.watch_recursively(&path);
        let new_path = tmpdir.path().join("renamed");

        std::fs::rename(&path, &new_path).expect("rename");

        rx.wait_ordered_exact([expected(&path).rename_any()])
            .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()]),
        );

        std::fs::rename(&new_path, &path).expect("rename2");

        rx.wait_ordered_exact([expected(&path).create_file()])
            .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf(), path]),
        );
    }

    #[test]
    fn rename_self_file_no_track() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        std::fs::File::create_new(&path).expect("create");

        watcher.watch(
            &path,
            WatchMode {
                recursive_mode: RecursiveMode::NonRecursive,
                target_mode: TargetMode::NoTrack,
            },
        );

        let new_path = tmpdir.path().join("renamed");

        std::fs::rename(&path, &new_path).expect("rename");

        rx.wait_ordered_exact([expected(&path).rename_any()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]),);

        let result = watcher.watcher.watch(
            &path,
            WatchMode {
                recursive_mode: RecursiveMode::NonRecursive,
                target_mode: TargetMode::NoTrack,
            },
        );
        assert!(matches!(
            result,
            Err(Error {
                paths: _,
                kind: ErrorKind::PathNotFound
            })
        ));
    }

    #[test]
    fn delete_file() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();
        let file = tmpdir.path().join("file");
        std::fs::write(&file, "").expect("write");

        watcher.watch_nonrecursively(&tmpdir);

        std::fs::remove_file(&file).expect("remove");

        rx.wait_ordered_exact([
            expected(&file).modify_any(),
            expected(&file).remove_any(),
            expected(tmpdir.path()).modify_data_any(),
            expected(&file).remove_any().optional().multiple(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf()]),
        );
    }

    #[test]
    fn delete_self_file() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();
        let file = tmpdir.path().join("file");
        std::fs::write(&file, "").expect("write");

        watcher.watch_nonrecursively(&file);

        std::fs::remove_file(&file).expect("remove");

        rx.wait_ordered_exact([
            expected(&file).modify_any(),
            expected(&file).remove_any(),
            expected(&file).remove_any().optional().multiple(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()])
        );

        std::fs::write(&file, "").expect("write");

        rx.wait_ordered_exact([expected(&file).create_file()]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf(), file])
        );
    }

    #[test]
    fn delete_self_file_no_track() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();
        let file = tmpdir.path().join("file");
        std::fs::write(&file, "").expect("write");

        watcher.watch(
            &file,
            WatchMode {
                recursive_mode: RecursiveMode::NonRecursive,
                target_mode: TargetMode::NoTrack,
            },
        );

        std::fs::remove_file(&file).expect("remove");

        rx.wait_ordered_exact([
            expected(&file).modify_any(),
            expected(&file).remove_any(),
            expected(&file).remove_any().optional().multiple(),
        ])
        .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));

        std::fs::write(&file, "").expect("write");

        rx.ensure_empty_with_wait();
    }

    #[test]
    fn create_write_overwrite() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();
        let overwritten_file = tmpdir.path().join("overwritten_file");
        let overwriting_file = tmpdir.path().join("overwriting_file");
        std::fs::write(&overwritten_file, "123").expect("write1");

        watcher.watch_nonrecursively(&tmpdir);

        std::fs::File::create(&overwriting_file).expect("create");
        std::fs::write(&overwriting_file, "321").expect("write2");
        std::fs::rename(&overwriting_file, &overwritten_file).expect("rename");

        rx.wait_ordered([
            // create for overwriting_file can be
            // create_file or create_other and may be missing
            expected(&overwriting_file).modify_data_size().optional(),
            expected(&overwriting_file)
                .modify_data_any()
                .optional()
                .multiple(),
            expected(&overwritten_file).create_file(),
            expected(&overwriting_file).rename_any().optional(),
            expected(&overwriting_file).remove_any().optional(),
        ]);
        assert!(
            // overwriting_file is sometimes included
            // this happens when the rename happens right before
            // the pathname -> file descriptor resolution is done
            watcher.get_watch_handles().is_superset(&HashSet::from([
                tmpdir.parent_path_buf(),
                tmpdir.to_path_buf(),
                overwritten_file
            ]))
        );
    }

    fn assert_track_path_continues_after_recreating_file_in_nested_directory(
        upgrade_from_no_track: bool,
    ) {
        let tmpdir = testdir();
        let (mut watcher, mut rx) = watcher();
        let nested_dir = tmpdir.path().join("nested");
        let watched_file = nested_dir.join("watched");
        let moved_file = tmpdir.path().join("moved");
        std::fs::create_dir(&nested_dir).expect("create nested dir");
        std::fs::write(&watched_file, "initial").expect("write watched file");

        watcher.watch_nonrecursively(&tmpdir);
        if upgrade_from_no_track {
            watcher.watch(
                &watched_file,
                WatchMode {
                    recursive_mode: RecursiveMode::NonRecursive,
                    target_mode: TargetMode::NoTrack,
                },
            );
        }
        watcher.watch_nonrecursively(&watched_file);

        std::fs::rename(&watched_file, &moved_file).expect("move watched file");
        std::fs::copy(&moved_file, &watched_file).expect("recreate watched file");
        std::fs::remove_file(&moved_file).expect("remove moved file");

        // Wait until the replacement events are drained before checking the next write.
        for _ in rx.iter() {}

        std::fs::write(&watched_file, "updated").expect("update watched file");
        let received_change = rx.iter().any(|event| {
            event.paths.iter().any(|path| path == &watched_file)
                && matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(ModifyKind::Data(_))
                )
        });

        assert!(
            received_change,
            "expected a change event after recreating the watched file"
        );
    }

    #[test]
    fn track_path_continues_after_recreating_file_in_nested_directory() {
        assert_track_path_continues_after_recreating_file_in_nested_directory(false);
    }

    #[test]
    fn track_path_upgrade_continues_after_recreating_file_in_nested_directory() {
        assert_track_path_continues_after_recreating_file_in_nested_directory(true);
    }

    #[test]
    fn create_dir() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();
        watcher.watch_recursively(&tmpdir);

        let path = tmpdir.path().join("entry");
        std::fs::create_dir(&path).expect("create");

        rx.wait_ordered_exact([
            expected(&path).create_folder(),
            expected(tmpdir.path()).modify_any(),
            // The attributes of a new directory may still settle after its watch is registered.
            expected(&path).modify_meta_any().optional(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf(), path]),
        );
    }

    #[test]
    fn chmod_dir() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        std::fs::create_dir(&path).expect("create_dir");
        let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
        permissions.set_readonly(true);

        watcher.watch_recursively(&tmpdir);
        std::fs::set_permissions(&path, permissions).expect("set_permissions");

        rx.wait_ordered_exact([expected(&path).modify_meta_any()])
            .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf(), path]),
        );
    }

    #[test]
    fn rename_dir() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        let new_path = tmpdir.path().join("new_path");
        std::fs::create_dir(&path).expect("create_dir");

        watcher.watch_recursively(&tmpdir);
        std::fs::rename(&path, &new_path).expect("rename");

        rx.wait_ordered_exact([
            expected(&new_path).create_folder(),
            expected(&path).rename_any(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf(), new_path]),
        );
    }

    #[test]
    fn delete_dir() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        std::fs::create_dir(&path).expect("create_dir");

        watcher.watch_recursively(&tmpdir);
        std::fs::remove_dir(&path).expect("remove");

        rx.wait_ordered_exact([
            expected(tmpdir.path()).modify_data_any().optional(),
            expected(&path).modify_any(),
            expected(&path).remove_any(),
            expected(&path).remove_any().optional().multiple(),
            expected(tmpdir.path()).modify_data_any().optional(),
            expected(tmpdir.path()).modify_any(),
            expected(&path).remove_any().optional().multiple(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf()]),
        );
    }

    #[test]
    fn delete_self_dir() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        std::fs::create_dir(&path).expect("create_dir");

        watcher.watch_recursively(&path);
        std::fs::remove_dir(&path).expect("remove");

        rx.wait_ordered_exact([
            expected(&path).modify_any(),
            expected(&path).remove_any(),
            expected(&path).remove_any().optional().multiple(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()]),
        );

        std::fs::create_dir(&path).expect("create_dir2");

        rx.wait_ordered_exact([
            expected(&path).create_folder(),
            // The attributes of a new directory may still settle after its watch is registered.
            expected(&path).modify_meta_any().optional(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf(), path.clone()])
        );
    }

    #[test]
    fn delete_self_dir_no_track() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        std::fs::create_dir(&path).expect("create_dir");

        watcher
            .watcher
            .watch(
                &path,
                WatchMode {
                    recursive_mode: RecursiveMode::Recursive,
                    target_mode: TargetMode::NoTrack,
                },
            )
            .expect("watch");
        std::fs::remove_dir(&path).expect("remove");

        rx.wait_ordered_exact([
            expected(tmpdir.path()).modify_data_any().optional(),
            expected(&path).modify_any(),
            expected(&path).remove_any(),
            expected(tmpdir.path()).modify_data_any().optional(),
            expected(&path).remove_any().optional().multiple(),
        ])
        .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]),);

        std::fs::create_dir(&path).expect("create_dir2");

        rx.ensure_empty_with_wait();
    }

    #[test]
    fn rename_dir_twice() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        let new_path = tmpdir.path().join("new_path");
        let new_path2 = tmpdir.path().join("new_path2");
        std::fs::create_dir(&path).expect("create_dir");

        watcher.watch_recursively(&tmpdir);
        std::fs::rename(&path, &new_path).expect("rename");
        std::fs::rename(&new_path, &new_path2).expect("rename2");

        rx.wait_unordered([
            expected(&path).rename_any(),
            expected(&new_path2).create_folder(),
        ]);
        assert!(
            // new_path is sometimes included
            // this happens when the rename happens right before
            // the pathname -> file descriptor resolution is done
            watcher.get_watch_handles().is_superset(&HashSet::from([
                tmpdir.parent_path_buf(),
                tmpdir.to_path_buf(),
                new_path2
            ]))
        );
    }

    #[test]
    fn move_out_of_watched_dir() {
        let tmpdir = testdir();
        let subdir = tmpdir.path().join("subdir");
        let (mut watcher, rx) = watcher();

        let path = subdir.join("entry");
        std::fs::create_dir_all(&subdir).expect("create_dir_all");
        std::fs::File::create_new(&path).expect("create");

        watcher.watch_recursively(&subdir);
        let new_path = tmpdir.path().join("entry");

        std::fs::rename(&path, &new_path).expect("rename");

        rx.wait_ordered_exact([
            expected(&subdir).modify_data_any(),
            expected(&path).rename_any(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf(), subdir])
        );
    }

    #[test]
    fn create_write_write_rename_write_remove() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let file1 = tmpdir.path().join("entry");
        let file2 = tmpdir.path().join("entry2");
        std::fs::File::create_new(&file2).expect("create file2");
        let new_path = tmpdir.path().join("renamed");

        watcher.watch_recursively(&tmpdir);
        std::fs::write(&file1, "123").expect("write 1");
        std::fs::write(&file2, "321").expect("write 2");
        std::fs::rename(&file1, &new_path).expect("rename");
        std::fs::write(&new_path, b"1").expect("write 3");
        std::fs::remove_file(&new_path).expect("remove");

        rx.wait_ordered([
            expected(&file2).modify_data_size().optional(),
            expected(&file2).modify_data_any(),
            expected(&new_path).create_file().optional(),
            expected(&file1).rename_any().optional(),
            expected(&new_path).remove_any().optional(),
        ]);
        assert!(
            // new_path is sometimes included
            // this happens when the rename happens right before
            // the pathname -> file descriptor resolution is done
            watcher.get_watch_handles().is_superset(&HashSet::from([
                tmpdir.parent_path_buf(),
                tmpdir.to_path_buf(),
                file2
            ]))
        );
    }

    #[test]
    fn rename_twice() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        std::fs::File::create_new(&path).expect("create");

        watcher.watch_recursively(&tmpdir);
        let new_path1 = tmpdir.path().join("renamed1");
        let new_path2 = tmpdir.path().join("renamed2");

        std::fs::rename(&path, &new_path1).expect("rename1");
        std::fs::rename(&new_path1, &new_path2).expect("rename2");

        rx.wait_ordered([
            // create for new_path1 can be
            // create_file or create_other and may be missing
            expected(&path).rename_any().optional(),
            expected(&new_path1).remove_any().optional(),
            expected(&new_path2).create_file(),
            expected(&path).rename_any().optional(),
            expected(&new_path1).rename_any().optional(),
            expected(&new_path1).remove_any().optional(),
            expected(&new_path2).create_file().optional(),
        ])
        .ensure_no_tail();
        assert!(
            // new_path1 is sometimes included
            // this happens when the rename happens right before
            // the pathname -> file descriptor resolution is done
            watcher.get_watch_handles().is_superset(&HashSet::from([
                tmpdir.parent_path_buf(),
                tmpdir.to_path_buf(),
                new_path2
            ]))
        );
    }

    #[test]
    fn set_file_mtime() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        let file = std::fs::File::create_new(&path).expect("create");

        watcher.watch_recursively(&tmpdir);

        file.set_modified(
            std::time::SystemTime::now()
                .checked_sub(Duration::from_secs(60 * 60))
                .expect("time"),
        )
        .expect("set_time");

        rx.wait_ordered_exact([expected(&path).modify_meta_any()])
            .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf(), path]),
        );
    }

    #[test]
    fn write_file_non_recursive_watch() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        std::fs::File::create_new(&path).expect("create");

        watcher.watch_nonrecursively(&path);

        std::fs::write(&path, b"123").expect("write");

        expect_write(&rx, &path);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf(), path]),
        );
    }

    #[test]
    fn write_to_a_hardlink_pointed_to_the_watched_file_triggers_an_event() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let subdir = tmpdir.path().join("subdir");
        let subdir2 = tmpdir.path().join("subdir2");
        let file = subdir.join("file");
        let hardlink = tmpdir.path().join("hardlink");

        std::fs::create_dir(&subdir).expect("create");
        std::fs::create_dir(&subdir2).expect("create2");
        std::fs::write(&file, "").expect("file");
        std::fs::hard_link(&file, &hardlink).expect("hardlink");

        watcher.watch_nonrecursively(&file);

        std::fs::write(&hardlink, "123123").expect("write to the hard link");

        expect_write(&rx, &file);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([subdir, file]));
    }

    #[test]
    #[ignore = "similar to https://github.com/notify-rs/notify/issues/727"]
    fn recursive_creation() {
        let tmpdir = testdir();
        let nested1 = tmpdir.path().join("1");
        let nested2 = tmpdir.path().join("1/2");
        let nested3 = tmpdir.path().join("1/2/3");
        let nested4 = tmpdir.path().join("1/2/3/4");
        let nested5 = tmpdir.path().join("1/2/3/4/5");
        let nested6 = tmpdir.path().join("1/2/3/4/5/6");
        let nested7 = tmpdir.path().join("1/2/3/4/5/6/7");
        let nested8 = tmpdir.path().join("1/2/3/4/5/6/7/8");
        let nested9 = tmpdir.path().join("1/2/3/4/5/6/7/8/9");

        let (mut watcher, rx) = watcher();

        watcher.watch_recursively(&tmpdir);

        std::fs::create_dir_all(&nested9).expect("create_dir_all");
        rx.wait_ordered([
            expected(&nested1).create_folder(),
            expected(&nested2).create_folder(),
            expected(&nested3).create_folder(),
            expected(&nested4).create_folder(),
            expected(&nested5).create_folder(),
            expected(&nested6).create_folder(),
            expected(&nested7).create_folder(),
            expected(&nested8).create_folder(),
            expected(&nested9).create_folder(),
        ]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([
                tmpdir.to_path_buf(),
                nested1,
                nested2,
                nested3,
                nested4,
                nested5,
                nested6,
                nested7,
                nested8,
                nested9
            ])
        );
    }

    #[test]
    fn upgrade_to_recursive() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("upgrade");
        let deep = tmpdir.path().join("upgrade/deep");
        let file = tmpdir.path().join("upgrade/deep/file");
        std::fs::create_dir_all(&deep).expect("create_dir");

        watcher.watch_nonrecursively(&path);
        std::fs::File::create_new(&file).expect("create");
        std::fs::remove_file(&file).expect("delete");

        rx.wait_ordered_exact([expected(&deep).modify_data_any().optional().multiple()]);

        watcher.watch_recursively(&path);
        std::fs::File::create_new(&file).expect("create");

        rx.wait_ordered_exact([
            expected(&deep).modify_data_any().optional().multiple(),
            expected(&file).create_file(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf(), path, deep, file])
        );
    }

    fn no_track(recursive_mode: RecursiveMode) -> WatchMode {
        WatchMode {
            recursive_mode,
            target_mode: TargetMode::NoTrack,
        }
    }

    /// Waits for the watcher to report an error, skipping the events before it.
    fn wait_error(rx: &test::Receiver) -> Error {
        loop {
            match rx.try_recv() {
                Ok(Err(err)) => return err,
                Ok(Ok(_)) => {}
                Err(err) => panic!("no error from the watcher: {err:?}"),
            }
        }
    }

    /// A directory made unreadable for the test, and readable again when dropped.
    struct Locked(PathBuf);

    impl Locked {
        /// `None` when running as root, which no permission locks out: the tests that need the
        /// lock then return early, and say so on stderr.
        fn lock(path: &Path) -> Option<Self> {
            Self::lock_with_mode(path, 0o000)
        }

        /// Like [`Self::lock`], but the directory can still be searched and written to: only
        /// listing it and opening it fail.
        fn lock_listing(path: &Path) -> Option<Self> {
            Self::lock_with_mode(path, 0o311)
        }

        fn lock_with_mode(path: &Path, mode: u32) -> Option<Self> {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
            let locked = Self(path.to_path_buf());
            std::fs::File::open(path).is_err().then_some(locked)
        }

        /// The directory was renamed: unlock it where it is now.
        fn moved_to(&mut self, path: &Path) {
            self.0 = path.to_path_buf();
        }
    }

    impl Drop for Locked {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755))
                .expect("chmod back");
        }
    }

    /// Waits for the events of a write to `path`. The kernel may post the data and the size
    /// change in separate kevents, so they come in any order.
    fn expect_write(rx: &test::Receiver, path: &Path) {
        rx.wait_unordered_exact([
            expected(path).modify_meta_any().optional(),
            expected(path).modify_data_any(),
            expected(path).modify_data_size(),
            expected(path).modify_meta_any().optional(),
        ])
        .ensure_no_tail();
    }

    /// Holds the event loop on the first event for its trigger path until released, so that
    /// whatever the test does meanwhile is read in one batch.
    struct Hold {
        held: std::sync::mpsc::Receiver<()>,
        release: std::sync::mpsc::Sender<()>,
    }

    impl Hold {
        /// Waits for the event loop to be held.
        fn wait(&self) {
            self.held
                .recv_timeout(Duration::from_secs(5))
                .expect("no event for the trigger");
        }

        fn release(self) {
            self.release.send(()).expect("release the event loop");
        }
    }

    /// A watcher whose handler holds the event loop on the first event for `trigger`.
    fn held_watcher(trigger: &Path) -> (TestWatcher<KqueueWatcher>, test::Receiver, Hold) {
        let trigger = trigger.to_path_buf();
        held_watcher_on(move |event| matches!(event, Ok(event) if event.paths == [trigger.clone()]))
    }

    /// A watcher whose handler holds the event loop on the first event that `hold_on` accepts.
    fn held_watcher_on(
        hold_on: impl Fn(&Result<Event>) -> bool + Send + 'static,
    ) -> (TestWatcher<KqueueWatcher>, test::Receiver, Hold) {
        let (tx, rx) = std::sync::mpsc::channel();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let mut hold = Some((held_tx, release_rx));
        let handler = move |event: Result<Event>| {
            let is_trigger = hold_on(&event);
            tx.send(event).ok();
            if is_trigger && let Some((held, release)) = hold.take() {
                held.send(()).ok();
                release.recv().ok();
            }
        };
        let watcher = KqueueWatcher::new(handler, Config::default()).expect("watcher");
        (
            TestWatcher {
                watcher,
                kind: crate::WatcherKind::Kqueue,
            },
            test::Receiver {
                rx,
                timeout: Duration::from_secs(1),
                detect_changes: None,
                kind: crate::WatcherKind::Kqueue,
            },
            Hold {
                held: held_rx,
                release: release_tx,
            },
        )
    }

    /// Whether `events` hold a creation of `path`.
    fn created_in(events: &[Event], path: &Path) -> bool {
        events
            .iter()
            .any(|event| event.paths == [path] && matches!(event.kind, EventKind::Create(_)))
    }

    #[test]
    fn track_path_follows_a_symlinked_ancestor() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let real = tmpdir.path().join("real");
        let link = tmpdir.path().join("link");
        let file = link.join("proj").join("a.js");
        std::fs::create_dir_all(real.join("proj")).expect("create_dir_all");
        std::fs::write(real.join("proj").join("a.js"), "").expect("write");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        watcher.watch_nonrecursively(&file);
        assert!(
            watcher
                .get_watch_handles()
                .is_superset(&HashSet::from([link.join("proj"), file.clone()]))
        );

        // An unrelated entry next to the symlink changes nothing for the root.
        std::fs::write(tmpdir.path().join("unrelated"), "").expect("write");
        std::fs::write(&file, "123").expect("write");
        expect_write(&rx, &file);

        std::fs::remove_file(&link).expect("remove symlink");
        rx.wait_ordered_exact([expected(&file).remove_file()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));

        std::os::unix::fs::symlink(&real, &link).expect("symlink again");
        rx.wait_ordered_exact([expected(&file).create_file()])
            .ensure_no_tail();

        std::fs::write(&file, "123456").expect("write");
        rx.wait_unordered([expected(&file).modify_data_any()]);
    }

    #[test]
    fn track_path_reports_a_root_below_a_symlinked_ancestor_that_appears() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let x = tmpdir.path().join("x");
        let y = tmpdir.path().join("y");
        let file = x.join("link").join("sub").join("a.js");
        std::fs::create_dir(&x).expect("create_dir");
        std::fs::create_dir_all(y.join("sub")).expect("create_dir_all");
        std::fs::write(y.join("sub").join("a.js"), "").expect("write");

        watcher.watch_nonrecursively(&file);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));
        assert!(watcher.watcher.get_chain_handles().contains(&x));

        std::os::unix::fs::symlink(&y, x.join("link")).expect("symlink");
        rx.wait_ordered_exact([expected(&file).create_file()])
            .ensure_no_tail();

        std::fs::write(&file, "123").expect("write");
        rx.wait_unordered([expected(&file).modify_data_any()]);
    }

    #[test]
    fn unwatch_keeps_a_root_that_is_an_ancestor_as_a_chain_handle() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let dir = tmpdir.path().join("dir");
        let sub = dir.join("sub");
        let file = sub.join("f.js");
        std::fs::create_dir_all(&sub).expect("create_dir_all");
        std::fs::write(&file, "").expect("write");

        watcher.watch_nonrecursively(&dir);
        watcher.watch_nonrecursively(&file);
        watcher.watcher.unwatch(&dir).expect("unwatch");
        let handles = watcher.get_watch_handles();
        assert!(handles.is_superset(&HashSet::from([sub.clone(), file.clone()])));
        assert!(!handles.contains(&dir));
        assert!(watcher.watcher.get_chain_handles().contains(&dir));

        std::fs::rename(&sub, dir.join("gone")).expect("rename away");
        rx.wait_ordered_exact([expected(&file).remove_file()])
            .ensure_no_tail();

        std::fs::create_dir(&sub).expect("create_dir");
        std::fs::write(&file, "").expect("write");
        rx.wait_ordered([expected(&file).create_file()]);
    }

    #[test]
    fn unwatch_of_a_recursive_root_keeps_the_tracked_roots_below_it() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let parent = tmpdir.path().join("parent");
        let a = parent.join("a");
        let file = a.join("b.js");
        std::fs::create_dir_all(&a).expect("create_dir_all");
        std::fs::write(&file, "").expect("write");

        watcher.watch_recursively(&parent);
        watcher.watch_nonrecursively(&file);
        watcher.watcher.unwatch(&parent).expect("unwatch");
        assert!(
            watcher
                .get_watch_handles()
                .is_superset(&HashSet::from([a.clone(), file.clone()]))
        );
        assert!(watcher.watcher.get_chain_handles().contains(&parent));

        std::fs::write(&file, "123").expect("write");
        expect_write(&rx, &file);

        // In two steps, each waited for: a directory removed while its write is looked at would
        // report a spurious error.
        std::fs::remove_file(&file).expect("remove b.js");
        rx.wait_unordered([expected(&file).remove_any()]);
        assert!(!watcher.get_watch_handles().contains(&file));
        std::fs::remove_dir(&a).expect("remove a");
        assert!(rx.sleep_until(|| !watcher.get_watch_handles().contains(&a)));

        std::fs::create_dir(&a).expect("create_dir");
        std::fs::write(&file, "").expect("write");
        rx.wait_ordered([expected(&file).create_file()]);
    }

    #[test]
    fn unwatch_of_a_file_below_a_recursive_root_keeps_its_handle() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let parent = tmpdir.path().join("parent");
        let file = parent.join("a").join("b.js");
        std::fs::create_dir_all(file.parent().unwrap()).expect("create_dir_all");
        std::fs::write(&file, "").expect("write");

        watcher.watch_recursively(&parent);
        watcher.watch_nonrecursively(&file);
        watcher.watcher.unwatch(&file).expect("unwatch");
        assert!(watcher.get_watch_handles().contains(&file));

        std::fs::write(&file, "123").expect("write");
        expect_write(&rx, &file);
    }

    #[test]
    fn unwatch_keeps_the_entry_handle_of_a_directory_root() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let lib = tmpdir.path().join("lib");
        let file = lib.join("a.js");
        std::fs::create_dir(&lib).expect("create_dir");
        std::fs::write(&file, "").expect("write");

        watcher.watch_nonrecursively(&tmpdir);
        watcher.watch_nonrecursively(&file);
        watcher.watcher.unwatch(&file).expect("unwatch");
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf(), lib.clone()])
        );

        let lib2 = tmpdir.path().join("lib2");
        std::fs::rename(&lib, &lib2).expect("rename");
        rx.wait_unordered_exact([expected(&lib2).create_folder(), expected(&lib).rename_any()]);
    }

    #[test]
    fn no_track_roots_follow_the_entity_when_an_ancestor_moves() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let lib = tmpdir.path().join("lib");
        let moved = tmpdir.path().join("moved");
        let a = lib.join("a.js");
        let n = lib.join("n.js");
        let ndir = lib.join("ndir");
        std::fs::create_dir_all(&ndir).expect("create_dir_all");
        std::fs::write(&a, "").expect("write");
        std::fs::write(&n, "").expect("write");

        watcher.watch_nonrecursively(&a);
        watcher.watch(&n, no_track(RecursiveMode::NonRecursive));
        watcher.watch(&ndir, no_track(RecursiveMode::Recursive));

        std::fs::rename(&lib, &moved).expect("rename away");
        rx.wait_ordered_exact([expected(&a).remove_file()])
            .ensure_no_tail();
        assert!(
            watcher
                .get_watch_handles()
                .is_superset(&HashSet::from([n.clone(), ndir.clone()]))
        );

        // The moved file still reports, under the name it was watched by.
        std::fs::write(moved.join("n.js"), "123").expect("write");
        expect_write(&rx, &n);

        std::fs::rename(&moved, &lib).expect("rename back");
        rx.wait_ordered_exact([expected(&a).create_file()])
            .ensure_no_tail();

        let g = ndir.join("g");
        std::fs::write(&g, "").expect("write");
        rx.wait_ordered([expected(&g).create_file()]);

        watcher.watcher.unwatch(&n).expect("unwatch n.js");
        watcher.watcher.unwatch(&ndir).expect("unwatch ndir");
    }

    #[test]
    fn unwatch_of_a_missing_root_drops_the_ancestor_watches() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let missing = tmpdir.path().join("pp").join("root");
        watcher.watch_nonrecursively(&missing);
        assert!(!watcher.watcher.get_chain_handles().is_empty());
        watcher.watcher.unwatch(&missing).expect("unwatch missing");
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));
        assert_eq!(watcher.watcher.get_chain_handles(), HashSet::from([]));

        let deleted = tmpdir.path().join("del.js");
        std::fs::write(&deleted, "").expect("write");
        watcher.watch_nonrecursively(&deleted);
        std::fs::remove_file(&deleted).expect("remove");
        rx.wait_ordered_exact([
            expected(&deleted).modify_any(),
            expected(&deleted).remove_any(),
            expected(&deleted).remove_any().optional().multiple(),
        ])
        .ensure_no_tail();
        watcher.watcher.unwatch(&deleted).expect("unwatch deleted");
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));
        assert_eq!(watcher.watcher.get_chain_handles(), HashSet::from([]));
    }

    #[test]
    fn no_track_recursive_root_survives_a_subdirectory_creation() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let dir = tmpdir.path().join("dir");
        let sub = dir.join("sub");
        std::fs::create_dir(&dir).expect("create_dir");

        watcher.watch(&dir, no_track(RecursiveMode::Recursive));
        std::fs::create_dir(&sub).expect("create_dir sub");
        rx.wait_ordered_exact([expected(&sub).create_folder(), expected(&dir).modify_any()]);
        // The round trip waits for the watch on the new directory to be registered.
        assert!(watcher.get_watch_handles().contains(&sub));

        let f = sub.join("f");
        let g = dir.join("g");
        std::fs::write(&f, "").expect("write f");
        std::fs::write(&g, "").expect("write g");
        rx.wait_unordered([expected(&f).create_file(), expected(&g).create_file()]);

        watcher.watcher.unwatch(&dir).expect("unwatch");
    }

    #[test]
    fn watch_below_an_unreadable_ancestor_fails() {
        let tmpdir = testdir();
        let (mut watcher, _rx) = watcher();

        let locked = tmpdir.path().join("locked");
        let file = locked.join("inner").join("f.js");
        std::fs::create_dir_all(file.parent().unwrap()).expect("create_dir_all");
        let Some(_locked) = Locked::lock(&locked) else {
            eprintln!("skipped: running as root, which no permission locks out");
            return;
        };

        let result = watcher.watcher.watch(&file, WatchMode::non_recursive());
        assert!(
            matches!(
                &result,
                Err(Error {
                    kind: ErrorKind::Io(err),
                    ..
                }) if err.kind() == std::io::ErrorKind::PermissionDenied
            ),
            "{result:?}"
        );
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));
        assert_eq!(watcher.watcher.get_chain_handles(), HashSet::from([]));
    }

    #[test]
    fn watching_a_missing_root_again_arms_it() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let p = tmpdir.path().join("p");
        let p2 = tmpdir.path().join("p2");
        let dir = p.join("d");
        let file = dir.join("f.js");
        std::fs::create_dir_all(&dir).expect("create_dir_all");
        std::fs::write(&file, "").expect("write");

        watcher.watch_nonrecursively(&dir);
        std::fs::rename(&p, &p2).expect("rename away");
        rx.wait_ordered_exact([expected(&dir).remove_folder()])
            .ensure_no_tail();

        let Some(mut locked) = Locked::lock(&p2.join("d")) else {
            eprintln!("skipped: running as root, which no permission locks out");
            return;
        };
        std::fs::rename(&p2, &p).expect("rename back");
        locked.moved_to(&dir);
        let error = wait_error(&rx);
        assert!(matches!(error.kind, ErrorKind::Io(_)), "{error:?}");
        assert!(!watcher.get_watch_handles().contains(&dir));

        drop(locked);
        watcher.watch_nonrecursively(&dir);
        rx.wait_ordered_exact([expected(&dir).create_folder()])
            .ensure_no_tail();

        std::fs::write(&file, "123").expect("write");
        expect_write(&rx, &file);
    }

    #[test]
    fn watching_the_filesystem_root_is_not_silently_missing() {
        let (mut watcher, _rx) = watcher();

        let root = Path::new("/");
        match watcher.watcher.watch(root, WatchMode::non_recursive()) {
            Ok(()) => assert!(watcher.get_watch_handles().contains(root)),
            Err(err) => assert!(matches!(err.kind, ErrorKind::Io(_)), "{err:?}"),
        }
    }

    #[test]
    fn failed_watch_leaves_no_handles() {
        let tmpdir = testdir();
        let (mut watcher, _rx) = watcher();

        let y = tmpdir.path().join("x").join("y");
        let looped = y.join("loop");
        std::fs::create_dir_all(&y).expect("create_dir_all");
        std::os::unix::fs::symlink(&looped, &looped).expect("symlink");

        let result = watcher.watcher.watch(&looped, WatchMode::non_recursive());
        assert!(
            matches!(
                &result,
                Err(Error {
                    kind: ErrorKind::Io(err),
                    ..
                }) if err.raw_os_error() == Some(libc::ELOOP)
            ),
            "{result:?}"
        );
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));
        assert_eq!(watcher.watcher.get_chain_handles(), HashSet::from([]));
    }

    #[test]
    fn writes_to_a_chain_ancestor_are_ignored() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let file = tmpdir.path().join("a").join("b").join("f.js");
        std::fs::create_dir_all(file.parent().unwrap()).expect("create_dir_all");
        std::fs::write(&file, "").expect("write");

        watcher.watch_nonrecursively(&file);
        let a = tmpdir.path().join("a");
        assert!(watcher.watcher.get_chain_handles().contains(&a));

        // Its entries are not listed: listing it fails now, which would report an error. As
        // root, which no permission locks out, only the events are looked at.
        let _locked = Locked::lock_listing(&a);
        for i in 0..3 {
            std::fs::write(a.join(format!("x{i}")), "").expect("write");
        }
        std::fs::write(&file, "123").expect("write");
        expect_write(&rx, &file);
    }

    #[test]
    fn unwatch_of_a_directory_root_drops_the_entry_handles() {
        let tmpdir = testdir();
        let (mut watcher, _rx) = watcher();

        let dir = tmpdir.path().join("dir");
        let e1 = dir.join("e1");
        let e2 = dir.join("e2");
        std::fs::create_dir(&dir).expect("create_dir");
        std::fs::write(&e1, "").expect("write");
        std::fs::create_dir(&e2).expect("create_dir e2");

        watcher.watch_nonrecursively(&dir);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf(), dir.clone(), e1, e2])
        );

        watcher.watcher.unwatch(&dir).expect("unwatch");
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));
        assert_eq!(watcher.watcher.get_chain_handles(), HashSet::from([]));
    }

    #[test]
    fn directory_root_replaced_by_a_file_keeps_its_handle() {
        let tmpdir = testdir();
        let (mut watcher, mut rx) = watcher();

        let lib = tmpdir.path().join("lib");
        let a = lib.join("a.js");
        std::fs::create_dir(&lib).expect("create_dir");
        std::fs::write(&a, "").expect("write");

        watcher.watch_nonrecursively(&lib);
        watcher.watch_nonrecursively(&a);

        // In two steps, each waited for: a directory removed while its write is looked at would
        // report a spurious error.
        std::fs::remove_file(&a).expect("remove a.js");
        rx.wait_unordered([expected(&a).remove_any()]);
        assert!(!watcher.get_watch_handles().contains(&a));
        std::fs::remove_dir(&lib).expect("remove lib");
        rx.wait_unordered([expected(&lib).remove_any()]);
        assert!(!watcher.get_watch_handles().contains(&lib));

        std::fs::write(&lib, "x").expect("write file at lib");
        rx.wait_ordered([expected(&lib).create_file()]);
        assert!(watcher.get_watch_handles().contains(&lib));

        // A sibling changes nothing for the file root, and the file keeps reporting.
        std::fs::write(tmpdir.path().join("sibling"), "").expect("write sibling");
        std::fs::write(&lib, "xyz").expect("write lib again");
        let events: Vec<Event> = rx.iter().collect();
        assert!(
            events.iter().any(|event| {
                event.paths == [lib.clone()]
                    && matches!(event.kind, EventKind::Modify(ModifyKind::Data(_)))
            }),
            "{events:#?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.kind, EventKind::Create(_))),
            "{events:#?}"
        );
    }

    #[test]
    fn failed_arm_leaves_the_root_missing_without_handles() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let p = tmpdir.path().join("p");
        let p2 = tmpdir.path().join("p2");
        let dir = p.join("d");
        let sub = dir.join("sub");
        let file = dir.join("f.js");
        std::fs::create_dir_all(&sub).expect("create_dir_all");
        std::fs::write(&file, "").expect("write");

        watcher.watch_recursively(&dir);
        std::fs::rename(&p, &p2).expect("rename away");
        rx.wait_ordered_exact([expected(&dir).remove_folder()])
            .ensure_no_tail();

        let Some(mut locked) = Locked::lock(&p2.join("d").join("sub")) else {
            eprintln!("skipped: running as root, which no permission locks out");
            return;
        };
        std::fs::rename(&p2, &p).expect("rename back");
        locked.moved_to(&sub);
        let error = wait_error(&rx);
        assert!(matches!(error.kind, ErrorKind::Io(_)), "{error:?}");
        assert_eq!(watcher.get_watch_handles(), HashSet::from([p]));

        drop(locked);
        watcher.watch_recursively(&dir);
        rx.wait_ordered_exact([expected(&dir).create_folder()])
            .ensure_no_tail();

        let s = sub.join("s.js");
        std::fs::write(&s, "").expect("write");
        rx.wait_ordered([expected(&s).create_file()]);
    }

    #[test]
    fn failed_watch_keeps_the_handles_of_the_roots_below() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let x = tmpdir.path().join("x");
        let a = x.join("a.js");
        let secret = x.join("secret");
        std::fs::create_dir_all(&secret).expect("create_dir_all");
        std::fs::write(&a, "").expect("write");

        watcher.watch_nonrecursively(&a);
        let handles = watcher.get_watch_handles();
        let chain = watcher.watcher.get_chain_handles();
        {
            let Some(_locked) = Locked::lock(&secret) else {
                eprintln!("skipped: running as root, which no permission locks out");
                return;
            };
            // The directory of the root, and one above it that is only its chain ancestor.
            for (path, mode) in [
                (x.as_path(), WatchMode::non_recursive()),
                (x.as_path(), WatchMode::recursive()),
                (tmpdir.path(), WatchMode::recursive()),
            ] {
                let result = watcher.watcher.watch(path, mode);
                assert!(
                    matches!(
                        &result,
                        Err(Error {
                            kind: ErrorKind::Io(err),
                            ..
                        }) if err.kind() == std::io::ErrorKind::PermissionDenied
                    ),
                    "{result:?}"
                );
                assert_eq!(watcher.get_watch_handles(), handles);
                assert_eq!(watcher.watcher.get_chain_handles(), chain);
            }
        }

        std::fs::write(&a, "123").expect("write");
        expect_write(&rx, &a);

        let moved = tmpdir.path().join("moved");
        std::fs::rename(&x, &moved).expect("rename away");
        rx.wait_ordered_exact([expected(&a).remove_file()])
            .ensure_no_tail();
        assert!(!watcher.get_watch_handles().contains(&a));
        std::fs::rename(&moved, &x).expect("rename back");
        rx.wait_ordered_exact([expected(&a).create_file()])
            .ensure_no_tail();
    }

    /// Creates `dir/stage`, a copy of `lib` with `sub/a.js` and `plain.js`.
    fn stage(dir: &Path) -> PathBuf {
        let stage = dir.join("stage");
        std::fs::create_dir_all(stage.join("sub")).expect("create_dir_all");
        std::fs::write(stage.join("sub").join("a.js"), "").expect("write");
        std::fs::write(stage.join("plain.js"), "").expect("write");
        stage
    }

    #[test]
    fn directory_root_swapped_for_a_staged_one_reports_the_root_below() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let lib = tmpdir.path().join("lib");
        let a = lib.join("sub").join("a.js");
        let plain = lib.join("plain.js");
        std::fs::create_dir_all(a.parent().unwrap()).expect("create_dir_all");
        std::fs::write(&a, "").expect("write");
        std::fs::write(&plain, "").expect("write");
        let stage = stage(tmpdir.path());

        watcher.watch_nonrecursively(&lib);
        watcher.watch_nonrecursively(&a);

        std::fs::rename(&lib, tmpdir.path().join("old")).expect("rename away");
        rx.wait_ordered([expected(&a).remove_file()]);
        // The round trip waits for the removal to be handled.
        assert!(!watcher.get_watch_handles().contains(&lib));
        std::fs::rename(&stage, &lib).expect("rename stage");
        rx.wait_ordered([expected(&a).create_file()]);

        std::fs::write(&a, "123").expect("write");
        rx.wait_unordered([expected(&a).modify_data_any()]);
        std::fs::write(&plain, "123").expect("write");
        rx.wait_unordered([expected(&plain).modify_data_any()]);
    }

    /// Watches the directory `lib` and the file `lib/sub/a.js`, and runs `replace` on the test
    /// directory while the event loop is held, so that the replacement of `lib` is read in one
    /// batch. The file is then reported removed and created, and both roots keep reporting.
    fn assert_replaced_in_one_batch(replace: impl FnOnce(&Path)) {
        let tmpdir = testdir();
        let trigger = tmpdir.path().join("trigger");
        let lib = tmpdir.path().join("lib");
        let a = lib.join("sub").join("a.js");
        let plain = lib.join("plain.js");
        std::fs::create_dir_all(a.parent().unwrap()).expect("create_dir_all");
        std::fs::write(&a, "").expect("write");
        std::fs::write(&plain, "").expect("write");
        std::fs::write(&trigger, "").expect("write");
        stage(tmpdir.path());

        let (mut watcher, rx, hold) = held_watcher(&trigger);
        watcher.watch_nonrecursively(&lib);
        watcher.watch_nonrecursively(&a);
        watcher.watch_nonrecursively(&trigger);

        std::fs::write(&trigger, "1").expect("write trigger");
        hold.wait();
        replace(tmpdir.path());
        hold.release();
        rx.wait_ordered([expected(&a).remove_file(), expected(&a).create_file()]);

        std::fs::write(&a, "123").expect("write");
        rx.wait_unordered([expected(&a).modify_data_any()]);
        std::fs::write(&plain, "123").expect("write");
        rx.wait_unordered([expected(&plain).modify_data_any()]);
    }

    #[test]
    fn directory_root_swapped_in_one_batch_reports_the_root_below() {
        assert_replaced_in_one_batch(|dir| {
            std::fs::rename(dir.join("lib"), dir.join("old")).expect("rename away");
            std::fs::rename(dir.join("stage"), dir.join("lib")).expect("rename stage");
        });
    }

    #[test]
    fn directory_root_removed_and_moved_in_in_one_batch_reports_the_root_below() {
        assert_replaced_in_one_batch(|dir| {
            std::fs::remove_dir_all(dir.join("lib")).expect("remove_dir_all");
            std::fs::rename(dir.join("stage"), dir.join("lib")).expect("rename stage");
        });
    }

    #[test]
    fn directory_root_regenerated_in_one_batch_reports_the_root_below() {
        assert_replaced_in_one_batch(|dir| {
            let lib = dir.join("lib");
            std::fs::remove_dir_all(&lib).expect("remove_dir_all");
            std::fs::create_dir_all(lib.join("sub")).expect("create_dir_all");
            std::fs::write(lib.join("sub").join("a.js"), "").expect("write");
            std::fs::write(lib.join("plain.js"), "").expect("write");
        });
    }

    #[test]
    fn every_root_below_an_ancestor_replaced_in_one_batch_is_reported() {
        let tmpdir = testdir();
        let trigger = tmpdir.path().join("trigger");
        let lib = tmpdir.path().join("lib");
        let a = lib.join("a.js");
        let b = lib.join("b.js");
        let stage = tmpdir.path().join("stage");
        for dir in [&lib, &stage] {
            std::fs::create_dir(dir).expect("create_dir");
            std::fs::write(dir.join("a.js"), "").expect("write");
            std::fs::write(dir.join("b.js"), "").expect("write");
        }
        std::fs::write(&trigger, "").expect("write");

        let (mut watcher, rx, hold) = held_watcher(&trigger);
        watcher.watch_nonrecursively(&a);
        watcher.watch_nonrecursively(&b);
        watcher.watch_nonrecursively(&trigger);

        std::fs::write(&trigger, "1").expect("write trigger");
        hold.wait();
        // The root renamed on its own is armed again first, which finds `lib` back before the
        // write to its parent is looked at.
        std::fs::rename(&a, lib.join("a2.js")).expect("rename a.js");
        std::fs::rename(&lib, tmpdir.path().join("old")).expect("rename away");
        std::fs::rename(&stage, &lib).expect("rename stage");
        hold.release();
        rx.wait_unordered([expected(&a).create_file(), expected(&b).create_file()]);

        std::fs::write(&b, "123").expect("write");
        rx.wait_unordered([expected(&b).modify_data_any()]);
    }

    #[test]
    fn ancestor_replaced_in_one_batch_is_looked_at_without_its_parent() {
        let tmpdir = testdir();
        let trigger = tmpdir.path().join("trigger");
        let locked = tmpdir.path().join("locked");
        let lib = locked.join("lib");
        let a = lib.join("a.js");
        let stage = locked.join("stage");
        for dir in [&lib, &stage] {
            std::fs::create_dir_all(dir).expect("create_dir_all");
            std::fs::write(dir.join("a.js"), "").expect("write");
        }
        std::fs::write(&trigger, "").expect("write");
        // Its parent cannot be opened, so no write to the parent shows the change.
        let Some(_locked) = Locked::lock_listing(&locked) else {
            eprintln!("skipped: running as root, which no permission locks out");
            return;
        };

        let (mut watcher, rx, hold) = held_watcher(&trigger);
        watcher.watch_nonrecursively(&a);
        watcher.watch_nonrecursively(&trigger);
        assert!(!watcher.watcher.get_chain_handles().contains(&locked));

        std::fs::write(&trigger, "1").expect("write trigger");
        hold.wait();
        std::fs::rename(&lib, locked.join("old")).expect("rename away");
        std::fs::rename(&stage, &lib).expect("rename stage");
        hold.release();
        rx.wait_ordered([expected(&a).remove_file(), expected(&a).create_file()]);

        std::fs::write(&a, "123").expect("write");
        rx.wait_unordered([expected(&a).modify_data_any()]);
    }

    #[test]
    fn staged_directory_moved_below_a_recursive_root_reports_the_root_in_it() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let p = tmpdir.path().join("p");
        let utils = p.join("utils");
        let a = utils.join("a.js");
        let stage = tmpdir.path().join("stage");
        std::fs::create_dir_all(&utils).expect("create_dir_all");
        std::fs::create_dir_all(&stage).expect("create_dir_all");
        std::fs::write(&a, "").expect("write");
        std::fs::write(stage.join("a.js"), "").expect("write");

        watcher.watch_recursively(&p);
        watcher.watch_nonrecursively(&a);

        std::fs::rename(&utils, tmpdir.path().join("gone")).expect("rename away");
        rx.wait_unordered([expected(&a).remove_file()]);
        // The round trip waits for the move to be handled.
        assert!(!watcher.get_watch_handles().contains(&utils));
        std::fs::rename(&stage, &utils).expect("rename stage");
        rx.wait_ordered([expected(&a).create_file()]);

        std::fs::write(&a, "123").expect("write");
        rx.wait_unordered([expected(&a).modify_data_any()]);
    }

    #[test]
    fn no_track_recursive_root_drops_the_handles_of_a_renamed_subdirectory() {
        let tmpdir = testdir();
        let (mut watcher, mut rx) = watcher();

        let r = tmpdir.path().join("r");
        let sub = r.join("sub");
        let sub2 = r.join("sub2");
        std::fs::create_dir_all(&sub).expect("create_dir_all");
        std::fs::write(sub.join("x.js"), "").expect("write");

        watcher.watch(&r, no_track(RecursiveMode::Recursive));
        std::fs::rename(&sub, &sub2).expect("rename");
        rx.wait_unordered([expected(&sub2).create_folder(), expected(&sub).rename_any()]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([r.clone(), sub2.clone(), sub2.join("x.js")])
        );

        std::fs::write(sub2.join("x.js"), "123").expect("write");
        let events: Vec<Event> = rx.iter().collect();
        assert!(
            events
                .iter()
                .any(|event| event.paths == [sub2.join("x.js")]),
            "{events:#?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| event.paths.iter().any(|path| path.starts_with(&sub))),
            "{events:#?}"
        );
    }

    #[test]
    fn no_track_recursive_root_drops_the_handles_of_a_subdirectory_moved_out() {
        let tmpdir = testdir();
        let (mut watcher, mut rx) = watcher();

        let r = tmpdir.path().join("r");
        let sub = r.join("sub");
        let x = sub.join("x.js");
        let out = tmpdir.path().join("out");
        std::fs::create_dir_all(&sub).expect("create_dir_all");
        std::fs::write(&x, "").expect("write");

        watcher.watch(&r, no_track(RecursiveMode::Recursive));
        std::fs::rename(&sub, &out).expect("move out");
        rx.wait_unordered([expected(&sub).rename_any()]);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([r.clone()]));

        // The moved file is not reported, under any name.
        std::fs::write(out.join("x.js"), "123").expect("write");
        let events: Vec<Event> = rx.iter().collect();
        assert!(events.is_empty(), "{events:#?}");

        std::fs::create_dir(&sub).expect("create_dir");
        rx.wait_unordered([expected(&sub).create_folder()]);
        // The round trip waits for the watch on the new directory to be registered.
        assert!(watcher.get_watch_handles().contains(&sub));
        std::fs::write(&x, "").expect("write");
        rx.wait_unordered([expected(&x).create_file()]);
        assert!(watcher.get_watch_handles().contains(&x));
        std::fs::write(&x, "123").expect("write");
        rx.wait_unordered([expected(&x).modify_data_any()]);
    }

    #[test]
    fn unwatch_of_a_nested_recursive_root_keeps_the_handles_of_the_outer_one() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let a = tmpdir.path().join("a");
        let b = a.join("b");
        let f = b.join("f.js");
        std::fs::create_dir_all(&b).expect("create_dir_all");
        std::fs::write(&f, "").expect("write");

        watcher.watch_recursively(&a);
        watcher.watch_recursively(&b);
        let handles = watcher.get_watch_handles();
        watcher.watcher.unwatch(&b).expect("unwatch");
        assert_eq!(watcher.get_watch_handles(), handles);

        std::fs::write(&f, "123").expect("write");
        expect_write(&rx, &f);

        let deep = b.join("deep");
        std::fs::create_dir(&deep).expect("create_dir");
        rx.wait_unordered([expected(&deep).create_folder()]);
    }

    #[test]
    fn unwatch_of_a_recursive_root_keeps_its_handle_for_the_directory_root_above() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let d = tmpdir.path().join("d");
        let p = d.join("p");
        let q = d.join("q");
        std::fs::create_dir_all(&p).expect("create_dir_all");
        std::fs::write(p.join("x.js"), "").expect("write");

        watcher.watch_nonrecursively(&d);
        watcher.watch_recursively(&p);
        watcher.watcher.unwatch(&p).expect("unwatch");
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf(), d.clone(), p.clone()])
        );

        std::fs::rename(&p, &q).expect("rename");
        rx.wait_unordered_exact([expected(&q).create_folder(), expected(&p).rename_any()]);
    }

    #[test]
    fn no_track_root_replaced_in_one_batch_is_not_watched_again() {
        let tmpdir = testdir();
        let d = tmpdir.path().join("d");
        let a = d.join("a.js");
        let n = d.join("n.js");
        std::fs::create_dir(&d).expect("create_dir");
        std::fs::write(&a, "").expect("write");
        std::fs::write(&n, "").expect("write");

        let (mut watcher, mut rx, hold) = held_watcher(&a);
        watcher.watch_nonrecursively(&a);
        watcher.watch(&n, no_track(RecursiveMode::NonRecursive));

        std::fs::write(&a, "1").expect("write trigger");
        hold.wait();
        // Written first, so that its rename is read before the write to its directory.
        std::fs::write(&n, "1").expect("write");
        std::fs::rename(&n, d.join("n2.js")).expect("rename");
        std::fs::write(&n, "").expect("write");
        hold.release();

        let events: Vec<Event> = rx.iter().collect();
        assert!(
            events.iter().any(|event| event.paths == [n.clone()]
                && matches!(event.kind, EventKind::Modify(ModifyKind::Name(_)))),
            "{events:#?}"
        );
        assert!(!created_in(&events, &n), "{events:#?}");

        watcher.watcher.unwatch(&a).expect("unwatch");
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));
        assert_eq!(watcher.watcher.get_chain_handles(), HashSet::from([]));
    }

    #[test]
    fn no_track_root_overwritten_by_a_rename_is_not_watched_again() {
        let tmpdir = testdir();
        let (mut watcher, mut rx) = watcher();

        let d = tmpdir.path().join("d");
        let a = d.join("a.js");
        let n = d.join("n.js");
        let other = d.join("other.js");
        std::fs::create_dir(&d).expect("create_dir");
        std::fs::write(&a, "").expect("write");
        std::fs::write(&n, "").expect("write");
        std::fs::write(&other, "").expect("write");

        watcher.watch_nonrecursively(&a);
        watcher.watch(&n, no_track(RecursiveMode::NonRecursive));
        std::fs::rename(&other, &n).expect("rename over");

        let events: Vec<Event> = rx.iter().collect();
        assert!(
            events
                .iter()
                .any(|event| event.paths == [n.clone()]
                    && matches!(event.kind, EventKind::Remove(_))),
            "{events:#?}"
        );
        assert!(!created_in(&events, &n), "{events:#?}");

        watcher.watcher.unwatch(&a).expect("unwatch");
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));
        assert_eq!(watcher.watcher.get_chain_handles(), HashSet::from([]));
    }

    #[test]
    fn ancestor_entry_replaced_by_a_file_keeps_its_handle() {
        let tmpdir = testdir();
        let (mut watcher, mut rx) = watcher();

        let t = tmpdir.path().join("t");
        let sub = t.join("sub");
        let f = sub.join("f.js");
        std::fs::create_dir_all(&sub).expect("create_dir_all");
        std::fs::write(&f, "").expect("write");

        watcher.watch_nonrecursively(&t);
        watcher.watch_nonrecursively(&f);

        // In two steps, each waited for: a directory removed while its write is looked at would
        // report a spurious error.
        std::fs::remove_file(&f).expect("remove f.js");
        rx.wait_unordered([expected(&f).remove_any()]);
        assert!(!watcher.get_watch_handles().contains(&f));
        std::fs::remove_dir(&sub).expect("remove sub");
        rx.wait_unordered([expected(&sub).remove_any()]);
        assert!(!watcher.get_watch_handles().contains(&sub));
        std::fs::write(&sub, "x").expect("write a file at sub");
        rx.wait_unordered([expected(&sub).create_file()]);
        // The round trip waits for the watch on the new file to be registered.
        assert!(watcher.get_watch_handles().contains(&sub));

        let assert_sub_reports = |rx: &mut test::Receiver| {
            std::fs::write(&sub, "xyz").expect("write sub");
            let events: Vec<Event> = rx.iter().collect();
            assert!(
                events.iter().any(|event| event.paths == [sub.clone()]
                    && matches!(event.kind, EventKind::Modify(ModifyKind::Data(_)))),
                "{events:#?}"
            );
            assert!(!created_in(&events, &sub), "{events:#?}");
        };
        assert_sub_reports(&mut rx);
        // Each new entry is reported: the file at `sub` is not taken for a new one.
        for i in 0..4 {
            let entry = t.join(format!("n{i}"));
            std::fs::write(&entry, "").expect("write");
            rx.wait_unordered([expected(&entry).create_file()]);
        }
        assert_sub_reports(&mut rx);
    }

    #[test]
    fn an_ancestor_that_cannot_be_opened_is_still_tracked() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let locked = tmpdir.path().join("locked");
        let moved = tmpdir.path().join("moved");
        let file = locked.join("inner").join("f.js");
        std::fs::create_dir_all(file.parent().unwrap()).expect("create_dir_all");
        std::fs::write(&file, "").expect("write");
        let Some(mut guard) = Locked::lock_listing(&locked) else {
            eprintln!("skipped: running as root, which no permission locks out");
            return;
        };

        watcher.watch_nonrecursively(&file);
        assert!(!watcher.watcher.get_chain_handles().contains(&locked));

        std::fs::rename(&locked, &moved).expect("rename away");
        guard.moved_to(&moved);
        rx.wait_ordered_exact([expected(&file).remove_file()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));

        // The moved file is not reported under the path it had.
        std::fs::write(moved.join("inner").join("f.js"), "123").expect("write");
        std::fs::rename(&moved, &locked).expect("rename back");
        guard.moved_to(&locked);
        rx.wait_ordered_exact([expected(&file).create_file()])
            .ensure_no_tail();

        std::fs::write(&file, "123456").expect("write");
        rx.wait_unordered([expected(&file).modify_data_any()]);
    }

    #[test]
    fn an_ancestor_gone_again_while_the_roots_below_are_armed_is_left_gone() {
        let tmpdir = testdir();
        let out = tmpdir.path().join("out");
        let x = out.join("x");
        let a = x.join("a.js");
        let b = x.join("b.js");
        let d = x.join("d");
        let stage = tmpdir.path().join("stage");
        for dir in [&out, &stage] {
            std::fs::create_dir_all(dir.join("x").join("d").join("sub")).expect("create_dir_all");
            std::fs::write(dir.join("x").join("a.js"), "").expect("write");
            std::fs::write(dir.join("x").join("b.js"), "").expect("write");
        }
        // The copy moved in holds a directory root that cannot be armed.
        let Some(mut locked) = Locked::lock(&stage.join("x").join("d").join("sub")) else {
            eprintln!("skipped: running as root, which no permission locks out");
            return;
        };

        // Held when the arm of `d` fails, right after it found `x` present again.
        let (mut watcher, rx, hold) = held_watcher_on(|event| event.is_err());
        watcher.watch_nonrecursively(&a);
        watcher.watch_nonrecursively(&b);
        watcher.watch_recursively(&d);

        std::fs::rename(&out, tmpdir.path().join("old")).expect("rename away");
        rx.wait_unordered([
            expected(&a).remove_file(),
            expected(&b).remove_file(),
            expected(&d).remove_folder(),
        ]);
        std::fs::rename(&stage, &out).expect("rename stage");
        locked.moved_to(&d.join("sub"));
        hold.wait();
        // Gone again before the handles the arms opened on `out` and `x` are registered, so the
        // handle on `out` does not see it.
        let gone = out.join("gone");
        std::fs::rename(&x, &gone).expect("rename x away");
        locked.moved_to(&gone.join("d").join("sub"));
        hold.release();
        // The round trip waits for the batch to be handled.
        watcher.get_watch_handles();
        rx.rx.try_iter().for_each(drop);

        std::fs::create_dir(&x).expect("create_dir");
        std::fs::write(&a, "").expect("write");
        std::fs::write(&b, "").expect("write");
        rx.wait_unordered([expected(&a).create_file(), expected(&b).create_file()]);
    }

    /// Watches the `NoTrack` directory root `n` and, while the event loop is held, writes to it,
    /// renames it away and creates a new directory at its path. The write is read first, and the
    /// scan of the new directory finds entries that the rename, read next, takes out of every
    /// root: none of them is watched.
    fn assert_no_track_root_replaced_after_a_write_leaves_no_handles(
        recursive_mode: RecursiveMode,
    ) {
        let tmpdir = testdir();
        let t = tmpdir.path().join("t.js");
        let n = tmpdir.path().join("n");
        std::fs::create_dir(&n).expect("create_dir");
        std::fs::write(n.join("e.js"), "").expect("write");
        std::fs::write(&t, "").expect("write");

        let (mut watcher, _rx, hold) = held_watcher(&t);
        watcher.watch_nonrecursively(&t);
        watcher.watch(&n, no_track(recursive_mode));

        std::fs::write(&t, "1").expect("write trigger");
        hold.wait();
        std::fs::write(n.join("new.js"), "").expect("write");
        std::fs::create_dir(n.join("sub2")).expect("create_dir");
        std::fs::rename(&n, tmpdir.path().join("n_old")).expect("rename away");
        std::fs::create_dir_all(n.join("sub2")).expect("create_dir_all");
        std::fs::write(n.join("new.js"), "").expect("write");
        std::fs::write(n.join("sub2").join("y.js"), "").expect("write");
        hold.release();

        // The root ended with the rename.
        let result = watcher.watcher.unwatch(&n);
        assert!(result.is_err(), "{result:?}");
        watcher.watcher.unwatch(&t).expect("unwatch");
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));
        assert_eq!(watcher.watcher.get_chain_handles(), HashSet::from([]));
    }

    #[test]
    fn no_track_root_replaced_after_a_write_leaves_no_handles() {
        assert_no_track_root_replaced_after_a_write_leaves_no_handles(RecursiveMode::NonRecursive);
    }

    #[test]
    fn no_track_recursive_root_replaced_after_a_write_leaves_no_handles() {
        assert_no_track_root_replaced_after_a_write_leaves_no_handles(RecursiveMode::Recursive);
    }
}
