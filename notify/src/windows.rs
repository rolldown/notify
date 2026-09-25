#![allow(missing_docs)]
//! Watcher implementation for Windows' directory management APIs
//!
//! For more information see the [ReadDirectoryChangesW reference][ref].
//!
//! [ref]: https://msdn.microsoft.com/en-us/library/windows/desktop/aa363950(v=vs.85).aspx

use crate::consolidating_path_trie::ConsolidatingPathTrie;
use crate::{
    BoundSender, Config, ErrorKind, PathsMut, Receiver, Sender, TargetMode, WatchMode, bounded,
    unbounded,
};
use crate::{Error, EventHandler, Result, Watcher};
use crate::{WatcherKind, event::*};
use rustc_hash::FxBuildHasher;
use std::alloc;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::io;
use std::os::raw::c_void;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::rc::Rc;
use std::slice;
use std::sync::{Arc, Mutex};
use std::thread;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_OPERATION_ABORTED, ERROR_SUCCESS, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED,
    FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES,
    FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SECURITY, FILE_NOTIFY_CHANGE_SIZE,
    FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    ReadDirectoryChangesW,
};
use windows_sys::Win32::System::IO::{CancelIo, OVERLAPPED};
use windows_sys::Win32::System::Threading::{
    CreateSemaphoreW, INFINITE, ReleaseSemaphore, WaitForSingleObjectEx,
};

const BUF_SIZE: u32 = 16384;

/// How many times a report looks again at the tracked paths after it opened handles: each look
/// that finds a directory opens one more level, and a path that keeps coming and going does not
/// keep the report looking.
const MAX_LOOKS: usize = 8;

fn windows_namespace_prefix_len(path: &[u16]) -> usize {
    let is_separator = |ch: u16| ch == '/' as u16 || ch == '\\' as u16;

    if path.len() >= 4
        && is_separator(path[0])
        && is_separator(path[1])
        && (path[2] == '?' as u16 || path[2] == '.' as u16)
        && is_separator(path[3])
    {
        4
    } else {
        0
    }
}

fn normalize_path_separators(path: PathBuf) -> PathBuf {
    let separator = '\\' as u16;
    let mut encoded_path: Vec<u16> = path.into_os_string().encode_wide().collect();
    let prefix_len = windows_namespace_prefix_len(&encoded_path);

    for ch in encoded_path.iter_mut().skip(prefix_len) {
        if *ch == '/' as u16 || *ch == '\\' as u16 {
            *ch = separator;
        }
    }

    PathBuf::from(OsString::from_wide(&encoded_path))
}

/// The resolved OS-level coverage for a user watch request.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedWatch {
    /// The directory we'd open with `CreateFileW` to receive content events
    /// for this watch, plus whether the user asked for a recursive subtree.
    primary: Option<(PathBuf, bool)>,
    /// Whether the watch also needs an auxiliary watch on the user path's
    /// direct parent so we can detect rename or delete events for the watched
    /// path itself.
    needs_tracked_parent: bool,
}

/// What re-resolving the tracked paths at or below a changed path found.
#[derive(Default)]
struct Reresolved {
    /// Whether a root resolves to something else than before, so that the handles have to
    /// converge.
    changed: bool,
    /// The roots that were missing and are there now.
    appeared: Vec<PathBuf>,
}

/// What converging the handles did.
struct Rebuilt {
    /// Why the handles that could not be opened failed, except the ones only on the way to a
    /// tracked path, which are skipped.
    failures: Vec<Error>,
    /// Whether a handle was opened: what appeared in its directory before it read is not reported
    /// by it.
    opened: bool,
}

/// The roots a `watch` or `commit` added or changed, with the entry each one replaced, which comes
/// back when the root cannot be watched.
type Added = HashMap<PathBuf, Option<(WatchMode, ResolvedWatch)>, FxBuildHasher>;

#[derive(Clone)]
struct ReadData {
    dir: PathBuf, // directory that is being watched
    watches: Rc<RefCell<HashMap<PathBuf, WatchMode, FxBuildHasher>>>,
    /// The ancestors of the tracked paths; a change to one of them changes what the paths below
    /// it can reach.
    ancestors: Rc<RefCell<HashSet<PathBuf, FxBuildHasher>>>,
    complete_sem: HANDLE,
    is_recursive: bool,
}

struct ReadDirectoryRequest {
    event_handler: Arc<Mutex<dyn EventHandler>>,
    buffer: [u8; BUF_SIZE as usize],
    handle: HANDLE,
    data: ReadData,
    action_tx: Sender<Action>,
}

impl ReadDirectoryRequest {
    fn unwatch_raw(&self) {
        let result = self
            .action_tx
            .send(Action::UnwatchRaw(self.data.dir.clone()));
        if let Err(e) = result {
            tracing::error!(?e, "failed to send UnwatchRaw action");
        }
    }
}

enum Action {
    Watch(PathBuf, WatchMode),
    Unwatch(PathBuf),
    UnwatchRaw(PathBuf),
    /// One completed read: its events, and the tracked paths or directories on the way to one
    /// that came or went.
    Report {
        changed: Vec<PathBuf>,
        events: Vec<Event>,
    },
    StageAndCommit(Vec<StagedChange>, BoundSender<Result<()>>),
    Stop,
    Configure(Config, BoundSender<Result<bool>>),
    #[cfg(test)]
    GetWatchHandles(BoundSender<HashSet<PathBuf>>),
}

enum StagedChange {
    Add(PathBuf, WatchMode),
    Remove(PathBuf),
}

struct WatchState {
    dir_handle: HANDLE,
    complete_sem: HANDLE,
}

struct ReadDirectoryChangesServer {
    tx: Sender<Action>,
    rx: Receiver<Action>,
    event_handler: Arc<Mutex<dyn EventHandler>>,
    cmd_tx: Sender<Result<PathBuf>>,
    /// The raw watch request registered by the user, keyed by user path.
    watches: Rc<RefCell<HashMap<PathBuf, WatchMode, FxBuildHasher>>>,
    /// Resolved OS-level coverage for each entry in `watches`, keyed by the
    /// same user path. Resolution needs a `metadata()` call, so it is cached
    /// here rather than recomputed on every rebuild.
    resolved_watches: HashMap<PathBuf, ResolvedWatch, FxBuildHasher>,
    watch_handles: HashMap<PathBuf, (WatchState, /* is_recursive */ bool), FxBuildHasher>,
    /// The ancestors of the tracked paths, shared with the event thread.
    ancestors: Rc<RefCell<HashSet<PathBuf, FxBuildHasher>>>,
    /// The handles opened only to see an ancestor of a tracked path come or go.
    chain_handles: HashSet<PathBuf, FxBuildHasher>,
    /// The ancestors of the tracked paths that are left without a handle of their own, since a
    /// recursive handle above sees them come and go.
    covered_ancestors: HashSet<PathBuf, FxBuildHasher>,
    /// The ancestors of the tracked paths whose handle could not be opened. They are not tried
    /// again on each event that shows them there, only by the next rebuild, or once an event of
    /// their own shows a directory come or go there.
    failed_ancestors: HashSet<PathBuf, FxBuildHasher>,
    /// Whether a handle that a tracked path needs was dropped out-of-band since the handles last
    /// converged: its directory may be back already, replaced within one read of its parent.
    handle_dropped: bool,
    wakeup_sem: HANDLE,
}

impl ReadDirectoryChangesServer {
    fn start(
        event_handler: Arc<Mutex<dyn EventHandler>>,
        cmd_tx: Sender<Result<PathBuf>>,
        wakeup_sem: HANDLE,
    ) -> Sender<Action> {
        let (action_tx, action_rx) = unbounded();
        // it is, in fact, ok to send the semaphore across threads
        let sem_temp = wakeup_sem as u64;
        let result = thread::Builder::new()
            .name("notify-rs windows loop".to_string())
            .spawn({
                let tx = action_tx.clone();
                move || {
                    let wakeup_sem = sem_temp as HANDLE;
                    let server = ReadDirectoryChangesServer {
                        tx,
                        rx: action_rx,
                        event_handler,
                        cmd_tx,
                        watches: Rc::new(RefCell::new(HashMap::default())),
                        resolved_watches: HashMap::default(),
                        watch_handles: HashMap::default(),
                        ancestors: Rc::new(RefCell::new(HashSet::default())),
                        chain_handles: HashSet::default(),
                        covered_ancestors: HashSet::default(),
                        failed_ancestors: HashSet::default(),
                        handle_dropped: false,
                        wakeup_sem,
                    };
                    server.run();
                }
            });
        if let Err(e) = result {
            tracing::error!(?e, "failed to spawn ReadDirectoryChangesWatcher thread");
        }
        action_tx
    }

    fn run(mut self) {
        loop {
            // process all available actions first
            let mut stopped = false;

            while let Ok(action) = self.rx.try_recv() {
                match action {
                    Action::Watch(path, watch_mode) => {
                        let res = self.add_watch(path, watch_mode);
                        let result = self.cmd_tx.send(res);
                        if let Err(e) = result {
                            tracing::error!(?e, "failed to send Watch result");
                        }
                    }
                    Action::Unwatch(path) => self.remove_watch(&path),
                    Action::UnwatchRaw(path) => self.remove_watch_raw(&path),
                    Action::Report { changed, events } => self.report(&changed, events),
                    Action::StageAndCommit(staged, tx) => {
                        let res = self.apply_staged(staged);
                        if let Err(e) = tx.send(res) {
                            tracing::error!(?e, "failed to send StageAndCommit result");
                        }
                    }
                    Action::Stop => {
                        stopped = true;
                        for (ws, _) in self.watch_handles.values() {
                            stop_watch(ws);
                        }
                        break;
                    }
                    Action::Configure(config, tx) => {
                        Self::configure_raw_mode(config, &tx);
                    }
                    #[cfg(test)]
                    Action::GetWatchHandles(tx) => {
                        let handles = self
                            .watch_handles
                            .keys()
                            .filter(|path| !self.chain_handles.contains(*path))
                            .cloned()
                            .collect();
                        tx.send(handles).unwrap();
                    }
                }
            }

            if stopped {
                break;
            }

            unsafe {
                // wait with alertable flag so that the completion routine fires
                WaitForSingleObjectEx(self.wakeup_sem, 100, 1);
            }
        }

        // we have to clean this up, since the watcher may be long gone
        unsafe {
            CloseHandle(self.wakeup_sem);
        }
    }

    #[tracing::instrument(level = "trace", skip(self))]
    fn add_watch(&mut self, path: PathBuf, watch_mode: WatchMode) -> Result<PathBuf> {
        let mut added = Added::default();
        self.add_watch_internal(
            path.clone(),
            watch_mode,
            &mut HashSet::default(),
            &mut added,
        )?;
        self.converge_added(&added).map_or(Ok(path), Err)
    }

    /// Register a single user watch: merge `mode` with any existing entry for
    /// `path` (so repeated watches of the same path only ever upgrade
    /// coverage), resolve it, and record both the raw request in `watches` and
    /// the resolved coverage in `resolved_watches`. A tracked path whose
    /// ancestors cannot be examined is not recorded; `examined` holds the
    /// ancestors this call found there already. `added` gets the entry the path
    /// had before the first time this call added it.
    ///
    /// Watch handles are left untouched; the caller drives `converge_added`.
    fn add_watch_internal(
        &mut self,
        path: PathBuf,
        mode: WatchMode,
        examined: &mut HashSet<PathBuf, FxBuildHasher>,
        added: &mut Added,
    ) -> Result<()> {
        let existing = self.watches.borrow().get(&path).copied();
        let merged = match existing {
            Some(existing) => {
                let mut merged = existing;
                merged.upgrade_with(mode);
                merged
            }
            None => mode,
        };
        if merged.target_mode == TargetMode::TrackPath {
            examine_ancestors(&path, examined)?;
        }
        let resolved = resolve_user_watch(&path, merged)?;
        let replaced = existing.zip(self.resolved_watches.get(&path).cloned());
        added.entry(path.clone()).or_insert(replaced);
        self.watches.borrow_mut().insert(path.clone(), merged);
        self.resolved_watches.insert(path, resolved);
        Ok(())
    }

    fn apply_staged(&mut self, staged: Vec<StagedChange>) -> Result<()> {
        tracing::trace!(change_count = staged.len(), "applying staged watch changes");
        let mut first_error: Option<Error> = None;
        // The ancestors the added paths share are examined once.
        let mut examined = HashSet::default();
        let mut added = Added::default();
        for change in staged {
            let res = match change {
                StagedChange::Add(path, mode) => {
                    self.add_watch_internal(path, mode, &mut examined, &mut added)
                }
                StagedChange::Remove(path) => {
                    // A path removed after it was added stays removed.
                    added.remove(&path);
                    self.remove_watch_internal(&path);
                    Ok(())
                }
            };
            if let Err(e) = res
                && first_error.is_none()
            {
                first_error = Some(e);
            }
        }
        if let Some(e) = self.converge_added(&added)
            && first_error.is_none()
        {
            first_error = Some(e);
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Converge the handles after `watch` or `commit` added the roots in `added`, and return why
    /// one of them cannot be watched: the handle of its directory, or of the parent that sees it
    /// come and go, cannot be opened. Such a root is not kept, as on inotify and kqueue: the entry
    /// it replaced comes back, or it is dropped if it is new, so that it does not fail later calls.
    /// The failures of the other roots are only logged. A tracked root that was missing and is
    /// watched now is reported as created, as when a look opens it.
    fn converge_added(&mut self, added: &Added) -> Option<Error> {
        let failures = self.rebuild_watch_handles().failures;
        let mut unwatchable: HashSet<PathBuf, FxBuildHasher> = HashSet::default();
        {
            let failed: HashSet<&Path, FxBuildHasher> = failures
                .iter()
                .flat_map(|failure| failure.paths.iter().map(PathBuf::as_path))
                .collect();
            for (root, replaced) in added {
                let Some(dir) = self.failed_dir_of(root, &failed) else {
                    continue;
                };
                tracing::debug!(
                    "cannot watch {}, since {} cannot be opened",
                    root.display(),
                    dir.display()
                );
                if let Some((mode, resolved)) = replaced {
                    self.watches.borrow_mut().insert(root.clone(), *mode);
                    self.resolved_watches.insert(root.clone(), resolved.clone());
                } else {
                    self.remove_watch_internal(root);
                }
                unwatchable.insert(dir);
            }
        }
        let mut error = None;
        for failure in failures {
            let concerns_added = failure
                .paths
                .iter()
                .any(|failed| unwatchable.contains(failed));
            if concerns_added && error.is_none() {
                error = Some(failure);
            } else {
                tracing::error!(?failure, "failed to rebuild watch handles");
            }
        }
        if !unwatchable.is_empty() {
            // The handles opened only for the roots that are not kept are closed again.
            for failure in self.rebuild_watch_handles().failures {
                tracing::error!(?failure, "failed to rebuild watch handles");
            }
        }
        self.report_rewatched(added);
        error
    }

    /// The directory that `root` needs but that has no handle, since it could not be opened: the
    /// one whose handle reports its entries, or the parent that sees it come and go.
    fn failed_dir_of(
        &self,
        root: &Path,
        failed: &HashSet<&Path, FxBuildHasher>,
    ) -> Option<PathBuf> {
        let resolved = self.resolved_watches.get(root)?;
        if let Some((dir, _)) = &resolved.primary
            && !self.is_handled(dir)
        {
            // Its handle may be one above it, which it shares with its siblings.
            return dir
                .ancestors()
                .find(|ancestor| failed.contains(ancestor))
                .map(Path::to_path_buf);
        }
        let parent = root.parent().filter(|_| resolved.needs_tracked_parent)?;
        failed.contains(parent).then(|| parent.to_path_buf())
    }

    /// Report as created the tracked roots in `added` that were missing and are watched now:
    /// watching a root again is the way to recover one whose handle could not be opened, and it is
    /// reported as on inotify and kqueue.
    fn report_rewatched(&self, added: &Added) {
        let created: Vec<Event> = added
            .iter()
            .filter(|(root, replaced)| {
                let was_missing = replaced.as_ref().is_some_and(|(mode, resolved)| {
                    mode.target_mode == TargetMode::TrackPath && resolved.primary.is_none()
                });
                was_missing
                    && self
                        .resolved_watches
                        .get(*root)
                        .and_then(|resolved| resolved.primary.as_ref())
                        .is_some_and(|(dir, _)| self.is_handled(dir))
            })
            .map(|(root, _)| {
                let kind = if root.is_dir() {
                    CreateKind::Folder
                } else {
                    CreateKind::File
                };
                Event::new(EventKind::Create(kind)).add_path(root.clone())
            })
            .collect();
        if created.is_empty() {
            return;
        }
        if let Ok(mut handler) = self.event_handler.lock() {
            for event in created {
                handler.handle_event(Ok(event));
            }
        }
    }

    /// Converge the open OS-level watch handles with the current set of user
    /// watches.
    fn rebuild_watch_handles(&mut self) -> Rebuilt {
        self.handle_dropped = false;
        let mut failures = Vec::new();

        // Drop resolved entries whose user watch is gone.
        // This is needed because the event thread can remove a `NoTrack` entry
        // from it directly (see `handle_event`) without touching `resolved_watches`.
        {
            let watches = self.watches.borrow();
            self.resolved_watches
                .retain(|path, _| watches.contains_key(path));
        }

        // Build `target`: the desired set of OS-level dirs to watch and their
        // recursive flags. It is the consolidated primary dir requests, plus a
        // non-recursive watch on the tracked parent of any watch that needs one.
        let mut trie = ConsolidatingPathTrie::new(true, 0);
        for resolved in self.resolved_watches.values() {
            if let Some((dir, _)) = &resolved.primary {
                trie.insert(dir);
            }
        }
        let mut target: HashMap<PathBuf, bool, FxBuildHasher> = trie
            .values()
            .into_iter()
            .map(|p| {
                let recursive = compute_recursive_flag(&p, &self.resolved_watches);
                (p, recursive)
            })
            .collect();
        for (path, resolved) in &self.resolved_watches {
            if resolved.needs_tracked_parent
                && let Some(parent) = path.parent()
                && !target.contains_key(parent)
                && dir_present_or(parent, self.watch_handles.contains_key(parent))
            {
                target.insert(parent.to_path_buf(), false);
            }
        }
        self.add_chain_targets(&mut target);
        tracing::trace!(desired = ?target, "rebuilding watch handles");

        let to_remove: Vec<PathBuf> = self
            .watch_handles
            .iter()
            .filter(|(p, (_, is_rec))| target.get(*p).is_none_or(|t| t != is_rec))
            .map(|(p, _)| p.clone())
            .collect();
        if !to_remove.is_empty() {
            tracing::trace!(
                ?to_remove,
                "closing watch handles that are no longer needed"
            );
        }
        for p in to_remove {
            if let Some((ws, _)) = self.watch_handles.remove(&p) {
                stop_watch(&ws);
            }
        }

        let to_open: Vec<(PathBuf, bool)> = target
            .into_iter()
            .filter(|(p, _)| !self.watch_handles.contains_key(p))
            .collect();
        let mut opened = false;
        for (path, is_recursive) in to_open {
            let is_chain = self.chain_handles.contains(&path);
            match self.add_watch_raw(path.clone(), is_recursive, false) {
                Ok(()) => opened = true,
                Err(e) if is_chain => {
                    // An ancestor that cannot be opened is skipped, as on inotify and kqueue: the
                    // handles that did open still report the paths below it. It gets another try
                    // with the next rebuild, or once an event of its own shows a directory come
                    // or go there, but not on each event that shows it there.
                    tracing::debug!(?e, "cannot watch ancestor: {}", path.display());
                    self.chain_handles.remove(&path);
                    if matches!(dir_present(&path), Ok(true)) {
                        self.failed_ancestors.insert(path);
                    }
                }
                Err(e) => failures.push(e),
            }
        }
        Rebuilt { failures, opened }
    }

    /// Add to `target` the ancestors of the tracked paths, so that a directory moved away or
    /// deleted above a tracked path is seen, and a missing one is seen once it appears. The ones
    /// below a recursive watch are seen through it.
    fn add_chain_targets(&mut self, target: &mut HashMap<PathBuf, bool, FxBuildHasher>) {
        let mut ancestors: HashSet<PathBuf, FxBuildHasher> = HashSet::default();
        for (path, mode) in self.watches.borrow().iter() {
            if mode.target_mode == TargetMode::TrackPath {
                ancestors.extend(path.ancestors().skip(1).map(Path::to_path_buf));
            }
        }
        // A tracked parent below a recursive watch still gets a handle of its own once it appears.
        let tracked_parents: HashSet<&Path, FxBuildHasher> = self
            .resolved_watches
            .iter()
            .filter(|(_, resolved)| resolved.needs_tracked_parent)
            .filter_map(|(path, _)| path.parent())
            .collect();
        self.chain_handles.clear();
        self.covered_ancestors.clear();
        self.failed_ancestors.clear();
        for ancestor in &ancestors {
            if target.contains_key(ancestor) {
                continue;
            }
            if target
                .iter()
                .any(|(dir, recursive)| *recursive && ancestor.starts_with(dir))
            {
                if !tracked_parents.contains(ancestor.as_path()) {
                    self.covered_ancestors.insert(ancestor.clone());
                }
                continue;
            }
            if !dir_present_or(ancestor, self.watch_handles.contains_key(ancestor)) {
                continue;
            }
            target.insert(ancestor.clone(), false);
            self.chain_handles.insert(ancestor.clone());
        }
        *self.ancestors.borrow_mut() = ancestors;
    }

    #[tracing::instrument(level = "trace", skip(self))]
    fn add_watch_raw(
        &mut self,
        path: PathBuf,
        is_recursive: bool,
        watching_file: bool,
    ) -> Result<()> {
        if let Some((ws, was_recursive)) = self.watch_handles.get(&path) {
            let need_upgrade_to_recursive = !*was_recursive && is_recursive;
            if !need_upgrade_to_recursive {
                tracing::trace!(
                    "watch handle already exists and no need to upgrade: {}",
                    path.display()
                );
                return Ok(());
            }
            tracing::trace!("upgrading watch handle to recursive: {}", path.display());
            stop_watch(ws);
        }

        #[cfg(test)]
        {
            tests::before_open(&path);
            if tests::open_fails(&path) {
                return Err(Error::path_not_found().add_path(path));
            }
        }
        let encoded_path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle;
        unsafe {
            handle = CreateFileW(
                encoded_path.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_DELETE | FILE_SHARE_WRITE,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            );

            if handle == INVALID_HANDLE_VALUE {
                return Err(if watching_file {
                    Error::generic(
                        "You attempted to watch a single file, but parent \
                         directory could not be opened.",
                    )
                    .add_path(path)
                } else {
                    // TODO: Call GetLastError for better error info?
                    Error::path_not_found().add_path(path)
                });
            }
        }
        // every watcher gets its own semaphore to signal completion
        let semaphore = unsafe { CreateSemaphoreW(ptr::null_mut(), 0, 1, ptr::null_mut()) };
        if semaphore.is_null() || semaphore == INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(handle);
            }
            return Err(Error::generic("Failed to create semaphore for watch.").add_path(path));
        }
        let rd = ReadData {
            dir: path.clone(),
            watches: Rc::clone(&self.watches),
            ancestors: Rc::clone(&self.ancestors),
            complete_sem: semaphore,
            is_recursive,
        };
        let ws = WatchState {
            dir_handle: handle,
            complete_sem: semaphore,
        };
        self.watch_handles.insert(path, (ws, is_recursive));
        start_read(
            &rd,
            Arc::clone(&self.event_handler),
            handle,
            self.tx.clone(),
        );
        Ok(())
    }

    /// Remove a single user watch from `watches` and `resolved_watches`,
    /// returning whether an entry was present. Watch handles are left
    /// untouched; the caller drives `rebuild_watch_handles`.
    fn remove_watch_internal(&mut self, path: &Path) -> bool {
        let removed = self.watches.borrow_mut().remove(path).is_some();
        self.resolved_watches.remove(path);
        removed
    }

    #[tracing::instrument(level = "trace", skip(self))]
    fn remove_watch(&mut self, path: &Path) {
        if self.remove_watch_internal(path) {
            for e in self.rebuild_watch_handles().failures {
                tracing::error!(?e, "failed to rebuild watch handles after remove_watch");
            }
        }
    }

    /// Drop a watch handle out-of-band when the OS reports that the directory is
    /// gone (or some other error invalidated it). The handle entry is purged
    /// without consulting `self.watches`, since the corresponding user watch
    /// may still be present; the next report re-opens it if a tracked path
    /// still needs it. A `NoTrack` watch stops with its directory.
    #[tracing::instrument(level = "trace", skip(self))]
    fn remove_watch_raw(&mut self, path: &Path) {
        if let Some((ws, _)) = self.watch_handles.remove(path) {
            stop_watch(&ws);
            self.handle_dropped |= self.serves_tracked_path(path);
        }
    }

    /// Whether the handle at `path` serves a tracked path: it is on the way to one, it is the
    /// parent that sees one come or go, or it reports one's entries.
    fn serves_tracked_path(&self, path: &Path) -> bool {
        if self.chain_handles.contains(path) {
            return true;
        }
        let watches = self.watches.borrow();
        self.resolved_watches.iter().any(|(root, resolved)| {
            watches
                .get(root)
                .is_some_and(|mode| mode.target_mode == TargetMode::TrackPath)
                && ((resolved.needs_tracked_parent && root.parent() == Some(path))
                    || resolved
                        .primary
                        .as_ref()
                        .is_some_and(|(dir, _)| dir.starts_with(path)))
        })
    }

    fn configure_raw_mode(_config: Config, tx: &BoundSender<Result<bool>>) {
        tx.send(Ok(false))
            .expect("configuration channel disconnect");
    }

    /// Deliver the events of one read. The tracked paths at or below a changed path may be
    /// reachable or out of reach now, so they are re-resolved first, and the handles converge
    /// when that changed what is to be watched: a root reported as created is already watched by
    /// the time the report arrives. A root whose handle cannot be opened is reported as an error
    /// instead, and counts as missing until it is seen again or watched again.
    fn report(&mut self, changed: &[PathBuf], events: Vec<Event>) {
        // An event of an ancestor whose handle could not be opened shows a directory come or go
        // there: the one that is there now gets another try.
        for path in changed {
            self.failed_ancestors.remove(path);
        }
        let mut derived = Vec::new();
        let mut appeared = Vec::new();
        let mut rebuild = self.handle_dropped;
        for path in changed {
            let found = self.reresolve_below(path, &mut derived);
            appeared.extend(found.appeared);
            rebuild = rebuild || found.changed || self.ancestor_out_of_step(path);
        }
        let mut errors = Vec::new();
        if rebuild {
            let mut rebuilt = self.rebuild_watch_handles();
            // A root, or a directory on the way to one, that appeared or went in a directory
            // whose handle was just opened, before the handle read, is not reported by it. Look
            // again now that they read, and open what the look found, until a look finds nothing
            // new; a look comes last, so that a root that appeared while the last handles were
            // opened is still reported.
            let mut looks = 0;
            while rebuilt.opened {
                looks += 1;
                let mut again = false;
                for path in changed {
                    let found = self.reresolve_below(path, &mut derived);
                    appeared.extend(found.appeared);
                    again = again || found.changed || self.chain_out_of_step_below(path);
                }
                if !again {
                    break;
                }
                if looks == MAX_LOOKS {
                    tracing::debug!(
                        "stopped looking again at the tracked paths after {looks} looks"
                    );
                    break;
                }
                rebuilt = self.rebuild_watch_handles();
            }
            errors = self.forget_unwatched(&appeared, rebuilt.failures, &mut derived);
        }
        if !derived.is_empty() {
            // The handle that saw a root appear reported it in this read already; the Create
            // derived for it would be a duplicate.
            let created: HashSet<&Path, FxBuildHasher> = events
                .iter()
                .filter(|event| matches!(event.kind, EventKind::Create(_)))
                .flat_map(|event| event.paths.iter().map(PathBuf::as_path))
                .collect();
            derived.retain(|event| {
                let reported = matches!(event.kind, EventKind::Create(_))
                    && event
                        .paths
                        .iter()
                        .any(|path| created.contains(path.as_path()));
                !reported
            });
        }
        if let Ok(mut handler) = self.event_handler.lock() {
            for event in events.into_iter().chain(derived) {
                handler.handle_event(Ok(event));
            }
            for error in errors {
                handler.handle_event(Err(error));
            }
        }
    }

    /// The roots that appeared in this report but got no handle, since it could not be opened,
    /// are not watched: their creation is not reported, their failure is, and they count as
    /// missing again, so that they are reported as created once a later look, or a `watch` of
    /// them, opens them. The failures of the other paths are only logged.
    fn forget_unwatched(
        &mut self,
        appeared: &[PathBuf],
        failures: Vec<Error>,
        derived: &mut Vec<Event>,
    ) -> Vec<Error> {
        let unwatched: Vec<(PathBuf, PathBuf)> = appeared
            .iter()
            .filter_map(|root| {
                let (dir, _) = self.resolved_watches.get(root)?.primary.as_ref()?;
                (!self.is_handled(dir)).then(|| (root.clone(), dir.clone()))
            })
            .collect();
        for (root, _) in &unwatched {
            derived.retain(|event| {
                !matches!(event.kind, EventKind::Create(_)) || !event.paths.contains(root)
            });
            self.resolved_watches.insert(
                root.clone(),
                ResolvedWatch {
                    primary: None,
                    needs_tracked_parent: true,
                },
            );
        }
        let mut errors = Vec::new();
        for failure in failures {
            let concerns_unwatched = unwatched
                .iter()
                .any(|(_, dir)| failure.paths.iter().any(|failed| dir.starts_with(failed)));
            if concerns_unwatched {
                errors.push(failure);
            } else {
                tracing::error!(
                    ?failure,
                    "failed to rebuild watch handles after a tracked path changed"
                );
            }
        }
        errors
    }

    /// Whether a handle reports the entries of `dir`: its own, or a recursive one above it.
    fn is_handled(&self, dir: &Path) -> bool {
        dir.ancestors().any(|handle| {
            self.watch_handles
                .get(handle)
                .is_some_and(|(_, recursive)| *recursive || handle == dir)
        })
    }

    /// Whether `path`, a directory on the way to a tracked path, came or went since the handles
    /// last converged: it is there without the handle it is meant to have, or it has a handle but
    /// is gone. One whose handle could not be opened is not meant to have one until it is tried
    /// again. One that cannot be examined keeps what it has, as in the rebuild.
    fn ancestor_out_of_step(&self, path: &Path) -> bool {
        if !self.ancestors.borrow().contains(path) {
            return false;
        }
        let has_handle = self.watch_handles.contains_key(path);
        match dir_present(path) {
            Ok(true) => {
                !has_handle
                    && !self.covered_ancestors.contains(path)
                    && !self.failed_ancestors.contains(path)
            }
            Ok(false) => has_handle,
            Err(e) => {
                tracing::warn!(
                    ?e,
                    "cannot tell whether a directory is there: {}",
                    path.display()
                );
                false
            }
        }
    }

    /// Whether a directory on the way to a tracked path, at or below `path`, came or went since
    /// the handles last converged.
    fn chain_out_of_step_below(&self, path: &Path) -> bool {
        let below: Vec<PathBuf> = {
            let ancestors = self.ancestors.borrow();
            // The ancestors of an ancestor are ancestors too: none is below another path.
            if !ancestors.contains(path) {
                return false;
            }
            ancestors
                .iter()
                .filter(|ancestor| ancestor.starts_with(path))
                .cloned()
                .collect()
        };
        below
            .iter()
            .any(|ancestor| self.ancestor_out_of_step(ancestor))
    }

    /// Re-resolve the tracked paths at or below `path` and report, into `derived`, the ones
    /// whose presence changed. The event on `path` itself was reported by the handle that saw it.
    fn reresolve_below(&mut self, path: &Path, derived: &mut Vec<Event>) -> Reresolved {
        let roots: Vec<(PathBuf, WatchMode)> = {
            let watches = self.watches.borrow();
            if self.ancestors.borrow().contains(path) {
                watches
                    .iter()
                    .filter(|(root, mode)| {
                        mode.target_mode == TargetMode::TrackPath && root.starts_with(path)
                    })
                    .map(|(root, mode)| (root.clone(), *mode))
                    .collect()
            } else {
                // Nothing is tracked below a path that is not an ancestor: only the path itself
                // can be a root.
                watches
                    .get_key_value(path)
                    .filter(|(_, mode)| mode.target_mode == TargetMode::TrackPath)
                    .map(|(root, mode)| (root.clone(), *mode))
                    .into_iter()
                    .collect()
            }
        };
        let mut found = Reresolved::default();
        for (root, mode) in roots {
            let resolved = match resolve_user_watch(&root, mode) {
                Ok(resolved) => resolved,
                Err(e) => {
                    tracing::debug!(?e, "cannot resolve tracked path: {}", root.display());
                    continue;
                }
            };
            let is_present = resolved.primary.is_some();
            let previous = self.resolved_watches.get(&root);
            let was_present = previous.is_some_and(|previous| previous.primary.is_some());
            found.changed |= previous != Some(&resolved);
            self.resolved_watches.insert(root.clone(), resolved);
            if is_present && !was_present {
                found.appeared.push(root.clone());
            }
            if root.as_path() == path || was_present == is_present {
                continue;
            }
            let kind = if is_present {
                EventKind::Create(if root.is_dir() {
                    CreateKind::Folder
                } else {
                    CreateKind::File
                })
            } else {
                EventKind::Remove(RemoveKind::Any)
            };
            derived.push(Event::new(kind).add_path(root));
        }
        found
    }
}

/// Resolve a user-supplied watch path + mode into a [`ResolvedWatch`] describing
/// which OS-level directories we'd want to watch.
fn resolve_user_watch(path: &Path, mode: WatchMode) -> Result<ResolvedWatch> {
    let is_track_path = mode.target_mode == TargetMode::TrackPath;

    // Note: reading metadata on a directory triggers a modify event
    match path.metadata().map_err(Error::io_watch) {
        Ok(meta) => {
            if meta.is_dir() {
                Ok(ResolvedWatch {
                    primary: Some((path.to_path_buf(), mode.recursive_mode.is_recursive())),
                    needs_tracked_parent: is_track_path,
                })
            } else if meta.is_file() {
                let parent = path.parent().unwrap_or(path).to_path_buf();
                Ok(ResolvedWatch {
                    primary: Some((parent, mode.recursive_mode.is_recursive())),
                    // For files we always have to watch the parent directory anyway,
                    // so the "tracked parent" rename-detection requirement is
                    // already covered by `primary`; no separate entry needed.
                    needs_tracked_parent: false,
                })
            } else {
                Err(
                    Error::generic("Input watch path is neither a file nor a directory.")
                        .add_path(path.to_path_buf()),
                )
            }
        }
        Err(err) => {
            // For TrackPath we keep the watch alive and rely on the parent dir
            // to tell us when something appears at `path`.
            if is_track_path && matches!(err.kind, ErrorKind::PathNotFound) {
                Ok(ResolvedWatch {
                    primary: None,
                    needs_tracked_parent: true,
                })
            } else {
                Err(err)
            }
        }
    }
}

/// Whether `path` is a directory that is there. A path that is missing, or a file where a
/// directory is needed, is waited for like a missing tracked path; any other failure is returned,
/// since the paths below might never be reached.
fn dir_present(path: &Path) -> Result<bool> {
    #[cfg(test)]
    if tests::examine_fails(path) {
        let e = io::Error::from(io::ErrorKind::PermissionDenied);
        return Err(Error::io(e).add_path(path.to_path_buf()));
    }
    match std::fs::metadata(path) {
        Ok(meta) => Ok(meta.is_dir()),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(false)
        }
        Err(e) => Err(Error::io(e).add_path(path.to_path_buf())),
    }
}

/// [`dir_present`] for the rebuild, which converges the handles of every path: a directory that
/// cannot be examined keeps what it has, a handle or none, and the failure is only logged, so
/// that it does not fail the watches of other paths. `watch` examined it when it added the paths
/// below it.
fn dir_present_or(path: &Path, has_handle: bool) -> bool {
    match dir_present(path) {
        Ok(present) => present,
        Err(e) => {
            tracing::warn!(
                ?e,
                "cannot tell whether a directory is there: {}",
                path.display()
            );
            has_handle
        }
    }
}

/// Examine the ancestors of a tracked path from the top down, up to the first one that is
/// missing: one that cannot be examined fails the watch that adds the path, rather than leave the
/// path waiting for a directory it might never reach. `examined` holds the ancestors found there
/// already, which the paths added together share.
fn examine_ancestors(path: &Path, examined: &mut HashSet<PathBuf, FxBuildHasher>) -> Result<()> {
    let ancestors: Vec<&Path> = path.ancestors().skip(1).collect();
    for ancestor in ancestors.into_iter().rev() {
        if examined.contains(ancestor) {
            continue;
        }
        if !dir_present(ancestor)? {
            break;
        }
        examined.insert(ancestor.to_path_buf());
    }
    Ok(())
}

/// Decide whether a consolidated OS-level watch on `target_path` must be opened
/// with `bWatchSubtree=1`.
fn compute_recursive_flag(
    target_path: &Path,
    resolved_watches: &HashMap<PathBuf, ResolvedWatch, FxBuildHasher>,
) -> bool {
    resolved_watches
        .values()
        .filter_map(|resolved| resolved.primary.as_ref())
        .any(|(dir, is_rec)| (dir == target_path && *is_rec) || dir.starts_with(target_path))
}

/// Returns `true` if an event on `event_path` is covered by some user-registered
/// watch.
fn is_event_covered(
    watches: &HashMap<PathBuf, WatchMode, FxBuildHasher>,
    event_path: &Path,
) -> bool {
    event_path.ancestors().enumerate().any(|(depth, ancestor)| {
        watches
            .get(ancestor)
            .is_some_and(|mode| depth <= 1 || mode.recursive_mode.is_recursive())
    })
}

fn stop_watch(ws: &WatchState) {
    tracing::trace!("removing ReadDirectoryChangesW watch");
    unsafe {
        let cio = CancelIo(ws.dir_handle);
        let ch = CloseHandle(ws.dir_handle);
        // have to wait for it, otherwise we leak the memory allocated for there read request
        if cio != 0 && ch != 0 {
            while WaitForSingleObjectEx(ws.complete_sem, INFINITE, 1) != WAIT_OBJECT_0 {
                // drain the apc queue, fix for https://github.com/notify-rs/notify/issues/287#issuecomment-801465550
            }
        }
        CloseHandle(ws.complete_sem);
    }
}

fn start_read(
    rd: &ReadData,
    event_handler: Arc<Mutex<dyn EventHandler>>,
    handle: HANDLE,
    action_tx: Sender<Action>,
) {
    tracing::trace!("starting ReadDirectoryChangesW watch: {}", rd.dir.display());

    let request = Box::new(ReadDirectoryRequest {
        event_handler,
        handle,
        buffer: [0u8; BUF_SIZE as usize],
        data: rd.clone(),
        action_tx,
    });

    let flags = FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_ATTRIBUTES
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_LAST_WRITE
        | FILE_NOTIFY_CHANGE_CREATION
        | FILE_NOTIFY_CHANGE_SECURITY;

    let monitor_subdir = i32::from(request.data.is_recursive);

    unsafe {
        #[expect(clippy::cast_ptr_alignment)]
        let overlapped =
            alloc::alloc_zeroed(alloc::Layout::new::<OVERLAPPED>()).cast::<OVERLAPPED>();
        // When using callback based async requests, we are allowed to use the hEvent member
        // for our own purposes

        let request = Box::leak(request);
        (*overlapped).hEvent = std::ptr::from_mut(request).cast();

        // This is using an asynchronous call with a completion routine for receiving notifications
        // An I/O completion port would probably be more performant
        let ret = ReadDirectoryChangesW(
            handle,
            request.buffer.as_mut_ptr().cast::<c_void>(),
            BUF_SIZE,
            monitor_subdir,
            flags,
            std::ptr::from_mut::<u32>(&mut 0u32), // not used for async reqs
            overlapped,
            Some(handle_event),
        );

        if ret == 0 {
            // error reading. retransmute request memory to allow drop.
            // Because of the error, ownership of the `overlapped` alloc was not passed
            // over to `ReadDirectoryChangesW`.
            // So we can claim ownership back.
            let _overlapped = Box::from_raw(overlapped);
            let request = Box::from_raw(request);
            ReleaseSemaphore(request.data.complete_sem, 1, ptr::null_mut());
        }
    }
}

#[expect(clippy::too_many_lines)]
unsafe extern "system" fn handle_event(
    error_code: u32,
    _bytes_written: u32,
    overlapped: *mut OVERLAPPED,
) {
    let overlapped: Box<OVERLAPPED> = unsafe { Box::from_raw(overlapped) };
    let request: Box<ReadDirectoryRequest> = unsafe { Box::from_raw(overlapped.hEvent.cast()) };

    let release_semaphore =
        || unsafe { ReleaseSemaphore(request.data.complete_sem, 1, ptr::null_mut()) };

    if error_code != ERROR_SUCCESS {
        tracing::trace!(
            path = ?request.data.dir,
            is_recursive = request.data.is_recursive,
            "ReadDirectoryChangesW handle_event called with error code {error_code}",
        );
    }

    match error_code {
        ERROR_OPERATION_ABORTED => {
            // received when dir is unwatched or watcher is shutdown; return and let overlapped/request get drop-cleaned
            release_semaphore();
            return;
        }
        ERROR_ACCESS_DENIED => {
            let dir = request.data.dir.clone();
            // This could happen when the watched directory is deleted or trashed, first check if it's the case.
            // If so, unwatch the directory and return, otherwise, continue to handle the event.
            if !dir.exists() {
                tracing::debug!(
                    path = ?request.data.dir,
                    is_recursive = request.data.is_recursive,
                    "ReadDirectoryChangesW handle_event: ERROR_ACCESS_DENIED event and directory no longer exists",
                );
                let is_no_track = request
                    .data
                    .watches
                    .borrow()
                    .get(&dir)
                    .is_some_and(|mode| mode.target_mode == TargetMode::NoTrack);
                if is_no_track {
                    // A `NoTrack` watch follows its directory, which is gone: it is forgotten, as
                    // when its parent reports it gone, so that no later rebuild opens whatever
                    // takes its place.
                    request.data.watches.borrow_mut().remove(&dir);
                    // Delivered by the server like every other event, so that it stays behind
                    // the reads that completed before it.
                    let ev = Event::new(EventKind::Remove(RemoveKind::Any)).add_path(dir);
                    let report = Action::Report {
                        changed: vec![],
                        events: vec![ev],
                    };
                    if let Err(e) = request.action_tx.send(report) {
                        tracing::error!(?e, "failed to send Report action");
                    }
                }
                request.unwatch_raw();
                release_semaphore();
                return;
            }
        }
        ERROR_SUCCESS => {
            // Success, continue to handle the event
        }
        _ => {
            // Some unidentified error occurred, log and unwatch the directory, then return.
            tracing::error!(
                "unknown error in ReadDirectoryChangesW for directory {}: {}",
                request.data.dir.display(),
                error_code
            );
            request.unwatch_raw();
            release_semaphore();
            return;
        }
    }

    // Get the next request queued up as soon as possible
    let action_tx = request.action_tx.clone();
    start_read(
        &request.data,
        Arc::clone(&request.event_handler),
        request.handle,
        request.action_tx,
    );

    let mut remove_paths = vec![];
    // The server delivers the events once it has acted on `changed`, so that a root reported as
    // created is already watched.
    let mut changed = vec![];
    let mut events = vec![];

    // The FILE_NOTIFY_INFORMATION struct has a variable length due to the variable length
    // string as its last member. Each struct contains an offset for getting the next entry in
    // the buffer.
    let mut cur_offset: *const u8 = request.buffer.as_ptr();
    // In Wine, FILE_NOTIFY_INFORMATION structs are packed placed in the buffer;
    // they are aligned to 16bit (WCHAR) boundary instead of 32bit required by FILE_NOTIFY_INFORMATION.
    // Hence, we need to use `read_unaligned` here to avoid UB.
    let mut cur_entry =
        unsafe { ptr::read_unaligned(cur_offset.cast::<FILE_NOTIFY_INFORMATION>()) };
    loop {
        // filename length is size in bytes, so / 2
        let len = cur_entry.FileNameLength as usize / 2;
        let encoded_path: &[u16] = unsafe {
            slice::from_raw_parts(
                cur_offset
                    .add(std::mem::offset_of!(FILE_NOTIFY_INFORMATION, FileName))
                    .cast(),
                len,
            )
        };
        // prepend root to get a full path
        let path = normalize_path_separators(
            request
                .data
                .dir
                .join(PathBuf::from(OsString::from_wide(encoded_path))),
        );

        // A tracked path, or a directory on the way to one, came or went: the server re-resolves
        // what can be reached now.
        let structural = matches!(
            cur_entry.Action,
            FILE_ACTION_ADDED
                | FILE_ACTION_REMOVED
                | FILE_ACTION_RENAMED_OLD_NAME
                | FILE_ACTION_RENAMED_NEW_NAME
        );
        if structural
            && (request.data.ancestors.borrow().contains(&path)
                || request
                    .data
                    .watches
                    .borrow()
                    .get(&path)
                    .is_some_and(|mode| mode.target_mode == TargetMode::TrackPath))
        {
            changed.push(path.clone());
        }

        let skip = !is_event_covered(&request.data.watches.borrow(), &path);

        tracing::trace!(
            handle_path = ?request.data.dir,
            is_recursive = request.data.is_recursive,
            ?path,
            skip,
            action = cur_entry.Action,
            "ReadDirectoryChangesW handle_event called",
        );

        if !skip {
            let newe = Event::new(EventKind::Any).add_path(path.clone());

            match cur_entry.Action {
                FILE_ACTION_RENAMED_OLD_NAME => {
                    remove_paths.push(path.clone());
                    let kind = EventKind::Modify(ModifyKind::Name(RenameMode::From));
                    events.push(newe.set_kind(kind));
                }
                FILE_ACTION_RENAMED_NEW_NAME => {
                    let kind = EventKind::Modify(ModifyKind::Name(RenameMode::To));
                    events.push(newe.set_kind(kind));
                }
                FILE_ACTION_ADDED => {
                    let kind = EventKind::Create(CreateKind::Any);
                    events.push(newe.set_kind(kind));
                }
                FILE_ACTION_REMOVED => {
                    remove_paths.push(path.clone());
                    let kind = EventKind::Remove(RemoveKind::Any);
                    events.push(newe.set_kind(kind));
                }
                FILE_ACTION_MODIFIED => {
                    let kind = EventKind::Modify(ModifyKind::Any);
                    events.push(newe.set_kind(kind));
                }
                _ => (),
            }
        }

        if cur_entry.NextEntryOffset == 0 {
            break;
        }
        cur_offset = unsafe { cur_offset.add(cur_entry.NextEntryOffset as usize) };
        cur_entry = unsafe { ptr::read_unaligned(cur_offset.cast::<FILE_NOTIFY_INFORMATION>()) };
    }

    tracing::trace!(
        ?remove_paths,
        "processing ReadDirectoryChangesW watch changes",
    );

    for path in remove_paths {
        let is_no_track = {
            request
                .data
                .watches
                .borrow()
                .get(&path)
                .is_some_and(|mode| mode.target_mode == TargetMode::NoTrack)
        };
        if is_no_track {
            request.data.watches.borrow_mut().remove(&path);
        }
    }

    if (!changed.is_empty() || !events.is_empty())
        && let Err(e) = action_tx.send(Action::Report { changed, events })
    {
        tracing::error!(?e, "failed to send Report action");
    }
}

/// Watcher implementation based on ReadDirectoryChanges
#[derive(Debug)]
pub struct ReadDirectoryChangesWatcher {
    tx: Sender<Action>,
    cmd_rx: Receiver<Result<PathBuf>>,
    wakeup_sem: HANDLE,
}

impl ReadDirectoryChangesWatcher {
    pub fn create(
        event_handler: Arc<Mutex<dyn EventHandler>>,
    ) -> Result<ReadDirectoryChangesWatcher> {
        let (cmd_tx, cmd_rx) = unbounded();

        let wakeup_sem = unsafe { CreateSemaphoreW(ptr::null_mut(), 0, 1, ptr::null_mut()) };
        if wakeup_sem.is_null() || wakeup_sem == INVALID_HANDLE_VALUE {
            return Err(Error::generic("Failed to create wakeup semaphore."));
        }

        let action_tx = ReadDirectoryChangesServer::start(event_handler, cmd_tx, wakeup_sem);

        Ok(ReadDirectoryChangesWatcher {
            tx: action_tx,
            cmd_rx,
            wakeup_sem,
        })
    }

    fn wakeup_server(&mut self) {
        // breaks the server out of its wait state.  right now this is really just an optimization,
        // so that if you add a watch you don't block for 100ms in watch() while the
        // server sleeps.
        unsafe {
            ReleaseSemaphore(self.wakeup_sem, 1, ptr::null_mut());
        }
    }

    fn send_action_require_ack(&mut self, action: Action, pb: &Path) -> Result<()> {
        self.tx
            .send(action)
            .map_err(|_| Error::generic("Error sending to internal channel"))?;

        // wake 'em up, we don't want to wait around for the ack
        self.wakeup_server();

        let ack_pb = self
            .cmd_rx
            .recv()
            .map_err(|_| Error::generic("Error receiving from command channel"))??;

        if pb == ack_pb.as_path() {
            Ok(())
        } else {
            Err(Error::generic(&format!(
                "Expected ack for {} but got \
                 ack for {}",
                pb.display(),
                ack_pb.display()
            )))
        }
    }

    fn watch_inner(&mut self, path: &Path, watch_mode: WatchMode) -> Result<()> {
        let pb = if path.is_absolute() {
            path.to_owned()
        } else {
            let p = env::current_dir().map_err(Error::io)?;
            p.join(path)
        };
        self.send_action_require_ack(Action::Watch(pb.clone(), watch_mode), &pb)
    }

    fn unwatch_inner(&mut self, path: &Path) -> Result<()> {
        let pb = if path.is_absolute() {
            path.to_owned()
        } else {
            let p = env::current_dir().map_err(Error::io)?;
            p.join(path)
        };
        let res = self
            .tx
            .send(Action::Unwatch(pb))
            .map_err(|_| Error::generic("Error sending to internal channel"));
        self.wakeup_server();
        res
    }
}

/// Batched [`PathsMut`] implementation for the Windows backend.
///
/// `add` and `remove` only stage the change in a local `Vec`; nothing crosses
/// the channel until `commit`, at which point the server applies the staged
/// changes in order and runs consolidation once. On error the first error
/// is returned: a path that could not be added, or whose directory could not
/// be opened, is left as it was, and the other changes stay applied.
struct WindowsPathsMut<'a> {
    watcher: &'a mut ReadDirectoryChangesWatcher,
    staged: Vec<StagedChange>,
}

impl WindowsPathsMut<'_> {
    fn absolutize(path: &Path) -> Result<PathBuf> {
        if path.is_absolute() {
            Ok(path.to_owned())
        } else {
            let cwd = env::current_dir().map_err(Error::io)?;
            Ok(cwd.join(path))
        }
    }
}

impl PathsMut for WindowsPathsMut<'_> {
    #[tracing::instrument(level = "debug", skip(self))]
    fn add(&mut self, path: &Path, watch_mode: WatchMode) -> Result<()> {
        let pb = Self::absolutize(path)?;
        self.staged.push(StagedChange::Add(pb, watch_mode));
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn remove(&mut self, path: &Path) -> Result<()> {
        let pb = Self::absolutize(path)?;
        self.staged.push(StagedChange::Remove(pb));
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn commit(self: Box<Self>) -> Result<()> {
        let WindowsPathsMut { watcher, staged } = *self;
        if staged.is_empty() {
            return Ok(());
        }
        let (tx, rx) = bounded(1);
        watcher
            .tx
            .send(Action::StageAndCommit(staged, tx))
            .map_err(|_| Error::generic("Error sending to internal channel"))?;
        watcher.wakeup_server();
        rx.recv()
            .map_err(|_| Error::generic("Error receiving from commit channel"))?
    }
}

impl Watcher for ReadDirectoryChangesWatcher {
    #[tracing::instrument(level = "debug", skip(event_handler))]
    #[expect(clippy::used_underscore_binding)]
    fn new<F: EventHandler>(event_handler: F, _config: Config) -> Result<Self> {
        let event_handler = Arc::new(Mutex::new(event_handler));
        Self::create(event_handler)
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn watch(&mut self, path: &Path, watch_mode: WatchMode) -> Result<()> {
        self.watch_inner(path, watch_mode)
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn unwatch(&mut self, path: &Path) -> Result<()> {
        self.unwatch_inner(path)
    }

    fn paths_mut<'me>(&'me mut self) -> Box<dyn PathsMut + 'me> {
        Box::new(WindowsPathsMut {
            watcher: self,
            staged: Vec::new(),
        })
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn configure(&mut self, config: Config) -> Result<bool> {
        let (tx, rx) = bounded(1);
        self.tx.send(Action::Configure(config, tx))?;
        rx.recv()?
    }

    fn kind() -> crate::WatcherKind {
        WatcherKind::ReadDirectoryChangesWatcher
    }

    #[cfg(test)]
    fn get_watch_handles(&self) -> HashSet<PathBuf> {
        let (tx, rx) = bounded(1);
        self.tx.send(Action::GetWatchHandles(tx)).unwrap();
        rx.recv().unwrap()
    }
}

impl Drop for ReadDirectoryChangesWatcher {
    fn drop(&mut self) {
        let result = self.tx.send(Action::Stop);
        if let Err(e) = result {
            tracing::error!(?e, "failed to send Stop action");
        }
        // better wake it up
        self.wakeup_server();
    }
}

// `ReadDirectoryChangesWatcher` is not Send/Sync because of the semaphore Handle.
// As said elsewhere it's perfectly safe to send it across threads.
unsafe impl Send for ReadDirectoryChangesWatcher {}
// Because all public methods are `&mut self` it's also perfectly safe to share references.
unsafe impl Sync for ReadDirectoryChangesWatcher {}

#[cfg(test)]
pub mod tests {
    use crate::{
        Error, ErrorKind, ReadDirectoryChangesWatcher, RecursiveMode, TargetMode, WatchMode,
        Watcher, event::EventKind, test::*, windows::normalize_path_separators,
    };

    use std::{
        collections::HashSet,
        ffi::OsString,
        os::windows::ffi::OsStringExt,
        path::{Path, PathBuf},
        sync::Mutex,
        time::Duration,
    };

    fn watcher() -> (TestWatcher<ReadDirectoryChangesWatcher>, Receiver) {
        channel()
    }

    /// The directories the tests make fail, as if their permissions denied it: their handle
    /// cannot be opened, and the ones that are not only failing to open cannot be examined
    /// either.
    static FAILING_DIRS: Mutex<Vec<(PathBuf, /* open_only */ bool)>> = Mutex::new(Vec::new());

    pub(super) fn examine_fails(path: &Path) -> bool {
        FAILING_DIRS.lock().is_ok_and(|dirs| {
            dirs.iter()
                .any(|(dir, open_only)| dir == path && !*open_only)
        })
    }

    pub(super) fn open_fails(path: &Path) -> bool {
        FAILING_DIRS
            .lock()
            .is_ok_and(|dirs| dirs.iter().any(|(dir, _)| dir == path))
    }

    /// A directory the watcher fails on until this is dropped.
    struct FailingDir(PathBuf);

    impl FailingDir {
        fn new(path: &Path) -> Self {
            Self::fail(path, false)
        }

        /// A directory that can be examined, but whose handle cannot be opened, as when its
        /// permissions deny listing it.
        fn open_only(path: &Path) -> Self {
            Self::fail(path, true)
        }

        fn fail(path: &Path, open_only: bool) -> Self {
            FAILING_DIRS
                .lock()
                .unwrap()
                .push((path.to_path_buf(), open_only));
            Self(path.to_path_buf())
        }
    }

    impl Drop for FailingDir {
        fn drop(&mut self) {
            if let Ok(mut dirs) = FAILING_DIRS.lock() {
                dirs.retain(|(dir, _)| dir != &self.0);
            }
        }
    }

    /// What the tests do right before the watcher opens the handle of a directory, once: what
    /// appears in the directory then is not reported by the handle.
    static BEFORE_OPEN: Mutex<Vec<(PathBuf, OpenAction)>> = Mutex::new(Vec::new());

    type OpenAction = Box<dyn FnOnce() + Send>;

    pub(super) fn before_open(dir: &Path) {
        let action = BEFORE_OPEN.lock().ok().and_then(|mut actions| {
            let index = actions.iter().position(|(path, _)| path == dir)?;
            Some(actions.swap_remove(index).1)
        });
        if let Some(action) = action {
            action();
        }
    }

    /// Runs `action` right before the watcher opens the handle of `dir` the next time.
    fn when_opening(dir: &Path, action: impl FnOnce() + Send + 'static) {
        BEFORE_OPEN
            .lock()
            .unwrap()
            .push((dir.to_path_buf(), Box::new(action)));
    }

    /// Waits for the watcher to report an error; a creation of `root` before it fails the test.
    fn wait_error_instead_of_create(rx: &Receiver, root: &Path) -> Error {
        loop {
            match rx.try_recv() {
                Ok(Err(error)) => return error,
                Ok(Ok(event)) => assert!(
                    !matches!(event.kind, EventKind::Create(_))
                        || !event.paths.iter().any(|path| path == root),
                    "a root that is not watched was reported as created: {event:?}"
                ),
                Err(e) => panic!("no error from the watcher: {e:?}"),
            }
        }
    }

    #[test]
    fn trash_dir() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = testdir();
        let child_dir = dir.path().join("child");
        std::fs::create_dir(&child_dir)?;

        let mut watcher = crate::recommended_watcher(|_| {
            // Do something with the event
        })?;
        watcher.watch(&child_dir, WatchMode::non_recursive())?;
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([dir.to_path_buf(), child_dir.clone()])
        );

        trash::delete(&child_dir)?;

        watcher.watch(dir.path(), WatchMode::non_recursive())?;
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([dir.parent_path_buf(), dir.to_path_buf()])
        );

        Ok(())
    }

    #[test]
    fn watcher_is_send_and_sync() {
        fn check<T: Send + Sync>() {}
        check::<ReadDirectoryChangesWatcher>();
    }

    #[test]
    fn normalize_joined_event_path_for_posix_watch_path() {
        let dir = PathBuf::from("G:/Feature");
        let raw_event_name: Vec<u16> = "22.mp4".encode_utf16().collect();
        let relative = PathBuf::from(OsString::from_wide(&raw_event_name));
        let path = normalize_path_separators(dir.join(relative));

        assert_eq!(path, PathBuf::from(r"G:\Feature\22.mp4"));
    }

    #[test]
    fn normalize_path_separators_keeps_windows_namespace_prefix() {
        let path = PathBuf::from(r"\\?\C:/very/long/file");
        let normalized = normalize_path_separators(path);
        assert_eq!(normalized, PathBuf::from(r"\\?\C:\very\long\file"));
    }

    #[test]
    fn create_file_normalized() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();
        let tmpdir_without_prefix =
            PathBuf::from(tmpdir.path().to_str().unwrap().replace("\\\\?\\", ""));
        let tmpdir_normalized =
            PathBuf::from(tmpdir_without_prefix.to_str().unwrap().replace('\\', "/"));
        watcher.watch_recursively(&tmpdir_normalized);

        let path = tmpdir_without_prefix.join("entry");
        std::fs::File::create_new(&path).expect("create");

        let event = rx.recv();
        assert_eq!(event.paths.len(), 1);
        assert_eq!(event.paths[0], path);
        assert_eq!(event.paths[0].to_str().unwrap(), path.to_str().unwrap());
    }

    #[test]
    fn create_file() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();
        watcher.watch_recursively(&tmpdir);

        let path = tmpdir.path().join("entry");
        std::fs::File::create_new(&path).expect("create");

        rx.wait_ordered_exact([expected(&path).create_any()])
            .ensure_no_tail();
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

        rx.wait_ordered_exact([expected(&path).create_any()])
            .ensure_no_tail();
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

        std::fs::create_dir_all(path.parent().unwrap()).expect("create");
        std::fs::File::create_new(&path).expect("create");

        // Reported by the parent once it is watched, or by the watcher if the file is there by
        // then; the kind differs.
        rx.wait_ordered([expected(&path).create()]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.path().join("entry")])
        );
    }

    #[test]
    fn create_self_file_below_missing_ancestors() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let parent = tmpdir.path().join("entry").join("deeper");
        let path = parent.join("nested");

        watcher.watch_nonrecursively(&path);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));

        // Each directory that appears on the way gets a handle in turn, although no root changes
        // yet; without it, `deeper` and the file would go unseen.
        std::fs::create_dir_all(&parent).expect("create_dir_all");
        std::fs::File::create_new(&path).expect("create");

        // Reported by the parent once it is watched, or by the watcher if the file is there by
        // then; the kind differs.
        rx.wait_ordered([expected(&path).create()]);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([parent]));
    }

    #[test]
    fn track_path_reports_a_root_that_appears_while_its_parent_is_opened() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let entry = tmpdir.path().join("entry");
        let deeper = entry.join("deeper");
        let path = deeper.join("nested");
        watcher.watch_nonrecursively(&path);

        // Each appears after the watcher looked for it, before the handle of its parent reads, so
        // no handle reports it: the looks after the handles opened find them.
        when_opening(&entry, {
            let deeper = deeper.clone();
            move || std::fs::create_dir(&deeper).expect("create_dir")
        });
        when_opening(&deeper, {
            let path = path.clone();
            move || drop(std::fs::File::create_new(&path).expect("create"))
        });
        std::fs::create_dir(&entry).expect("create_dir");

        rx.wait_ordered_exact([expected(&path).create_file()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([deeper]));
    }

    #[test]
    fn watch_fails_below_an_ancestor_that_cannot_be_examined() {
        let tmpdir = testdir();
        let (mut watcher, _rx) = watcher();

        let locked = tmpdir.path().join("locked");
        let path = locked.join("inner").join("f.js");
        std::fs::create_dir_all(locked.join("inner")).expect("create_dir_all");
        let _failing = FailingDir::new(&locked);

        let error = watcher
            .watcher
            .watch(&path, WatchMode::non_recursive())
            .expect_err("watch below an ancestor that cannot be examined");
        assert!(matches!(error.kind, ErrorKind::Io(_)), "{error:?}");
        assert_eq!(error.paths, vec![locked]);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));

        // The path was not added, so it does not fail the next watch.
        let other = tmpdir.path().join("other.js");
        std::fs::write(&other, "1").expect("write");
        watcher.watch_nonrecursively(&other);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()])
        );
    }

    #[test]
    fn a_root_whose_directory_cannot_be_opened_is_not_watched() {
        let tmpdir = testdir();
        let other_dir = testdir();
        let (mut watcher, rx) = watcher();

        let locked = tmpdir.path().join("locked");
        let a = locked.join("a.js");
        std::fs::create_dir(&locked).expect("create_dir");
        std::fs::write(&a, "1").expect("write");
        let b = other_dir.path().join("b.js");
        let c = other_dir.path().join("c.js");
        let d = other_dir.path().join("d.js");
        std::fs::write(&b, "1").expect("write");
        std::fs::write(&c, "1").expect("write");
        std::fs::write(&d, "1").expect("write");
        let failing = FailingDir::open_only(&locked);

        let error = watcher
            .watcher
            .watch(&a, WatchMode::non_recursive())
            .expect_err("watch a file in a directory that cannot be opened");
        assert_eq!(error.paths, vec![locked.clone()]);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));

        // It is not kept, so it does not fail the next watch, and fails only its own add in a
        // commit.
        watcher.watch_nonrecursively(&b);
        let mut paths = watcher.watcher.paths_mut();
        paths.add(&a, WatchMode::non_recursive()).expect("add");
        paths.add(&c, WatchMode::non_recursive()).expect("add");
        let error = paths.commit().expect_err("commit");
        assert_eq!(error.paths, vec![locked]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([other_dir.to_path_buf()])
        );

        // Nor is it watched once its directory can be opened.
        drop(failing);
        watcher.watch_nonrecursively(&d);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([other_dir.to_path_buf()])
        );

        std::fs::write(&c, "2").expect("write");
        rx.wait_ordered_exact([expected(&c).modify_any().multiple()])
            .ensure_no_tail();
    }

    #[test]
    fn an_ancestor_that_could_not_be_opened_is_tried_again_once_it_comes_back() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let ancestor = tmpdir.path().join("ancestor");
        let parent = ancestor.join("parent");
        let path = parent.join("x.js");
        // A root next to `ancestor`: the handle that reports it changing also reports `ancestor`
        // coming and going, so its change arrives once the server acted on those.
        let marker = tmpdir.path().join("marker");
        std::fs::create_dir(&ancestor).expect("create_dir");
        std::fs::create_dir(&marker).expect("create_dir");
        watcher.watch_nonrecursively(&marker);

        let failing = FailingDir::open_only(&ancestor);
        watcher.watch_nonrecursively(&path);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf(), marker.clone()])
        );

        // The directory created in its place can be opened.
        std::fs::remove_dir(&ancestor).expect("remove_dir");
        drop(failing);
        std::fs::create_dir(&ancestor).expect("create_dir");
        let mut permissions = std::fs::metadata(&marker).expect("metadata").permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&marker, permissions).expect("set_permissions");
        rx.wait_ordered_exact([expected(&marker).modify_any()])
            .ensure_no_tail();

        // The new directory has a handle, which sees the parent of the file appear.
        std::fs::create_dir(&parent).expect("create_dir");
        assert!(
            rx.sleep_until(|| {
                watcher.get_watch_handles()
                    == HashSet::from([tmpdir.to_path_buf(), marker.clone(), parent.clone()])
            }),
            "the parent of the file got no handle: {:?}",
            watcher.get_watch_handles()
        );
        std::fs::File::create_new(&path).expect("create");
        rx.wait_ordered_exact([expected(&path).create_any()])
            .ensure_no_tail();
    }

    #[test]
    fn an_ancestor_that_cannot_be_examined_later_does_not_fail_other_watches() {
        let tmpdir = testdir();
        let other_dir = testdir();
        let (mut watcher, rx) = watcher();

        // `lib` is the parent that sees `dir` come and go, so its handle shows; the one of
        // `tmpdir` is only on the way.
        let lib = tmpdir.path().join("lib");
        let dir = lib.join("dir");
        let moved = tmpdir.path().join("moved");
        std::fs::create_dir_all(&dir).expect("create_dir_all");
        watcher.watch_nonrecursively(&dir);

        let failing_tmpdir = FailingDir::new(tmpdir.path());
        let failing_lib = FailingDir::new(&lib);
        let b = other_dir.path().join("b.js");
        let c = other_dir.path().join("c.js");
        std::fs::write(&b, "1").expect("write");
        std::fs::write(&c, "1").expect("write");
        watcher.watch_nonrecursively(&b);
        let mut paths = watcher.watcher.paths_mut();
        paths.add(&c, WatchMode::non_recursive()).expect("add");
        paths.commit().expect("commit");

        // The handles that were open stay open.
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([lib.clone(), dir.clone(), other_dir.to_path_buf()])
        );
        drop(failing_tmpdir);
        drop(failing_lib);
        std::fs::rename(&lib, &moved).expect("rename away");
        rx.wait_ordered_exact([expected(&dir).remove_any()])
            .ensure_no_tail();
    }

    #[test]
    fn track_path_reports_an_error_for_a_root_that_appears_but_cannot_be_watched() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let lib = tmpdir.path().join("lib");
        let dir = lib.join("dir");
        let moved = tmpdir.path().join("moved");
        std::fs::create_dir_all(&dir).expect("create_dir_all");
        watcher.watch_nonrecursively(&dir);

        std::fs::rename(&lib, &moved).expect("rename away");
        rx.wait_ordered([expected(&dir).remove_any()]);

        let failing = FailingDir::new(&dir);
        std::fs::rename(&moved, &lib).expect("rename back");
        let error = wait_error_instead_of_create(&rx, &dir);
        assert_eq!(error.paths, vec![dir.clone()]);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([lib.clone()]));

        // Watching it again opens it once it can be opened, and reports it.
        drop(failing);
        watcher.watch_nonrecursively(&dir);
        rx.wait_ordered_exact([expected(&dir).create_folder()])
            .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([lib, dir.clone()])
        );
        let file = dir.join("file");
        std::fs::File::create_new(&file).expect("create");
        rx.wait_ordered([expected(&file).create_any()]);
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

        std::fs::rename(&lib, &moved).expect("rename away");
        rx.wait_unordered([expected(&a).remove_any(), expected(&b).remove_any()]);

        std::fs::rename(&moved, &lib).expect("rename back");
        rx.wait_unordered([expected(&a).create_file(), expected(&b).create_file()]);

        std::fs::write(&a, "2").expect("write");
        rx.wait_unordered([expected(&a).modify_any()]);
    }

    #[test]
    fn track_path_watches_a_directory_root_once_it_appears() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let dir = tmpdir.path().join("dir");
        watcher.watch_recursively(&dir);

        std::fs::create_dir(&dir).expect("create_dir");
        rx.wait_unordered([expected(&dir).create()]);

        std::fs::File::create_new(dir.join("file")).expect("create");
        rx.wait_unordered([expected(dir.join("file")).create()]);
    }

    #[test]
    fn write_file() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        std::fs::File::create_new(&path).expect("create");

        watcher.watch_recursively(&tmpdir);
        std::fs::write(&path, b"123").expect("write");

        rx.wait_ordered_exact([expected(&path).modify_any().multiple()])
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

        rx.wait_ordered_exact([expected(&path).modify_any()])
            .ensure_no_tail();
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
            expected(&path).rename_from(),
            expected(&new_path).rename_to(),
            expected(tmpdir.path()).modify_any(),
        ])
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

        rx.wait_ordered_exact([expected(&path).rename_to(), expected(&path).modify_any()])
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
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()])
        );

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

        rx.wait_ordered_exact([expected(&file).remove_any()])
            .ensure_no_tail();
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

        rx.wait_ordered_exact([expected(&file).remove_any()]);
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()])
        );

        std::fs::write(&file, "").expect("write");

        rx.wait_ordered_exact([expected(&file).create_any()]);
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

        rx.wait_ordered_exact([expected(&file).remove_any()]);
        // TODO: can remove from watch, but currently not removed
        // assert_eq!(watcher.get_watch_handles(), HashSet::from([]));

        // std::fs::write(&file, "").expect("write");

        // rx.ensure_empty_with_wait();
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
            expected(&overwriting_file).create_any(),
            expected(tmpdir.path()).modify_any(),
            expected(&overwriting_file).modify_any().multiple(),
            expected(&overwritten_file).remove_any(),
            expected(tmpdir.path()).modify_any().optional(),
            expected(&overwriting_file).rename_from(),
            expected(&overwritten_file).rename_to(),
            expected(tmpdir.path()).modify_any().optional(),
        ])
        .ensure_no_tail();
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

        rx.wait_ordered_exact([
            expected(&overwritten_file).remove_any(),
            expected(&overwritten_file).rename_to(),
        ])
        .ensure_no_tail();
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

        std::fs::rename(&watched_file, &moved_file).expect("move watched file");
        std::fs::copy(&moved_file, &watched_file).expect("recreate watched file");
        std::fs::remove_file(&moved_file).expect("remove moved file");

        // Wait until the replacement events are drained before checking the next write.
        for _ in rx.iter() {}

        std::fs::write(&watched_file, "updated").expect("update watched file");
        let received_change = rx.iter().any(|event| {
            event.paths.iter().any(|path| path == &watched_file)
                && matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_))
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

        rx.wait_ordered_exact([expected(&overwritten_file).remove_any()])
            .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()]) // TODO: can remove from watch, but currently not removed
        );
    }

    #[test]
    fn create_dir() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();
        watcher.watch_recursively(&tmpdir);

        let path = tmpdir.path().join("entry");
        std::fs::create_dir(&path).expect("create");

        rx.wait_ordered_exact([expected(&path).create_any()])
            .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf()])
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

        rx.wait_ordered_exact([expected(&path).modify_any()])
            .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf()])
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
            expected(&path).rename_from(),
            expected(&new_path).rename_to(),
            expected(tmpdir.path()).modify_any(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf()])
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

        rx.wait_ordered_exact([expected(&path).remove_any()])
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

        rx.wait_ordered_exact([expected(&path).remove_any()])
            .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()])
        );

        std::fs::create_dir(&path).expect("create_dir2");

        rx.wait_ordered_exact([expected(&path).create_any()])
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

        rx.wait_ordered_exact([expected(&path).remove_any()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([]));

        std::fs::create_dir(&path).expect("create_dir2");

        rx.ensure_empty_with_wait();
    }

    #[test]
    fn delete_self_dir_no_track_stays_unwatched_when_another_handle_reports() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let gone = tmpdir.path().join("gone");
        let other = tmpdir.path().join("other");
        std::fs::create_dir(&gone).expect("create_dir");
        std::fs::create_dir(&other).expect("create_dir");
        let no_track = WatchMode {
            recursive_mode: RecursiveMode::Recursive,
            target_mode: TargetMode::NoTrack,
        };
        watcher.watch(&gone, no_track);
        watcher.watch(&other, no_track);

        std::fs::remove_dir(&gone).expect("remove");
        rx.wait_ordered_exact([expected(&gone).remove_any()])
            .ensure_no_tail();
        std::fs::create_dir(&gone).expect("create_dir2");

        // The server acts on the report of the other handle after it dropped the one of `gone`.
        // That the dropped handle serves no tracked path only spares that report a rebuild, which
        // would find nothing to change.
        let file = other.join("file");
        std::fs::File::create_new(&file).expect("create");
        rx.wait_ordered_exact([expected(&file).create_any()])
            .ensure_no_tail();
        assert_eq!(watcher.get_watch_handles(), HashSet::from([other.clone()]));

        // The watch of `gone` stopped with its directory, so the next watch does not open the new
        // one either.
        let third = tmpdir.path().join("third");
        std::fs::create_dir(&third).expect("create_dir");
        watcher.watch(&third, no_track);
        assert_eq!(watcher.get_watch_handles(), HashSet::from([other, third]));

        std::fs::File::create_new(gone.join("file")).expect("create");
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
            expected(&path).rename_from(),
            expected(&new_path).rename_to(),
            expected(tmpdir.path()).modify_any(),
            expected(&new_path).rename_from(),
            expected(&new_path2).rename_to(),
            expected(tmpdir.path()).modify_any(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf()])
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

        rx.wait_ordered_exact([expected(path).remove_any()])
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
            expected(&file1).create_any(),
            expected(&file1).modify_any().multiple(),
            expected(tmpdir.path()).modify_any(),
            expected(&file2).modify_any().multiple(),
            expected(&file1).rename_from(),
            expected(&new_path).rename_to(),
            expected(tmpdir.path()).modify_any(),
            expected(&new_path).modify_any().multiple(),
            expected(&new_path).remove_any(),
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
            expected(&path).rename_from(),
            expected(&new_path1).rename_to(),
            expected(tmpdir.path()).modify_any(),
            expected(&new_path1).rename_from(),
            expected(&new_path2).rename_to(),
            expected(tmpdir.path()).modify_any(),
        ])
        .ensure_no_tail();
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

        rx.wait_ordered_exact([expected(&path).modify_any()])
            .ensure_no_tail();
    }

    #[test]
    fn write_file_non_recursive_watch() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        std::fs::File::create_new(&path).expect("create");

        watcher.watch_nonrecursively(&path);

        std::fs::write(&path, b"123").expect("write");

        rx.wait_ordered_exact([expected(&path).modify_any().multiple()])
            .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()])
        );
    }

    #[test]
    fn write_to_a_hardlink_pointed_to_the_file_in_the_watched_dir_doesnt_trigger_an_event() {
        let tmpdir = testdir();
        let (mut watcher, mut rx) = watcher();

        let subdir = tmpdir.path().join("subdir");
        let subdir2 = tmpdir.path().join("subdir2");
        let file = subdir.join("file");
        let hardlink = subdir2.join("hardlink");

        std::fs::create_dir(&subdir).expect("create");
        std::fs::create_dir(&subdir2).expect("create");
        std::fs::write(&file, "").expect("file");
        std::fs::hard_link(&file, &hardlink).expect("hardlink");

        watcher.watch_nonrecursively(&file);

        std::fs::write(&hardlink, "123123").expect("write to the hard link");

        let events = rx.iter().collect::<Vec<_>>();
        assert!(events.is_empty(), "unexpected events: {events:#?}");
        assert_eq!(watcher.get_watch_handles(), HashSet::from([subdir]));
    }

    #[test]
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
        rx.wait_ordered_exact([
            expected(&nested1).create_any(),
            expected(&nested2).create_any(),
            expected(&nested3).create_any(),
            expected(&nested4).create_any(),
            expected(&nested5).create_any(),
            expected(&nested6).create_any(),
            expected(&nested7).create_any(),
            expected(&nested8).create_any(),
            expected(&nested9).create_any(),
        ])
        .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.parent_path_buf(), tmpdir.to_path_buf()])
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

        watcher.watch_recursively(&path);
        std::fs::File::create_new(&file).expect("create");

        rx.wait_ordered_exact([expected(&deep).modify_any(), expected(&file).create_any()])
            .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf(), path])
        );
    }

    /// Watching 10+ sibling subdirs collapses their OS-level watch into the
    /// shared parent dir (the consolidation threshold defined on
    /// [`ConsolidatingPathTrie`] is 10).
    #[test]
    fn consolidate_many_siblings() {
        let tmpdir = testdir();
        let (mut watcher, _rx) = watcher();

        let mut subdirs = Vec::new();
        for i in 0..10 {
            let sub = tmpdir.path().join(format!("c{i}"));
            std::fs::create_dir(&sub).expect("create_dir");
            subdirs.push(sub);
        }
        let mut pm = watcher.watcher.paths_mut();
        for sub in &subdirs {
            pm.add(sub, WatchMode::recursive()).expect("paths_mut add");
        }
        pm.commit().expect("paths_mut commit");

        // Consolidation collapses the 10 sibling watches to a single recursive
        // watch on `tmpdir`. The 10 user-level `tracked_parent` entries all
        // point at `tmpdir` and so are absorbed by the consolidated primary;
        // no separate handle is opened on `tmpdir.parent_path_buf()`.
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()])
        );
    }

    /// After consolidation, events for files created inside each child dir are
    /// still delivered through the consolidated parent watch.
    #[test]
    fn consolidate_delivers_child_events() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let mut subdirs = Vec::new();
        for i in 0..10 {
            let sub = tmpdir.path().join(format!("c{i}"));
            std::fs::create_dir(&sub).expect("create_dir");
            subdirs.push(sub);
        }
        let mut pm = watcher.watcher.paths_mut();
        for sub in &subdirs {
            pm.add(sub, WatchMode::recursive()).expect("paths_mut add");
        }
        pm.commit().expect("paths_mut commit");

        // Create a file inside one of the consolidated child dirs; the event
        // must still arrive even though no OS handle sits directly on `c5`.
        let file = subdirs[5].join("f");
        std::fs::File::create_new(&file).expect("create");
        rx.wait_ordered_exact([expected(&file).create_any()])
            .ensure_no_tail();
    }

    /// Mixing recursive and non-recursive watches under a shared parent
    /// consolidates them into a recursive parent watch. Events deep inside
    /// the recursive child reach the user; events deeper than 1 level inside
    /// a non-recursive child are filtered out.
    #[test]
    fn mixed_recursive_consolidates_to_recursive() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let mut subdirs = Vec::new();
        for i in 0..10 {
            let sub = tmpdir.path().join(format!("c{i}"));
            std::fs::create_dir(&sub).expect("create_dir");
            subdirs.push(sub);
        }
        let recursive_child = subdirs[0].clone();
        let nonrecursive_child = subdirs[1].clone();
        let deep_under_rec = recursive_child.join("deep");
        std::fs::create_dir(&deep_under_rec).expect("create_dir");
        let deep_under_nonrec = nonrecursive_child.join("deep");
        std::fs::create_dir(&deep_under_nonrec).expect("create_dir");

        let mut pm = watcher.watcher.paths_mut();
        for (i, sub) in subdirs.iter().enumerate() {
            let mode = if i == 0 {
                WatchMode::recursive()
            } else {
                WatchMode::non_recursive()
            };
            pm.add(sub, mode).expect("paths_mut add");
        }
        pm.commit().expect("paths_mut commit");

        // File 1: under the recursive child
        let file_under_rec = deep_under_rec.join("f");
        std::fs::File::create_new(&file_under_rec).expect("create");

        // File 2: under the non-recursive child
        let file_under_nonrec = deep_under_nonrec.join("f");
        std::fs::File::create_new(&file_under_nonrec).expect("create");

        // We expect the deep-recursive file event but NOT the deep-nonrec one.
        rx.wait_ordered_exact([expected(&file_under_rec).create_any()])
            .ensure_no_tail();
    }
}
