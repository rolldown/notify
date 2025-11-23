#![allow(missing_docs)]
//! Watcher implementation for Windows' directory management APIs
//!
//! For more information see the [ReadDirectoryChangesW reference][ref].
//!
//! [ref]: https://msdn.microsoft.com/en-us/library/windows/desktop/aa363950(v=vs.85).aspx

use crate::{
    BoundSender, Config, ErrorKind, Receiver, Sender, TargetMode, WatchMode, bounded, unbounded,
};
use crate::{Error, EventHandler, RecursiveMode, Result, Watcher};
use crate::{WatcherKind, event::*};
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

#[derive(Clone)]
struct ReadData {
    dir: PathBuf, // directory that is being watched
    watches: Rc<RefCell<HashMap<PathBuf, WatchMode>>>,
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
        let _ = self
            .action_tx
            .send(Action::UnwatchRaw(self.data.dir.clone()));
    }
}

enum Action {
    Watch(PathBuf, WatchMode),
    Unwatch(PathBuf),
    UnwatchRaw(PathBuf),
    Stop,
    Configure(Config, BoundSender<Result<bool>>),
    #[cfg(test)]
    GetWatchHandles(BoundSender<HashSet<PathBuf>>),
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
    watches: Rc<RefCell<HashMap<PathBuf, WatchMode>>>,
    watch_handles: HashMap<PathBuf, (WatchState, /* is_recursive */ bool)>,
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
        let _ = thread::Builder::new()
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
                        watches: Rc::new(RefCell::new(HashMap::new())),
                        watch_handles: HashMap::new(),
                        wakeup_sem,
                    };
                    server.run();
                }
            });
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
                        let _ = self.cmd_tx.send(res);
                    }
                    Action::Unwatch(path) => self.remove_watch(path),
                    Action::UnwatchRaw(path) => self.remove_watch_raw(path),
                    Action::Stop => {
                        stopped = true;
                        for (ws, _) in self.watch_handles.values() {
                            stop_watch(ws);
                        }
                        break;
                    }
                    Action::Configure(config, tx) => {
                        self.configure_raw_mode(config, tx);
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

    fn add_watch(&mut self, path: PathBuf, watch_mode: WatchMode) -> Result<PathBuf> {
        let existing_watch_mode = self.watches.borrow().get(&path).cloned();
        if let Some(existing) = existing_watch_mode {
            let need_upgrade_to_recursive = match existing.recursive_mode {
                RecursiveMode::Recursive => false,
                RecursiveMode::NonRecursive => {
                    watch_mode.recursive_mode == RecursiveMode::Recursive
                }
            };
            let need_to_watch_parent_newly = match existing.target_mode {
                TargetMode::TrackPath => false,
                TargetMode::NoTrack => watch_mode.target_mode == TargetMode::TrackPath,
            };
            if need_to_watch_parent_newly && let Some(parent) = path.parent() {
                self.add_watch_raw(parent.to_path_buf(), false, false)?;
            }
            if !need_upgrade_to_recursive {
                return Ok(path);
            }
        } else if watch_mode.target_mode == TargetMode::TrackPath
            && let Some(parent) = path.parent()
        {
            self.add_watch_raw(parent.to_path_buf(), false, false)?;
        }

        let metadata = match path.metadata().map_err(Error::io_watch) {
            Ok(meta) => {
                // path must be either a file or directory
                if !meta.is_dir() && !meta.is_file() {
                    return Err(Error::generic(
                        "Input watch path is neither a file nor a directory.",
                    )
                    .add_path(path));
                }
                meta
            }
            Err(err) => {
                if watch_mode.target_mode == TargetMode::TrackPath
                    && matches!(err.kind, ErrorKind::PathNotFound)
                {
                    self.watches.borrow_mut().insert(path.clone(), watch_mode);
                    return Ok(path);
                }
                return Err(err);
            }
        };

        let (watching_file, dir_target) = {
            if metadata.is_dir() {
                (false, path.clone())
            } else {
                // emulate file watching by watching the parent directory
                (true, path.parent().unwrap().to_path_buf())
            }
        };

        self.add_watch_raw(
            dir_target,
            watch_mode.recursive_mode.is_recursive(),
            watching_file,
        )?;

        let upgraded_watch_mode = if let Some(mut existing) = existing_watch_mode {
            existing.upgrade_with(watch_mode);
            existing
        } else {
            watch_mode
        };
        self.watches
            .borrow_mut()
            .insert(path.clone(), upgraded_watch_mode);

        Ok(path)
    }

    fn add_watch_raw(
        &mut self,
        path: PathBuf,
        is_recursive: bool,
        watching_file: bool,
    ) -> Result<()> {
        if let Some((ws, was_recursive)) = self.watch_handles.get(&path) {
            let need_upgrade_to_recursive = !*was_recursive && is_recursive;
            if !need_upgrade_to_recursive {
                return Ok(());
            }
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
        start_read(&rd, self.event_handler.clone(), handle, self.tx.clone());
        Ok(())
    }

    fn remove_watch(&mut self, path: PathBuf) {
        if self.watches.borrow_mut().remove(&path).is_some() {
            self.remove_watch_raw(path);
        }
    }

    fn remove_watch_raw(&mut self, path: PathBuf) {
        if let Some((ws, _)) = self.watch_handles.remove(&path) {
            stop_watch(&ws);
        } else if let Some(parent_path) = path.parent()
            && self.watches.borrow().get(parent_path).is_none()
            && self
                .watches
                .borrow()
                .keys()
                .filter(|p| p.starts_with(parent_path))
                .count()
                == 0
            && let Some((ws, _)) = self.watch_handles.remove(parent_path)
        {
            // if the parent path is not watched, the watch handle is used for the files under it
            // if no files under it are watched anymore, we can stop the watch on the parent path
            stop_watch(&ws);
        }
    }

    fn configure_raw_mode(&mut self, _config: Config, tx: BoundSender<Result<bool>>) {
        tx.send(Ok(false))
            .expect("configuration channel disconnect");
    }
}

fn stop_watch(ws: &WatchState) {
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

    let monitor_subdir = if request.data.is_recursive { 1 } else { 0 };

    unsafe {
        let overlapped = alloc::alloc_zeroed(alloc::Layout::new::<OVERLAPPED>()) as *mut OVERLAPPED;
        // When using callback based async requests, we are allowed to use the hEvent member
        // for our own purposes

        let request = Box::leak(request);
        (*overlapped).hEvent = request as *mut _ as _;

        // This is using an asynchronous call with a completion routine for receiving notifications
        // An I/O completion port would probably be more performant
        let ret = ReadDirectoryChangesW(
            handle,
            request.buffer.as_mut_ptr() as *mut c_void,
            BUF_SIZE,
            monitor_subdir,
            flags,
            &mut 0u32 as *mut u32, // not used for async reqs
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

unsafe extern "system" fn handle_event(
    error_code: u32,
    _bytes_written: u32,
    overlapped: *mut OVERLAPPED,
) {
    let overlapped: Box<OVERLAPPED> = unsafe { Box::from_raw(overlapped) };
    let request: Box<ReadDirectoryRequest> = unsafe { Box::from_raw(overlapped.hEvent as *mut _) };

    let release_semaphore =
        || unsafe { ReleaseSemaphore(request.data.complete_sem, 1, ptr::null_mut()) };

    fn emit_event(event_handler: &Mutex<dyn EventHandler>, res: Result<Event>) {
        if let Ok(mut guard) = event_handler.lock() {
            let f: &mut dyn EventHandler = &mut *guard;
            f.handle_event(res);
        }
    }
    let event_handler = |res| emit_event(&request.event_handler, res);

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
                if request
                    .data
                    .watches
                    .borrow()
                    .get(&dir)
                    .is_some_and(|mode| mode.target_mode == TargetMode::NoTrack)
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
            log::error!(
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
        request.event_handler.clone(),
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
        unsafe { ptr::read_unaligned(cur_offset as *const FILE_NOTIFY_INFORMATION) };
    loop {
        // filename length is size in bytes, so / 2
        let len = cur_entry.FileNameLength as usize / 2;
        let encoded_path: &[u16] = unsafe {
            slice::from_raw_parts(
                cur_offset.add(std::mem::offset_of!(FILE_NOTIFY_INFORMATION, FileName)) as _,
                len,
            )
        };
        // prepend root to get a full path
        let path = request
            .data
            .dir
            .join(PathBuf::from(OsString::from_wide(encoded_path)));

        // if we are watching a single file, ignore the event unless the path is exactly
        // the watched file
        let skip = !(request
            .data
            .watches
            .borrow()
            .contains_key(&request.data.dir)
            || request.data.watches.borrow().contains_key(&path));

        if !skip {
            log::trace!(
                "Event: path = `{}`, action = {:?}",
                path.display(),
                cur_entry.Action
            );

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
            };
        }

        if cur_entry.NextEntryOffset == 0 {
            break;
        }
        cur_offset = unsafe { cur_offset.add(cur_entry.NextEntryOffset as usize) };
        cur_entry = unsafe { ptr::read_unaligned(cur_offset as *const FILE_NOTIFY_INFORMATION) };
    }

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

    fn send_action_require_ack(&mut self, action: Action, pb: &PathBuf) -> Result<()> {
        self.tx
            .send(action)
            .map_err(|_| Error::generic("Error sending to internal channel"))?;

        // wake 'em up, we don't want to wait around for the ack
        self.wakeup_server();

        let ack_pb = self
            .cmd_rx
            .recv()
            .map_err(|_| Error::generic("Error receiving from command channel"))??;

        if pb.as_path() != ack_pb.as_path() {
            Err(Error::generic(&format!(
                "Expected ack for {:?} but got \
                 ack for {:?}",
                pb, ack_pb
            )))
        } else {
            Ok(())
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

impl Watcher for ReadDirectoryChangesWatcher {
    fn new<F: EventHandler>(event_handler: F, _config: Config) -> Result<Self> {
        let event_handler = Arc::new(Mutex::new(event_handler));
        Self::create(event_handler)
    }

    fn watch(&mut self, path: &Path, watch_mode: WatchMode) -> Result<()> {
        self.watch_inner(path, watch_mode)
    }

    fn unwatch(&mut self, path: &Path) -> Result<()> {
        self.unwatch_inner(path)
    }

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
        let _ = self.tx.send(Action::Stop);
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
        Watcher, test::*,
    };

    use std::{collections::HashSet, time::Duration};

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
    fn create_file() {
        let tmpdir = testdir();
        let (mut watcher, mut rx) = watcher();
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
        let (mut watcher, mut rx) = watcher();

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
        let (mut watcher, mut rx) = watcher();

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
        let (mut watcher, mut rx) = watcher();

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
        let (mut watcher, mut rx) = watcher();

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
        let (mut watcher, mut rx) = watcher();

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
        let (mut watcher, mut rx) = watcher();

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
        let (mut watcher, mut rx) = watcher();

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
        let (mut watcher, mut rx) = watcher();
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
        let (mut watcher, mut rx) = watcher();
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
        let (mut watcher, mut rx) = watcher();
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

        // rx.ensure_empty();
    }

    #[test]
    fn create_write_overwrite() {
        let tmpdir = testdir();
        let (mut watcher, mut rx) = watcher();
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
            expected(tmpdir.path()).modify_any(),
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
        let (mut watcher, mut rx) = watcher();
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
        let (mut watcher, mut rx) = watcher();
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
        let (mut watcher, mut rx) = watcher();
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
        let (mut watcher, mut rx) = watcher();

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
        let (mut watcher, mut rx) = watcher();

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
        let (mut watcher, mut rx) = watcher();

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
        let (mut watcher, mut rx) = watcher();

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
        let (mut watcher, mut rx) = watcher();

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

        rx.ensure_empty();
    }

    #[test]
    fn rename_dir_twice() {
        let tmpdir = testdir();
        let (mut watcher, mut rx) = watcher();

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
        let (mut watcher, mut rx) = watcher();

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
        let (mut watcher, mut rx) = watcher();

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
        let (mut watcher, mut rx) = watcher();

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
        let (mut watcher, mut rx) = watcher();

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
        let (mut watcher, mut rx) = watcher();

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
        assert_eq!(
            watcher.get_watch_handles(),
            HashSet::from([subdir.to_path_buf()])
        );
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

        let (mut watcher, mut rx) = watcher();

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
        let (mut watcher, mut rx) = watcher();

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
}
