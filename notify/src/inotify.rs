//! Watcher implementation for the inotify Linux API
//!
//! The inotify API provides a mechanism for monitoring filesystem events.  Inotify can be used to
//! monitor individual files, or to monitor directories.  When a directory is monitored, inotify
//! will return events for the directory itself, and for files inside the directory.

use super::event::*;
use super::{Config, Error, ErrorKind, EventHandler, RecursiveMode, Result, WatchMode, Watcher};
use crate::bimap::BiHashMap;
use crate::{BoundSender, Receiver, Sender, TargetMode, bounded, unbounded};
use inotify as inotify_sys;
use inotify_sys::{EventMask, Inotify, WatchDescriptor, WatchMask};
use rustc_hash::FxBuildHasher;
use std::collections::{BTreeSet, HashMap};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::metadata;
use std::io;
use std::ops::Bound;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use walkdir::WalkDir;

const INOTIFY: mio::Token = mio::Token(0);
const MESSAGE: mio::Token = mio::Token(1);

/// What the ancestors of a tracked path are watched for: only their entries coming and going.
const ENTRY_MASK: WatchMask = WatchMask::CREATE
    .union(WatchMask::DELETE)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::MOVED_TO);
const FULL_MASK: WatchMask = ENTRY_MASK
    .union(WatchMask::ATTRIB)
    .union(WatchMask::OPEN)
    .union(WatchMask::CLOSE_WRITE)
    .union(WatchMask::MODIFY);
const SELF_MASK: WatchMask = WatchMask::DELETE_SELF.union(WatchMask::MOVE_SELF);
/// What inotify reports whatever a watch asks for, besides the removal of the watch.
const UNMASKED_EVENTS: EventMask = EventMask::ISDIR.union(EventMask::UNMOUNT);

#[derive(Clone, Copy, Debug)]
struct WatchInfo {
    mask: WatchMask,
    is_dir: bool,
}

#[cfg(test)]
impl WatchInfo {
    fn entries_only(self) -> bool {
        !self.mask.contains(WatchMask::MODIFY)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootState {
    Missing,
    File,
    Directory,
}

/// A path the user watches. With `TargetMode::TrackPath` the path is tracked through every
/// ancestor: the ones that exist are watched for their entries, so that the root is reported
/// removed when any of them goes away, and created and watched again when it is reachable again.
#[derive(Clone, Copy, Debug)]
struct RootWatch {
    mode: WatchMode,
    state: RootState,
}

/// The roots right below a directory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DirRoots {
    count: usize,
    /// The ones without a handle of their own: the directory reports them.
    without_handle: usize,
}

/// The other spellings of watched inodes.
///
/// inotify watches inodes, not paths: a directory reached through more than one path (a symlink
/// and its target, a bind mount, a path through `..`), or a file with more than one link, gets one
/// descriptor, whichever path watches it. `watch_handles` binds the descriptor to one of them; the
/// others are its aliases, each with the mask it asked for, and the kernel watches the inode for
/// all of them. Every event of the descriptor is handled once per spelling, for what that spelling
/// asked for, so each spelling reports like a watch of its own: every root under any of them gets
/// its events, under its own path.
///
/// A recursive root over a tree with many links to one directory, such as the `node_modules/.pnpm`
/// of a pnpm workspace, reaches the directory and everything below it through each link: the
/// kernel watches each directory once, but each directory is bound once per spelling, and each of
/// its events is reported once per spelling. A new directory there is walked once, not once per
/// spelling; see [`EventLoop::add_entry_watches`]. Leaving such trees out of a recursive root keeps
/// the cost down.
///
/// A file root with more than one link gets a watch of its own besides its parent's: the spelling
/// written through is reported twice then, by the parent and by the file.
#[derive(Debug, Default)]
struct Aliases {
    of: HashMap<WatchDescriptor, Vec<PathBuf>, FxBuildHasher>,
    at: HashMap<PathBuf, (WatchDescriptor, WatchInfo), FxBuildHasher>,
}

impl Aliases {
    fn get(&self, path: &Path) -> Option<(&WatchDescriptor, &WatchInfo)> {
        self.at.get(path).map(|(w, info)| (w, info))
    }

    fn paths_of(&self, w: &WatchDescriptor) -> &[PathBuf] {
        self.of.get(w).map_or(&[], Vec::as_slice)
    }

    fn insert(&mut self, w: WatchDescriptor, path: PathBuf, info: WatchInfo) {
        self.of.entry(w.clone()).or_default().push(path.clone());
        self.at.insert(path, (w, info));
    }

    fn set_info(&mut self, path: &Path, info: WatchInfo) {
        if let Some((_, old)) = self.at.get_mut(path) {
            *old = info;
        }
    }

    fn remove(&mut self, path: &Path) -> Option<WatchDescriptor> {
        let (w, _) = self.at.remove(path)?;
        if let Some(paths) = self.of.get_mut(&w) {
            paths.retain(|other| other != path);
            if paths.is_empty() {
                self.of.remove(&w);
            }
        }
        Some(w)
    }

    /// Removes the alias of `w` that was added first.
    fn remove_first_of(&mut self, w: &WatchDescriptor) -> Option<(PathBuf, WatchInfo)> {
        let path = self.paths_of(w).first()?.clone();
        let (_, info) = *self.at.get(&path)?;
        self.remove(&path);
        Some((path, info))
    }

    fn remove_all_of(&mut self, w: &WatchDescriptor) -> Vec<PathBuf> {
        let paths = self.of.remove(w).unwrap_or_default();
        for path in &paths {
            self.at.remove(path);
        }
        paths
    }

    fn clear(&mut self) {
        self.of.clear();
        self.at.clear();
    }
}

/// A path to watch once the events of a read are handled: whether it is watched recursively, and
/// whether it is a file without hard links, which its parent reports.
type AddWatch = (PathBuf, bool, bool);

/// What the events of one read ask for, once they are all handled.
#[derive(Debug, Default)]
struct WatchChanges {
    /// The entries to watch, each under the spellings that watch it; see
    /// [`EventLoop::add_entry_watches`].
    add_watches: Vec<Vec<AddWatch>>,
    /// The spellings under which the entry of the event being handled is to be watched.
    entry_watches: Vec<AddWatch>,
    /// Where the entries of `add_watches` are, by the descriptor of their directory and their
    /// name: an entry that several events of the read are about is watched once.
    queued_entries: HashMap<(WatchDescriptor, Option<OsString>), usize, FxBuildHasher>,
    remove_watches: Vec<PathBuf>,
    remove_watches_no_syscall: Vec<PathBuf>,
    vanished: Vec<PathBuf>,
    appeared: Vec<PathBuf>,
    ignored: Vec<WatchDescriptor>,
}

impl WatchChanges {
    /// Queues the entry that the event of `w` about `name` is about, under the spellings its
    /// handling asked for, unless another event of the read queued it the same way already. An
    /// entry queued another way, such as a file that a directory replaced, is queued again.
    fn queue_entry(&mut self, w: &WatchDescriptor, name: Option<&OsStr>) {
        if self.entry_watches.is_empty() {
            return;
        }
        let spellings = std::mem::take(&mut self.entry_watches);
        let key = (w.clone(), name.map(OsStr::to_os_string));
        if let Some(&queued) = self.queued_entries.get(&key)
            && self.add_watches[queued] == spellings
        {
            return;
        }
        self.queued_entries.insert(key, self.add_watches.len());
        self.add_watches.push(spellings);
    }
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
    inotify: Option<Inotify>,
    event_handler: Box<dyn EventHandler>,
    watches: HashMap<PathBuf, RootWatch, FxBuildHasher>,
    watch_handles: BiHashMap<WatchDescriptor, PathBuf, WatchInfo, FxBuildHasher>,
    aliases: Aliases,
    /// The inode each descriptor watches, as the path it was first bound to found it: a spelling
    /// that leads to another inode by now is stale, and a mask set through it would change the
    /// watch of that inode instead.
    inodes: HashMap<WatchDescriptor, (u64, u64), FxBuildHasher>,
    /// How many tracked roots lie strictly below each path.
    ancestors: HashMap<PathBuf, usize, FxBuildHasher>,
    /// The paths of `watches` and of the handles, aliases included, in order: the ones below a path
    /// follow it.
    root_paths: BTreeSet<PathBuf>,
    handle_paths: BTreeSet<PathBuf>,
    /// The roots right below each directory that has any.
    dir_roots: HashMap<PathBuf, DirRoots, FxBuildHasher>,
    /// The paths that got a handle during the arm of a root, which drops them when it fails.
    armed: Option<Vec<PathBuf>>,
    /// How many handles the watcher can hold, like a full inotify watch table.
    #[cfg(test)]
    watch_limit: Option<usize>,
    /// The cookie of the last entry moved away, and its spellings: the entry moved to with the same
    /// cookie is paired with them.
    rename_from: Option<(u32, Vec<PathBuf>)>,
    follow_links: bool,
}

/// Watcher implementation based on inotify
#[derive(Debug)]
pub struct INotifyWatcher {
    channel: Sender<EventLoopMsg>,
    waker: Arc<mio::Waker>,
}

enum EventLoopMsg {
    AddWatch(PathBuf, WatchMode, Sender<Result<()>>),
    RemoveWatch(PathBuf, Sender<Result<()>>),
    Shutdown,
    Configure(Config, BoundSender<Result<bool>>),
    #[cfg(test)]
    GetWatchHandles(BoundSender<Vec<(PathBuf, WatchInfo)>>),
    #[cfg(test)]
    SetWatchLimit(Option<usize>, BoundSender<()>),
}

#[inline]
fn add_watch_by_event(
    path: &PathBuf,
    is_file_without_hardlinks: bool,
    watches: &HashMap<PathBuf, RootWatch, FxBuildHasher>,
    add_watches: &mut Vec<AddWatch>,
) {
    if let Some(root) = watches.get(path) {
        add_watches.push((
            path.to_owned(),
            root.mode.recursive_mode.is_recursive(),
            is_file_without_hardlinks,
        ));
        return;
    }

    let Some(parent) = path.parent() else {
        return;
    };
    if let Some(root) = watches.get(parent) {
        add_watches.push((
            path.to_owned(),
            root.mode.recursive_mode.is_recursive(),
            is_file_without_hardlinks,
        ));
        return;
    }

    for ancestor in parent.ancestors().skip(1) {
        if let Some(root) = watches.get(ancestor)
            && root.mode.recursive_mode == RecursiveMode::Recursive
        {
            add_watches.push((path.to_owned(), true, is_file_without_hardlinks));
            return;
        }
    }
}

impl EventLoop {
    pub fn new(
        inotify: Inotify,
        event_handler: Box<dyn EventHandler>,
        follow_links: bool,
    ) -> Result<Self> {
        let (event_loop_tx, event_loop_rx) = unbounded::<EventLoopMsg>();
        let poll = mio::Poll::new()?;

        let event_loop_waker = Arc::new(mio::Waker::new(poll.registry(), MESSAGE)?);

        let inotify_fd = inotify.as_raw_fd();
        let mut evented_inotify = mio::unix::SourceFd(&inotify_fd);
        poll.registry()
            .register(&mut evented_inotify, INOTIFY, mio::Interest::READABLE)?;

        let event_loop = EventLoop {
            running: true,
            poll,
            event_loop_waker,
            event_loop_tx,
            event_loop_rx,
            inotify: Some(inotify),
            event_handler,
            watches: HashMap::default(),
            watch_handles: BiHashMap::default(),
            aliases: Aliases::default(),
            inodes: HashMap::default(),
            ancestors: HashMap::default(),
            root_paths: BTreeSet::new(),
            handle_paths: BTreeSet::new(),
            dir_roots: HashMap::default(),
            armed: None,
            #[cfg(test)]
            watch_limit: None,
            rename_from: None,
            follow_links,
        };
        Ok(event_loop)
    }

    // Run the event loop.
    pub fn run(self) {
        let result = thread::Builder::new()
            .name("notify-rs inotify loop".to_string())
            .spawn(|| self.event_loop_thread());
        if let Err(e) = result {
            tracing::error!(?e, "failed to start inotify event loop thread");
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
            INOTIFY => {
                // inotify has something to tell us.
                self.handle_inotify();
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
                EventLoopMsg::RemoveWatch(path, tx) => {
                    let result = tx.send(self.remove_watch(path));
                    if let Err(e) = result {
                        tracing::error!(?e, "failed to send RemoveWatch result");
                    }
                }
                EventLoopMsg::Shutdown => {
                    let result = self.remove_all_watches();
                    if let Err(e) = result {
                        tracing::error!(?e, "failed to remove all watches on shutdown");
                    }
                    if let Some(inotify) = self.inotify.take() {
                        let result = inotify.close();
                        if let Err(e) = result {
                            tracing::error!(?e, "failed to close inotify instance on shutdown");
                        }
                    }
                    self.running = false;
                    break;
                }
                EventLoopMsg::Configure(config, tx) => {
                    Self::configure_raw_mode(config, &tx);
                }
                #[cfg(test)]
                EventLoopMsg::GetWatchHandles(tx) => {
                    self.check_indexes();
                    let handles = self
                        .watch_handles
                        .iter()
                        .map(|(_, path, info)| (path.clone(), *info))
                        .chain(
                            self.aliases
                                .at
                                .iter()
                                .map(|(path, (_, info))| (path.clone(), *info)),
                        )
                        .collect();
                    tx.send(handles).unwrap();
                }
                #[cfg(test)]
                EventLoopMsg::SetWatchLimit(limit, tx) => {
                    self.watch_limit = limit;
                    tx.send(()).unwrap();
                }
            }
        }
    }

    fn configure_raw_mode(_config: Config, tx: &BoundSender<Result<bool>>) {
        tx.send(Ok(false))
            .expect("configuration channel disconnected");
    }

    fn is_watched_path(watches: &HashMap<PathBuf, RootWatch, FxBuildHasher>, path: &Path) -> bool {
        if watches.contains_key(path) {
            return true;
        }

        let Some(parent) = path.parent() else {
            return false;
        };
        if watches.contains_key(parent) {
            return true;
        }

        parent.ancestors().skip(1).any(|ancestor| {
            watches
                .get(ancestor)
                .is_some_and(|root| root.mode.recursive_mode == RecursiveMode::Recursive)
        })
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

    /// `path` exists now: a root there is present, and a directory may lead to roots below it.
    ///
    /// The event says whether the entry is a directory, but not where a symlink leads: an ancestor
    /// is followed, as the chain is armed through it.
    fn note_present(
        watches: &mut HashMap<PathBuf, RootWatch, FxBuildHasher>,
        ancestors: &HashMap<PathBuf, usize, FxBuildHasher>,
        path: &Path,
        is_dir: bool,
        appeared: &mut Vec<PathBuf>,
    ) {
        let is_dir = is_dir
            || (ancestors.contains_key(path) && metadata(path).is_ok_and(|meta| meta.is_dir()));
        if let Some(root) = watches.get_mut(path) {
            root.state = if is_dir {
                RootState::Directory
            } else {
                RootState::File
            };
        }
        if is_dir && ancestors.contains_key(path) {
            appeared.push(path.to_path_buf());
        }
    }

    fn handle_inotify(&mut self) {
        let mut changes = WatchChanges::default();
        let mut buffer = [0; 1024];
        // Read all buffers available.
        while let Some(inotify) = self.inotify.as_mut() {
            match inotify.read_events(&mut buffer) {
                Ok(events) => {
                    let mut num_events = 0;
                    for event in events {
                        num_events += 1;
                        self.handle_inotify_event(&event, &mut changes);
                    }

                    // All events read. Break out.
                    if num_events == 0 {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No events read. Break out.
                    break;
                }
                Err(e) => {
                    self.event_handler.handle_event(Err(Error::io(e)));
                }
            }
        }

        let WatchChanges {
            add_watches,
            remove_watches,
            remove_watches_no_syscall,
            vanished,
            appeared,
            ignored,
            ..
        } = changes;
        tracing::trace!(
            ?add_watches,
            ?remove_watches,
            "processing inotify watch changes"
        );

        // The kernel dropped these watches, as their inodes are gone: the handles are dead, and
        // must not pass for live ones when their paths are armed again below.
        for w in ignored {
            self.unbind_by_left(&w);
        }

        for path in remove_watches_no_syscall {
            self.forget_no_track_root(&path);
            self.drop_handles_below(&path, false, true);
        }

        for path in remove_watches {
            self.forget_no_track_root(&path);
            self.drop_handles_below(&path, false, false);
        }

        for path in vanished {
            self.vanish_below(&path);
        }

        for spellings in add_watches {
            if let Err(add_watch_error) = self.add_entry_watches(spellings) {
                // The handler should be notified if we have reached the limit.
                // Otherwise, the user might expect that a recursive watch
                // is continuing to work correctly, but it's not.
                if let ErrorKind::MaxFilesWatch = add_watch_error.kind {
                    self.event_handler.handle_event(Err(add_watch_error));

                    // After that kind of a error we should stop adding watches,
                    // because the limit has already reached and all next calls
                    // will return us only the same error.
                    break;
                }
            }
        }

        for path in appeared {
            self.rearm_below(&path);
        }
    }

    /// Handles an inotify event once for each spelling of its descriptor; see [`Aliases`]. An
    /// entry that appeared is queued once, under the spellings that watch it.
    ///
    /// A rename is reported under each spelling, all with the same tracker: the `From` event of
    /// each spelling first, then the `To` and `Both` events of each spelling. Each `Both` event
    /// pairs a spelling with the one it came from, which shares the longest path prefix with it;
    /// a consumer that pairs `From` and `To` events by tracker has to pair them by that prefix as
    /// well, or it pairs one spelling with another.
    fn handle_inotify_event(
        &mut self,
        event: &inotify_sys::Event<&OsStr>,
        changes: &mut WatchChanges,
    ) {
        tracing::trace!(?event, "inotify event received");

        if event.mask.contains(EventMask::Q_OVERFLOW) {
            let ev = Ok(Event::new(EventKind::Other).set_flag(Flag::Rescan));
            self.event_handler.handle_event(ev);
        }

        let Some((primary, primary_info)) = self.watch_handles.get_by_left(&event.wd) else {
            tracing::debug!(?event, "inotify event with unknown descriptor");
            return;
        };

        if event.mask.contains(EventMask::IGNORED) {
            tracing::trace!("inotify dropped the watch: {}", primary.display());
            changes.ignored.push(event.wd.clone());
            return;
        }

        // Each spelling gets what it asked for, as the kernel watches the inode for all of them.
        let aliases = self.aliases.paths_of(&event.wd).iter().filter_map(|alias| {
            let (_, info) = self.aliases.get(alias)?;
            Some((alias, info))
        });
        let spellings: Vec<(PathBuf, EventMask)> = std::iter::once((primary, primary_info))
            .chain(aliases)
            .map(|(spelling, info)| {
                let path = match event.name {
                    Some(name) => spelling.join(name),
                    None => spelling.clone(),
                };
                let asked = EventMask::from_bits_truncate(info.mask.bits()).union(UNMASKED_EVENTS);
                (path, event.mask.intersection(asked))
            })
            .collect();
        for (path, mask) in &spellings {
            if !mask.difference(EventMask::ISDIR).is_empty() {
                self.handle_inotify_event_at(*mask, event.cookie, path, changes);
            }
        }
        changes.queue_entry(&event.wd, event.name);
    }

    /// Handles the inotify event `mask` at `path`, one spelling of the entry it is about.
    #[expect(clippy::too_many_lines)]
    fn handle_inotify_event_at(
        &mut self,
        mask: EventMask,
        cookie: u32,
        path: &PathBuf,
        changes: &mut WatchChanges,
    ) {
        let mut evs = Vec::new();

        if mask.contains(EventMask::MOVED_FROM) {
            self.remove_watch_by_event(path, &mut changes.remove_watches);

            match &mut self.rename_from {
                Some((from_cookie, from)) if *from_cookie == cookie => from.push(path.clone()),
                _ => self.rename_from = Some((cookie, vec![path.clone()])),
            }

            if Self::is_watched_path(&self.watches, path) {
                evs.push(
                    Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
                        .add_path(path.clone())
                        .set_tracker(cookie as usize),
                );
            }
            Self::note_gone(
                &mut self.watches,
                &self.ancestors,
                path,
                &mut changes.vanished,
            );
        } else if mask.contains(EventMask::MOVED_TO) {
            if Self::is_watched_path(&self.watches, path) {
                evs.push(
                    Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::To)))
                        .set_tracker(cookie as usize)
                        .add_path(path.clone()),
                );

                if let Some(from) = self.renamed_from(cookie, path) {
                    evs.push(
                        Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                            .set_tracker(cookie as usize)
                            .add_path(from)
                            .add_path(path.clone()),
                    );
                }
            }

            let is_file_without_hardlinks = !mask.contains(EventMask::ISDIR)
                && metadata(path).is_ok_and(|m| m.is_file_without_hardlinks());
            add_watch_by_event(
                path,
                is_file_without_hardlinks,
                &self.watches,
                &mut changes.entry_watches,
            );
            Self::note_present(
                &mut self.watches,
                &self.ancestors,
                path,
                mask.contains(EventMask::ISDIR),
                &mut changes.appeared,
            );
        }
        if mask.contains(EventMask::MOVE_SELF) {
            self.remove_watch_by_event(path, &mut changes.remove_watches);
            if let Some(root) = self.watches.get_mut(path) {
                root.state = RootState::Missing;
            }
            if Self::is_watched_path(&self.watches, path) {
                evs.push(
                    Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
                        .add_path(path.clone()),
                );
                // TODO stat the path and get to new path
                // - emit To and Both events
                // - change prefix for further events
            }
        }
        if mask.contains(EventMask::CREATE) {
            let is_dir = mask.contains(EventMask::ISDIR);
            if Self::is_watched_path(&self.watches, path) {
                evs.push(
                    Event::new(EventKind::Create(if is_dir {
                        CreateKind::Folder
                    } else {
                        CreateKind::File
                    }))
                    .add_path(path.clone()),
                );
            }
            let is_file_without_hardlinks =
                !is_dir && metadata(path).is_ok_and(|m| m.is_file_without_hardlinks());
            add_watch_by_event(
                path,
                is_file_without_hardlinks,
                &self.watches,
                &mut changes.entry_watches,
            );
            Self::note_present(
                &mut self.watches,
                &self.ancestors,
                path,
                is_dir,
                &mut changes.appeared,
            );
        }
        if mask.contains(EventMask::DELETE) {
            if Self::is_watched_path(&self.watches, path) {
                evs.push(
                    Event::new(EventKind::Remove(if mask.contains(EventMask::ISDIR) {
                        RemoveKind::Folder
                    } else {
                        RemoveKind::File
                    }))
                    .add_path(path.clone()),
                );
            }
            self.remove_watch_by_event(path, &mut changes.remove_watches);
            Self::note_gone(
                &mut self.watches,
                &self.ancestors,
                path,
                &mut changes.vanished,
            );
        }
        if mask.contains(EventMask::DELETE_SELF) {
            let remove_kind = match self.handle_at(path) {
                Some((_, info)) if info.is_dir => RemoveKind::Folder,
                Some(_) => RemoveKind::File,
                None => RemoveKind::Other,
            };
            if let Some(root) = self.watches.get_mut(path) {
                root.state = RootState::Missing;
            }
            if Self::is_watched_path(&self.watches, path) {
                evs.push(Event::new(EventKind::Remove(remove_kind)).add_path(path.clone()));
            }
            self.remove_watch_by_event(path, &mut changes.remove_watches);
        }
        if mask.contains(EventMask::UNMOUNT) {
            if Self::is_watched_path(&self.watches, path) {
                evs.push(Event::new(EventKind::Remove(RemoveKind::Other)).add_path(path.clone()));
            }
            // The kernel has already removed this watch descriptor and will
            // emit IGNORED; clean up internal state without inotify_rm_watch.
            // ref. https://www.man7.org/linux/man-pages/man7/inotify.7.html
            self.remove_watch_by_event(path, &mut changes.remove_watches_no_syscall);
            // The roots below are cut off like on a delete. A later mount
            // makes no inotify event, so they are watched again only when
            // the mount point itself is created or watched anew.
            Self::note_gone(
                &mut self.watches,
                &self.ancestors,
                path,
                &mut changes.vanished,
            );
        }
        if mask.contains(EventMask::MODIFY) && Self::is_watched_path(&self.watches, path) {
            evs.push(
                Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any)))
                    .add_path(path.clone()),
            );
        }
        if mask.contains(EventMask::CLOSE_WRITE) && Self::is_watched_path(&self.watches, path) {
            evs.push(
                Event::new(EventKind::Access(AccessKind::Close(AccessMode::Write)))
                    .add_path(path.clone()),
            );
        }
        if mask.contains(EventMask::CLOSE_NOWRITE) && Self::is_watched_path(&self.watches, path) {
            evs.push(
                Event::new(EventKind::Access(AccessKind::Close(AccessMode::Read)))
                    .add_path(path.clone()),
            );
        }
        if mask.contains(EventMask::ATTRIB) && Self::is_watched_path(&self.watches, path) {
            evs.push(
                Event::new(EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)))
                    .add_path(path.clone()),
            );
        }
        if mask.contains(EventMask::OPEN) && Self::is_watched_path(&self.watches, path) {
            evs.push(
                Event::new(EventKind::Access(AccessKind::Open(AccessMode::Any)))
                    .add_path(path.clone()),
            );
        }

        for ev in evs {
            self.event_handler.handle_event(Ok(ev));
        }
    }

    /// The spelling of the entry moved away with `cookie` that the entry moved to `to` is paired
    /// with: the one that shares the longest path prefix with `to`, which is reached through the
    /// same spelling of the directories the entry moved between, else the first one; a spelling
    /// that is not watched is not paired.
    fn renamed_from(&self, cookie: u32, to: &Path) -> Option<PathBuf> {
        let (from_cookie, from) = self.rename_from.as_ref()?;
        if *from_cookie != cookie {
            return None;
        }
        from.iter()
            .filter(|from| Self::is_watched_path(&self.watches, from))
            // Of the ones with the longest prefix, the last is kept: the first spelling, reversed.
            .rev()
            .max_by_key(|from| common_prefix_len(from, to))
            .cloned()
    }

    /// The entity at `path` is gone: its handles go, and so does a NoTrack root there, which
    /// follows the entity, even when it was reported by its parent and had no handle of its own.
    fn remove_watch_by_event(&self, path: &PathBuf, remove_watches: &mut Vec<PathBuf>) {
        if self.watch_handles.contains_right(path)
            || self.aliases.get(path).is_some()
            || self
                .watches
                .get(path)
                .is_some_and(|root| root.mode.target_mode == TargetMode::NoTrack)
        {
            remove_watches.push(path.clone());
        }
    }

    /// The roots below `path` are out of reach: report the ones that were present, and drop the
    /// watches below, which sit on moved or deleted inodes.
    fn vanish_below(&mut self, path: &Path) {
        let roots: Vec<PathBuf> = paths_below(&self.root_paths, path)
            .filter(|root| root.as_path() != path)
            .cloned()
            .collect();
        for root in roots {
            let Some(watch) = self.watches.get_mut(&root) else {
                continue;
            };
            if watch.mode.target_mode != TargetMode::TrackPath || watch.state == RootState::Missing
            {
                continue;
            }
            let kind = if watch.state == RootState::Directory {
                RemoveKind::Folder
            } else {
                RemoveKind::File
            };
            watch.state = RootState::Missing;
            self.event_handler
                .handle_event(Ok(Event::new(EventKind::Remove(kind)).add_path(root)));
        }
        self.drop_handles_below(path, true, false);
    }

    /// `path` is a directory again: watch the roots below it that can be reached now.
    ///
    /// Each root that cannot be armed is reported with an error of its own, naming it, and stays
    /// missing until an ancestor comes back again or it is watched again. A full watch table does
    /// not stop the other roots: the failed arm gives its handles back, and many roots need no new
    /// one, as the parent that reports them is watched already.
    fn rearm_below(&mut self, path: &Path) {
        let roots: Vec<(PathBuf, RecursiveMode)> = paths_below(&self.root_paths, path)
            .filter(|root| root.as_path() != path)
            .filter_map(|root| {
                let watch = self.watches.get(root)?;
                (watch.mode.target_mode == TargetMode::TrackPath
                    && watch.state == RootState::Missing)
                    .then(|| (root.clone(), watch.mode.recursive_mode))
            })
            .collect();
        for (root, recursive_mode) in roots {
            match self.arm_root_or_rollback(&root, recursive_mode) {
                Ok(RootState::Missing) => {}
                Ok(state) => self.note_armed(&root, state),
                Err(error) => {
                    let error = if error.paths.contains(&root) {
                        error
                    } else {
                        error.add_path(root)
                    };
                    self.event_handler.handle_event(Err(error));
                }
            }
        }
    }

    /// A missing root is present now: records it and reports it created.
    fn note_armed(&mut self, root: &Path, state: RootState) {
        if let Some(watch) = self.watches.get_mut(root) {
            watch.state = state;
        }
        let kind = if state == RootState::Directory {
            CreateKind::Folder
        } else {
            CreateKind::File
        };
        self.event_handler.handle_event(Ok(
            Event::new(EventKind::Create(kind)).add_path(root.to_path_buf())
        ));
    }

    /// A NoTrack root at `path` follows its entity, which is gone: forgets the root.
    fn forget_no_track_root(&mut self, path: &Path) {
        if self
            .watches
            .get(path)
            .is_some_and(|root| root.mode.target_mode == TargetMode::NoTrack)
        {
            self.remove_root(path);
        }
    }

    /// The entity at `path` moved away or is gone: drops the handles at and below `path`, which
    /// sit on the moved or deleted inodes, except the ones a NoTrack root still needs, as NoTrack
    /// follows the entity. With `keep_root`, a present root at `path` keeps the handle it has just
    /// got.
    ///
    /// Handles are keyed by path, so a kept handle stays under the old path of its moved inode. A
    /// tracked root that needs a handle at the same path later, its own or its parent's, finds the
    /// kept one there and is armed on the moved inode: the new entity at its path is not watched.
    ///
    /// A NoTrack file that its parent reports, having no handle of its own, cannot follow the
    /// entity: the root is dropped and reported removed, although the file still exists, so that
    /// watching it again arms it anew. Whether the file has a handle of its own depends on the
    /// order of the watches: it has none when its parent was already watched for everything, as
    /// the parent of a tracked root, when it was watched. The same roots watched the other way
    /// round follow the file, and report it under its old path.
    fn drop_handles_below(&mut self, path: &Path, keep_root: bool, without_os_call: bool) {
        let no_track_roots: Vec<(PathBuf, RootWatch)> = paths_below(&self.root_paths, path)
            .filter_map(|root| {
                let watch = self.watches.get(root)?;
                (watch.mode.target_mode == TargetMode::NoTrack).then(|| (root.clone(), *watch))
            })
            .collect();
        let keeps_root = keep_root
            && self
                .watches
                .get(path)
                .is_some_and(|root| root.state != RootState::Missing);
        let handles: Vec<PathBuf> = paths_below(&self.handle_paths, path)
            .filter(|handle_path| {
                let kept = is_kept_by_no_track_root(&no_track_roots, handle_path)
                    || (keeps_root && handle_path.as_path() == path);
                !kept
            })
            .cloned()
            .collect();
        self.remove_handles(handles, without_os_call);

        for (root, watch) in no_track_roots {
            if root.as_path() == path
                || self.has_handle(&root)
                || root.parent().is_some_and(|parent| self.has_handle(parent))
            {
                continue;
            }
            tracing::debug!(
                "dropping the NoTrack root {} that its parent reported",
                root.display()
            );
            self.remove_root(&root);
            if watch.state != RootState::Missing {
                self.event_handler
                    .handle_event(Ok(
                        Event::new(EventKind::Remove(RemoveKind::File)).add_path(root)
                    ));
            }
        }
    }

    /// Removes the handles at `paths`; without an OS call when the kernel has removed their
    /// watches already.
    fn remove_handles(&mut self, paths: Vec<PathBuf>, without_os_call: bool) {
        for path in paths {
            if without_os_call {
                self.unbind_by_right(&path);
            } else {
                self.remove_handle(&path);
            }
        }
    }

    /// Removes the handle at `path`, if there is one. The kernel watch stays while another spelling
    /// of the inode has a handle, and is narrowed down to what the others ask for.
    fn remove_handle(&mut self, path: &Path) {
        let Some(w) = self.handle_at(path).map(|(w, _)| w.clone()) else {
            return;
        };
        let before = self.kernel_mask(&w);
        tracing::trace!("removing inotify watch: {}", path.display());
        self.unbind_by_right(path);
        self.sync_kernel_mask(&w, before);
    }

    /// Makes the kernel watch the inode of `w` for what its spellings ask for, now that it watches
    /// it for `before`, and removes the watch when no spelling is left. inotify changes a mask only
    /// through a path, and only a spelling that still leads to the inode is used: a stale one leads
    /// to another inode, whose watch the change would narrow instead. When no spelling leads to the
    /// inode any more, the watch keeps its wider mask, whose extra events are filtered out.
    fn sync_kernel_mask(&mut self, w: &WatchDescriptor, before: WatchMask) {
        let spellings = self.spellings(w);
        if spellings.is_empty() {
            self.remove_kernel_watch(w.clone());
            return;
        }
        let mask = self.kernel_mask(w);
        if mask == before {
            return;
        }
        for path in spellings {
            if !self.leads_to(w, &path) {
                tracing::debug!("{} leads to another inode by now", path.display());
                continue;
            }
            tracing::trace!(
                ?mask,
                "changing the mask of inotify watch: {}",
                path.display()
            );
            match self.add_kernel_watch(&path, mask) {
                None => return,
                Some(Ok(other)) if other == *w => return,
                // The path changed since it was checked.
                Some(Ok(other)) => {
                    tracing::debug!("{} leads to another inode by now", path.display());
                    self.restore_kernel_watch(other, &path);
                }
                Some(Err(e)) => {
                    tracing::debug!(?e, "cannot change the mask of {}", path.display());
                }
            }
        }
        tracing::debug!(
            ?mask,
            "no spelling could narrow the watch, which keeps its wider mask"
        );
    }

    /// Whether `path` still leads to the inode that `w` watches.
    fn leads_to(&self, w: &WatchDescriptor, path: &Path) -> bool {
        self.inodes
            .get(w)
            .is_none_or(|inode| metadata(path).is_ok_and(|meta| (meta.dev(), meta.ino()) == *inode))
    }

    /// An add through `path` changed the watch `w` of another inode than meant: gives it back the
    /// mask its spellings ask for, or removes it when it is new.
    fn restore_kernel_watch(&mut self, w: WatchDescriptor, path: &Path) {
        if self.watch_handles.get_by_left(&w).is_none() {
            self.remove_kernel_watch(w);
            return;
        }
        let mask = self.kernel_mask(&w);
        if let Some(Err(e)) = self.add_kernel_watch(path, mask) {
            tracing::debug!(?e, "cannot change the mask of {}", path.display());
        }
    }

    /// Watches the inode at `path` for `mask`, which replaces the mask of a watch it has already.
    fn add_kernel_watch(
        &mut self,
        path: &Path,
        mask: WatchMask,
    ) -> Option<io::Result<WatchDescriptor>> {
        let inotify = self.inotify.as_mut()?;
        Some(inotify.watches().add(path, mask))
    }

    /// Removes a kernel watch; a failure is only logged, as the kernel drops the watch of a
    /// deleted inode by itself.
    fn remove_kernel_watch(&mut self, w: WatchDescriptor) {
        if let Some(ref mut inotify) = self.inotify
            && let Err(e) = inotify.watches().remove(w)
        {
            tracing::trace!(?e, "inotify watch was already gone");
        }
    }

    #[tracing::instrument(level = "trace", skip(self))]
    fn add_watch(&mut self, path: PathBuf, watch_mode: WatchMode) -> Result<()> {
        if let Some(existing) = self.watches.get(&path).copied() {
            let need_upgrade_to_recursive = match existing.mode.recursive_mode {
                RecursiveMode::Recursive => false,
                RecursiveMode::NonRecursive => {
                    watch_mode.recursive_mode == RecursiveMode::Recursive
                }
            };
            let need_to_track = match existing.mode.target_mode {
                TargetMode::TrackPath => false,
                TargetMode::NoTrack => watch_mode.target_mode == TargetMode::TrackPath,
            };
            tracing::trace!(
                ?need_upgrade_to_recursive,
                ?need_to_track,
                "upgrading existing watch for path: {}",
                path.display()
            );
            if need_to_track {
                self.track_ancestors(&path);
            }
            if let Err(error) = self.upgrade_root(
                &path,
                need_to_track,
                need_upgrade_to_recursive && existing.state != RootState::Missing,
            ) {
                if need_to_track {
                    self.untrack_ancestors(&path);
                }
                return Err(error);
            }
            let root = self.watches.get_mut(&path).unwrap();
            root.mode.upgrade_with(watch_mode);
            let mode = root.mode;
            // A tracked root that could not be armed when it was last seen gets another try,
            // so that watching it again is the way to recover from a failed arm.
            if mode.target_mode == TargetMode::TrackPath && root.state == RootState::Missing {
                let state = self.arm_root_or_rollback(&path, mode.recursive_mode)?;
                if state != RootState::Missing {
                    self.note_armed(&path, state);
                }
            }
            return Ok(());
        }

        if watch_mode.target_mode == TargetMode::TrackPath {
            // The ancestors are counted first, so that a failed arm releases the chain it added.
            self.track_ancestors(&path);
            let state = match self.arm_root_or_rollback(&path, watch_mode.recursive_mode) {
                Ok(state) => state,
                Err(error) => {
                    self.untrack_ancestors(&path);
                    return Err(error);
                }
            };
            self.insert_root(
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
            meta.is_file_without_hardlinks(),
            true,
        )?;
        let state = if meta.is_dir() {
            RootState::Directory
        } else {
            RootState::File
        };
        self.insert_root(
            path,
            RootWatch {
                mode: watch_mode,
                state,
            },
        );

        Ok(())
    }

    /// Arms the chain of a root that is tracked from now on, and watches the entries of a root
    /// that is recursive from now on.
    fn upgrade_root(&mut self, path: &Path, track: bool, recurse: bool) -> Result<()> {
        if track {
            self.arm_chain(path)?;
        }
        if recurse && metadata(path).map_err(Error::io)?.is_dir() {
            self.add_maybe_recursive_watch(path.to_path_buf(), true, false, true)?;
        }
        Ok(())
    }

    /// Arms a root, and drops what the arm installed at and below the root when it fails halfway,
    /// so that a missing root never has live handles. The ancestors it armed stay: they are
    /// counted, and released with the root.
    fn arm_root_or_rollback(
        &mut self,
        root: &Path,
        recursive_mode: RecursiveMode,
    ) -> Result<RootState> {
        self.armed = Some(Vec::new());
        let result = self.arm_root(root, recursive_mode);
        let armed = self.armed.take().unwrap_or_default();
        if let Err(error) = &result {
            let added: Vec<PathBuf> = armed
                .into_iter()
                .filter(|handle_path| handle_path.starts_with(root))
                .collect();
            tracing::debug!(
                ?error,
                "dropping the {} handles of a root that could not be armed: {}",
                added.len(),
                root.display()
            );
            for handle_path in added {
                self.remove_handle(&handle_path);
            }
        }
        result
    }

    /// Watches a tracked root: its ancestors, and the root itself if it exists. The parent reports
    /// the root, so the root only gets a watch of its own when it is a directory or a hardlink.
    fn arm_root(&mut self, root: &Path, recursive_mode: RecursiveMode) -> Result<RootState> {
        if !self.arm_chain(root)? {
            return Ok(RootState::Missing);
        }
        let meta = match metadata(root) {
            Ok(meta) => meta,
            Err(e) if is_absent(&e) => return Ok(RootState::Missing),
            Err(e) => return Err(Error::io_watch(e).add_path(root.to_path_buf())),
        };
        self.add_maybe_recursive_watch(
            root.to_path_buf(),
            recursive_mode.is_recursive() && meta.is_dir(),
            meta.is_file_without_hardlinks(),
            false,
        )?;
        Ok(if meta.is_dir() {
            RootState::Directory
        } else {
            RootState::File
        })
    }

    /// Watches the ancestors of `root` that exist, from the top down, for their entries; the
    /// parent for everything. Returns whether the parent exists: a root without a parent is
    /// watched directly.
    fn arm_chain(&mut self, root: &Path) -> Result<bool> {
        let Some(parent) = root.parent() else {
            return Ok(true);
        };
        let ancestors: Vec<PathBuf> = root.ancestors().skip(1).map(Path::to_path_buf).collect();
        for ancestor in ancestors.into_iter().rev() {
            if !dir_present(&ancestor)? {
                return Ok(false);
            }
            if ancestor == parent {
                self.add_single_watch(ancestor, false, false)?;
            } else if let Err(e) = self.add_watch_with_mask(ancestor.clone(), ENTRY_MASK, false) {
                tracing::debug!(?e, "cannot watch ancestor: {}", ancestor.display());
            }
        }
        Ok(true)
    }

    fn track_ancestors(&mut self, root: &Path) {
        for ancestor in root.ancestors().skip(1) {
            *self.ancestors.entry(ancestor.to_path_buf()).or_insert(0) += 1;
        }
    }

    /// Forgets the ancestors of an unwatched root, dropping the watches nobody needs any more.
    /// Only the parent can lose its role while other roots still count it, as it may no longer be
    /// the parent of a root; the roots right below a higher ancestor are the same as before.
    fn untrack_ancestors(&mut self, root: &Path) {
        let parent = root.parent();
        for ancestor in root.ancestors().skip(1) {
            let Some(count) = self.ancestors.get_mut(ancestor) else {
                continue;
            };
            *count -= 1;
            if *count == 0 {
                self.ancestors.remove(ancestor);
            } else if Some(ancestor) != parent {
                continue;
            }
            self.release_handle(ancestor);
        }
    }

    /// Whether a root needs the handle at `path`, besides the tracked roots that count `path` as
    /// an ancestor: `path` is a root, it lies below a recursive root, or a root right below it is
    /// reported by it, having no handle of its own. A directory root reports its entries through
    /// its own handle: the handles of its entries are not its.
    fn is_handle_needed(&self, path: &Path) -> bool {
        self.watches.contains_key(path)
            || self.is_below_recursive_root(path)
            || self.has_dependent_file_root(path)
    }

    fn is_below_recursive_root(&self, path: &Path) -> bool {
        path.ancestors().skip(1).any(|ancestor| {
            self.watches
                .get(ancestor)
                .is_some_and(|root| root.mode.recursive_mode.is_recursive())
        })
    }

    fn has_dependent_file_root(&self, dir: &Path) -> bool {
        self.dir_roots
            .get(dir)
            .is_some_and(|roots| roots.without_handle > 0)
    }

    /// The mask an ancestor of tracked roots needs: everything as the parent of a root, else only
    /// its entries.
    fn chain_mask(&self, dir: &Path) -> WatchMask {
        if self.dir_roots.contains_key(dir) {
            FULL_MASK
        } else {
            ENTRY_MASK
        }
    }

    #[tracing::instrument(level = "trace", skip(self))]
    fn add_maybe_recursive_watch(
        &mut self,
        path: PathBuf,
        is_recursive: bool,
        is_file_without_hardlinks: bool,
        watch_self: bool,
    ) -> Result<()> {
        if is_recursive {
            self.add_recursive_watch(&path, watch_self, &mut Vec::new())
        } else {
            self.add_single_watch(path, is_file_without_hardlinks, watch_self)
        }
    }

    /// Watches the directories at and below `path` for everything, and `path` for its own removal
    /// too with `watch_self`. Each directory is added to `dirs` once it is watched, so that a walk
    /// that fails halfway leaves the ones watched so far.
    fn add_recursive_watch(
        &mut self,
        path: &Path,
        mut watch_self: bool,
        dirs: &mut Vec<PathBuf>,
    ) -> Result<()> {
        for entry in WalkDir::new(path)
            .follow_links(self.follow_links)
            .into_iter()
            .filter_map(filter_dir)
        {
            let dir = entry.into_path();
            self.add_single_watch(dir.clone(), false, watch_self)?;
            watch_self = false;
            dirs.push(dir);
        }
        Ok(())
    }

    /// Watches an entry that appeared in a watched directory, under each of the `spellings` of the
    /// directory that asked for it.
    ///
    /// inotify gives each directory one descriptor, however it is reached, so the entry is walked
    /// once, through the first spelling that watches it recursively: the other ones are bound to
    /// the handles that the walk found, under their own paths, without a walk of their own. A walk
    /// per spelling would read each new directory once per spelling, and each of these opens would
    /// be reported under every spelling. A full watch table stops the entry, and is passed on;
    /// any other failure only concerns the spelling it names.
    fn add_entry_watches(&mut self, spellings: Vec<AddWatch>) -> Result<()> {
        let (recursive, single): (Vec<AddWatch>, Vec<AddWatch>) = spellings
            .into_iter()
            .partition(|(_, is_recursive, _)| *is_recursive);
        let mut recursive = recursive.into_iter().map(|(path, _, _)| path);
        if let Some(walked) = recursive.next() {
            let mut dirs = Vec::new();
            let result = self.add_recursive_watch(&walked, false, &mut dirs);
            for path in recursive {
                self.bind_walked_dirs(&walked, &dirs, &path);
            }
            stop_on_full_table(result)?;
        }
        for (path, _, is_file_without_hardlinks) in single {
            stop_on_full_table(self.add_single_watch(path, is_file_without_hardlinks, false))?;
        }
        Ok(())
    }

    /// Binds the directories at and below `path` to the handles that the walk of `walked`, another
    /// spelling of the same directory, found at `dirs`: they are the same inodes, which the kernel
    /// watches for everything already.
    fn bind_walked_dirs(&mut self, walked: &Path, dirs: &[PathBuf], path: &Path) {
        for dir in dirs {
            let Ok(relative) = dir.strip_prefix(walked) else {
                continue;
            };
            let Some(w) = self.handle_at(dir).map(|(w, _)| w.clone()) else {
                continue;
            };
            let spelling = if relative.as_os_str().is_empty() {
                path.to_path_buf()
            } else {
                path.join(relative)
            };
            self.bind_walked_dir(w, spelling);
        }
    }

    /// Binds `path` to the handle `w`, like [`Self::add_single_watch`] without an add: the kernel
    /// watches the inode for everything already. A handle of its own that asks for more than that
    /// goes through an add.
    fn bind_walked_dir(&mut self, w: WatchDescriptor, path: PathBuf) {
        let existing = self.handle_at(&path).map(|(_, info)| info.mask);
        if existing.is_some_and(|existing| existing.contains(FULL_MASK)) {
            return;
        }
        let mask = existing.map_or(FULL_MASK, |existing| existing.union(FULL_MASK));
        let result = if self.kernel_mask(&w).contains(mask) {
            tracing::trace!("binding the walked {}", path.display());
            self.record_handle(w, path, mask)
        } else {
            self.add_single_watch(path, false, false)
        };
        if let Err(e) = result {
            tracing::debug!(?e, "cannot watch the walked directory");
        }
    }

    /// Watches `path` for everything; see [`Self::add_watch_with_mask`].
    #[tracing::instrument(level = "trace", skip(self))]
    fn add_single_watch(
        &mut self,
        path: PathBuf,
        is_file_without_hardlinks: bool,
        watch_self: bool,
    ) -> Result<()> {
        let mask = if watch_self {
            FULL_MASK.union(SELF_MASK)
        } else {
            FULL_MASK
        };
        self.add_watch_with_mask(path, mask, is_file_without_hardlinks)
    }

    /// Watches `path` for `mask`, besides what it is watched for already.
    #[tracing::instrument(level = "trace", skip(self))]
    fn add_watch_with_mask(
        &mut self,
        path: PathBuf,
        mask: WatchMask,
        is_file_without_hardlinks: bool,
    ) -> Result<()> {
        let existing = self.handle_at(&path).map(|(_, info)| info.mask);
        if existing.is_some_and(|existing| existing.contains(mask)) {
            tracing::trace!(
                "watch handle already exists and no need to upgrade: {}",
                path.display()
            );
            return Ok(());
        }

        if is_file_without_hardlinks
            && let Some(parent) = path.parent()
            && self
                .handle_at(parent)
                .is_some_and(|(_, info)| info.mask.contains(FULL_MASK))
        {
            tracing::trace!(
                "parent dir watch handle already exists and is a file without hardlinks: {}",
                path.display()
            );
            return Ok(());
        }

        let mask = existing.map_or(mask, |existing| existing.union(mask));

        #[cfg(test)]
        if existing.is_none()
            && self
                .watch_limit
                .is_some_and(|limit| self.handle_paths.len() >= limit)
        {
            return Err(add_watch_error(
                io::Error::from_raw_os_error(libc::ENOSPC),
                path,
            ));
        }

        tracing::trace!("adding inotify watch: {}", path.display());
        // The inode may be watched through another spelling already: its mask is kept.
        let Some(result) = self.add_kernel_watch(&path, mask.union(WatchMask::MASK_ADD)) else {
            return Ok(());
        };
        let w = result.map_err(|e| add_watch_error(e, path.clone()))?;
        self.record_handle(w, path, mask)
    }

    /// Records the handle inotify returned for `path`, which asked for `mask`: the kernel watches
    /// the inode for it, besides what the other spellings of the inode ask for; see [`Aliases`].
    ///
    /// A handle that `path` had on another descriptor is stale, as the path leads to another inode
    /// by now. `path` is bound to the new descriptor first, and only then is the old one narrowed
    /// down to what its other spellings ask for: should the narrowing go through a spelling that
    /// leads to the new inode as well, the new watch is given back what its spellings ask for,
    /// `path` included, instead of being removed.
    fn record_handle(&mut self, w: WatchDescriptor, path: PathBuf, mask: WatchMask) -> Result<()> {
        let stale = match self
            .handle_at(&path)
            .map(|(old, info)| (old.clone(), *info))
        {
            Some((old, info)) if old == w => {
                self.set_handle_info(&path, WatchInfo { mask, ..info });
                return Ok(());
            }
            Some((old, _)) => Some(old),
            None => None,
        };
        let (is_dir, inode) = match self.watch_handles.get_by_left(&w) {
            Some((other, info)) => {
                tracing::debug!(
                    "{} is another spelling of the watched {}",
                    path.display(),
                    other.display()
                );
                (info.is_dir, None)
            }
            None => match metadata(&path) {
                Ok(meta) => (meta.is_dir(), Some((meta.dev(), meta.ino()))),
                Err(e) => {
                    // No spelling has the new descriptor, which goes.
                    self.remove_kernel_watch(w);
                    return Err(Error::io(e).add_path(path));
                }
            },
        };
        let stale = stale.map(|old| {
            let before = self.kernel_mask(&old);
            self.unbind_by_right(&path);
            (old, before)
        });
        if let Some(inode) = inode {
            self.inodes.insert(w.clone(), inode);
        }
        self.bind_handle(w, path, WatchInfo { mask, is_dir });
        if let Some((old, before)) = stale {
            self.sync_kernel_mask(&old, before);
        }
        Ok(())
    }

    /// Changes the mask of the handle at `path`. The kernel watch keeps what the other spellings
    /// of the inode ask for. A failure keeps the handle with its wider mask, whose extra events are
    /// filtered out, unless nothing is at the path any more: the handle is stale then, and goes.
    fn set_handle_mask(&mut self, path: &Path, mask: WatchMask) {
        let Some((w, info)) = self.handle_at(path).map(|(w, info)| (w.clone(), *info)) else {
            return;
        };
        if info.mask == mask {
            return;
        }
        let before = self.kernel_mask(&w);
        self.set_handle_info(path, WatchInfo { mask, ..info });
        let after = self.kernel_mask(&w);
        if after == before {
            return;
        }
        tracing::trace!(
            ?mask,
            "changing the mask of inotify watch: {}",
            path.display()
        );
        match self.add_kernel_watch(path, after) {
            None => {}
            Some(Ok(new_w)) if new_w == w => {}
            // The path leads to another inode by now: the old handle is stale, and the path moves
            // to the new one, which the add has just watched for `after`.
            Some(Ok(new_w)) => {
                self.set_handle_info(path, info);
                if let Err(e) = self.record_handle(new_w.clone(), path.to_path_buf(), mask) {
                    tracing::debug!(?e, "cannot watch {} again", path.display());
                }
                self.sync_kernel_mask(&new_w, after);
            }
            Some(Err(e)) if is_absent(&e) => {
                tracing::debug!(?e, "the watched {} is gone", path.display());
                self.set_handle_info(path, info);
                self.remove_handle(path);
            }
            Some(Err(e)) => {
                tracing::debug!(?e, "cannot change the mask of {}", path.display());
                self.set_handle_info(path, info);
            }
        }
    }

    /// Unwatches a root. The handles the other roots still need stay: a handle that other tracked
    /// roots count as an ancestor becomes a chain handle, and the tracked roots below a recursive
    /// root are armed again, in case the walk took a handle of theirs. Once the root is found, the
    /// unwatch succeeds and all of its bookkeeping runs: a handle that cannot be changed, as its
    /// directory is gone or cannot be read any more, is only logged.
    #[tracing::instrument(level = "trace", skip(self))]
    fn remove_watch(&mut self, path: PathBuf) -> Result<()> {
        let Some(root) = self.remove_root(&path) else {
            return Err(Error::watch_not_found().add_path(path));
        };
        let reported_by_parent = !self.has_handle(&path);
        if root.mode.recursive_mode.is_recursive() {
            self.release_handles_below(&path);
        } else {
            self.release_handle(&path);
        }
        if root.mode.target_mode == TargetMode::TrackPath {
            self.untrack_ancestors(&path);
        }
        if reported_by_parent && let Some(parent) = path.parent() {
            self.release_handle(parent);
        }
        Ok(())
    }

    /// Drops the handle at `path`, unless a root still needs it; an ancestor of tracked roots keeps
    /// the mask they need.
    fn release_handle(&mut self, path: &Path) {
        if !self.has_handle(path) || self.is_handle_needed(path) {
            return;
        }
        if self.ancestors.contains_key(path) {
            let mask = self.chain_mask(path);
            self.set_handle_mask(path, mask);
        } else {
            self.remove_handle(path);
        }
    }

    /// [`Self::release_handle`] for every handle at and below `path`, then arms the tracked roots
    /// below again.
    fn release_handles_below(&mut self, path: &Path) {
        let handles: Vec<PathBuf> = paths_below(&self.handle_paths, path).cloned().collect();
        for handle_path in handles {
            self.release_handle(&handle_path);
        }
        let roots: Vec<(PathBuf, RecursiveMode)> = paths_below(&self.root_paths, path)
            .filter_map(|root| {
                let watch = self.watches.get(root)?;
                (watch.mode.target_mode == TargetMode::TrackPath
                    && watch.state != RootState::Missing)
                    .then(|| (root.clone(), watch.mode.recursive_mode))
            })
            .collect();
        for (root, recursive_mode) in roots {
            if let Err(e) = self.arm_root(&root, recursive_mode) {
                tracing::debug!(?e, "cannot arm the root again: {}", root.display());
            }
        }
    }

    fn remove_all_watches(&mut self) -> Result<()> {
        if let Some(ref mut inotify) = self.inotify {
            let mut inotify_watches = inotify.watches();
            for (w, p, _) in &self.watch_handles {
                inotify_watches
                    .remove(w.clone())
                    .map_err(|e| Error::io(e).add_path(p.into()))?;
            }
            self.watch_handles.clear();
            self.aliases.clear();
            self.inodes.clear();
            self.watches.clear();
            self.ancestors.clear();
            self.root_paths.clear();
            self.handle_paths.clear();
            self.dir_roots.clear();
        }
        Ok(())
    }

    /// Adds a root, keeping the indexes of `watches` up to date.
    fn insert_root(&mut self, path: PathBuf, watch: RootWatch) {
        if self.watches.insert(path.clone(), watch).is_some() {
            return;
        }
        let has_handle = self.has_handle(&path);
        if let Some(parent) = path.parent() {
            let roots = self.dir_roots.entry(parent.to_path_buf()).or_default();
            roots.count += 1;
            if !has_handle {
                roots.without_handle += 1;
            }
        }
        self.root_paths.insert(path);
    }

    /// Removes a root, keeping the indexes of `watches` up to date.
    fn remove_root(&mut self, path: &Path) -> Option<RootWatch> {
        let watch = self.watches.remove(path)?;
        self.root_paths.remove(path);
        let has_handle = self.has_handle(path);
        if let Some(parent) = path.parent()
            && let Some(roots) = self.dir_roots.get_mut(parent)
        {
            roots.count = roots.count.saturating_sub(1);
            if !has_handle {
                roots.without_handle = roots.without_handle.saturating_sub(1);
            }
            if roots.count == 0 {
                self.dir_roots.remove(parent);
            }
        }
        Some(watch)
    }

    /// The handle at `path`: the descriptor of its inode, and what `path` asked for.
    fn handle_at(&self, path: &Path) -> Option<(&WatchDescriptor, &WatchInfo)> {
        self.watch_handles
            .get_by_right(path)
            .or_else(|| self.aliases.get(path))
    }

    fn has_handle(&self, path: &Path) -> bool {
        self.handle_at(path).is_some()
    }

    /// Every spelling that has the handle `w`, the one it is bound to first.
    fn spellings(&self, w: &WatchDescriptor) -> Vec<PathBuf> {
        self.watch_handles
            .get_by_left(w)
            .map(|(path, _)| path)
            .into_iter()
            .chain(self.aliases.paths_of(w))
            .cloned()
            .collect()
    }

    /// What the kernel watches the inode of `w` for: what all of its spellings ask for.
    fn kernel_mask(&self, w: &WatchDescriptor) -> WatchMask {
        let primary = self.watch_handles.get_by_left(w).map(|(_, info)| info.mask);
        self.aliases
            .paths_of(w)
            .iter()
            .filter_map(|path| self.aliases.get(path))
            .map(|(_, info)| info.mask)
            .chain(primary)
            .fold(WatchMask::empty(), WatchMask::union)
    }

    /// Records the handle `w` at `path`, which has none, keeping the indexes of `watch_handles` up
    /// to date: `path` is an alias when another spelling has the handle already.
    fn bind_handle(&mut self, w: WatchDescriptor, path: PathBuf, info: WatchInfo) {
        self.note_handle_new(&path);
        if self.watch_handles.get_by_left(&w).is_some() {
            self.aliases.insert(w, path, info);
        } else {
            self.watch_handles.insert(w, path, info);
        }
    }

    /// Changes what the handle at `path` asks for.
    fn set_handle_info(&mut self, path: &Path, info: WatchInfo) {
        match self
            .watch_handles
            .get_by_right(path)
            .map(|(w, _)| w.clone())
        {
            Some(w) => {
                self.watch_handles.insert(w, path.to_path_buf(), info);
            }
            None => self.aliases.set_info(path, info),
        }
    }

    /// Forgets the handle `w` under every spelling, keeping the indexes of `watch_handles` up to
    /// date.
    fn unbind_by_left(&mut self, w: &WatchDescriptor) {
        self.inodes.remove(w);
        if let Some((path, _)) = self.watch_handles.remove_by_left(w) {
            self.note_handle_gone(&path);
        }
        for path in self.aliases.remove_all_of(w) {
            self.note_handle_gone(&path);
        }
    }

    /// Forgets the handle at `path`, keeping the indexes of `watch_handles` up to date. An alias
    /// of the handle takes it over.
    fn unbind_by_right(&mut self, path: &Path) -> Option<WatchDescriptor> {
        let w = match self.watch_handles.remove_by_right(path) {
            Some((w, _)) => {
                match self.aliases.remove_first_of(&w) {
                    Some((alias, info)) => {
                        self.watch_handles.insert(w.clone(), alias, info);
                    }
                    None => {
                        self.inodes.remove(&w);
                    }
                }
                w
            }
            None => self.aliases.remove(path)?,
        };
        self.note_handle_gone(path);
        Some(w)
    }

    fn note_handle_new(&mut self, path: &Path) {
        self.handle_paths.insert(path.to_path_buf());
        if let Some(armed) = &mut self.armed {
            armed.push(path.to_path_buf());
        }
        if self.watches.contains_key(path)
            && let Some(roots) = path
                .parent()
                .and_then(|parent| self.dir_roots.get_mut(parent))
        {
            roots.without_handle = roots.without_handle.saturating_sub(1);
        }
    }

    fn note_handle_gone(&mut self, path: &Path) {
        self.handle_paths.remove(path);
        if self.watches.contains_key(path)
            && let Some(roots) = path
                .parent()
                .and_then(|parent| self.dir_roots.get_mut(parent))
        {
            roots.without_handle += 1;
        }
    }
}

#[cfg(test)]
impl EventLoop {
    /// Checks the indexes against the maps they are kept for.
    fn check_indexes(&self) {
        let root_paths: BTreeSet<PathBuf> = self.watches.keys().cloned().collect();
        assert_eq!(self.root_paths, root_paths, "the index of the roots");
        let handle_paths: BTreeSet<PathBuf> = (&self.watch_handles)
            .into_iter()
            .map(|(_, path, _)| path.clone())
            .chain(self.aliases.at.keys().cloned())
            .collect();
        assert_eq!(
            self.handle_paths.len(),
            self.watch_handles.iter().count() + self.aliases.at.len(),
            "a path is bound twice"
        );
        assert_eq!(self.handle_paths, handle_paths, "the index of the handles");
        for (w, paths) in &self.aliases.of {
            assert!(
                self.watch_handles.get_by_left(w).is_some(),
                "the aliases {paths:?} of an unbound descriptor"
            );
            assert!(!paths.is_empty(), "an empty list of aliases");
            for path in paths {
                assert!(
                    self.aliases
                        .at
                        .get(path)
                        .is_some_and(|(other, _)| other == w),
                    "the index of the alias {path:?}"
                );
            }
        }
        let aliases: usize = self.aliases.of.values().map(Vec::len).sum();
        assert_eq!(aliases, self.aliases.at.len(), "the index of the aliases");
        let bound: std::collections::HashSet<&WatchDescriptor> =
            self.watch_handles.iter().map(|(w, _, _)| w).collect();
        let inodes: std::collections::HashSet<&WatchDescriptor> = self.inodes.keys().collect();
        assert_eq!(bound, inodes, "the inodes of the descriptors");
        let mut dir_roots: HashMap<PathBuf, DirRoots, FxBuildHasher> = HashMap::default();
        for root in self.watches.keys() {
            if let Some(parent) = root.parent() {
                let roots = dir_roots.entry(parent.to_path_buf()).or_default();
                roots.count += 1;
                if !self.has_handle(root) {
                    roots.without_handle += 1;
                }
            }
        }
        assert_eq!(
            self.dir_roots, dir_roots,
            "the roots right below each directory"
        );
    }
}

/// The paths of `paths` at and below `path`, which follow it in order.
fn paths_below<'a>(
    paths: &'a BTreeSet<PathBuf>,
    path: &'a Path,
) -> impl Iterator<Item = &'a PathBuf> {
    paths
        .range::<Path, _>((Bound::Included(path), Bound::Unbounded))
        .take_while(move |other| other.starts_with(path))
}

/// Whether one of the NoTrack roots at or below a gone path still needs the handle at
/// `handle_path`: its own, or one below it when it is recursive. A directory root reports its
/// entries through its own handle.
fn is_kept_by_no_track_root(roots: &[(PathBuf, RootWatch)], handle_path: &Path) -> bool {
    roots.iter().any(|(root, watch)| {
        root == handle_path
            || (watch.mode.recursive_mode.is_recursive() && handle_path.starts_with(root))
    })
}

/// How many components `a` and `b` share from the start.
fn common_prefix_len(a: &Path, b: &Path) -> usize {
    a.components()
        .zip(b.components())
        .take_while(|(a, b)| a == b)
        .count()
}

/// Whether the error says that nothing is at the path (yet).
fn is_absent(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// Whether `path` leads to a directory, following symlinks. An absent path is not one; any other
/// failure is an error, as nothing can be watched through the path.
fn dir_present(path: &Path) -> Result<bool> {
    match metadata(path) {
        Ok(meta) => Ok(meta.is_dir()),
        Err(e) if is_absent(&e) => Ok(false),
        Err(e) => Err(Error::io(e).add_path(path.to_path_buf())),
    }
}

/// Passes on a full watch table, which stops the adds that follow; any other failure is only
/// logged, as it concerns the path it names.
fn stop_on_full_table(result: Result<()>) -> Result<()> {
    match result {
        Err(e) if matches!(e.kind, ErrorKind::MaxFilesWatch) => Err(e),
        Err(e) => {
            tracing::debug!(?e, "cannot watch a new entry");
            Ok(())
        }
        Ok(()) => Ok(()),
    }
}

fn add_watch_error(e: io::Error, path: PathBuf) -> Error {
    if e.raw_os_error() == Some(libc::ENOSPC) {
        // do not report inotify limits as "no more space" on linux #266
        Error::new(ErrorKind::MaxFilesWatch)
    } else if e.kind() == io::ErrorKind::NotFound {
        Error::new(ErrorKind::PathNotFound)
    } else {
        Error::io(e)
    }
    .add_path(path)
}

/// return `DirEntry` when it is a directory
fn filter_dir(e: walkdir::Result<walkdir::DirEntry>) -> Option<walkdir::DirEntry> {
    if let Ok(e) = e
        && e.file_type().is_dir()
    {
        return Some(e);
    }
    None
}

impl INotifyWatcher {
    fn from_event_handler(
        event_handler: Box<dyn EventHandler>,
        follow_links: bool,
    ) -> Result<Self> {
        let inotify = Inotify::init()?;
        let event_loop = EventLoop::new(inotify, event_handler, follow_links)?;
        let channel = event_loop.event_loop_tx.clone();
        let waker = Arc::clone(&event_loop.event_loop_waker);
        event_loop.run();
        Ok(INotifyWatcher { channel, waker })
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

        // we expect the event loop to live and reply => unwraps must not panic
        self.channel.send(msg).unwrap();
        self.waker.wake().unwrap();
        rx.recv().unwrap()
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

        // we expect the event loop to live and reply => unwraps must not panic
        self.channel.send(msg).unwrap();
        self.waker.wake().unwrap();
        rx.recv().unwrap()
    }
}

impl Watcher for INotifyWatcher {
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
    fn unwatch(&mut self, path: &Path) -> Result<()> {
        self.unwatch_inner(path)
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn configure(&mut self, config: Config) -> Result<bool> {
        let (tx, rx) = bounded(1);
        self.channel.send(EventLoopMsg::Configure(config, tx))?;
        self.waker.wake()?;
        rx.recv()?
    }

    fn kind() -> crate::WatcherKind {
        crate::WatcherKind::Inotify
    }

    /// The watches that report the watched paths, without the ancestors watched for their entries
    /// only; see [`INotifyWatcher::get_chain_handles`].
    #[cfg(test)]
    fn get_watch_handles(&self) -> std::collections::HashSet<std::path::PathBuf> {
        self.handles(|info| !info.entries_only())
    }
}

#[cfg(test)]
impl INotifyWatcher {
    /// Makes the watcher fail to add a watch once it holds `limit` handles, as a full inotify
    /// watch table does.
    fn set_watch_limit(&self, limit: Option<usize>) {
        let (tx, rx) = bounded(1);
        self.channel
            .send(EventLoopMsg::SetWatchLimit(limit, tx))
            .unwrap();
        self.waker.wake().unwrap();
        rx.recv().unwrap();
    }

    /// The ancestors of tracked paths, watched for their entries only.
    fn get_chain_handles(&self) -> std::collections::HashSet<std::path::PathBuf> {
        self.handles(|info| info.entries_only())
    }

    fn handles(
        &self,
        keep: impl Fn(&WatchInfo) -> bool,
    ) -> std::collections::HashSet<std::path::PathBuf> {
        let (tx, rx) = bounded(1);
        self.channel
            .send(EventLoopMsg::GetWatchHandles(tx))
            .unwrap();
        self.waker.wake().unwrap();
        rx.recv()
            .unwrap()
            .into_iter()
            .filter(|(_, info)| keep(info))
            .map(|(path, _)| path)
            .collect()
    }
}

impl Drop for INotifyWatcher {
    fn drop(&mut self) {
        // we expect the event loop to live => unwrap must not panic
        self.channel.send(EventLoopMsg::Shutdown).unwrap();
        self.waker.wake().unwrap();
    }
}

trait MetadataNotifyExt {
    fn is_file_without_hardlinks(&self) -> bool;
}

impl MetadataNotifyExt for std::fs::Metadata {
    #[inline]
    fn is_file_without_hardlinks(&self) -> bool {
        self.is_file() && self.nlink() == 1
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        fs::Permissions,
        os::unix::fs::{MetadataExt, PermissionsExt, symlink},
        path::{Path, PathBuf},
        sync::{Arc, atomic::AtomicBool, mpsc},
        thread::{self, available_parallelism},
        time::Duration,
    };

    use super::{Config, Error, ErrorKind, Event, INotifyWatcher, Result, Watcher};

    use crate::{
        RecursiveMode, TargetMode,
        config::WatchMode,
        event::{
            AccessKind, AccessMode, CreateKind, DataChange, EventKind, ModifyKind, RemoveKind,
        },
        test::*,
    };

    fn watcher() -> (TestWatcher<INotifyWatcher>, Receiver) {
        channel()
    }

    #[test]
    fn inotify_watcher_is_send_and_sync() {
        fn check<T: Send + Sync>() {}
        check::<INotifyWatcher>();
    }

    #[test]
    fn native_error_type_on_missing_path() {
        let mut watcher = INotifyWatcher::new(|_| {}, Config::default()).unwrap();

        let result = watcher.watch(
            &PathBuf::from("/some/non/existant/path"),
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

        // a tracked path is waited for, however deep the missing part
        watcher
            .watch(
                &PathBuf::from("/some/non/existant/path"),
                WatchMode::non_recursive(),
            )
            .unwrap();
    }

    /// Runs manually.
    ///
    /// * Save actual value of the limit: `MAX_USER_WATCHES=$(sysctl -n fs.inotify.max_user_watches)`
    /// * Run the test.
    /// * Set the limit to 0: `sudo sysctl fs.inotify.max_user_watches=0` while test is running
    /// * Wait for the test to complete
    /// * Restore the limit `sudo sysctl fs.inotify.max_user_watches=$MAX_USER_WATCHES`
    #[test]
    #[ignore = "requires changing sysctl fs.inotify.max_user_watches while test is running"]
    fn recursive_watch_calls_handler_if_creating_a_file_raises_max_files_watch() {
        use std::time::Duration;

        let tmpdir = tempfile::tempdir().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let (proc_changed_tx, proc_changed_rx) = std::sync::mpsc::channel();
        let proc_path = Path::new("/proc/sys/fs/inotify/max_user_watches");
        let mut watcher = INotifyWatcher::new(
            move |result: Result<Event>| match result {
                Ok(event) => {
                    if event.paths.first().is_some_and(|path| path == proc_path) {
                        proc_changed_tx.send(()).unwrap();
                    }
                }
                Err(e) => tx.send(e).unwrap(),
            },
            Config::default(),
        )
        .unwrap();

        watcher
            .watch(tmpdir.path(), WatchMode::recursive())
            .unwrap();
        watcher
            .watch(proc_path, WatchMode::non_recursive())
            .unwrap();

        // give the time to set the limit
        proc_changed_rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap();

        let child_dir = tmpdir.path().join("child");
        std::fs::create_dir(child_dir).unwrap();

        let result = rx.recv_timeout(Duration::from_millis(500));

        assert!(
            matches!(
                &result,
                Ok(Error {
                    kind: ErrorKind::MaxFilesWatch,
                    paths: _,
                })
            ),
            "expected {:?}, found: {:#?}",
            ErrorKind::MaxFilesWatch,
            result
        );
    }

    /// https://github.com/notify-rs/notify/issues/678
    #[test]
    fn race_condition_on_unwatch_and_pending_events_with_deleted_descriptor() {
        let tmpdir = tempfile::tempdir().expect("tmpdir");
        let (tx, rx) = mpsc::channel();
        let mut inotify = INotifyWatcher::new(
            move |e: Result<Event>| {
                let e = match e {
                    Ok(e) if e.paths.is_empty() => e,
                    Ok(_) | Err(_) => return,
                };
                let _ = tx.send(e);
            },
            Config::default(),
        )
        .expect("inotify creation");

        let dir_path = tmpdir.path();
        let file_path = dir_path.join("foo");
        std::fs::File::create(&file_path).unwrap();

        let stop = Arc::new(AtomicBool::new(false));

        let handles: Vec<_> = (0..available_parallelism().unwrap().get().max(4))
            .map(|_| {
                let file_path = file_path.clone();
                let stop = Arc::clone(&stop);
                thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let _ = std::fs::File::open(&file_path).unwrap();
                    }
                })
            })
            .collect();

        let non_recursive = WatchMode::non_recursive();
        for _ in 0..(handles.len() * 4) {
            inotify.watch(dir_path, non_recursive).unwrap();
            inotify.unwatch(dir_path).unwrap();
        }

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for handle in handles {
            handle.join().ok().unwrap_or_default();
        }

        drop(inotify);

        let events: Vec<_> = rx.into_iter().map(|e| format!("{e:?}")).collect();

        const LOG_LEN: usize = 10;
        let events_len = events.len();
        assert!(
            events.is_empty(),
            "expected no events without path, but got {events_len}. first 10: {:#?}",
            &events[..LOG_LEN.min(events_len)]
        );
    }

    #[test]
    fn create_file() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();
        watcher.watch_recursively(&tmpdir);

        let path = tmpdir.path().join("entry");
        std::fs::File::create_new(&path).expect("create");

        rx.wait_ordered_exact([
            expected(tmpdir.path()).access_open_any().optional(),
            expected(&path).create_file(),
            expected(&path).access_open_any(),
            expected(&path).access_close_write(),
        ]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf()])
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
            expected(&path).create_file(),
            expected(&path).access_open_any(),
            expected(&path).access_close_write(),
        ]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()])
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
        assert!(matches!(
            result,
            Err(Error {
                paths: _,
                kind: ErrorKind::PathNotFound
            })
        ));
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

        // The parent is watched once its creation is seen; the file may exist by then, in which
        // case the watcher reports it itself and the open and close are not seen.
        rx.wait_ordered([expected(&path).create_file()]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.path().join("entry")])
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
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([lib.clone(), lib.join("sub")])
        );
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
        rx.wait_unordered_exact([expected(&a).remove_file(), expected(&b).remove_file()]);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));

        std::fs::rename(&moved, &lib).expect("rename back");
        rx.wait_unordered_exact([expected(&a).create_file(), expected(&b).create_file()]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([lib.clone(), lib.join("sub")])
        );

        std::fs::write(&a, "2").expect("write");
        rx.wait_ordered([expected(&a).modify_data_any()]);
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
        rx.wait_unordered([expected(&child).remove_folder()]);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));

        std::fs::create_dir_all(&child).expect("create_dir_all");
        rx.wait_unordered([expected(&child).create_folder()]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([parent, child.clone()])
        );

        std::fs::File::create_new(child.join("file")).expect("create");
        rx.wait_ordered([expected(child.join("file")).create_file()]);
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
    fn create_file_nested_in_recursive_watch() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let nested1_dir = tmpdir.path().join("nested1");
        let nested2_dir = nested1_dir.join("nested2");
        std::fs::create_dir_all(&nested2_dir).expect("create_dir");

        watcher.watch_recursively(&tmpdir);

        let path = nested2_dir.join("entry");
        std::fs::File::create_new(&path).expect("create");

        rx.wait_ordered_exact([
            expected(tmpdir.path()).access_open_any().optional(),
            expected(&nested1_dir).access_open_any().optional(),
            expected(&nested2_dir).access_open_any().optional(),
            expected(&path).create_file(),
            expected(&path).access_open_any(),
            expected(&path).access_close_write(),
        ]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([
                tmpdir.parent_path_buf(),
                tmpdir.to_path_buf(),
                nested1_dir,
                nested2_dir
            ])
        );
    }

    #[test]
    fn write_file() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        std::fs::File::create_new(&path).expect("create");

        watcher.watch_recursively(&tmpdir);
        std::fs::write(&path, b"123").expect("write");

        rx.wait_ordered_exact([
            expected(tmpdir.path()).access_open_any().optional(),
            expected(&path).access_open_any(),
            expected(&path).modify_data_any().multiple(),
            expected(&path).access_close_write(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf()])
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

        rx.wait_ordered_exact([
            expected(tmpdir.path()).access_open_any().optional(),
            expected(&path).modify_meta_any(),
        ]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf()])
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
            expected(tmpdir.path()).access_open_any().optional(),
            expected(&path).rename_from(),
            expected(&new_path).rename_to(),
            expected([path, new_path]).rename_both(),
        ])
        .ensure_trackers_len(1)
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf()])
        );
    }

    #[test]
    fn rename_self_file() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        std::fs::File::create_new(&path).expect("create");

        watcher.watch_nonrecursively(&path);
        let new_path = tmpdir.path().join("renamed");

        std::fs::rename(&path, &new_path).expect("rename");

        rx.wait_ordered_exact([expected(&path).rename_from()])
            .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()])
        );

        std::fs::rename(&new_path, &path).expect("rename2");

        rx.wait_ordered_exact([expected(&path).rename_to()])
            .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()])
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

        rx.wait_ordered_exact([expected(&path).rename_from()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));

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
            expected(tmpdir.path()).access_open_any().optional(),
            expected(&file).remove_file(),
        ]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf()])
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

        rx.wait_ordered_exact([expected(&file).remove_file()]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()])
        );

        std::fs::write(&file, "").expect("write");

        rx.wait_ordered_exact([expected(&file).create_file()]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()])
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
            expected(&file).modify_meta_any(),
            expected(&file).remove_file(),
        ]);
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

        rx.wait_ordered_exact([
            expected(tmpdir.path()).access_open_any().optional(),
            expected(&overwriting_file).create_file(),
            expected(&overwriting_file).access_open_any(),
            expected(&overwriting_file).access_close_write(),
            expected(&overwriting_file).access_open_any(),
            expected(&overwriting_file).modify_data_any().multiple(),
            expected(&overwriting_file).access_close_write().multiple(),
            expected(&overwriting_file).rename_from(),
            expected(&overwritten_file).rename_to(),
            expected([&overwriting_file, &overwritten_file]).rename_both(),
        ])
        .ensure_no_tail()
        .ensure_trackers_len(1);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf()])
        );
    }

    #[test]
    fn create_self_write_overwrite() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();
        let overwritten_file = tmpdir.path().join("overwritten_file");
        let overwriting_file = tmpdir.path().join("overwriting_file");
        std::fs::write(&overwritten_file, "123").expect("write1");

        watcher.watch_nonrecursively(&overwritten_file);

        std::fs::File::create(&overwriting_file).expect("create");
        std::fs::write(&overwriting_file, "321").expect("write2");
        std::fs::rename(&overwriting_file, &overwritten_file).expect("rename");

        rx.wait_ordered_exact([expected(&overwritten_file).rename_to()])
            .ensure_no_tail()
            .ensure_trackers_len(1);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()])
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
        let mut expected_handles = HashSet::from([
            tmpdir.parent_path_buf(),
            tmpdir.to_path_buf(),
            nested_dir.clone(),
        ]);
        if upgrade_from_no_track {
            expected_handles.insert(watched_file.clone());
        }
        assert_eq!(watcher.get_watch_handles(), expected_handles);

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
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf(), nested_dir,])
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
    fn create_self_write_overwrite_no_track() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();
        let overwritten_file = tmpdir.path().join("overwritten_file");
        let overwriting_file = tmpdir.path().join("overwriting_file");
        std::fs::write(&overwritten_file, "123").expect("write1");

        watcher.watch(
            &overwritten_file,
            WatchMode {
                recursive_mode: RecursiveMode::NonRecursive,
                target_mode: TargetMode::NoTrack,
            },
        );

        std::fs::File::create(&overwriting_file).expect("create");
        std::fs::write(&overwriting_file, "321").expect("write2");
        std::fs::rename(&overwriting_file, &overwritten_file).expect("rename");

        rx.wait_ordered_exact([
            expected(&overwritten_file).modify_meta_any(),
            expected(&overwritten_file).remove_file(),
        ])
        .ensure_no_tail()
        .ensure_trackers_len(0);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));
    }

    #[test]
    fn create_dir() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();
        watcher.watch_recursively(&tmpdir);

        let path = tmpdir.path().join("entry");
        std::fs::create_dir(&path).expect("create");

        rx.wait_ordered_exact([
            expected(tmpdir.path()).access_open_any().optional(),
            expected(&path).create_folder(),
        ]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf(), path])
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

        rx.wait_ordered_exact([
            expected(tmpdir.path()).access_open_any().optional(),
            expected(&path).access_open_any().optional(),
            expected(&path).modify_meta_any(),
            expected(&path).modify_meta_any(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf(), path])
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
            expected(tmpdir.path()).access_open_any().optional(),
            expected(&path).access_open_any().optional(),
            expected(&path).rename_from(),
            expected(&new_path).rename_to(),
            expected([&path, &new_path]).rename_both(),
        ])
        .ensure_trackers_len(1);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf(), new_path])
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
            expected(tmpdir.path()).access_open_any().optional(),
            expected(&path).access_open_any().optional(),
            expected(&path).remove_folder(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf()])
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
            expected(&path).access_open_any().optional(),
            expected(&path).remove_folder(),
            expected(&path).access_open_any().optional(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()])
        );

        std::fs::create_dir(&path).expect("create_dir2");

        rx.wait_ordered_exact([
            expected(&path).access_open_any().optional(),
            expected(&path).create_folder(),
            expected(&path).access_open_any().optional(),
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

        rx.wait_ordered_exact([expected(&path).remove_folder()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));

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

        rx.wait_ordered_exact([
            expected(tmpdir.path()).access_open_any().optional(),
            expected(&path).access_open_any().optional(),
            expected(&path).rename_from(),
            expected(&new_path).rename_to(),
            expected([&path, &new_path]).rename_both(),
            expected(&new_path).access_open_any().optional(),
            expected(&new_path).rename_from(),
            expected(&new_path2).rename_to(),
            expected([&new_path, &new_path2]).rename_both(),
        ])
        .ensure_trackers_len(2);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf(), new_path2])
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
            expected(&subdir).access_open_any(),
            expected(&path).rename_from(),
        ])
        .ensure_trackers_len(1)
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

        rx.wait_ordered_exact([
            expected(tmpdir.path()).access_open_any().optional(),
            expected(&file1).create_file(),
            expected(&file1).access_open_any(),
            expected(&file1).modify_data_any().multiple(),
            expected(&file1).access_close_write(),
            expected(&file2).access_open_any(),
            expected(&file2).modify_data_any().multiple(),
            expected(&file2).access_close_write(),
            expected(&file1).access_open_any().optional(),
            expected(&file1).rename_from(),
            expected(&new_path).rename_to(),
            expected([&file1, &new_path]).rename_both(),
            expected(&new_path).access_open_any(),
            expected(&new_path).modify_data_any().multiple(),
            expected(&new_path).access_close_write(),
            expected(&new_path).remove_file(),
        ]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf()])
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

        rx.wait_ordered_exact([
            expected(tmpdir.path()).access_open_any().optional(),
            expected(&path).access_open_any().optional(),
            expected(&path).rename_from(),
            expected(&new_path1).rename_to(),
            expected([&path, &new_path1]).rename_both(),
            expected(&new_path1).access_open_any().optional(),
            expected(&new_path1).rename_from(),
            expected(&new_path2).rename_to(),
            expected([&new_path1, &new_path2]).rename_both(),
        ])
        .ensure_no_tail()
        .ensure_trackers_len(2);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf()])
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

        rx.wait_ordered_exact([
            expected(tmpdir.path()).access_open_any().optional(),
            expected(&path).modify_data_any(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf()])
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

        rx.wait_ordered_exact([
            expected(&path).access_open_any(),
            expected(&path).modify_data_any().multiple(),
            expected(&path).access_close_write(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()])
        );
    }

    #[test]
    fn watch_recursively_then_unwatch_child_stops_events_from_child() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let subdir = tmpdir.path().join("subdir");
        let file = subdir.join("file");
        std::fs::create_dir(&subdir).expect("create");

        watcher.watch_recursively(&tmpdir);

        std::fs::File::create(&file).expect("create");

        rx.wait_ordered_exact([
            expected(tmpdir.path()).access_open_any().optional(),
            expected(&subdir).access_open_any().optional(),
            expected(&file).create_file(),
            expected(&file).access_open_any(),
            expected(&file).access_close_write(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf(), subdir])
        );

        // TODO: https://github.com/rolldown/notify/issues/8
        // watcher.watcher.unwatch(&subdir).expect("unwatch");

        // std::fs::write(&file, b"123").expect("write");

        // std::fs::remove_dir_all(&subdir).expect("remove_dir_all");

        // rx.wait_ordered_exact([
        //     expected(&subdir).access_open_any().optional(),
        //     expected(&subdir).remove_folder(),
        // ])
        // .ensure_no_tail();
    }

    #[test]
    fn write_to_a_hardlink_pointed_to_the_watched_file_triggers_an_event() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let subdir = tmpdir.path().join("subdir");
        let subdir2 = tmpdir.path().join("subdir2");
        let file = subdir.join("file");
        let hardlink = subdir2.join("hardlink");

        std::fs::create_dir(&subdir).expect("create");
        std::fs::create_dir(&subdir2).expect("create2");
        std::fs::write(&file, "").expect("file");
        std::fs::hard_link(&file, &hardlink).expect("hardlink");

        watcher.watch_nonrecursively(&file);

        std::fs::write(&hardlink, "123123").expect("write to the hard link");

        rx.wait_ordered_exact([
            expected(&file).access_open_any(),
            expected(&file).modify_data_any().multiple(),
            expected(&file).access_close_write(),
        ]);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([subdir, file]));
    }

    #[test]
    fn write_to_a_hardlink_pointed_to_the_watched_file_triggers_an_event_even_if_the_parent_is_watched()
     {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let subdir1 = tmpdir.path().join("subdir1");
        let subdir2 = subdir1.join("subdir2");
        let file = subdir2.join("file");
        let hardlink = tmpdir.path().join("hardlink");

        std::fs::create_dir_all(&subdir2).expect("create");
        std::fs::write(&file, "").expect("file");
        std::fs::hard_link(&file, &hardlink).expect("hardlink");

        watcher.watch_nonrecursively(&subdir2);
        watcher.watch_nonrecursively(&file);

        std::fs::write(&hardlink, "123123").expect("write to the hard link");

        rx.wait_ordered_exact([
            expected(&subdir2).access_open_any().optional(),
            expected(&file).access_open_any(),
            expected(&file).modify_data_any().multiple(),
            expected(&file).access_close_write(),
        ]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([subdir1, subdir2, file])
        );
    }

    #[test]
    fn write_to_a_hardlink_pointed_to_the_file_in_the_watched_dir_doesnt_trigger_an_event() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let subdir = tmpdir.path().join("subdir");
        let subdir2 = tmpdir.path().join("subdir2");
        let file = subdir.join("file");
        let hardlink = subdir2.join("hardlink");

        std::fs::create_dir(&subdir).expect("create");
        std::fs::create_dir(&subdir2).expect("create");
        std::fs::write(&file, "").expect("file");
        std::fs::hard_link(&file, &hardlink).expect("hardlink");

        watcher.watch_nonrecursively(&subdir);

        std::fs::write(&hardlink, "123123").expect("write to the hard link");

        rx.wait_ordered_exact([expected(&subdir).access_open_any().optional()])
            .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf(), subdir])
        );
    }

    #[test]
    #[ignore = "see https://github.com/notify-rs/notify/issues/727"]
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

        rx.ensure_empty_with_wait();

        watcher.watch_recursively(&path);
        std::fs::File::create_new(&file).expect("create");

        rx.wait_ordered([
            expected(&file).create_file(),
            expected(&file).access_open_any(),
            expected(&file).access_close_write(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf(), path, deep])
        );
    }

    fn no_track(recursive_mode: RecursiveMode) -> WatchMode {
        WatchMode {
            recursive_mode,
            target_mode: TargetMode::NoTrack,
        }
    }

    /// `path` and every ancestor of it.
    fn ancestors_of(path: &Path) -> HashSet<PathBuf> {
        path.ancestors().map(Path::to_path_buf).collect()
    }

    /// Checks that the watcher holds no handle.
    fn assert_no_handles(watcher: &TestWatcher<INotifyWatcher>) {
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));
        assert_eq!(watcher.watcher.get_chain_handles(), HashSet::from([]));
    }

    /// The events that came before now, once all of them have come, without a fixed wait: a
    /// dangling symlink named `.sentinel` is created in the first of `dirs`, the spellings of a
    /// watched directory, and each of them reports it created, and nothing else, after the events
    /// that came before. The symlink is removed again.
    fn events_before_sentinel(rx: &Receiver, dirs: &[&Path]) -> Vec<Event> {
        let sentinels: Vec<PathBuf> = dirs.iter().map(|dir| dir.join(".sentinel")).collect();
        symlink("missing", &sentinels[0]).expect("symlink");
        let mut pending: HashSet<&PathBuf> = sentinels.iter().collect();
        let mut before = Vec::new();
        while !pending.is_empty() {
            let event = rx.recv();
            if let [path] = event.paths.as_slice()
                && pending.remove(path)
            {
                assert_eq!(event.kind, EventKind::Create(CreateKind::File), "{event:?}");
            } else {
                before.push(event);
            }
        }
        std::fs::remove_file(&sentinels[0]).expect("remove");
        rx.wait_unordered_exact(sentinels.iter().map(|path| expected(path).remove_file()))
            .ensure_no_tail();
        before
    }

    /// Skips the access events that the walk of a recursive root causes; see
    /// [`events_before_sentinel`].
    fn skip_access_events(rx: &Receiver, dirs: &[&Path]) {
        for event in events_before_sentinel(rx, dirs) {
            assert!(
                matches!(event.kind, EventKind::Access(_)),
                "expected an access event, got {event:?}"
            );
        }
    }

    /// The kinds of the events of a write to an existing file; see [`wait_per_path`].
    const WRITE: [EventKind; 3] = [
        EventKind::Access(AccessKind::Open(AccessMode::Any)),
        EventKind::Modify(ModifyKind::Data(DataChange::Any)),
        EventKind::Access(AccessKind::Close(AccessMode::Write)),
    ];

    /// The kinds of the events of a write that creates a file; see [`wait_per_path`].
    const CREATE_AND_WRITE: [EventKind; 4] = [
        EventKind::Create(CreateKind::File),
        EventKind::Access(AccessKind::Open(AccessMode::Any)),
        EventKind::Modify(ModifyKind::Data(DataChange::Any)),
        EventKind::Access(AccessKind::Close(AccessMode::Write)),
    ];

    const REMOVE: [EventKind; 1] = [EventKind::Remove(RemoveKind::File)];

    /// Waits for the events of each path, in order, and fails on any other event. The events of
    /// different paths may come interleaved, as one inotify event is reported under each spelling
    /// in turn. A kind that comes again in a row counts once, as one write may be reported in
    /// pieces.
    fn wait_per_path(rx: &Receiver, expected: &[(&Path, &[EventKind])]) {
        let expected: HashMap<PathBuf, Vec<EventKind>> = expected
            .iter()
            .map(|(path, kinds)| (path.to_path_buf(), kinds.to_vec()))
            .collect();
        let mut received: HashMap<PathBuf, Vec<EventKind>> = HashMap::new();
        while expected
            .iter()
            .any(|(path, kinds)| received.get(path) != Some(kinds))
        {
            let event = match rx.try_recv() {
                Ok(result) => result.expect("event"),
                Err(e) => panic!("{e:?}: expected {expected:#?}, received {received:#?}"),
            };
            let [path] = event.paths.as_slice() else {
                panic!("expected an event at one path, got {event:?}");
            };
            let kinds = received.entry(path.clone()).or_default();
            if kinds.last() != Some(&event.kind) {
                kinds.push(event.kind);
            }
            assert!(
                expected
                    .get(path)
                    .is_some_and(|expected| expected.starts_with(kinds)),
                "unexpected {event:?}: expected {expected:#?}, received {received:#?}"
            );
        }
        rx.ensure_empty_with_wait();
    }

    /// The kinds of the events at each path, in order; a kind that comes again in a row counts
    /// once, as in [`wait_per_path`].
    fn kinds_per_path(events: Vec<Event>) -> HashMap<PathBuf, Vec<EventKind>> {
        let mut kinds: HashMap<PathBuf, Vec<EventKind>> = HashMap::new();
        for event in events {
            let [path] = event.paths.as_slice() else {
                panic!("expected an event at one path, got {event:?}");
            };
            let kinds = kinds.entry(path.clone()).or_default();
            if kinds.last() != Some(&event.kind) {
                kinds.push(event.kind);
            }
        }
        kinds
    }

    /// Whether the test runs as root, which reads a directory whatever its mode says: a test that
    /// needs a directory it cannot read checks nothing then, and says so.
    fn runs_as_root(dir: &Path) -> bool {
        let as_root = std::fs::metadata(dir).expect("metadata").uid() == 0;
        if as_root {
            eprintln!("skipped: root reads a directory whatever its mode says");
        }
        as_root
    }

    fn assert_io_error_at(result: Result<()>, path: &Path) {
        match result {
            Err(Error {
                kind: ErrorKind::Io(_),
                paths,
            }) => assert_eq!(paths, vec![path.to_path_buf()]),
            other => panic!("expected an io error at {path:?}, got {other:?}"),
        }
    }

    #[test]
    fn unwatch_keeps_the_parent_a_no_track_file_root_is_reported_by() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let lib = tmpdir.path().join("lib");
        let a = lib.join("a.js");
        let b = lib.join("b.js");
        std::fs::create_dir(&lib).expect("create_dir");
        std::fs::write(&a, "1").expect("write");
        std::fs::write(&b, "1").expect("write");

        watcher.watch_nonrecursively(&a);
        watcher.watch(&b, no_track(RecursiveMode::NonRecursive));
        assert_eq!(watcher.get_watch_handles(), HashSet::from([lib.clone()]));

        watcher.watcher.unwatch(&a).expect("unwatch");
        assert_eq!(watcher.get_watch_handles(), HashSet::from([lib]));

        std::fs::write(&b, "2").expect("write");
        rx.wait_ordered_exact([
            expected(&b).access_open_any(),
            expected(&b).modify_data_any().multiple(),
            expected(&b).access_close_write(),
        ])
        .ensure_no_tail();

        watcher.watcher.unwatch(&b).expect("unwatch");
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));
        assert_eq!(watcher.watcher.get_chain_handles(), HashSet::from([]));
    }

    #[test]
    fn unwatch_of_a_root_above_other_roots_keeps_it_as_an_ancestor() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let dir = tmpdir.path().join("dir");
        let sub = dir.join("sub");
        let file = sub.join("f.js");
        let gone = dir.join("gone");
        std::fs::create_dir_all(&sub).expect("create_dir_all");
        std::fs::write(&file, "1").expect("write");

        watcher.watch_nonrecursively(&dir);
        watcher.watch_nonrecursively(&file);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf(), dir.clone(), sub.clone()])
        );

        watcher.watcher.unwatch(&dir).expect("unwatch");
        assert_eq!(watcher.get_watch_handles(), HashSet::from([sub.clone()]));
        let mut chain = ancestors_of(tmpdir.path());
        chain.insert(dir.clone());
        assert_eq!(watcher.watcher.get_chain_handles(), chain);

        std::fs::rename(&sub, &gone).expect("rename");
        rx.wait_ordered_exact([expected(&file).remove_file()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));

        std::fs::create_dir(&sub).expect("create_dir");
        std::fs::write(&file, "2").expect("write");
        rx.wait_ordered([expected(&file).create_file()]);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([sub]));

        std::fs::write(&file, "3").expect("write");
        rx.wait_ordered([expected(&file).modify_data_any()]);

        watcher.watcher.unwatch(&file).expect("unwatch");
        assert_no_handles(&watcher);
    }

    #[test]
    fn unwatch_of_a_recursive_root_keeps_the_roots_below_watched() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let parent = tmpdir.path().join("parent");
        let a = parent.join("a");
        let b = a.join("b.js");
        let other = parent.join("other");
        std::fs::create_dir_all(&a).expect("create_dir_all");
        std::fs::create_dir(&other).expect("create_dir");
        std::fs::write(&b, "1").expect("write");

        watcher.watch_recursively(&parent);
        watcher.watch_nonrecursively(&b);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([
                tmpdir.to_path_buf(),
                parent.clone(),
                a.clone(),
                other.clone()
            ])
        );
        // The walk of the recursive root opened its directories.
        skip_access_events(&rx, &[&parent]);

        watcher.watcher.unwatch(&parent).expect("unwatch");
        assert_eq!(watcher.get_watch_handles(), HashSet::from([a.clone()]));
        let mut chain = ancestors_of(tmpdir.path());
        chain.insert(parent.clone());
        assert_eq!(watcher.watcher.get_chain_handles(), chain);

        std::fs::write(&b, "2").expect("write");
        rx.wait_ordered_exact([
            expected(&b).access_open_any(),
            expected(&b).modify_data_any().multiple(),
            expected(&b).access_close_write(),
        ])
        .ensure_no_tail();

        // Nothing else below the unwatched root is reported any more.
        std::fs::write(other.join("x"), "1").expect("write");
        std::fs::write(parent.join("y"), "1").expect("write");
        rx.ensure_empty_with_wait();

        std::fs::remove_dir_all(&a).expect("remove_dir_all");
        rx.wait_ordered_exact([expected(&b).remove_file()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));

        std::fs::create_dir(&a).expect("create_dir");
        std::fs::write(&b, "3").expect("write");
        rx.wait_ordered([expected(&b).create_file()]);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([a]));

        watcher.watcher.unwatch(&b).expect("unwatch");
        assert_no_handles(&watcher);
    }

    #[test]
    fn no_track_roots_below_a_moved_ancestor_follow_their_entity_or_are_dropped() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let lib = tmpdir.path().join("lib");
        let a = lib.join("a.js");
        let n = lib.join("n.js");
        let ndir = lib.join("ndir");
        let g = ndir.join("g");
        let moved = tmpdir.path().join("moved");
        std::fs::create_dir_all(&ndir).expect("create_dir_all");
        std::fs::write(&a, "1").expect("write");
        std::fs::write(&n, "1").expect("write");
        std::fs::write(&g, "1").expect("write");

        watcher.watch_nonrecursively(&a);
        watcher.watch(&n, no_track(RecursiveMode::NonRecursive));
        watcher.watch(&ndir, no_track(RecursiveMode::Recursive));
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([lib.clone(), ndir.clone()])
        );

        // The tracked root is cut off; the NoTrack directory follows its entity, and the NoTrack
        // file, reported by its parent so far, cannot: it is dropped.
        std::fs::rename(&lib, &moved).expect("rename away");
        rx.wait_unordered_exact([
            expected(&ndir).access_open_any().optional(),
            expected(&a).remove_file(),
            expected(&n).remove_file(),
        ])
        .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([ndir.clone()]));

        std::fs::write(moved.join("ndir").join("g"), "2").expect("write");
        rx.wait_ordered_exact([
            expected(&g).access_open_any(),
            expected(&g).modify_data_any().multiple(),
            expected(&g).access_close_write(),
        ])
        .ensure_no_tail();

        std::fs::rename(&moved, &lib).expect("rename back");
        rx.wait_ordered_exact([expected(&a).create_file()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([lib, ndir]));

        std::fs::write(&n, "2").expect("write");
        rx.ensure_empty_with_wait();

        // Watching the dropped root again arms it anew.
        watcher.watch(&n, no_track(RecursiveMode::NonRecursive));
        std::fs::write(&n, "3").expect("write");
        rx.wait_ordered_exact([
            expected(&n).access_open_any(),
            expected(&n).modify_data_any().multiple(),
            expected(&n).access_close_write(),
        ])
        .ensure_no_tail();
        watcher.watcher.unwatch(&n).expect("unwatch");
    }

    #[test]
    fn track_path_arms_through_a_symlinked_ancestor_that_appears() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let x = tmpdir.path().join("x");
        let y = tmpdir.path().join("y");
        let link = x.join("link");
        let sub = link.join("sub");
        let a = sub.join("a.js");
        std::fs::create_dir(&x).expect("create_dir");
        std::fs::create_dir_all(y.join("sub")).expect("create_dir_all");
        std::fs::write(y.join("sub").join("a.js"), "1").expect("write");

        watcher.watch_nonrecursively(&a);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));
        assert!(watcher.watcher.get_chain_handles().contains(&x));

        symlink(&y, &link).expect("symlink");
        rx.wait_ordered_exact([expected(&a).create_file()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([sub]));

        std::fs::write(&a, "2").expect("write");
        rx.wait_ordered_exact([
            expected(&a).access_open_any(),
            expected(&a).modify_data_any().multiple(),
            expected(&a).access_close_write(),
        ])
        .ensure_no_tail();
    }

    #[test]
    fn track_path_rearms_below_a_symlinked_ancestor_that_is_recreated() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let real = tmpdir.path().join("real");
        let real_file = real.join("f.txt");
        let link = tmpdir.path().join("link");
        let file = link.join("f.txt");
        std::fs::create_dir(&real).expect("create_dir");
        std::fs::write(&real_file, "1").expect("write");
        symlink(&real, &link).expect("symlink");

        watcher.watch_nonrecursively(&file);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([link.clone()]));

        std::fs::remove_file(&link).expect("remove symlink");
        rx.wait_ordered_exact([expected(&file).remove_file()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));

        symlink(&real, &link).expect("symlink");
        rx.wait_ordered_exact([expected(&file).create_file()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([link]));

        std::fs::write(&real_file, "2").expect("write");
        rx.wait_ordered_exact([
            expected(&file).access_open_any(),
            expected(&file).modify_data_any().multiple(),
            expected(&file).access_close_write(),
        ])
        .ensure_no_tail();
    }

    #[test]
    fn track_path_through_another_spelling_of_a_watched_directory() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let real = tmpdir.path().join("real");
        let sub = real.join("sub");
        let link = tmpdir.path().join("link");
        let a = link.join("a.js");
        let b = sub.join("b.js");
        std::fs::create_dir_all(&sub).expect("create_dir_all");
        symlink(&real, &link).expect("symlink");
        std::fs::write(&a, "1").expect("write");
        std::fs::write(&b, "1").expect("write");

        watcher.watch_nonrecursively(&a);
        // `real` is the directory watched as `link`: the chain goes through the same descriptor.
        watcher.watch_nonrecursively(&b);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([link.clone(), sub])
        );

        std::fs::write(&a, "2").expect("write");
        rx.wait_ordered_exact([
            expected(&a).access_open_any(),
            expected(&a).modify_data_any().multiple(),
            expected(&a).access_close_write(),
        ])
        .ensure_no_tail();

        std::fs::write(&b, "2").expect("write");
        rx.wait_ordered_exact([
            expected(&b).access_open_any(),
            expected(&b).modify_data_any().multiple(),
            expected(&b).access_close_write(),
        ])
        .ensure_no_tail();

        watcher.watcher.unwatch(&b).expect("unwatch");
        assert_eq!(watcher.get_watch_handles(), HashSet::from([link]));

        std::fs::remove_file(&a).expect("remove");
        rx.wait_ordered_exact([expected(&a).remove_file()])
            .ensure_no_tail();
        rx.ensure_empty_with_wait();

        watcher.watcher.unwatch(&a).expect("unwatch");
        assert_no_handles(&watcher);
    }

    #[test]
    fn track_path_through_another_spelling_of_a_directory_watched_as_an_ancestor() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let real = tmpdir.path().join("real");
        let sub = real.join("sub");
        let link = tmpdir.path().join("link");
        let a = link.join("a.js");
        let b = sub.join("b.js");
        std::fs::create_dir_all(&sub).expect("create_dir_all");
        symlink(&real, &link).expect("symlink");
        std::fs::write(&a, "1").expect("write");
        std::fs::write(&b, "1").expect("write");

        watcher.watch_nonrecursively(&b);
        // `link` is the directory watched as the ancestor `real`: the root's parent shares its
        // descriptor, which watches it for everything now.
        watcher.watch_nonrecursively(&a);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([link, sub]));
        let mut chain = ancestors_of(tmpdir.path());
        chain.insert(real);
        assert_eq!(watcher.watcher.get_chain_handles(), chain);

        std::fs::write(&a, "2").expect("write");
        rx.wait_ordered_exact([
            expected(&a).access_open_any(),
            expected(&a).modify_data_any().multiple(),
            expected(&a).access_close_write(),
        ])
        .ensure_no_tail();

        std::fs::remove_file(&a).expect("remove");
        rx.wait_ordered_exact([expected(&a).remove_file()])
            .ensure_no_tail();

        std::fs::write(&a, "3").expect("write");
        rx.wait_ordered_exact([
            expected(&a).create_file(),
            expected(&a).access_open_any(),
            expected(&a).modify_data_any().multiple(),
            expected(&a).access_close_write(),
        ])
        .ensure_no_tail();

        std::fs::write(&b, "2").expect("write");
        rx.wait_ordered_exact([
            expected(&b).access_open_any(),
            expected(&b).modify_data_any().multiple(),
            expected(&b).access_close_write(),
        ])
        .ensure_no_tail();
        rx.ensure_empty_with_wait();

        watcher.watcher.unwatch(&a).expect("unwatch");
        watcher.watcher.unwatch(&b).expect("unwatch");
        assert_no_handles(&watcher);
    }

    #[test]
    fn track_path_through_another_spelling_of_a_directory_root() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let real = tmpdir.path().join("real");
        let sub = real.join("sub");
        let link = tmpdir.path().join("link");
        let b = sub.join("b.js");
        let x = link.join("x.js");
        std::fs::create_dir_all(&sub).expect("create_dir_all");
        symlink(&real, &link).expect("symlink");
        std::fs::write(&b, "1").expect("write");

        watcher.watch_nonrecursively(&b);
        watcher.watch_nonrecursively(&link);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf(), link.clone(), sub])
        );

        std::fs::File::create_new(&x).expect("create");
        rx.wait_ordered_exact([
            expected(&x).create_file(),
            expected(&x).access_open_any(),
            expected(&x).access_close_write(),
        ])
        .ensure_no_tail();

        std::fs::remove_file(&x).expect("remove");
        rx.wait_ordered_exact([expected(&x).remove_file()])
            .ensure_no_tail();
        rx.ensure_empty_with_wait();

        watcher.watcher.unwatch(&link).expect("unwatch");
        watcher.watcher.unwatch(&b).expect("unwatch");
        assert_no_handles(&watcher);
    }

    /// A pnpm layout: `node_modules/pkg` links to `packages/pkg`, `node_modules` is watched
    /// recursively, through the link, and a file of the package is watched by its real path; with
    /// `file_first`, the file is watched first. inotify watches the directories of the package
    /// once, and each root gets the events of the file under its own spelling.
    fn assert_every_spelling_is_reported(file_first: bool) {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let packages = tmpdir.path().join("packages");
        let pkg = packages.join("pkg");
        let src = pkg.join("src");
        let file = src.join("index.ts");
        let node_modules = tmpdir.path().join("node_modules");
        let linked_pkg = node_modules.join("pkg");
        let linked_src = linked_pkg.join("src");
        let linked_file = linked_src.join("index.ts");
        std::fs::create_dir_all(&src).expect("create_dir_all");
        std::fs::create_dir(&node_modules).expect("create_dir");
        symlink("../packages/pkg", &linked_pkg).expect("symlink");
        std::fs::write(&file, "1").expect("write");

        if file_first {
            watcher.watch_nonrecursively(&file);
            watcher.watch_recursively(&node_modules);
        } else {
            watcher.watch_recursively(&node_modules);
            watcher.watch_nonrecursively(&file);
        }
        skip_access_events(&rx, &[&node_modules]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([
                tmpdir.to_path_buf(),
                node_modules.clone(),
                linked_pkg.clone(),
                linked_src.clone(),
                src.clone(),
            ])
        );
        let mut chain = ancestors_of(&tmpdir.parent_path_buf());
        chain.extend([packages.clone(), pkg.clone()]);
        assert_eq!(watcher.watcher.get_chain_handles(), chain);

        std::fs::write(&file, "2").expect("write");
        wait_per_path(&rx, &[(&file, &WRITE), (&linked_file, &WRITE)]);

        std::fs::remove_file(&file).expect("remove");
        wait_per_path(&rx, &[(&file, &REMOVE), (&linked_file, &REMOVE)]);
        std::fs::write(&file, "3").expect("write");
        wait_per_path(
            &rx,
            &[
                (&file, &CREATE_AND_WRITE),
                (&linked_file, &CREATE_AND_WRITE),
            ],
        );

        // The recursive root keeps its events once the file is unwatched.
        watcher.watcher.unwatch(&file).expect("unwatch");
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([
                tmpdir.to_path_buf(),
                node_modules.clone(),
                linked_pkg,
                linked_src,
            ])
        );
        assert_eq!(
            watcher.watcher.get_chain_handles(),
            ancestors_of(&tmpdir.parent_path_buf())
        );
        std::fs::write(&file, "4").expect("write");
        wait_per_path(&rx, &[(&linked_file, &WRITE)]);

        // The file keeps its events once the recursive root is unwatched.
        watcher.watch_nonrecursively(&file);
        watcher.watcher.unwatch(&node_modules).expect("unwatch");
        assert_eq!(watcher.get_watch_handles(), HashSet::from([src]));
        let mut chain = ancestors_of(tmpdir.path());
        chain.extend([packages, pkg]);
        assert_eq!(watcher.watcher.get_chain_handles(), chain);
        std::fs::write(&file, "5").expect("write");
        wait_per_path(&rx, &[(&file, &WRITE)]);
        std::fs::remove_file(&file).expect("remove");
        wait_per_path(&rx, &[(&file, &REMOVE)]);
        std::fs::write(&file, "6").expect("write");
        wait_per_path(&rx, &[(&file, &CREATE_AND_WRITE)]);

        watcher.watcher.unwatch(&file).expect("unwatch");
        assert_no_handles(&watcher);
    }

    #[test]
    fn a_file_watched_by_its_real_path_below_a_recursive_root_through_a_link() {
        assert_every_spelling_is_reported(false);
    }

    #[test]
    fn a_recursive_root_through_a_link_to_the_directory_of_a_watched_file() {
        assert_every_spelling_is_reported(true);
    }

    /// `real` and `link` are two spellings of one directory: the parent of `a` and of `c`, and an
    /// ancestor of `b`. With `parent_first`, `a` is watched before `b`. Each spelling is armed for
    /// the roots below it, whichever order they are watched in.
    fn assert_both_spellings_are_armed(parent_first: bool) {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let real = tmpdir.path().join("real");
        let sub = real.join("sub");
        let link = tmpdir.path().join("link");
        let a = link.join("a.js");
        let b = sub.join("b.js");
        let c = real.join("c.js");
        std::fs::create_dir_all(&sub).expect("create_dir_all");
        symlink(&real, &link).expect("symlink");
        std::fs::write(&a, "1").expect("write");
        std::fs::write(&b, "1").expect("write");
        std::fs::write(&c, "1").expect("write");

        if parent_first {
            watcher.watch_nonrecursively(&a);
            watcher.watch_nonrecursively(&b);
        } else {
            watcher.watch_nonrecursively(&b);
            watcher.watch_nonrecursively(&a);
        }
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([link.clone(), sub.clone()])
        );
        let mut chain = ancestors_of(tmpdir.path());
        chain.insert(real.clone());
        assert_eq!(watcher.watcher.get_chain_handles(), chain);

        // A root right below `real` is reported under its own path.
        watcher.watch_nonrecursively(&c);
        let handles = HashSet::from([link.clone(), real.clone(), sub.clone()]);
        assert_eq!(watcher.get_watch_handles(), handles);
        assert_eq!(
            watcher.watcher.get_chain_handles(),
            ancestors_of(tmpdir.path())
        );
        std::fs::write(&c, "2").expect("write");
        wait_per_path(&rx, &[(&c, &WRITE)]);
        std::fs::write(&a, "2").expect("write");
        wait_per_path(&rx, &[(&a, &WRITE)]);

        // `sub` comes back, and `b` is armed again through `real`.
        std::fs::remove_dir_all(&sub).expect("remove_dir_all");
        rx.wait_ordered_exact([expected(&b).remove_file()])
            .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([link, real.clone()])
        );
        std::fs::create_dir(&sub).expect("create_dir");
        assert!(
            rx.sleep_until(|| watcher.get_watch_handles().contains(&sub)),
            "the parent of the root is not watched again"
        );
        std::fs::write(&b, "2").expect("write");
        wait_per_path(&rx, &[(&b, &CREATE_AND_WRITE)]);
        assert_eq!(watcher.get_watch_handles(), handles);

        // `real` keeps its watch once `link` is not needed any more.
        watcher.watcher.unwatch(&a).expect("unwatch");
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([real, sub.clone()])
        );
        assert_eq!(
            watcher.watcher.get_chain_handles(),
            ancestors_of(tmpdir.path())
        );
        std::fs::write(&c, "3").expect("write");
        wait_per_path(&rx, &[(&c, &WRITE)]);
        std::fs::remove_dir_all(&sub).expect("remove_dir_all");
        rx.wait_ordered_exact([expected(&b).remove_file()])
            .ensure_no_tail();

        watcher.watcher.unwatch(&b).expect("unwatch");
        watcher.watcher.unwatch(&c).expect("unwatch");
        assert_no_handles(&watcher);
    }

    #[test]
    fn both_spellings_are_armed_with_the_parent_watched_first() {
        assert_both_spellings_are_armed(true);
    }

    #[test]
    fn both_spellings_are_armed_with_the_ancestor_watched_first() {
        assert_both_spellings_are_armed(false);
    }

    #[test]
    fn roots_below_two_spellings_of_an_ancestor_are_reported_when_it_moves() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let real = tmpdir.path().join("real");
        let link = tmpdir.path().join("link");
        let moved = tmpdir.path().join("moved");
        let shared = real.join("shared");
        let linked_shared = link.join("shared");
        let b = shared.join("sub").join("b.js");
        let a = linked_shared.join("sub2").join("a.js");
        std::fs::create_dir_all(shared.join("sub")).expect("create_dir_all");
        std::fs::create_dir(shared.join("sub2")).expect("create_dir");
        symlink(&real, &link).expect("symlink");
        std::fs::write(&b, "1").expect("write");
        std::fs::write(&a, "1").expect("write");

        watcher.watch_nonrecursively(&b);
        // The grandparent of `a` is `shared`, an ancestor of `b`, through the link.
        watcher.watch_nonrecursively(&a);
        let handles = HashSet::from([shared.join("sub"), linked_shared.join("sub2")]);
        let mut chain = ancestors_of(tmpdir.path());
        chain.extend([real, link]);
        assert_eq!(watcher.get_watch_handles(), handles);
        let mut full_chain = chain.clone();
        full_chain.extend([shared.clone(), linked_shared]);
        assert_eq!(watcher.watcher.get_chain_handles(), full_chain);

        std::fs::rename(&shared, &moved).expect("rename away");
        rx.wait_unordered_exact([expected(&b).remove_file(), expected(&a).remove_file()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));
        assert_eq!(watcher.watcher.get_chain_handles(), chain);

        std::fs::rename(&moved, &shared).expect("rename back");
        rx.wait_unordered_exact([expected(&b).create_file(), expected(&a).create_file()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), handles);
        assert_eq!(watcher.watcher.get_chain_handles(), full_chain);

        std::fs::write(&a, "2").expect("write");
        wait_per_path(&rx, &[(&a, &WRITE)]);
        std::fs::write(&b, "2").expect("write");
        wait_per_path(&rx, &[(&b, &WRITE)]);

        watcher.watcher.unwatch(&a).expect("unwatch");
        watcher.watcher.unwatch(&b).expect("unwatch");
        assert_no_handles(&watcher);
    }

    #[test]
    fn a_root_below_another_spelling_of_a_watched_parent_is_armed() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let real = tmpdir.path().join("real");
        let link = tmpdir.path().join("link");
        let x = real.join("x.js");
        let b = link.join("b.js");
        std::fs::create_dir(&real).expect("create_dir");
        std::fs::write(&x, "1").expect("write");
        std::fs::write(real.join("b.js"), "1").expect("write");

        watcher.watch_nonrecursively(&x);
        watcher.watch_nonrecursively(&b);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([real.clone()]));

        // `link` appears as the directory watched as `real`.
        symlink(&real, &link).expect("symlink");
        rx.wait_ordered_exact([expected(&b).create_file()])
            .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([real.clone(), link.clone()])
        );

        std::fs::write(real.join("b.js"), "2").expect("write");
        wait_per_path(&rx, &[(&b, &WRITE)]);
        std::fs::write(&x, "2").expect("write");
        wait_per_path(&rx, &[(&x, &WRITE)]);

        // `real` keeps its handle when `link` goes.
        std::fs::remove_file(&link).expect("remove symlink");
        rx.wait_ordered_exact([expected(&b).remove_file()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([real]));
        std::fs::write(&x, "3").expect("write");
        wait_per_path(&rx, &[(&x, &WRITE)]);

        watcher.watcher.unwatch(&x).expect("unwatch");
        watcher.watcher.unwatch(&b).expect("unwatch");
        assert_no_handles(&watcher);
    }

    #[test]
    fn each_spelling_gets_the_events_it_asked_for() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let real = tmpdir.path().join("real");
        let link = tmpdir.path().join("link");
        std::fs::create_dir(&real).expect("create_dir");
        symlink(&real, &link).expect("symlink");

        watcher.watch_nonrecursively(&real);
        // A NoTrack root follows its directory, which reports its own removal.
        watcher.watch(&link, no_track(RecursiveMode::NonRecursive));
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf(), real.clone(), link.clone()])
        );

        // `real` is reported removed by its parent only.
        std::fs::remove_dir(&real).expect("remove_dir");
        rx.wait_unordered_exact([
            expected(&real).remove_folder(),
            expected(&link).remove_folder(),
        ])
        .ensure_no_tail();
        rx.ensure_empty_with_wait();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()])
        );

        watcher.watcher.unwatch(&real).expect("unwatch");
        assert_no_handles(&watcher);
    }

    #[test]
    fn a_rename_is_reported_under_each_spelling_with_its_own_pair() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let real = tmpdir.path().join("real");
        let link = tmpdir.path().join("link");
        for dir in ["a", "b", "sub"] {
            std::fs::create_dir_all(real.join(dir)).expect("create_dir_all");
        }
        symlink(&real, &link).expect("symlink");
        for file in ["a.js", "a/g", "x.js"] {
            std::fs::write(real.join(file), "1").expect("write");
        }

        watcher.watch_recursively(&real);
        watcher.watch_recursively(&link);
        skip_access_events(&rx, &[&real, &link]);

        // In one directory, between two directories, and into a directory below: each `To` is
        // paired with the `From` of its own spelling.
        for (from, to) in [("a.js", "b.js"), ("a/g", "b/g"), ("x.js", "sub/y.js")] {
            std::fs::rename(real.join(from), real.join(to)).expect("rename");
            let [from, to, linked_from, linked_to] = [
                real.join(from),
                real.join(to),
                link.join(from),
                link.join(to),
            ];
            rx.wait_unordered_exact([
                expected(&from).rename_from(),
                expected(&to).rename_to(),
                expected([&from, &to]).rename_both(),
                expected(&linked_from).rename_from(),
                expected(&linked_to).rename_to(),
                expected([&linked_from, &linked_to]).rename_both(),
            ])
            .ensure_no_tail();
        }

        watcher.watcher.unwatch(&real).expect("unwatch");
        watcher.watcher.unwatch(&link).expect("unwatch");
        assert_no_handles(&watcher);
    }

    /// A tree moved in below `name` in the first of `spellings`, the spellings of a recursive root,
    /// and the events each spelling reports for it until it is watched under all of them: the
    /// kinds at each path of the tree, relative to the tree, counted.
    fn events_of_a_tree_moved_in(
        rx: &Receiver,
        watcher: &TestWatcher<INotifyWatcher>,
        outside: &Path,
        name: &str,
        spellings: &[&Path],
    ) -> Vec<HashMap<(PathBuf, EventKind), usize>> {
        let tree = outside.join(name);
        std::fs::create_dir_all(tree.join("sub")).expect("create_dir_all");
        std::fs::rename(&tree, spellings[0].join(name)).expect("rename");
        assert!(
            rx.sleep_until(|| {
                let handles = watcher.get_watch_handles();
                spellings
                    .iter()
                    .all(|spelling| handles.contains(&spelling.join(name).join("sub")))
            }),
            "the tree is not watched under each spelling"
        );
        let events = events_before_sentinel(rx, spellings);
        let roots: Vec<PathBuf> = spellings
            .iter()
            .map(|spelling| spelling.join(name))
            .collect();
        let mut counts = vec![HashMap::new(); roots.len()];
        for event in events {
            let [path] = event.paths.as_slice() else {
                panic!("expected an event at one path, got {event:?}");
            };
            let Some((i, relative)) = roots
                .iter()
                .enumerate()
                .find_map(|(i, root)| Some((i, path.strip_prefix(root).ok()?)))
            else {
                panic!("unexpected {event:?}");
            };
            *counts[i]
                .entry((relative.to_path_buf(), event.kind))
                .or_default() += 1;
        }
        counts
    }

    #[test]
    fn a_new_directory_below_several_spellings_is_walked_once() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let real = tmpdir.path().join("real");
        let link1 = tmpdir.path().join("link1");
        let link2 = tmpdir.path().join("link2");
        let outside = tmpdir.path().join("outside");
        std::fs::create_dir(&real).expect("create_dir");
        std::fs::create_dir(&outside).expect("create_dir");
        symlink(&real, &link1).expect("symlink");
        symlink(&real, &link2).expect("symlink");

        // What one recursive root reports for a new tree, its walk included.
        watcher.watch_recursively(&real);
        skip_access_events(&rx, &[&real]);
        let [single] = events_of_a_tree_moved_in(&rx, &watcher, &outside, "a", &[&real])
            .try_into()
            .expect("one spelling");
        assert!(
            single.contains_key(&(
                PathBuf::from("sub"),
                EventKind::Access(AccessKind::Open(AccessMode::Any))
            )),
            "the walk of the new tree is reported: {single:#?}"
        );

        // Each spelling reports the same, as the tree is walked once, not once per spelling.
        watcher.watch_recursively(&link1);
        watcher.watch_recursively(&link2);
        let spellings = [real.as_path(), &link1, &link2];
        skip_access_events(&rx, &spellings);
        for counts in events_of_a_tree_moved_in(&rx, &watcher, &outside, "b", &spellings) {
            assert_eq!(counts, single);
        }

        for root in spellings {
            watcher.watcher.unwatch(root).expect("unwatch");
        }
        assert_no_handles(&watcher);
    }

    /// `l1` and `l2` link to `real`: `l1` is watched as the parent of `l1/x.js`, for everything,
    /// and `l2` as an ancestor of `l2/sub/y.js`, for its entries only, so they share the descriptor
    /// of `real` with different masks. `real` is then moved away and made anew: both handles are
    /// stale, as their paths lead to the new directory. With `watch_real`, `real` is a root too,
    /// which is watched on the new directory once it is reported created, before this returns.
    fn watch_two_spellings_that_go_stale(
        tmpdir: &Path,
        watcher: &mut TestWatcher<INotifyWatcher>,
        rx: &Receiver,
        watch_real: bool,
    ) -> [PathBuf; 3] {
        let real = tmpdir.join("real");
        let l1 = tmpdir.join("l1");
        let l2 = tmpdir.join("l2");
        std::fs::create_dir_all(real.join("sub")).expect("create_dir_all");
        std::fs::write(real.join("x.js"), "1").expect("write");
        std::fs::write(real.join("sub").join("y.js"), "1").expect("write");
        symlink(&real, &l1).expect("symlink");
        symlink(&real, &l2).expect("symlink");

        watcher.watch_nonrecursively(l1.join("x.js"));
        watcher.watch_nonrecursively(l2.join("sub").join("y.js"));
        if watch_real {
            watcher.watch_nonrecursively(&real);
        }
        std::fs::rename(&real, tmpdir.join("real.old")).expect("rename");
        std::fs::create_dir(&real).expect("create_dir");
        if watch_real {
            rx.wait_ordered_exact([
                expected(&real).rename_from(),
                expected(&real).create_folder(),
            ])
            .ensure_no_tail();
        }
        [real, l1, l2]
    }

    #[test]
    fn a_no_track_root_through_a_stale_spelling_watches_the_new_directory() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();
        let [real, l1, l2] =
            watch_two_spellings_that_go_stale(tmpdir.path(), &mut watcher, &rx, false);

        // `l1` moves to the new directory, and narrowing the old one through the stale `l2` leaves
        // the new watch alone.
        watcher.watch(&l1, no_track(RecursiveMode::NonRecursive));
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([l1.clone(), l2.join("sub")])
        );
        let mut chain = ancestors_of(tmpdir.path());
        chain.insert(l2.clone());
        assert_eq!(watcher.watcher.get_chain_handles(), chain);

        let new = l1.join("new.txt");
        std::fs::File::create_new(real.join("new.txt")).expect("create");
        rx.wait_ordered_exact([
            expected(&new).create_file(),
            expected(&new).access_open_any(),
            expected(&new).access_close_write(),
        ])
        .ensure_no_tail();
        rx.ensure_empty_with_wait();

        watcher.watcher.unwatch(&l1).expect("unwatch");
        watcher.watcher.unwatch(&l1.join("x.js")).expect("unwatch");
        watcher
            .watcher
            .unwatch(&l2.join("sub").join("y.js"))
            .expect("unwatch");
        assert_no_handles(&watcher);
    }

    #[test]
    fn a_chain_handle_narrowed_through_a_stale_spelling_stays_live() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();
        let [real, l1, l2] =
            watch_two_spellings_that_go_stale(tmpdir.path(), &mut watcher, &rx, false);
        let z = l1.join("deep").join("z.js");
        watcher.watch_nonrecursively(&z);

        // `l1` is only an ancestor of `z.js` from now on: its mask is narrowed on the new
        // directory, and narrowing the old one through the stale `l2` leaves the new watch alone.
        watcher.watcher.unwatch(&l1.join("x.js")).expect("unwatch");
        assert_eq!(watcher.get_watch_handles(), HashSet::from([l2.join("sub")]));
        let mut chain = ancestors_of(tmpdir.path());
        chain.extend([l1.clone(), l2.clone()]);
        assert_eq!(watcher.watcher.get_chain_handles(), chain);

        std::fs::create_dir(real.join("deep")).expect("create_dir");
        assert!(
            rx.sleep_until(|| watcher.get_watch_handles().contains(&l1.join("deep"))),
            "the parent of the root is not watched"
        );
        std::fs::write(real.join("deep").join("z.js"), "1").expect("write");
        wait_per_path(&rx, &[(&z, &CREATE_AND_WRITE)]);

        watcher.watcher.unwatch(&z).expect("unwatch");
        watcher
            .watcher
            .unwatch(&l2.join("sub").join("y.js"))
            .expect("unwatch");
        assert_no_handles(&watcher);
    }

    #[test]
    fn a_stale_spelling_does_not_narrow_the_watch_of_the_new_directory() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();
        let [real, l1, l2] =
            watch_two_spellings_that_go_stale(tmpdir.path(), &mut watcher, &rx, true);

        // `l1` moves to the descriptor of the new `real`, and asks for its removal as well.
        watcher.watch(&l1, no_track(RecursiveMode::NonRecursive));
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([
                tmpdir.to_path_buf(),
                real.clone(),
                l1.clone(),
                l2.join("sub")
            ])
        );

        std::fs::remove_dir(&real).expect("remove_dir");
        rx.wait_unordered_exact([
            expected(&real).remove_folder(),
            expected(&l1).remove_folder(),
        ])
        .ensure_no_tail();
        rx.ensure_empty_with_wait();

        watcher.watcher.unwatch(&real).expect("unwatch");
        watcher.watcher.unwatch(&l1.join("x.js")).expect("unwatch");
        watcher
            .watcher
            .unwatch(&l2.join("sub").join("y.js"))
            .expect("unwatch");
        assert_no_handles(&watcher);
    }

    #[test]
    fn two_hard_links_each_report_a_write_through_either() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let d1 = tmpdir.path().join("d1");
        let d2 = tmpdir.path().join("d2");
        let f = d1.join("f");
        let g = d2.join("g");
        std::fs::create_dir(&d1).expect("create_dir");
        std::fs::create_dir(&d2).expect("create_dir");
        std::fs::write(&f, "1").expect("write");
        std::fs::hard_link(&f, &g).expect("hard_link");

        watcher.watch_nonrecursively(&f);
        watcher.watch_nonrecursively(&g);
        // The sentinel of `events_before_sentinel`, which `d2` reports.
        let sentinel = d2.join(".sentinel");
        watcher.watch_nonrecursively(&sentinel);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([d1, d2.clone(), f.clone(), g.clone()])
        );

        // The link written through is reported by its parent too, which comes first, and the
        // file once more: all the events of the write are there once the sentinel is.
        let both = HashMap::from([(f.clone(), WRITE.to_vec()), (g.clone(), WRITE.to_vec())]);
        std::fs::write(&f, "2").expect("write");
        assert_eq!(kinds_per_path(events_before_sentinel(&rx, &[&d2])), both);
        std::fs::write(&g, "3").expect("write");
        assert_eq!(kinds_per_path(events_before_sentinel(&rx, &[&d2])), both);

        // `g` keeps the watch of the inode once `f` is unwatched.
        watcher.watcher.unwatch(&f).expect("unwatch");
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([d2.clone(), g.clone()])
        );
        let only_g = HashMap::from([(g.clone(), WRITE.to_vec())]);
        std::fs::write(&f, "4").expect("write");
        assert_eq!(kinds_per_path(events_before_sentinel(&rx, &[&d2])), only_g);
        std::fs::write(&g, "5").expect("write");
        assert_eq!(kinds_per_path(events_before_sentinel(&rx, &[&d2])), only_g);

        watcher.watcher.unwatch(&g).expect("unwatch");
        watcher.watcher.unwatch(&sentinel).expect("unwatch");
        assert_no_handles(&watcher);
    }

    #[test]
    fn a_path_through_dot_dot_is_tracked_next_to_the_plain_one() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let a = tmpdir.path().join("a");
        let b = a.join("b");
        let c = b.join("c");
        let file = c.join("file");
        let dotted_c = b.join("..").join("b").join("c");
        let dotted = dotted_c.join("file");
        let moved = a.join("moved");
        std::fs::create_dir_all(&c).expect("create_dir_all");
        std::fs::write(&file, "1").expect("write");

        watcher.watch_nonrecursively(&dotted);
        watcher.watch_nonrecursively(&file);
        let handles = HashSet::from([c.clone(), dotted_c]);
        assert_eq!(watcher.get_watch_handles(), handles);
        std::fs::write(&file, "2").expect("write");
        wait_per_path(&rx, &[(&file, &WRITE), (&dotted, &WRITE)]);

        std::fs::remove_dir_all(&c).expect("remove_dir_all");
        rx.wait_unordered_exact([
            expected(&file).remove_file(),
            expected(&dotted).remove_file(),
        ])
        .ensure_no_tail();
        std::fs::create_dir(&c).expect("create_dir");
        assert!(
            rx.sleep_until(|| watcher.get_watch_handles() == handles),
            "the parents of the roots are not watched again"
        );
        std::fs::write(&file, "3").expect("write");
        wait_per_path(
            &rx,
            &[(&file, &CREATE_AND_WRITE), (&dotted, &CREATE_AND_WRITE)],
        );

        std::fs::rename(&b, &moved).expect("rename away");
        rx.wait_unordered_exact([
            expected(&file).remove_file(),
            expected(&dotted).remove_file(),
        ])
        .ensure_no_tail();
        std::fs::rename(&moved, &b).expect("rename back");
        rx.wait_unordered_exact([
            expected(&file).create_file(),
            expected(&dotted).create_file(),
        ])
        .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), handles);
        std::fs::write(&file, "4").expect("write");
        wait_per_path(&rx, &[(&file, &WRITE), (&dotted, &WRITE)]);

        watcher.watcher.unwatch(&dotted).expect("unwatch");
        watcher.watcher.unwatch(&file).expect("unwatch");
        assert_no_handles(&watcher);
    }

    #[test]
    fn unwatch_of_a_directory_root_below_another_drops_its_handle() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let dir = tmpdir.path().join("dir");
        let sub = dir.join("sub");
        let file = sub.join("x.js");
        std::fs::create_dir_all(&sub).expect("create_dir_all");

        watcher.watch_nonrecursively(&dir);
        watcher.watch_nonrecursively(&sub);
        watcher.watcher.unwatch(&sub).expect("unwatch");
        // The outer root reports its entries through its own handle.
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf(), dir.clone()])
        );
        watcher.watcher.unwatch(&dir).expect("unwatch");
        assert_no_handles(&watcher);

        // A directory at the same path later is watched anew.
        std::fs::remove_dir(&sub).expect("remove_dir");
        std::fs::create_dir(&sub).expect("create_dir");
        std::fs::write(&file, "1").expect("write");
        watcher.watch_nonrecursively(&file);
        std::fs::write(&file, "2").expect("write");
        rx.wait_ordered_exact([
            expected(&file).access_open_any(),
            expected(&file).modify_data_any().multiple(),
            expected(&file).access_close_write(),
        ])
        .ensure_no_tail();
        std::fs::remove_file(&file).expect("remove");
        rx.wait_ordered_exact([expected(&file).remove_file()])
            .ensure_no_tail();
    }

    #[test]
    fn unwatch_of_a_directory_root_and_a_tracked_path_below_it_drops_the_chain() {
        let tmpdir = testdir();
        let (mut watcher, _rx) = watcher();

        let dir = tmpdir.path().join("dir");
        let file = dir.join("a").join("b").join("c.js");
        std::fs::create_dir_all(file.parent().unwrap()).expect("create_dir_all");
        std::fs::write(&file, "1").expect("write");

        watcher.watch_nonrecursively(&dir);
        watcher.watch_nonrecursively(&file);
        watcher.watcher.unwatch(&file).expect("unwatch");
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf(), dir.clone()])
        );
        assert_eq!(
            watcher.watcher.get_chain_handles(),
            ancestors_of(&tmpdir.parent_path_buf())
        );
        watcher.watcher.unwatch(&dir).expect("unwatch");
        assert_no_handles(&watcher);
    }

    /// Checks nothing as root; as another user in docker:
    /// `docker run --rm --user 65534:65534 -e HOME=/tmp -e CARGO_HOME=/tmp/cargo
    /// -e RUSTUP_HOME=/tmp/rustup -e CARGO_TARGET_DIR=/tmp/target -v "$PWD":/w -w /w rust:1.97
    /// cargo test -p rolldown-notify --lib inotify::`
    #[test]
    fn unwatch_succeeds_when_the_mask_of_a_handle_cannot_be_changed() {
        let tmpdir = testdir();
        if runs_as_root(tmpdir.path()) {
            return;
        }
        let (mut watcher, _rx) = watcher();

        let dir = tmpdir.path().join("dir");
        let file = dir.join("sub").join("f.js");
        std::fs::create_dir_all(file.parent().unwrap()).expect("create_dir_all");
        std::fs::write(&file, "1").expect("write");

        watcher.watch_nonrecursively(&dir);
        watcher.watch_nonrecursively(&file);

        // The handle of `dir` stays for the tracked file below, but cannot be narrowed down.
        std::fs::set_permissions(&dir, Permissions::from_mode(0o000)).expect("set_permissions");
        let result = watcher.watcher.unwatch(&dir);
        std::fs::set_permissions(&dir, Permissions::from_mode(0o755)).expect("set_permissions");
        result.expect("unwatch");

        watcher.watcher.unwatch(&file).expect("unwatch");
        assert_no_handles(&watcher);
    }

    #[test]
    fn a_full_watch_table_does_not_stop_the_other_roots_from_being_armed() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let lib = tmpdir.path().join("lib");
        let moved = tmpdir.path().join("moved");
        let a = lib.join("a.js");
        let sub1 = lib.join("sub1");
        let sub2 = lib.join("sub2");
        std::fs::create_dir_all(&sub1).expect("create_dir_all");
        std::fs::create_dir(&sub2).expect("create_dir");
        std::fs::write(&a, "1").expect("write");

        watcher.watch_nonrecursively(&a);
        watcher.watch_nonrecursively(&sub1);
        watcher.watch_nonrecursively(&sub2);

        std::fs::rename(&lib, &moved).expect("rename away");
        rx.wait_unordered_exact([
            expected(&a).remove_file(),
            expected(&sub1).remove_folder(),
            expected(&sub2).remove_folder(),
        ])
        .ensure_no_tail();

        // Room for one more handle: the parent of the roots, which reports the file root.
        let held = watcher.get_watch_handles().len() + watcher.watcher.get_chain_handles().len();
        watcher.watcher.set_watch_limit(Some(held + 1));
        std::fs::rename(&moved, &lib).expect("rename back");
        let mut created = Vec::new();
        let mut failed = Vec::new();
        while created.len() + failed.len() < 3 {
            match rx.recv_result() {
                Ok(event) => {
                    assert_eq!(event.kind, EventKind::Create(CreateKind::File), "{event:?}");
                    created.extend(event.paths);
                }
                Err(error) => {
                    assert!(
                        matches!(error.kind, ErrorKind::MaxFilesWatch),
                        "expected a full watch table, got {error:?}"
                    );
                    failed.extend(error.paths);
                }
            }
        }
        rx.ensure_empty_with_wait();
        assert_eq!(created, vec![a.clone()]);
        assert_eq!(
            failed.into_iter().collect::<HashSet<_>>(),
            HashSet::from([sub1.clone(), sub2.clone()])
        );
        assert_eq!(watcher.get_watch_handles(), HashSet::from([lib.clone()]));

        // Watching a root again arms it once there is room.
        watcher.watcher.set_watch_limit(None);
        watcher.watch_nonrecursively(&sub1);
        watcher.watch_nonrecursively(&sub2);
        rx.wait_ordered_exact([
            expected(&sub1).create_folder(),
            expected(&sub2).create_folder(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([lib, sub1.clone(), sub2])
        );

        let file = sub1.join("f.js");
        std::fs::File::create_new(&file).expect("create");
        rx.wait_ordered([expected(&file).create_file()]);
    }

    #[test]
    fn a_no_track_file_watched_before_a_tracked_sibling_follows_the_file() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let lib = tmpdir.path().join("lib");
        let moved = tmpdir.path().join("moved");
        let n = lib.join("n.js");
        let t = lib.join("t.js");
        std::fs::create_dir(&lib).expect("create_dir");
        std::fs::write(&n, "1").expect("write");
        std::fs::write(&t, "1").expect("write");

        // The NoTrack file gets a handle of its own, as its parent is not watched yet.
        watcher.watch(&n, no_track(RecursiveMode::NonRecursive));
        watcher.watch_nonrecursively(&t);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([lib.clone(), n.clone()])
        );

        std::fs::rename(&lib, &moved).expect("rename away");
        rx.wait_ordered_exact([expected(&t).remove_file()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([n.clone()]));

        // It follows the file, and reports it under its old path.
        std::fs::write(moved.join("n.js"), "2").expect("write");
        rx.wait_ordered_exact([
            expected(&n).access_open_any(),
            expected(&n).modify_data_any().multiple(),
            expected(&n).access_close_write(),
        ])
        .ensure_no_tail();
        rx.ensure_empty_with_wait();

        watcher.watcher.unwatch(&n).expect("unwatch");
        watcher.watcher.unwatch(&t).expect("unwatch");
        assert_no_handles(&watcher);
    }

    #[test]
    fn watch_fails_when_an_ancestor_cannot_be_resolved() {
        let tmpdir = testdir();
        let (mut watcher, _rx) = watcher();

        let loop_link = tmpdir.path().join("loop");
        symlink("loop", &loop_link).expect("symlink");

        let result = watcher
            .watcher
            .watch(&loop_link.join("f.js"), WatchMode::non_recursive());
        assert_io_error_at(result, &loop_link);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));
        assert_eq!(watcher.watcher.get_chain_handles(), HashSet::from([]));
    }

    #[test]
    fn watch_fails_and_leaks_nothing_when_the_root_cannot_be_resolved() {
        let tmpdir = testdir();
        let (mut watcher, _rx) = watcher();

        let deep = tmpdir.path().join("x").join("y");
        let loop_link = deep.join("loop");
        std::fs::create_dir_all(&deep).expect("create_dir_all");
        symlink("loop", &loop_link).expect("symlink");

        let result = watcher
            .watcher
            .watch(&loop_link, WatchMode::non_recursive());
        assert_io_error_at(result, &loop_link);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));
        assert_eq!(watcher.watcher.get_chain_handles(), HashSet::from([]));
    }

    #[test]
    fn watch_again_arms_a_root_that_stayed_missing() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let parent = tmpdir.path().join("parent");
        let dir = parent.join("dir");
        let target = parent.join("target");
        std::fs::create_dir(&parent).expect("create_dir");
        // A dangling symlink: the root leads nowhere until its target appears.
        symlink("target", &dir).expect("symlink");

        watcher.watch_nonrecursively(&dir);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([parent.clone()]));

        std::fs::create_dir(&target).expect("create_dir");
        rx.ensure_empty_with_wait();

        watcher.watch_nonrecursively(&dir);
        rx.wait_ordered_exact([expected(&dir).create_folder()])
            .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([parent, dir.clone()])
        );

        let file = dir.join("file");
        std::fs::File::create_new(&file).expect("create");
        rx.wait_ordered([expected(&file).create_file()]);
    }

    #[test]
    fn track_path_watches_a_root_without_a_parent() {
        let (mut watcher, _rx) = watcher();
        let root = Path::new("/");

        watcher.watch_nonrecursively(root);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([root.to_path_buf()])
        );

        watcher.watcher.unwatch(root).expect("unwatch");
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));
    }

    /// Checks nothing as root; as another user in docker:
    /// `docker run --rm --user 65534:65534 -e HOME=/tmp -e CARGO_HOME=/tmp/cargo
    /// -e RUSTUP_HOME=/tmp/rustup -e CARGO_TARGET_DIR=/tmp/target -v "$PWD":/w -w /w rust:1.97
    /// cargo test -p rolldown-notify --lib inotify::`
    #[test]
    fn a_root_armed_halfway_keeps_no_handles_until_it_is_armed_fully() {
        let tmpdir = testdir();
        if runs_as_root(tmpdir.path()) {
            return;
        }
        let (mut watcher, rx) = watcher();

        let parent = tmpdir.path().join("parent");
        let moved = tmpdir.path().join("moved");
        let dir = parent.join("dir");
        let sub = dir.join("sub");
        let file = dir.join("f.js");
        std::fs::create_dir_all(&sub).expect("create_dir_all");
        std::fs::write(&file, "1").expect("write");

        watcher.watch_recursively(&dir);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([parent.clone(), dir.clone(), sub.clone()])
        );

        // The walks of the recursive root open its directories: those events are skipped.
        std::fs::rename(&parent, &moved).expect("rename away");
        rx.wait_ordered([expected(&dir).remove_folder()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));

        // The root comes back with a subdirectory that cannot be watched.
        std::fs::set_permissions(moved.join("dir").join("sub"), Permissions::from_mode(0o000))
            .expect("set_permissions");
        std::fs::rename(&moved, &parent).expect("rename back");
        let error = loop {
            match rx.recv_result() {
                Ok(event) => assert!(
                    matches!(event.kind, EventKind::Access(_)),
                    "expected an io error, got {event:?}"
                ),
                Err(error) => break error,
            }
        };
        assert!(
            matches!(error.kind, ErrorKind::Io(_)),
            "expected an io error, got {error:?}"
        );
        assert_eq!(watcher.get_watch_handles(), HashSet::from([parent.clone()]));

        // The root has no handle: a write inside is not seen, only the walk's opens arrive.
        std::fs::write(&file, "2").expect("write");
        thread::sleep(Duration::from_millis(50));
        for event in rx.rx.try_iter() {
            let event = event.expect("event");
            assert!(
                matches!(event.kind, EventKind::Access(_)) && !event.paths.contains(&file),
                "the root is not armed, got {event:?}"
            );
        }

        std::fs::set_permissions(&sub, Permissions::from_mode(0o755)).expect("set_permissions");
        std::fs::rename(&parent, &moved).expect("rename away");
        std::fs::rename(&moved, &parent).expect("rename back");
        rx.wait_ordered([expected(&dir).create_folder()]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([parent, dir, sub])
        );
    }
}
