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
use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;
use std::env;
use std::ffi::OsString;
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

/// A user-registered watch request, paired with its resolved OS-level coverage.
///
/// `mode` preserves the user's intent. `primary` is the directory we'd open with
/// `CreateFileW` to receive content events for this watch, plus whether the user
/// asked for a recursive subtree. `tracked_parent` is an optional extra directory
/// whose direct children include `primary` (or the user path itself, for
/// nonexistent + [`TargetMode::TrackPath`]) — its watch lets us detect rename or
/// delete events for the watched path itself.
#[derive(Debug, Clone)]
struct UserWatch {
    mode: WatchMode,
    primary: Option<(PathBuf, bool)>,
    tracked_parent: Option<PathBuf>,
}

#[derive(Clone)]
struct ReadData {
    dir: PathBuf, // directory that is being watched
    watches: Rc<RefCell<HashMap<PathBuf, UserWatch, FxBuildHasher>>>,
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
    watches: Rc<RefCell<HashMap<PathBuf, UserWatch, FxBuildHasher>>>,
    watch_handles: HashMap<PathBuf, (WatchState, /* is_recursive */ bool), FxBuildHasher>,
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
                        watch_handles: HashMap::default(),
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
                        let handles = self.watch_handles.keys().cloned().collect();
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
        let merged_mode = self.merge_user_watch_mode(&path, watch_mode);
        // On Windows, reading metadata on a directory *before* its parent dir
        // has a `ReadDirectoryChangesW` watch active causes the parent watch
        // to silently miss any future `FILE_ACTION_MODIFIED` event for that
        // dir (the metadata call appears to "consume" the dir's modification
        // slot). To preserve event delivery, pre-open the tracked-parent
        // watch before resolving metadata, matching the order the previous
        // per-call code used.
        self.pre_open_tracked_parent(&path, merged_mode)?;
        let user_watch = resolve_user_watch(&path, merged_mode)?;
        self.watches.borrow_mut().insert(path.clone(), user_watch);
        self.rebuild_watch_handles()?;
        Ok(path)
    }

    /// Open a non-recursive watch on `path.parent()` if `mode` is in
    /// [`TargetMode::TrackPath`] and no handle exists yet. The only input
    /// needed is the user path itself, so this can run before any metadata
    /// resolution and is safe to use to avoid the Windows quirk described in
    /// [`add_watch`].
    fn pre_open_tracked_parent(&mut self, path: &Path, mode: WatchMode) -> Result<()> {
        if mode.target_mode != TargetMode::TrackPath {
            return Ok(());
        }
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        if self.watch_handles.contains_key(parent) {
            return Ok(());
        }
        self.add_watch_raw(parent.to_path_buf(), false, false)
    }

    /// Merge an incoming watch_mode with any existing entry for `path`, so that
    /// repeated calls to watch the same path only ever upgrade coverage.
    fn merge_user_watch_mode(&self, path: &Path, watch_mode: WatchMode) -> WatchMode {
        match self.watches.borrow().get(path) {
            Some(existing) => {
                let mut merged = existing.mode;
                merged.upgrade_with(watch_mode);
                merged
            }
            None => watch_mode,
        }
    }

    fn apply_staged(&mut self, staged: Vec<StagedChange>) -> Result<()> {
        let mut first_error: Option<Error> = None;
        for change in staged {
            let res = match change {
                StagedChange::Add(path, mode) => {
                    let merged = self.merge_user_watch_mode(&path, mode);
                    // Pre-open `tracked_parent` before metadata to avoid the
                    // Windows quirk documented on `add_watch`.
                    self.pre_open_tracked_parent(&path, merged).and_then(|()| {
                        resolve_user_watch(&path, merged).map(|uw| {
                            self.watches.borrow_mut().insert(path, uw);
                        })
                    })
                }
                StagedChange::Remove(path) => {
                    self.watches.borrow_mut().remove(&path);
                    Ok(())
                }
            };
            if let Err(e) = res
                && first_error.is_none()
            {
                first_error = Some(e);
            }
        }
        // Even if some staged changes failed to resolve, apply the successful ones
        // so the watcher state remains consistent with `self.watches`.
        if let Err(e) = self.rebuild_watch_handles()
            && first_error.is_none()
        {
            first_error = Some(e);
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Build the desired set of OS-level dirs from `self.watches`, run consolidation
    /// over the *primary* dir requests, and then layer in any [`UserWatch::tracked_parent`]
    /// entries as auxiliary non-recursive watches if they aren't already covered.
    /// Finally diff against `self.watch_handles` to converge open handles.
    ///
    /// `tracked_parent` is deliberately kept outside the consolidation trie:
    /// merging it with the primary would force the primary watch up to the parent
    /// and make it recursive, which floods the event filter with sibling events
    /// that the user never asked about.
    fn rebuild_watch_handles(&mut self) -> Result<()> {
        // 1. Run consolidation over only the primary dir requests.
        let mut trie = ConsolidatingPathTrie::new(true, 0);
        {
            let watches = self.watches.borrow();
            for uw in watches.values() {
                if let Some((dir, _)) = &uw.primary {
                    trie.insert(dir);
                }
            }
        }
        let primary_paths = trie.values();

        // 2. Compute desired recursive flag for each consolidated primary.
        let mut target: HashMap<PathBuf, bool, FxBuildHasher> = {
            let watches = self.watches.borrow();
            primary_paths
                .into_iter()
                .map(|p| {
                    let recursive = compute_recursive_flag(&p, &watches);
                    (p, recursive)
                })
                .collect()
        };

        // 3. Layer in tracked_parent entries. If the parent already appears in
        //    `target` (whether as a consolidated parent or a directly requested
        //    primary), the existing entry is sufficient for rename detection on
        //    direct children. Otherwise add it as a non-recursive auxiliary watch.
        {
            let watches = self.watches.borrow();
            for uw in watches.values() {
                if let Some(parent) = &uw.tracked_parent
                    && !target.contains_key(parent)
                {
                    target.insert(parent.clone(), false);
                }
            }
        }

        // 4. Diff `target` against `self.watch_handles`. Close handles that
        //    shouldn't exist, recreate those whose recursive flag changed, leave
        //    matching handles untouched, and open any new ones.
        let to_remove: Vec<PathBuf> = self
            .watch_handles
            .iter()
            .filter(|(p, (_, is_rec))| target.get(*p).is_none_or(|t| t != is_rec))
            .map(|(p, _)| p.clone())
            .collect();
        for p in to_remove {
            if let Some((ws, _)) = self.watch_handles.remove(&p) {
                stop_watch(&ws);
            }
        }

        // Open new handles in a deterministic order: shallower paths first,
        // ties broken lexically. This matches the previous per-call flow which
        // opened the tracked-parent watch before the primary.
        let mut to_open: Vec<(PathBuf, bool)> = target
            .into_iter()
            .filter(|(p, _)| !self.watch_handles.contains_key(p))
            .collect();
        to_open.sort_by(|(a, _), (b, _)| {
            a.components()
                .count()
                .cmp(&b.components().count())
                .then_with(|| a.cmp(b))
        });

        let mut first_error: Option<Error> = None;
        for (path, is_recursive) in to_open {
            if let Err(e) = self.add_watch_raw(path, is_recursive, false)
                && first_error.is_none()
            {
                first_error = Some(e);
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
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

    #[tracing::instrument(level = "trace", skip(self))]
    fn remove_watch(&mut self, path: &Path) {
        if self.watches.borrow_mut().remove(path).is_some()
            && let Err(e) = self.rebuild_watch_handles()
        {
            tracing::error!(?e, "failed to rebuild watch handles after remove_watch");
        }
    }

    /// Drop a watch handle out-of-band when the OS reports that the directory is
    /// gone (or some other error invalidated it). The handle entry is purged
    /// without consulting `self.watches`, since the corresponding user watch
    /// may still be present and will be re-opened on the next rebuild.
    #[tracing::instrument(level = "trace", skip(self))]
    fn remove_watch_raw(&mut self, path: &Path) {
        if let Some((ws, _)) = self.watch_handles.remove(path) {
            stop_watch(&ws);
        }
    }

    fn configure_raw_mode(_config: Config, tx: &BoundSender<Result<bool>>) {
        tx.send(Ok(false))
            .expect("configuration channel disconnect");
    }
}

/// Resolve a user-supplied watch path + mode into a [`UserWatch`] describing
/// which OS-level directories we'd want to watch. Mirrors the metadata + parent
/// logic that the old per-call `add_watch` used to inline.
fn resolve_user_watch(path: &Path, mode: WatchMode) -> Result<UserWatch> {
    let tracked_parent_for_dir = if mode.target_mode == TargetMode::TrackPath {
        path.parent().map(Path::to_path_buf)
    } else {
        None
    };

    match path.metadata().map_err(Error::io_watch) {
        Ok(meta) => {
            if meta.is_dir() {
                Ok(UserWatch {
                    mode,
                    primary: Some((path.to_path_buf(), mode.recursive_mode.is_recursive())),
                    tracked_parent: tracked_parent_for_dir,
                })
            } else if meta.is_file() {
                let parent = path.parent().unwrap_or(path).to_path_buf();
                Ok(UserWatch {
                    mode,
                    // For files we always have to watch the parent directory anyway,
                    // so the "tracked parent" rename-detection requirement is
                    // already covered by `primary`; no separate entry needed.
                    primary: Some((parent, mode.recursive_mode.is_recursive())),
                    tracked_parent: None,
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
            if mode.target_mode == TargetMode::TrackPath
                && matches!(err.kind, ErrorKind::PathNotFound)
            {
                Ok(UserWatch {
                    mode,
                    primary: None,
                    tracked_parent: tracked_parent_for_dir,
                })
            } else {
                Err(err)
            }
        }
    }
}

/// Decide whether a consolidated OS-level watch on `target_path` must be opened
/// with `bWatchSubtree=1`. The flag is true whenever some user's primary lives
/// strictly below `target_path` (the trie consolidated it, so we need deeper
/// visibility) or matches `target_path` with `Recursive` mode.
fn compute_recursive_flag(
    target_path: &Path,
    watches: &HashMap<PathBuf, UserWatch, FxBuildHasher>,
) -> bool {
    for uw in watches.values() {
        let Some((dir, is_rec)) = &uw.primary else {
            continue;
        };
        if dir == target_path {
            if *is_rec {
                return true;
            }
        } else if dir.starts_with(target_path) {
            return true;
        }
    }
    false
}

/// Returns `true` if an event on `event_path` is covered by some user-registered
/// watch. Walks ancestors of `event_path`: any direct parent or self-match
/// always counts; a deeper recursive ancestor also counts.
fn is_event_covered(
    watches: &HashMap<PathBuf, UserWatch, FxBuildHasher>,
    event_path: &Path,
) -> bool {
    if watches.contains_key(event_path) {
        return true;
    }
    for (depth, ancestor) in event_path.ancestors().enumerate().skip(1) {
        if let Some(uw) = watches.get(ancestor)
            && (depth == 1 || uw.mode.recursive_mode.is_recursive())
        {
            return true;
        }
    }
    false
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

    fn emit_event(event_handler: &Mutex<dyn EventHandler>, res: Result<Event>) {
        if let Ok(mut guard) = event_handler.lock() {
            let f: &mut dyn EventHandler = &mut *guard;
            f.handle_event(res);
        }
    }
    let event_handler = |res| emit_event(&request.event_handler, res);

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
                if request
                    .data
                    .watches
                    .borrow()
                    .get(&dir)
                    .is_some_and(|uw| uw.mode.target_mode == TargetMode::NoTrack)
                {
                    let ev = Event::new(EventKind::Remove(RemoveKind::Any)).add_path(dir);
                    event_handler(Ok(ev));
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
    start_read(
        &request.data,
        Arc::clone(&request.event_handler),
        request.handle,
        request.action_tx,
    );

    let mut remove_paths = vec![];

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

        // Filter events by walking ancestors of `path` against `self.watches`,
        // independent of which OS-level dir produced the event. This is required
        // because watches may be consolidated to a higher parent directory than
        // any user explicitly requested.
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
                    let ev = newe.set_kind(kind);
                    event_handler(Ok(ev));
                }
                FILE_ACTION_RENAMED_NEW_NAME => {
                    let kind = EventKind::Modify(ModifyKind::Name(RenameMode::To));
                    let ev = newe.set_kind(kind);
                    event_handler(Ok(ev));
                }
                FILE_ACTION_ADDED => {
                    let kind = EventKind::Create(CreateKind::Any);
                    let ev = newe.set_kind(kind);
                    event_handler(Ok(ev));
                }
                FILE_ACTION_REMOVED => {
                    remove_paths.push(path.clone());
                    let kind = EventKind::Remove(RemoveKind::Any);
                    let ev = newe.set_kind(kind);
                    event_handler(Ok(ev));
                }
                FILE_ACTION_MODIFIED => {
                    let kind = EventKind::Modify(ModifyKind::Any);
                    let ev = newe.set_kind(kind);
                    event_handler(Ok(ev));
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
                .is_some_and(|uw| uw.mode.target_mode == TargetMode::NoTrack)
        };
        if is_no_track {
            request.data.watches.borrow_mut().remove(&path);
        }
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
/// changes in order and runs consolidation **once**. On error the first error
/// is propagated and the remaining staged operations are skipped at staging
/// time, but any operations that did make it into `self.watches` before the
/// failure remain applied.
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
        Watcher, test::*, windows::normalize_path_separators,
    };

    use std::{
        collections::HashSet, ffi::OsString, os::windows::ffi::OsStringExt, path::PathBuf,
        time::Duration,
    };

    fn watcher() -> (TestWatcher<ReadDirectoryChangesWatcher>, Receiver) {
        channel()
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
    #[ignore = "TODO: not implemented"]
    fn create_self_file_nested() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry/nested");

        watcher.watch_nonrecursively(&path);

        std::fs::create_dir_all(path.parent().unwrap()).expect("create");
        std::fs::File::create_new(&path).expect("create");

        rx.wait_ordered_exact([expected(&path).create_any()])
            .ensure_no_tail();
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()])
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
            expected(tmpdir.path()).modify_any(),
            expected(&path).modify_any().multiple(),
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
            expected(tmpdir.path()).modify_any(),
            expected(&path).modify_any(),
        ])
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
            expected(tmpdir.path()).modify_any(),
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

        rx.wait_ordered_exact([
            expected(tmpdir.path()).modify_any(),
            expected(&file).remove_any(),
        ])
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
            expected(tmpdir.path()).modify_any(),
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

        rx.wait_ordered_exact([
            expected(tmpdir.path()).modify_any(),
            expected(&path).modify_any(),
        ])
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
            expected(tmpdir.path()).modify_any(),
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

        rx.wait_ordered_exact([
            expected(tmpdir.path()).modify_any(),
            expected(&path).remove_any(),
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
            HashSet::from([tmpdir.to_path_buf()])
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
            expected(tmpdir.path()).modify_any(),
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

        rx.wait_ordered_exact([expected(&subdir).modify_any(), expected(path).remove_any()])
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
            expected(tmpdir.path()).modify_any(),
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
            expected(tmpdir.path()).modify_any(),
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

        rx.wait_ordered_exact([
            expected(tmpdir.path()).modify_any(),
            expected(&path).modify_any(),
        ])
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

        rx.wait_ordered_exact([expected(&path).modify_any()])
            .ensure_no_tail();

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
        rx.wait_ordered(std::iter::once(expected(&file).create_any()));
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

        // File 1: under the *recursive* child, two levels deep → should be
        // delivered through the consolidated recursive parent watch.
        let file_under_rec = deep_under_rec.join("f");
        std::fs::File::create_new(&file_under_rec).expect("create");

        // File 2: under the *non-recursive* child, two levels deep → must be
        // filtered out because the user's intent on `c1` was NonRecursive.
        let file_under_nonrec = deep_under_nonrec.join("f");
        std::fs::File::create_new(&file_under_nonrec).expect("create");

        // We expect the deep-recursive file event but NOT the deep-nonrec one.
        // Use wait_ordered (allows extra events of any kind that aren't the
        // forbidden one) — checking absence of `file_under_nonrec` would
        // require a longer observation window in the harness; for this test
        // we assert the event we *do* expect arrives.
        rx.wait_ordered(std::iter::once(expected(&file_under_rec).create_any()));
    }

    /// Once the sibling count drops below the consolidation threshold, the
    /// next rebuild expands back into individual per-watch handles. This
    /// mirrors the fsevents backend, which also rebuilds the consolidation
    /// trie from scratch on every change.
    #[test]
    fn unwatch_below_threshold_deconsolidates() {
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

        // While consolidated: one handle on the shared parent only.
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()])
        );

        // Dropping one user watch leaves 9 siblings, below the threshold of
        // 10. The next rebuild reverts to per-watch handles plus the shared
        // tracked_parent (`tmpdir`) needed for rename detection on each.
        let mut pm = watcher.watcher.paths_mut();
        pm.remove(&subdirs[3]).expect("paths_mut remove");
        pm.commit().expect("paths_mut commit");

        let mut expected: HashSet<PathBuf> = subdirs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 3)
            .map(|(_, p)| p.clone())
            .collect();
        expected.insert(tmpdir.to_path_buf());
        assert_eq!(watcher.get_watch_handles(), expected);
    }

    /// Adding 100 sibling paths via `paths_mut().commit()` opens a single
    /// consolidated parent watch and still delivers events for files created
    /// in each.
    #[test]
    fn paths_mut_batched_commit() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let mut subdirs = Vec::new();
        for i in 0..100 {
            let sub = tmpdir.path().join(format!("c{i:03}"));
            std::fs::create_dir(&sub).expect("create_dir");
            subdirs.push(sub);
        }
        let mut pm = watcher.watcher.paths_mut();
        for sub in &subdirs {
            pm.add(sub, WatchMode::recursive()).expect("paths_mut add");
        }
        pm.commit().expect("paths_mut commit");

        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([tmpdir.to_path_buf()])
        );

        // Spot-check that events arrive for a few representative children.
        for idx in [0_usize, 50, 99] {
            let file = subdirs[idx].join("entry");
            std::fs::File::create_new(&file).expect("create");
            rx.wait_ordered(std::iter::once(expected(&file).create_any()));
        }
        let _ = Duration::from_millis(0); // keep `Duration` import alive
    }
}
