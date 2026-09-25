//! Watcher implementation for Darwin's FSEvents API
//!
//! The FSEvents API provides a mechanism to notify clients about directories they ought to re-scan
//! in order to keep their internal data structures up-to-date with respect to the true state of
//! the file system. (For example, when files or directories are created, modified, or removed.) It
//! sends these notifications "in bulk", possibly notifying the client of changes to several
//! directories in a single callback.
//!
//! For more information see the [FSEvents API reference][ref].
//!
//! TODO: document event translation
//!
//! [ref]: https://developer.apple.com/library/mac/documentation/Darwin/Reference/FSEvents_Ref/

#![allow(non_upper_case_globals, dead_code)]

use crate::consolidating_path_trie::ConsolidatingPathTrie;
use crate::{
    Config, Error, ErrorKind, EventHandler, PathsMut, Result, Sender, WatchMode, Watcher, unbounded,
};
use crate::{TargetMode, event::*};
use objc2_core_foundation as cf;
use objc2_core_services as fs;
use rustc_hash::FxBuildHasher;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, OsStr};
use std::fmt;
use std::hash::RandomState;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

bitflags::bitflags! {
  #[repr(C)]
  #[derive(Debug)]
  struct StreamFlags: u32 {
    const NONE = fs::kFSEventStreamEventFlagNone;
    const MUST_SCAN_SUBDIRS = fs::kFSEventStreamEventFlagMustScanSubDirs;
    const USER_DROPPED = fs::kFSEventStreamEventFlagUserDropped;
    const KERNEL_DROPPED = fs::kFSEventStreamEventFlagKernelDropped;
    const IDS_WRAPPED = fs::kFSEventStreamEventFlagEventIdsWrapped;
    const HISTORY_DONE = fs::kFSEventStreamEventFlagHistoryDone;
    const ROOT_CHANGED = fs::kFSEventStreamEventFlagRootChanged;
    const MOUNT = fs::kFSEventStreamEventFlagMount;
    const UNMOUNT = fs::kFSEventStreamEventFlagUnmount;
    const ITEM_CREATED = fs::kFSEventStreamEventFlagItemCreated;
    const ITEM_REMOVED = fs::kFSEventStreamEventFlagItemRemoved;
    const INODE_META_MOD = fs::kFSEventStreamEventFlagItemInodeMetaMod;
    const ITEM_RENAMED = fs::kFSEventStreamEventFlagItemRenamed;
    const ITEM_MODIFIED = fs::kFSEventStreamEventFlagItemModified;
    const FINDER_INFO_MOD = fs::kFSEventStreamEventFlagItemFinderInfoMod;
    const ITEM_CHANGE_OWNER = fs::kFSEventStreamEventFlagItemChangeOwner;
    const ITEM_XATTR_MOD = fs::kFSEventStreamEventFlagItemXattrMod;
    const IS_FILE = fs::kFSEventStreamEventFlagItemIsFile;
    const IS_DIR = fs::kFSEventStreamEventFlagItemIsDir;
    const IS_SYMLINK = fs::kFSEventStreamEventFlagItemIsSymlink;
    const OWN_EVENT = fs::kFSEventStreamEventFlagOwnEvent;
    const IS_HARDLINK = fs::kFSEventStreamEventFlagItemIsHardlink;
    const IS_LAST_HARDLINK = fs::kFSEventStreamEventFlagItemIsLastHardlink;
    const ITEM_CLONED = fs::kFSEventStreamEventFlagItemCloned;
  }
}

/// FSEvents-based `Watcher` implementation
pub struct FsEventWatcher {
    paths: cf::CFRetained<cf::CFMutableArray<cf::CFString>>,
    since_when: fs::FSEventStreamEventId,
    latency: cf::CFTimeInterval,
    flags: fs::FSEventStreamCreateFlags,
    event_handler: Arc<Mutex<dyn EventHandler>>,
    runloop: Option<(cf::CFRetained<cf::CFRunLoop>, thread::JoinHandle<()>)>,
    watches: HashMap<PathBuf, WatchMode, FxBuildHasher>,
    gone_roots: Arc<GoneRoots>,
    max_fsevent_paths: usize,
}

/// The `TrackPath` roots that are not there. FSEvents reports a root that comes back through
/// `ROOT_CHANGED`, except when it comes back within a few milliseconds of going; a root that
/// went is therefore looked at again shortly after, on a thread of its own.
#[derive(Debug, Default)]
struct GoneRoots {
    state: Mutex<GoneRootsState>,
    /// Wakes the looking thread up when the watcher stops, so that it does not sleep on.
    wake: Condvar,
}

#[derive(Debug, Default)]
struct GoneRootsState {
    /// The roots that are gone, and how they were seen to go.
    roots: HashMap<PathBuf, Went, FxBuildHasher>,
    /// The roots whose coming back was reported from the disk, by a `ROOT_CHANGED`, an
    /// ancestor's event or the looking thread, when, and which file was there. FSEvents may send
    /// the root's own `ITEM_CREATED` a moment later; that is the same creation, and is not
    /// reported twice. A root that is not a stream path of its own gets no such event when an
    /// ancestor comes back, so a mark is only good for a moment, lest it swallow a later, real
    /// creation.
    created_reported: HashMap<PathBuf, (Instant, FileId), FxBuildHasher>,
    /// When the stale marks were last dropped.
    pruned_at: Option<Instant>,
    /// Bumped each time a root goes, so that the looking thread does not stop before it has
    /// seen the latest one.
    epoch: u64,
    looking: bool,
    /// The looking thread, joined when the watcher is dropped.
    looker: Option<thread::JoinHandle<()>>,
    stopped: bool,
}

/// How a root was seen to go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Went {
    /// By its own event, which reports it. An ancestor's event has nothing more to report, but a
    /// `ROOT_CHANGED` still reports the root as changed.
    OwnEvent,
    /// By a root change: a `ROOT_CHANGED`, an ancestor's event, or missing when watched.
    RootChange,
}

/// Which file a path is: its device and inode.
type FileId = (u64, u64);

fn file_id(meta: &std::fs::Metadata) -> FileId {
    (meta.dev(), meta.ino())
}

impl GoneRootsState {
    /// How long a creation reported from the disk stands for the root's own `ITEM_CREATED`.
    const CREATED_REPORTED_FOR: Duration = Duration::from_secs(2);

    /// `root` is back as the file `id`, and that was reported from the disk. The marks that went
    /// stale are dropped now and then, since a root that never gets its own event never uses its
    /// mark up.
    fn mark_created(&mut self, root: PathBuf, id: FileId) {
        let now = Instant::now();
        if self
            .pruned_at
            .is_none_or(|at| now.duration_since(at) >= Self::CREATED_REPORTED_FOR)
        {
            self.created_reported
                .retain(|_, (at, _)| now.duration_since(*at) < Self::CREATED_REPORTED_FOR);
            self.pruned_at = Some(now);
        }
        self.created_reported.insert(root, (now, id));
    }

    /// Whether the creation of `root`, which is the file `id` now, was reported from the disk a
    /// moment ago. A root that is another file, or none, was deleted since, and its creation is a
    /// new one. The mark is used up either way.
    fn take_created(&mut self, root: &Path, id: Option<FileId>) -> bool {
        self.created_reported
            .remove(root)
            .is_some_and(|(at, marked)| {
                at.elapsed() < Self::CREATED_REPORTED_FOR && Some(marked) == id
            })
    }
}

/// Resets `looking` when the looking thread ends early: a panicking handler must not keep the
/// next root that goes from being looked at. The thread disarms it when it ends normally, as
/// it resets `looking` itself then, under the lock that saw no new root.
struct LookingGuard<'a> {
    gone: &'a GoneRoots,
    armed: bool,
}

impl Drop for LookingGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.gone.lock().looking = false;
        }
    }
}

impl GoneRoots {
    const LOOK_AGAIN_AFTER: [Duration; 2] =
        [Duration::from_millis(200), Duration::from_millis(1800)];

    fn lock(&self) -> MutexGuard<'_, GoneRootsState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `root` went, as a root change says; look at it again in a moment unless FSEvents reports
    /// it back first. Says how the root was seen to go before, if it was, as far as this knows:
    /// `None` for a root that was there.
    fn went(
        this: &Arc<Self>,
        root: &Path,
        event_handler: &Arc<Mutex<dyn EventHandler>>,
    ) -> Option<Went> {
        let mut gone = this.lock();
        gone.created_reported.remove(root);
        let before = gone.roots.insert(root.to_path_buf(), Went::RootChange);
        gone.epoch += 1;
        if gone.looking || gone.stopped {
            return before;
        }
        gone.looking = true;
        drop(gone);
        let gone = Arc::clone(this);
        let event_handler = Arc::clone(event_handler);
        let spawned = thread::Builder::new()
            .name("notify-rs fsevent roots".to_string())
            .spawn(move || Self::look_again(&gone, &event_handler));
        match spawned {
            Ok(looker) => this.lock().looker = Some(looker),
            Err(e) => {
                tracing::error!(
                    ?e,
                    "failed to spawn the thread that looks at gone roots again"
                );
                this.lock().looking = false;
            }
        }
        before
    }

    /// Sleeps for `delay`, or until the watcher stops. Says whether to go on.
    fn sleep(&self, delay: Duration) -> bool {
        let gone = self.lock();
        let (gone, _) = self
            .wake
            .wait_timeout_while(gone, delay, |gone| !gone.stopped)
            .unwrap_or_else(PoisonError::into_inner);
        !gone.stopped
    }

    fn look_again(this: &Arc<Self>, event_handler: &Arc<Mutex<dyn EventHandler>>) {
        let mut looking = LookingGuard {
            gone: this,
            armed: true,
        };
        loop {
            let epoch = this.lock().epoch;
            for delay in Self::LOOK_AGAIN_AFTER {
                if !this.sleep(delay) {
                    return;
                }
                let roots: Vec<PathBuf> = this.lock().roots.keys().cloned().collect();
                for root in roots {
                    let Ok(meta) = std::fs::metadata(&root) else {
                        continue;
                    };
                    {
                        let mut gone = this.lock();
                        if gone.stopped {
                            return;
                        }
                        if gone.roots.remove(&root).is_none() {
                            continue;
                        }
                        gone.mark_created(root.clone(), file_id(&meta));
                    }
                    let mut handler = event_handler.lock().unwrap_or_else(PoisonError::into_inner);
                    deliver(&mut *handler, root_created(meta.is_dir(), root));
                }
            }
            if this.done_looking(epoch, &mut looking) {
                return;
            }
        }
    }

    /// Whether the looking thread is done: no root went since `epoch`, or the watcher stopped.
    /// `looking` is then reset, and the guard disarmed, under the same lock as the check. A root
    /// that goes right after starts a looker of its own, whose `looking` the guard must not
    /// reset: that would let a third one start and leave the second out of `stop`'s join.
    fn done_looking(&self, epoch: u64, looking: &mut LookingGuard<'_>) -> bool {
        let mut gone = self.lock();
        if gone.epoch != epoch && !gone.stopped {
            return false;
        }
        gone.looking = false;
        looking.armed = false;
        true
    }

    /// Stops the looking thread and waits for it, so that no event reaches the handler once the
    /// watcher is dropped.
    fn stop(&self) {
        let looker = {
            let mut gone = self.lock();
            gone.stopped = true;
            gone.looker.take()
        };
        self.wake.notify_all();
        // A handler that drops the watcher from the looking thread itself cannot wait for it.
        if let Some(looker) = looker
            && looker.thread().id() != thread::current().id()
            && looker.join().is_err()
        {
            tracing::error!("the thread that looks at gone roots again panicked");
        }
    }
}

/// The event for a root that is back.
fn root_created(is_dir: bool, root: PathBuf) -> Event {
    let kind = if is_dir {
        CreateKind::Folder
    } else {
        CreateKind::File
    };
    Event::new(EventKind::Create(kind))
        .set_info("root changed")
        .add_path(root)
}

/// The event for a root that is out of reach.
fn root_gone(root: PathBuf) -> Event {
    Event::new(EventKind::Remove(RemoveKind::Any))
        .set_info("root changed")
        .add_path(root)
}

/// Hands `event` to the handler. A panicking handler must not unwind into CoreServices, nor take
/// the looking thread down.
fn deliver(handler: &mut dyn EventHandler, event: Event) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handler.handle_event(Ok(event));
    }))
    .map_err(|_| {
        tracing::error!("panic in FSEvents event handler; dropping event");
    });
}

/// What a `ROOT_CHANGED` event says about the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootChange {
    /// The root went: it is not there any more.
    Gone,
    /// The root is not there, and was not before either: it went already, or never was.
    StillGone,
    /// The root is back, a directory or a file.
    Present { is_dir: bool },
    /// The root is there, and there is nothing to report: its coming back was reported already,
    /// or the watch does not track the path.
    Reported,
}

// FSEvents applies the path limit across live streams, so all watcher instances
// in this process must share the same count.
static ACTIVE_FSEVENTS_PATHS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct FseventsPathReservation {
    active_paths: &'static AtomicUsize,
    path_count: usize,
}

impl FseventsPathReservation {
    fn acquire(
        active_paths: &'static AtomicUsize,
        path_count: usize,
        budget: usize,
    ) -> std::result::Result<Self, usize> {
        active_paths
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |active_path_count| {
                active_path_count
                    .checked_add(path_count)
                    .filter(|&combined_path_count| combined_path_count <= budget)
            })
            .map(|_| Self {
                active_paths,
                path_count,
            })
    }
}

impl Drop for FseventsPathReservation {
    fn drop(&mut self) {
        let previous = self
            .active_paths
            .fetch_sub(self.path_count, Ordering::Relaxed);
        debug_assert!(previous >= self.path_count);
    }
}

impl fmt::Debug for FsEventWatcher {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("FsEventWatcher")
            .field("paths", &self.paths)
            .field("since_when", &self.since_when)
            .field("latency", &self.latency)
            .field("flags", &self.flags)
            .field("event_handler", &Arc::as_ptr(&self.event_handler))
            .field("runloop", &self.runloop)
            .field("watches", &self.watches)
            .field("gone_roots", &self.gone_roots)
            .field("max_fsevent_paths", &self.max_fsevent_paths)
            .finish()
    }
}

// CFMutableArrayRef is a type alias to *mut libc::c_void, so FsEventWatcher is not Send/Sync
// automatically. It's Send because the pointer is not used in other threads.
unsafe impl Send for FsEventWatcher {}

// It's Sync because all methods that change the mutable state use `&mut self`.
unsafe impl Sync for FsEventWatcher {}

fn translate_flags(
    flags: &StreamFlags,
    precise: bool,
    root: Option<RootChange>,
    create_reported: bool,
) -> Vec<Event> {
    let mut evs = Vec::new();
    translate_flags_with(flags, precise, root, create_reported, |ev| evs.push(ev));
    evs
}

// Keep this in sync with `translate_flags_with`; the callback uses it to avoid path clones.
fn translated_event_count(flags: &StreamFlags, precise: bool) -> usize {
    if flags.contains(StreamFlags::HISTORY_DONE) {
        return 0;
    }

    let mut count = usize::from(flags.contains(StreamFlags::MUST_SCAN_SUBDIRS));
    if !precise {
        return count + 1;
    }

    let root_changed = flags.contains(StreamFlags::ROOT_CHANGED);
    count += usize::from(root_changed);
    count += usize::from(flags.contains(StreamFlags::MOUNT));
    count += usize::from(flags.contains(StreamFlags::UNMOUNT));
    count += usize::from(flags.contains(StreamFlags::ITEM_CREATED));
    count += usize::from(flags.contains(StreamFlags::ITEM_RENAMED) && !root_changed);
    count += usize::from(flags.contains(StreamFlags::INODE_META_MOD));
    count += usize::from(flags.contains(StreamFlags::FINDER_INFO_MOD));
    count += usize::from(flags.contains(StreamFlags::ITEM_CHANGE_OWNER));
    count += usize::from(flags.contains(StreamFlags::ITEM_XATTR_MOD));
    count += usize::from(flags.contains(StreamFlags::ITEM_MODIFIED));
    count += usize::from(flags.contains(StreamFlags::ITEM_REMOVED) && !root_changed);
    count
}

/// `root` says, for a `ROOT_CHANGED` event, what became of the root; `None` counts as gone.
/// `create_reported` says that the creation the flags carry was reported already, from the disk,
/// and is left out.
#[expect(clippy::too_many_lines)]
fn translate_flags_with(
    flags: &StreamFlags,
    precise: bool,
    root: Option<RootChange>,
    create_reported: bool,
    mut emit: impl FnMut(Event),
) {
    // «Denotes a sentinel event sent to mark the end of the "historical" events
    // sent as a result of specifying a `sinceWhen` value in the FSEvents.Create
    // call that created this event stream. After invoking the client's callback
    // with all the "historical" events that occurred before now, the client's
    // callback will be invoked with an event where the HistoryDone flag is set.
    // The client should ignore the path supplied in this callback.»
    // — https://www.mbsplugins.eu/FSEventsNextEvent.shtml
    //
    // As a result, we just stop processing here and return an empty vec, which
    // will ignore this completely and not emit any Events whatsoever.
    if flags.contains(StreamFlags::HISTORY_DONE) {
        return;
    }

    // `ITEM_CLONED` can be present alongside other flags (including create/modify/remove).
    // Preserve any existing `info` (like "root changed"), but annotate otherwise so downstream
    // can detect and filter clone-related events. See https://github.com/notify-rs/notify/issues/465.
    let clone_related = precise && flags.contains(StreamFlags::ITEM_CLONED);
    let own_process_id = if precise && flags.contains(StreamFlags::OWN_EVENT) {
        Some(std::process::id())
    } else {
        None
    };

    let mut emit_event = |mut ev: Event| {
        if clone_related && ev.info().is_none() {
            ev.attrs.set_info("is: clone");
        }
        if let Some(process_id) = own_process_id {
            ev.attrs.set_process_id(process_id);
        }
        emit(ev);
    };

    // FSEvents provides two possible hints as to why events were dropped,
    // however documentation on what those mean is scant, so we just pass them
    // through in the info attr field. The intent is clear enough, and the
    // additional information is provided if the user wants it.
    if flags.contains(StreamFlags::MUST_SCAN_SUBDIRS) {
        let e = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        emit_event(if flags.contains(StreamFlags::USER_DROPPED) {
            e.set_info("rescan: user dropped")
        } else if flags.contains(StreamFlags::KERNEL_DROPPED) {
            e.set_info("rescan: kernel dropped")
        } else {
            e
        });
    }

    // In imprecise mode, let's not even bother parsing the kind of the event
    // except for the above very special events.
    if !precise {
        emit(Event::new(EventKind::Any));
        return;
    }

    // A watched root changed: it, or a directory above it, was renamed, removed or brought back.
    // The disk says which. A root that is back is reported as created, unless the flags carry
    // its own creation, which is reported below. For a root that went, the flags say whether
    // it was renamed; otherwise it is treated as removed rather than guessed to be renamed.
    let root_changed = flags.contains(StreamFlags::ROOT_CHANGED);
    if root_changed {
        match root {
            Some(RootChange::Present { is_dir }) => {
                if !flags.contains(StreamFlags::ITEM_CREATED) {
                    let kind = if is_dir {
                        CreateKind::Folder
                    } else {
                        CreateKind::File
                    };
                    emit_event(Event::new(EventKind::Create(kind)).set_info("root changed"));
                }
            }
            Some(RootChange::Reported | RootChange::StillGone) => {}
            Some(RootChange::Gone) | None => {
                let kind = if flags.contains(StreamFlags::ITEM_REMOVED) {
                    if flags.contains(StreamFlags::IS_DIR) {
                        EventKind::Remove(RemoveKind::Folder)
                    } else if flags.contains(StreamFlags::IS_FILE) {
                        EventKind::Remove(RemoveKind::File)
                    } else {
                        EventKind::Remove(RemoveKind::Any)
                    }
                } else if flags.contains(StreamFlags::ITEM_RENAMED) {
                    EventKind::Modify(ModifyKind::Name(RenameMode::From))
                } else {
                    EventKind::Remove(RemoveKind::Any)
                };
                emit_event(Event::new(kind).set_info("root changed"));
            }
        }
    }

    // A path was mounted at the event path; we treat that as a create.
    if flags.contains(StreamFlags::MOUNT) {
        emit_event(Event::new(EventKind::Create(CreateKind::Other)).set_info("mount"));
    }

    // A path was unmounted at the event path; we treat that as a remove.
    if flags.contains(StreamFlags::UNMOUNT) {
        emit_event(Event::new(EventKind::Remove(RemoveKind::Other)).set_info("mount"));
    }

    if flags.contains(StreamFlags::ITEM_CREATED) && !create_reported {
        emit_event(if flags.contains(StreamFlags::IS_DIR) {
            Event::new(EventKind::Create(CreateKind::Folder))
        } else if flags.contains(StreamFlags::IS_FILE) {
            Event::new(EventKind::Create(CreateKind::File))
        } else {
            let e = Event::new(EventKind::Create(CreateKind::Other));
            if flags.contains(StreamFlags::IS_SYMLINK) {
                e.set_info("is: symlink")
            } else if flags.contains(StreamFlags::IS_HARDLINK) {
                e.set_info("is: hardlink")
            } else if flags.contains(StreamFlags::ITEM_CLONED) {
                e.set_info("is: clone")
            } else {
                Event::new(EventKind::Create(CreateKind::Any))
            }
        });
    }

    // FSEvents provides no mechanism to associate the old and new sides of a
    // rename event.
    // Avoid emitting duplicate events around a root change by checking `root_changed`.
    if flags.contains(StreamFlags::ITEM_RENAMED) && !root_changed {
        emit_event(Event::new(EventKind::Modify(ModifyKind::Name(
            RenameMode::Any,
        ))));
    }

    // This is only described as "metadata changed", but it may be that it's
    // only emitted for some more precise subset of events... if so, will need
    // amending, but for now we have an Any-shaped bucket to put it in.
    if flags.contains(StreamFlags::INODE_META_MOD) {
        emit_event(Event::new(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Any,
        ))));
    }

    if flags.contains(StreamFlags::FINDER_INFO_MOD) {
        emit_event(
            Event::new(EventKind::Modify(ModifyKind::Metadata(MetadataKind::Other)))
                .set_info("meta: finder info"),
        );
    }

    if flags.contains(StreamFlags::ITEM_CHANGE_OWNER) {
        emit_event(Event::new(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Ownership,
        ))));
    }

    if flags.contains(StreamFlags::ITEM_XATTR_MOD) {
        emit_event(Event::new(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Extended,
        ))));
    }

    // This is specifically described as a data change, which we take to mean
    // is a content change.
    if flags.contains(StreamFlags::ITEM_MODIFIED) {
        emit_event(Event::new(EventKind::Modify(ModifyKind::Data(
            DataChange::Content,
        ))));
    }

    // Avoid emitting duplicate events around a root change by checking `root_changed`.
    if flags.contains(StreamFlags::ITEM_REMOVED) && !root_changed {
        emit_event(if flags.contains(StreamFlags::IS_DIR) {
            Event::new(EventKind::Remove(RemoveKind::Folder))
        } else if flags.contains(StreamFlags::IS_FILE) {
            Event::new(EventKind::Remove(RemoveKind::File))
        } else {
            let e = Event::new(EventKind::Remove(RemoveKind::Other));
            if flags.contains(StreamFlags::IS_SYMLINK) {
                e.set_info("is: symlink")
            } else if flags.contains(StreamFlags::IS_HARDLINK) {
                e.set_info("is: hardlink")
            } else if flags.contains(StreamFlags::ITEM_CLONED) {
                e.set_info("is: clone")
            } else {
                Event::new(EventKind::Remove(RemoveKind::Any))
            }
        });
    }
}

struct StreamContextInfo {
    event_handler: Arc<Mutex<dyn EventHandler>>,
    watches: HashMap<PathBuf, WatchMode, FxBuildHasher>,
    gone_roots: Arc<GoneRoots>,
}

// Free the context when the stream created by `FSEventStreamCreate` is released.
extern "C-unwind" fn release_context(info: *const libc::c_void) {
    // Safety:
    // - The [documentation] for `FSEventStreamContext` states that `release` is only
    //   called when the stream is deallocated, so it is safe to convert `info` back into a
    //   box and drop it.
    //
    // [docs]: https://developer.apple.com/documentation/coreservices/fseventstreamcontext?language=objc
    unsafe {
        drop(Box::from_raw(info.cast::<StreamContextInfo>().cast_mut()));
    }
}

struct FsEventPathsMut<'a>(&'a mut FsEventWatcher);
impl<'a> FsEventPathsMut<'a> {
    fn new(watcher: &'a mut FsEventWatcher) -> Self {
        watcher.stop();
        Self(watcher)
    }
}
impl PathsMut for FsEventPathsMut<'_> {
    #[tracing::instrument(level = "debug", skip(self))]
    fn add(&mut self, path: &Path, watch_mode: WatchMode) -> Result<()> {
        self.0.append_path(path, watch_mode)
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn remove(&mut self, path: &Path) -> Result<()> {
        self.0.remove_path(path)
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn commit(self: Box<Self>) -> Result<()> {
        self.0.run()
    }
}

impl FsEventWatcher {
    fn from_event_handler(
        event_handler: Arc<Mutex<dyn EventHandler>>,
        max_fsevent_paths: usize,
    ) -> Self {
        FsEventWatcher {
            paths: cf::CFMutableArray::empty(),
            since_when: fs::kFSEventStreamEventIdSinceNow,
            latency: 0.0,
            flags: fs::kFSEventStreamCreateFlagFileEvents
                | fs::kFSEventStreamCreateFlagNoDefer
                | fs::kFSEventStreamCreateFlagWatchRoot,
            event_handler,
            runloop: None,
            watches: HashMap::default(),
            gone_roots: Arc::default(),
            max_fsevent_paths,
        }
    }

    fn watch_inner(&mut self, path: &Path, watch_mode: WatchMode) -> Result<()> {
        self.stop();
        let result = self.append_path(path, watch_mode);
        self.run()?;
        result
    }

    fn unwatch_inner(&mut self, path: &Path) -> Result<()> {
        self.stop();
        let result = self.remove_path(path);
        self.run()?;
        result
    }

    #[inline]
    fn is_running(&self) -> bool {
        self.runloop.is_some()
    }

    fn stop(&mut self) {
        if !self.is_running() {
            return;
        }

        if let Some((runloop, thread_handle)) = self.runloop.take() {
            while !runloop.is_waiting() {
                thread::yield_now();
            }

            runloop.stop();

            // Wait for the thread to shut down.
            thread_handle.join().expect("thread to shut down");
        }
    }

    fn remove_path(&mut self, path: &Path) -> Result<()> {
        if path == Path::new("") {
            return Err(Error::watch_not_found());
        }
        let p = canonicalize_lenient(path);
        {
            let mut gone = self.gone_roots.lock();
            gone.roots.remove(&p);
            gone.created_reported.remove(&p);
        }
        match self.watches.remove(&p) {
            Some(_) => Ok(()),
            None => Err(Error::watch_not_found()),
        }
    }

    // https://github.com/thibaudgg/rb-fsevent/blob/master/ext/fsevent_watch/main.c
    fn append_path(&mut self, path: &Path, watch_mode: WatchMode) -> Result<()> {
        if (!path.exists() && watch_mode.target_mode != TargetMode::TrackPath)
            || path == Path::new("")
        {
            return Err(Error::path_not_found().add_path(path.into()));
        }
        let canonical_path = canonicalize_lenient(path);

        // A root that is not there yet is looked at again once the stream runs, since one that
        // appears before the stream starts is not reported by FSEvents.
        if watch_mode.target_mode == TargetMode::TrackPath && !canonical_path.exists() {
            GoneRoots::went(&self.gone_roots, &canonical_path, &self.event_handler);
        }
        self.watches.insert(canonical_path, watch_mode);
        Ok(())
    }

    fn update_paths_based_on_watches(&mut self) {
        let paths_to_watch = {
            let mut trie = ConsolidatingPathTrie::new(true, self.max_fsevent_paths);
            for path in self.watches.keys() {
                trie.insert(path.clone());
            }
            for anchor in self.missing_root_anchors() {
                trie.insert(anchor);
            }
            trie.values()
        };
        tracing::debug!("Watching the following paths: {paths_to_watch:?}");
        let paths_to_watch_set = paths_to_watch
            .iter()
            .map(|p| p.to_string_lossy().to_lowercase())
            .collect::<HashSet<_>>();
        let mut already_included_paths =
            HashSet::<String, RandomState>::with_capacity(self.paths.len());

        // remove no longer watched paths
        let mut to_remove = Vec::new();
        for (idx, item) in self.paths.iter().enumerate() {
            if paths_to_watch_set.contains(&item.to_string()) {
                already_included_paths.insert(item.to_string());
            } else {
                to_remove.push(cf::CFIndex::try_from(idx).unwrap());
            }
        }
        for idx in to_remove.iter().rev() {
            // SAFETY: `the_array` is not `None` and the generic is correct, `idx` is in-bounds
            unsafe {
                cf::CFMutableArray::remove_value_at_index(Some(self.paths.as_opaque()), *idx);
            };
        }

        // add new paths
        for path in paths_to_watch {
            if !already_included_paths.contains(&path.to_string_lossy().to_lowercase()) {
                self.paths
                    .append(&cf::CFString::from_str(&path.to_string_lossy()));
            }
        }
    }

    /// The anchors of the `TrackPath` roots that are gone, as far as this knows: missing when
    /// watched, or seen to go while a stream ran. The roots that are there cost nothing. The
    /// anchors are found here only, at a stream rebuild: a root whose ancestors go two levels
    /// deep while the stream runs is anchored at the next `watch`, `unwatch` or commit, and one
    /// that is back keeps its anchor until then.
    fn missing_root_anchors(&self) -> Vec<PathBuf> {
        let gone: Vec<PathBuf> = self.gone_roots.lock().roots.keys().cloned().collect();
        gone.into_iter()
            .filter(|root| {
                self.watches
                    .get(root)
                    .is_some_and(|mode| mode.target_mode == TargetMode::TrackPath)
            })
            .filter_map(|root| missing_root_anchor(&root))
            .collect()
    }

    fn run(&mut self) -> Result<()> {
        if self.watches.is_empty() {
            return Ok(());
        }

        self.update_paths_based_on_watches();

        // Over roughly RLIMIT_NOFILE/10 paths across all live streams, FSEvents
        // closes fd 0, which this process owns. The corruption then surfaces as
        // EBADF on unrelated files.
        let path_count = self.paths.iter().count();
        let budget = fsevents_path_budget().unwrap_or(usize::MAX);
        let path_reservation =
            match FseventsPathReservation::acquire(&ACTIVE_FSEVENTS_PATHS, path_count, budget) {
                Ok(reservation) => reservation,
                Err(active_path_count) => {
                    let combined_path_count = active_path_count.saturating_add(path_count);
                    tracing::error!(
                        "refusing FSEvents stream: {combined_path_count} active paths exceed the \
                         safe limit of {budget}. Raise RLIMIT_NOFILE, watch fewer paths, or use \
                         macos_kqueue."
                    );
                    return Err(Error::new(ErrorKind::MaxFilesWatch));
                }
            };

        // We need to associate the stream context with our callback in order to propagate events
        // to the rest of the system. This will be owned by the stream, and will be freed when the
        // stream is closed. This means we will leak the context if we panic before reaching
        // `FSEventStreamRelease`.
        let context = Box::into_raw(Box::new(StreamContextInfo {
            event_handler: Arc::clone(&self.event_handler),
            watches: self.watches.clone(),
            gone_roots: Arc::clone(&self.gone_roots),
        }));

        let mut stream_context = fs::FSEventStreamContext {
            version: 0,
            info: context.cast::<libc::c_void>(),
            retain: None,
            release: Some(release_context),
            copyDescription: None,
        };

        let stream = unsafe {
            fs::FSEventStreamCreate(
                cf::kCFAllocatorDefault,
                Some(callback),
                &raw mut stream_context,
                self.paths.as_opaque(),
                self.since_when,
                self.latency,
                self.flags,
            )
        };

        // Wrapper to help send CFRunLoop types across threads.
        struct CFRunLoopSendWrapper(cf::CFRetained<cf::CFRunLoop>);
        // Safety:
        // - According to the Apple documentation, it's safe to move `CFRunLoop`s across threads.
        //   https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/Multithreading/ThreadSafetySummary/ThreadSafetySummary.html
        unsafe impl Send for CFRunLoopSendWrapper {}

        // Wrapper to help send FSEventStreamRef types across threads.
        struct FSEventStreamSendWrapper(fs::FSEventStreamRef);
        // SAFETY: Unclear?
        unsafe impl Send for FSEventStreamSendWrapper {}

        // move into thread
        let stream = FSEventStreamSendWrapper(stream);

        // channel to pass runloop around
        let (rl_tx, rl_rx) = unbounded();

        let thread_handle = thread::Builder::new()
            .name("notify-rs fsevents loop".to_string())
            .spawn(move || {
                // Keep the shared path count reserved until this stream is released.
                let _path_reservation = path_reservation;
                let _ = &stream;
                let stream = stream.0;

                unsafe {
                    // CFRunLoop::current() returns None only in OOM situations
                    let cur_runloop = cf::CFRunLoop::current().unwrap();

                    #[expect(deprecated)]
                    fs::FSEventStreamScheduleWithRunLoop(
                        stream,
                        &cur_runloop,
                        cf::kCFRunLoopDefaultMode.unwrap(),
                    );
                    if !fs::FSEventStreamStart(stream) {
                        fs::FSEventStreamInvalidate(stream);
                        fs::FSEventStreamRelease(stream);
                        rl_tx
                            .send(Err(Error::generic("unable to start FSEvent stream")))
                            .expect("Unable to send error for FSEventStreamStart");
                        return;
                    }

                    // the calling to CFRunLoopRun will be terminated by CFRunLoopStop call in drop()
                    rl_tx
                        .send(Ok(CFRunLoopSendWrapper(cur_runloop)))
                        .expect("Unable to send runloop to watcher");

                    cf::CFRunLoop::run();
                    fs::FSEventStreamStop(stream);
                    // There are edge-cases, when many events are pending,
                    // despite the stream being stopped, that the stream's
                    // associated callback will be invoked. Purging events
                    // is intended to prevent this.
                    let event_id = fs::FSEventsGetCurrentEventId();
                    let device = fs::FSEventStreamGetDeviceBeingWatched(stream);
                    if !fs::FSEventsPurgeEventsForDeviceUpToEventId(device, event_id) {
                        tracing::error!(
                            "FSEventsPurgeEventsForDeviceUpToEventId failed for device {device}, event id {event_id}",
                        );
                    }
                    fs::FSEventStreamInvalidate(stream);
                    fs::FSEventStreamRelease(stream);
                }
            })?;
        // block until runloop has been sent
        let runloop_wrapper = rl_rx.recv().unwrap()?;
        self.runloop = Some((runloop_wrapper.0, thread_handle));

        Ok(())
    }

    fn configure_raw_mode(_config: Config, tx: &Sender<Result<bool>>) {
        tx.send(Ok(false))
            .expect("configuration channel disconnect");
    }
}

/// The path as FSEvents reports it: canonical, even when it does not exist yet. The deepest
/// existing ancestor is canonicalized and the missing tail is appended as given, so that a
/// missing path below a symlinked prefix (`/tmp`, `/var`) matches the events for it, and is found
/// again by the same name once it exists.
fn canonicalize_lenient(path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
    };
    for ancestor in path.ancestors() {
        if let Ok(canonical) = ancestor.canonicalize() {
            return match path.strip_prefix(ancestor) {
                Ok(tail) if !tail.as_os_str().is_empty() => canonical.join(tail),
                Ok(_) | Err(_) => canonical,
            };
        }
    }
    path
}

/// For a `TrackPath` root whose parent is not there, the deepest ancestor that is. FSEvents'
/// `WatchRoot` re-arms a missing root natively only when its last component is the one missing;
/// a root further down is not reported when it appears, nor when the ancestor that exists is
/// moved. Watching that ancestor too brings the events of the directories in between to the
/// callback, which looks at the roots below them. It costs a recursive stream on the ancestor
/// until the next stream rebuild, by which time the root usually exists, and folds the roots
/// below the ancestor into it.
///
/// An ancestor fewer than [`MIN_ANCHOR_DEPTH`] components below the root of the file system,
/// such as `/Users/me` or `/private/tmp`, is never watched for this: it would bring every event
/// of a home directory to the callback. Such a root keeps FSEvents' native behaviour, which
/// follows the missing last component only.
fn missing_root_anchor(root: &Path) -> Option<PathBuf> {
    let parent = root.parent()?;
    if parent.exists() {
        return None;
    }
    parent
        .ancestors()
        .skip(1)
        .find(|ancestor| ancestor.exists())
        .filter(|ancestor| path_depth(ancestor) >= MIN_ANCHOR_DEPTH)
        .map(Path::to_path_buf)
}

/// How deep an anchor must be below the root of the file system, in components.
const MIN_ANCHOR_DEPTH: usize = 3;

/// How many named components `path` has.
fn path_depth(path: &Path) -> usize {
    path.components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .count()
}

// A twelfth rather than a tenth: the edge also shifts with how many descriptors
// the process already holds.
fn fsevents_path_budget() -> Option<usize> {
    let mut limit = unsafe { std::mem::zeroed::<libc::rlimit>() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } != 0 {
        return None;
    }
    let soft = usize::try_from(limit.rlim_cur).ok()?;
    Some(soft / 12)
}

extern "C-unwind" fn callback(
    stream_ref: fs::ConstFSEventStreamRef,
    info: *mut libc::c_void,
    num_events: libc::size_t,                          // size_t numEvents
    event_paths: NonNull<libc::c_void>,                // void *eventPaths
    event_flags: NonNull<fs::FSEventStreamEventFlags>, // const FSEventStreamEventFlags eventFlags[]
    event_ids: NonNull<fs::FSEventStreamEventId>,      // const FSEventStreamEventId eventIds[]
) {
    // Never unwind into CoreServices; if something goes wrong, drop the events and log.
    // This also protects against panics from user-provided `EventHandler` implementations.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        callback_impl(
            stream_ref,
            info,
            num_events,
            event_paths,
            event_flags,
            event_ids,
        );
    }))
    .map_err(|_| {
        tracing::error!("panic in FSEvents callback; dropping pending events");
    });
}

unsafe fn callback_impl(
    _stream_ref: fs::ConstFSEventStreamRef,
    info: *mut libc::c_void,
    num_events: libc::size_t,                          // size_t numEvents
    event_paths: NonNull<libc::c_void>,                // void *eventPaths
    event_flags: NonNull<fs::FSEventStreamEventFlags>, // const FSEventStreamEventFlags eventFlags[]
    _event_ids: NonNull<fs::FSEventStreamEventId>,     // const FSEventStreamEventId eventIds[]
) {
    let event_paths = event_paths.as_ptr() as *const *const libc::c_char;
    let info = unsafe { &*info.cast::<StreamContextInfo>() };
    let mut event_handler_guard = None;
    // The handler is locked once per callback, when the first event is handed to it.
    let mut emit = |event: Event| {
        let event_handler = event_handler_guard.get_or_insert_with(|| {
            info.event_handler
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        });
        deliver(&mut **event_handler, event);
    };

    for p in 0..num_events {
        // Paths are not guaranteed to be valid UTF-8 (e.g. NFS); keep them as raw bytes.
        let path = unsafe { CStr::from_ptr(*event_paths.add(p)) };
        let path = Path::new(OsStr::from_bytes(path.to_bytes()));

        let raw_flag = unsafe { *event_flags.as_ptr().add(p) };
        let flag = StreamFlags::from_bits_truncate(raw_flag);
        let unknown_bits = raw_flag & !StreamFlags::all().bits();
        if unknown_bits != 0 {
            // `FSEventStreamEventFlags` is an extensible bitfield; tolerate future flags.
            tracing::trace!("unknown FSEventStreamEventFlags bits: 0x{unknown_bits:08x}");
        }

        tracing::trace!(
            target = "rolldown-notify::fsevent::details",
            ?path,
            ?flag,
            "FSEvent raw event received"
        );

        // A directory above tracked roots was created, removed or renamed, or a stream path
        // changed: the roots below it are looked at, whether or not the path is handled itself.
        // A file has nothing below it.
        if flag.intersects(
            StreamFlags::ROOT_CHANGED
                | StreamFlags::ITEM_CREATED
                | StreamFlags::ITEM_REMOVED
                | StreamFlags::ITEM_RENAMED,
        ) && (!flag.contains(StreamFlags::IS_FILE) || flag.contains(StreamFlags::IS_DIR))
        {
            check_roots_below(info, path, &mut emit);
        }

        let mut handle_event = false;
        for (watch_path, mode) in &info.watches {
            if path.starts_with(watch_path) {
                if mode.recursive_mode.is_recursive() || path == watch_path {
                    handle_event = true;
                    break;
                } else if let Some(parent_path) = path.parent()
                    && parent_path == watch_path
                {
                    handle_event = true;
                    break;
                }
            }
        }

        if !handle_event {
            continue;
        }

        tracing::trace!(?path, ?flag, "FSEvent event received");

        let translated_count = translated_event_count(&flag, true);
        if translated_count == 0 {
            continue;
        }

        let tracked = info
            .watches
            .get(path)
            .is_some_and(|mode| mode.target_mode == TargetMode::TrackPath);
        let (root, create_reported) = if flag.contains(StreamFlags::ROOT_CHANGED) {
            (Some(root_change(info, path, &flag, tracked)), false)
        } else {
            (None, tracked && root_seen(info, path, &flag))
        };
        translate_flags_with(&flag, true, root, create_reported, |mut ev| {
            ev.paths.push(path.to_path_buf());
            emit(ev);
        });
    }
}

/// Looks at the `TrackPath` roots below `path`, which an event says was created, removed or
/// renamed. FSEvents reports nothing for a root that is not a stream path of its own: one that
/// another root covers, one folded into its parent with its siblings, or one below a directory
/// that does not exist yet. The disk says what became of each: a root that is out of reach is
/// reported as removed, one that is back as created.
///
/// The disk is read when the callback runs, not when the event happened, and only a change
/// between there and gone is reported. A directory that goes and comes back before that, or is
/// swapped for another, may leave some roots below it, or all, unreported. Each call also goes
/// through every watch, once more than the filter in the callback does; it runs only for the
/// events of directories and of paths of unknown kind, not for those of files.
fn check_roots_below(info: &StreamContextInfo, path: &Path, emit: &mut impl FnMut(Event)) {
    for (root, mode) in &info.watches {
        if mode.target_mode != TargetMode::TrackPath
            || root.as_path() == path
            || !root.starts_with(path)
        {
            continue;
        }
        match std::fs::metadata(root) {
            Ok(meta) => {
                let mut gone = info.gone_roots.lock();
                if gone.roots.remove(root).is_none() {
                    continue;
                }
                gone.mark_created(root.clone(), file_id(&meta));
                drop(gone);
                emit(root_created(meta.is_dir(), root.clone()));
            }
            // A root that went already, by its own event or not, has nothing more to report.
            Err(_) => {
                if GoneRoots::went(&info.gone_roots, root, &info.event_handler).is_none() {
                    emit(root_gone(root.clone()));
                }
            }
        }
    }
}

/// What a `ROOT_CHANGED` event at `path` says about that root. A root that is not tracked has
/// nothing to say while it is there: the change was its own, and its own events report it.
fn root_change(
    info: &StreamContextInfo,
    path: &Path,
    flag: &StreamFlags,
    tracked: bool,
) -> RootChange {
    match std::fs::metadata(path) {
        Ok(meta) => {
            if !tracked {
                return RootChange::Reported;
            }
            let mut gone = info.gone_roots.lock();
            if gone.roots.remove(path).is_some() {
                // Flags that carry the root's own creation report it; no event is left to use
                // a mark up.
                if !flag.contains(StreamFlags::ITEM_CREATED) {
                    gone.mark_created(path.to_path_buf(), file_id(&meta));
                }
                RootChange::Present {
                    is_dir: meta.is_dir(),
                }
            } else {
                RootChange::Reported
            }
        }
        // A root seen to go by its own event only is reported as changed all the same, whichever
        // of the two FSEvents sends first.
        Err(_) => {
            if !tracked
                || GoneRoots::went(&info.gone_roots, path, &info.event_handler)
                    != Some(Went::RootChange)
            {
                RootChange::Gone
            } else {
                RootChange::StillGone
            }
        }
    }
}

/// A tracked root's own event, which may be the root going or coming back: FSEvents does not
/// always say so through `ROOT_CHANGED`, and never for a root that is not a stream path of its
/// own, so the event is the report. Says whether the creation the event carries was reported
/// already, from the disk.
///
/// The root's first own event since its coming back was reported uses the mark up, whatever it
/// says: it may be a deletion handled once the root is back, and the next creation is then a new
/// one. The creation the event carries is the one reported only while the root is still the file
/// that was there.
fn root_seen(info: &StreamContextInfo, path: &Path, flag: &StreamFlags) -> bool {
    if !flag.intersects(
        StreamFlags::ITEM_CREATED | StreamFlags::ITEM_REMOVED | StreamFlags::ITEM_RENAMED,
    ) {
        return false;
    }
    let id = std::fs::metadata(path).ok().map(|meta| file_id(&meta));
    let mut gone = info.gone_roots.lock();
    let reported = gone.take_created(path, id);
    if id.is_some() {
        gone.roots.remove(path);
    } else if flag.intersects(StreamFlags::ITEM_REMOVED | StreamFlags::ITEM_RENAMED) {
        // The root went: an ancestor's event later finds it gone already.
        gone.roots
            .entry(path.to_path_buf())
            .or_insert(Went::OwnEvent);
    }
    flag.contains(StreamFlags::ITEM_CREATED) && reported
}

impl Watcher for FsEventWatcher {
    /// Create a new watcher.
    #[tracing::instrument(level = "debug", skip(event_handler))]
    fn new<F: EventHandler>(event_handler: F, config: Config) -> Result<Self> {
        Ok(Self::from_event_handler(
            Arc::new(Mutex::new(event_handler)),
            config.max_fsevent_paths(),
        ))
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn watch(&mut self, path: &Path, watch_mode: WatchMode) -> Result<()> {
        self.watch_inner(path, watch_mode)
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn paths_mut<'me>(&'me mut self) -> Box<dyn PathsMut + 'me> {
        Box::new(FsEventPathsMut::new(self))
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn unwatch(&mut self, path: &Path) -> Result<()> {
        self.unwatch_inner(path)
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn configure(&mut self, config: Config) -> Result<bool> {
        let (tx, rx) = unbounded();
        Self::configure_raw_mode(config, &tx);
        rx.recv()?
    }

    fn kind() -> crate::WatcherKind {
        crate::WatcherKind::Fsevent
    }
}

impl Drop for FsEventWatcher {
    fn drop(&mut self) {
        self.stop();
        self.gone_roots.stop();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::{ErrorKind, RecursiveMode, TargetMode};

    use super::*;
    use crate::test::*;

    fn watcher() -> (TestWatcher<FsEventWatcher>, Receiver) {
        channel()
    }

    /// Long enough for a second report of a root to show up: FSEvents sends the root's own
    /// event in a later callback, and the looking thread fires 200 ms after a root went. Its
    /// second look, about 2 s after, is not waited for; see `LOOK_SETTLE`.
    const SETTLE: Duration = Duration::from_millis(500);

    /// Long enough for a report from the looking thread's second look to show up as well.
    const LOOK_SETTLE: Duration = Duration::from_millis(2500);

    /// The events that come within `within`.
    fn events_within(rx: &Receiver, within: Duration) -> Vec<Event> {
        let deadline = Instant::now() + within;
        let mut events = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.rx.recv_timeout(left) {
                Ok(Ok(event)) => events.push(event),
                Ok(Err(e)) => panic!("Got an error from the watcher: {e:?}"),
                Err(_) => break,
            }
        }
        events
    }

    /// The kinds of the events for `path` that report a root change, in order.
    fn root_changes(events: &[Event], path: &Path) -> Vec<EventKind> {
        events
            .iter()
            .filter(|event| event.paths == [path] && event.info() == Some("root changed"))
            .map(|event| event.kind)
            .collect()
    }

    /// How many events for `path` report a creation, with or without a root change.
    fn creates(events: &[Event], path: &Path) -> usize {
        events
            .iter()
            .filter(|event| event.paths == [path] && event.kind.is_create())
            .count()
    }

    /// How many events for `path` report a removal, with or without a root change.
    fn removes(events: &[Event], path: &Path) -> usize {
        events
            .iter()
            .filter(|event| event.paths == [path] && event.kind.is_remove())
            .count()
    }

    /// Makes `count` files in `parent`, which must not exist yet. FSEvents adds a path's recent
    /// flags to its next events, so a file written a moment ago is reported as created again
    /// when it is deleted; the files are written in a directory of their own, renamed to
    /// `parent` afterwards, so that FSEvents holds nothing for their paths.
    fn files_without_history(parent: &Path, count: usize) -> Vec<PathBuf> {
        let staging = parent.with_extension("staging");
        std::fs::create_dir(&staging).expect("create_dir staging");
        for i in 0..count {
            std::fs::write(staging.join(format!("file{i}")), "1").expect("write");
        }
        std::fs::rename(&staging, parent).expect("rename staging");
        (0..count)
            .map(|i| parent.join(format!("file{i}")))
            .collect()
    }

    /// Watches all of `roots` in one go, as rolldown does.
    fn watch_all(watcher: &mut TestWatcher<FsEventWatcher>, roots: &[PathBuf], mode: WatchMode) {
        let mut paths = watcher.watcher.paths_mut();
        for root in roots {
            paths.add(root, mode).expect("add");
        }
        paths.commit().expect("commit");
    }

    /// The paths the stream watches.
    fn stream_paths(watcher: &FsEventWatcher) -> Vec<PathBuf> {
        watcher
            .paths
            .iter()
            .map(|path| PathBuf::from(path.to_string()))
            .collect()
    }

    /// Deletes `root` and makes it again: each is reported once, although the root's coming back
    /// was reported from the disk a moment before.
    fn assert_recreated_once(rx: &Receiver, root: &Path) {
        std::fs::remove_file(root).expect("remove");
        let events = events_within(rx, LOOK_SETTLE);
        assert_eq!(removes(&events, root), 1, "{events:#?}");
        assert_eq!(creates(&events, root), 0, "{events:#?}");

        std::fs::File::create_new(root).expect("create");
        let events = events_within(rx, LOOK_SETTLE);
        assert_eq!(creates(&events, root), 1, "{events:#?}");
    }

    /// Deletes `root` and makes it again at once, a moment after its coming back was reported
    /// from the disk: FSEvents may send the root's own deletion when it is back already, and
    /// the creation that follows is a new one. The root's own events report each once; the
    /// `ROOT_CHANGED` of a root that is a stream path of its own may report the deletion too,
    /// when it finds the root gone.
    fn assert_made_again_at_once(rx: &Receiver, root: &Path) {
        let _ = events_within(rx, Duration::from_millis(300));
        std::fs::remove_file(root).expect("remove");
        std::fs::File::create_new(root).expect("create");
        let events = events_within(rx, LOOK_SETTLE);
        let changes = root_changes(&events, root);
        assert!(
            changes.is_empty() || changes == [EventKind::Remove(RemoveKind::Any)],
            "{events:#?}"
        );
        assert_eq!(removes(&events, root) - changes.len(), 1, "{events:#?}");
        assert_eq!(creates(&events, root), 1, "{events:#?}");
    }

    #[expect(clippy::print_stdout)]
    #[test]
    fn test_fsevent_watcher_drop() {
        use super::*;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();

        let (tx, rx) = std::sync::mpsc::channel();

        {
            let mut watcher = FsEventWatcher::new(tx, Config::default()).unwrap();
            watcher.watch(dir.path(), WatchMode::recursive()).unwrap();
            thread::sleep(Duration::from_millis(2000));
            println!("is running -> {}", watcher.is_running());

            thread::sleep(Duration::from_millis(1000));
            watcher.unwatch(dir.path()).unwrap();
            println!("is running -> {}", watcher.is_running());
        }

        thread::sleep(Duration::from_millis(1000));

        for res in rx {
            let e = res.unwrap();
            println!("debug => {:?} {:?}", e.kind, e.paths);
        }

        println!("in test: {} works", file!());
    }

    #[test]
    fn test_steam_context_info_send_and_sync() {
        fn check_send<T: Send + Sync>() {}
        check_send::<StreamContextInfo>();
    }

    #[test]
    fn callback_impl_handles_non_utf8_paths_without_panicking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::ptr;

        let (tx, rx) = std::sync::mpsc::channel::<crate::Result<Event>>();
        let event_handler: Arc<Mutex<dyn EventHandler>> = Arc::new(Mutex::new(tx));

        let mut watches = HashMap::default();
        watches.insert(PathBuf::from("/tmp"), WatchMode::recursive());

        let context = Box::new(StreamContextInfo {
            event_handler,
            watches,
            gone_roots: Arc::default(),
        });
        let context_ptr = Box::into_raw(context).cast::<libc::c_void>();

        let bytes = b"/tmp/\xff";
        let c_path = CString::new(bytes.as_slice()).expect("cstring");
        let path_ptrs = [c_path.as_ptr()];
        let event_paths =
            NonNull::new(path_ptrs.as_ptr().cast::<libc::c_void>().cast_mut()).unwrap();

        let flags_arr = [StreamFlags::ITEM_CREATED.bits() as fs::FSEventStreamEventFlags];
        let event_flags = NonNull::new(flags_arr.as_ptr().cast_mut()).unwrap();

        let ids_arr = [0 as fs::FSEventStreamEventId];
        let event_ids = NonNull::new(ids_arr.as_ptr().cast_mut()).unwrap();

        let res = std::panic::catch_unwind(|| unsafe {
            callback_impl(
                ptr::null(),
                context_ptr,
                1,
                event_paths,
                event_flags,
                event_ids,
            );
        });
        unsafe {
            drop(Box::from_raw(context_ptr.cast::<StreamContextInfo>()));
        }

        assert!(res.is_ok(), "callback_impl should not panic");

        let event = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("expected event")
            .expect("expected Ok(Event)");
        assert!(
            event.kind.is_create(),
            "expected create event, got {event:?}"
        );
        assert_eq!(event.paths.len(), 1);
        assert_eq!(event.paths[0].as_os_str().as_bytes(), bytes);
    }

    #[test]
    fn callback_impl_ignores_unknown_flag_bits_without_panicking() {
        use std::ffi::CString;
        use std::ptr;

        let (tx, rx) = std::sync::mpsc::channel::<crate::Result<Event>>();
        let event_handler: Arc<Mutex<dyn EventHandler>> = Arc::new(Mutex::new(tx));

        let mut watches = HashMap::default();
        watches.insert(PathBuf::from("/tmp"), WatchMode::recursive());

        let context = Box::new(StreamContextInfo {
            event_handler,
            watches,
            gone_roots: Arc::default(),
        });
        let context_ptr = Box::into_raw(context).cast::<libc::c_void>();

        let c_path = CString::new("/tmp/file").expect("cstring");
        let path_ptrs = [c_path.as_ptr()];
        let event_paths =
            NonNull::new(path_ptrs.as_ptr().cast::<libc::c_void>().cast_mut()).unwrap();

        // Include an unknown bit so the old `from_bits(...).unwrap_or_else(panic!)` behavior
        // would have panicked. New behavior should tolerate it.
        let unknown_mask = !StreamFlags::all().bits();
        let unknown_bit = unknown_mask & unknown_mask.wrapping_neg();
        assert_ne!(unknown_bit, 0, "StreamFlags unexpectedly uses all bits");
        let raw_flag = StreamFlags::ITEM_CREATED.bits() | unknown_bit;
        assert!(
            StreamFlags::from_bits(raw_flag).is_none(),
            "raw_flag must include an unknown bit for this test to be meaningful"
        );

        let flags_arr = [raw_flag as fs::FSEventStreamEventFlags];
        let event_flags = NonNull::new(flags_arr.as_ptr().cast_mut()).unwrap();

        let ids_arr = [0 as fs::FSEventStreamEventId];
        let event_ids = NonNull::new(ids_arr.as_ptr().cast_mut()).unwrap();

        let res = std::panic::catch_unwind(|| unsafe {
            callback_impl(
                ptr::null(),
                context_ptr,
                1,
                event_paths,
                event_flags,
                event_ids,
            );
        });
        unsafe {
            drop(Box::from_raw(context_ptr.cast::<StreamContextInfo>()));
        }

        assert!(res.is_ok(), "callback_impl should not panic");

        let event = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("expected event")
            .expect("expected Ok(Event)");
        assert!(
            event.kind.is_create(),
            "expected create event, got {event:?}"
        );
    }

    #[test]
    fn translate_flags_ignores_is_file_only_events() {
        assert!(translate_flags(&StreamFlags::IS_FILE, true, None, false).is_empty());
        assert!(
            translate_flags(
                &(StreamFlags::IS_FILE | StreamFlags::ITEM_CLONED),
                true,
                None,
                false,
            )
            .is_empty(),
            "type-only clone flags should not produce events"
        );
    }

    #[test]
    fn translate_flags_sets_clone_info_for_file_events() {
        let create = translate_flags(
            &(StreamFlags::ITEM_CREATED | StreamFlags::IS_FILE | StreamFlags::ITEM_CLONED),
            true,
            None,
            false,
        );
        assert_eq!(create.len(), 1);
        assert_eq!(create[0].kind, EventKind::Create(CreateKind::File));
        assert_eq!(create[0].info(), Some("is: clone"));

        let modify = translate_flags(
            &(StreamFlags::INODE_META_MOD
                | StreamFlags::ITEM_MODIFIED
                | StreamFlags::IS_FILE
                | StreamFlags::ITEM_CLONED),
            true,
            None,
            false,
        );
        assert_eq!(modify.len(), 2);
        assert!(
            modify
                .iter()
                .any(|e| matches!(e.kind, EventKind::Modify(ModifyKind::Metadata(_))))
        );
        assert!(
            modify
                .iter()
                .any(|e| matches!(e.kind, EventKind::Modify(ModifyKind::Data(_))))
        );
        assert!(
            modify.iter().all(|e| e.info() == Some("is: clone")),
            "all events should be annotated as clone-related: {modify:?}"
        );
    }

    #[test]
    fn translate_flags_does_not_override_existing_info() {
        let evs = translate_flags(
            &(StreamFlags::ROOT_CHANGED
                | StreamFlags::ITEM_REMOVED
                | StreamFlags::IS_FILE
                | StreamFlags::ITEM_CLONED),
            true,
            None,
            false,
        );
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].info(), Some("root changed"));
    }

    #[test]
    fn translate_flags_reports_what_became_of_a_changed_root() {
        let root_changed = StreamFlags::ROOT_CHANGED;
        assert!(translate_flags(&root_changed, true, Some(RootChange::Reported), false).is_empty());
        assert!(
            translate_flags(&root_changed, true, Some(RootChange::StillGone), false).is_empty()
        );

        for (is_dir, kind) in [(true, CreateKind::Folder), (false, CreateKind::File)] {
            let evs = translate_flags(
                &root_changed,
                true,
                Some(RootChange::Present { is_dir }),
                false,
            );
            assert_eq!(evs.len(), 1, "{evs:?}");
            assert_eq!(evs[0].kind, EventKind::Create(kind));
            assert_eq!(evs[0].info(), Some("root changed"));
        }

        // The flags carry the root's own creation: that is the one report.
        let evs = translate_flags(
            &(StreamFlags::ROOT_CHANGED | StreamFlags::ITEM_CREATED | StreamFlags::IS_FILE),
            true,
            Some(RootChange::Present { is_dir: false }),
            false,
        );
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(evs[0].kind, EventKind::Create(CreateKind::File));
        assert_eq!(evs[0].info(), None);

        for root in [Some(RootChange::Gone), None] {
            let evs = translate_flags(&root_changed, true, root, false);
            assert_eq!(evs.len(), 1, "{evs:?}");
            assert_eq!(evs[0].kind, EventKind::Remove(RemoveKind::Any));
            assert_eq!(evs[0].info(), Some("root changed"));
        }

        let evs = translate_flags(
            &(StreamFlags::ROOT_CHANGED | StreamFlags::ITEM_RENAMED),
            true,
            Some(RootChange::Gone),
            false,
        );
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(
            evs[0].kind,
            EventKind::Modify(ModifyKind::Name(RenameMode::From))
        );
        assert_eq!(evs[0].info(), Some("root changed"));

        let evs = translate_flags(
            &(StreamFlags::ROOT_CHANGED | StreamFlags::ITEM_REMOVED | StreamFlags::IS_DIR),
            true,
            Some(RootChange::Gone),
            false,
        );
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(evs[0].kind, EventKind::Remove(RemoveKind::Folder));
        assert_eq!(evs[0].info(), Some("root changed"));
    }

    #[test]
    fn translate_flags_leaves_out_a_create_reported_already() {
        let flags = StreamFlags::ITEM_CREATED | StreamFlags::ITEM_MODIFIED | StreamFlags::IS_FILE;
        let evs = translate_flags(&flags, true, None, false);
        assert_eq!(evs.len(), 2, "{evs:?}");
        assert_eq!(evs[0].kind, EventKind::Create(CreateKind::File));

        let evs = translate_flags(&flags, true, None, true);
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(
            evs[0].kind,
            EventKind::Modify(ModifyKind::Data(DataChange::Content))
        );
    }

    #[test]
    fn canonicalize_lenient_resolves_the_existing_part() {
        let tmpdir = testdir();
        let real = tmpdir.path().join("real");
        let link = tmpdir.path().join("link");
        std::fs::create_dir(&real).expect("create_dir");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        assert_eq!(canonicalize_lenient(&link), real);
        assert_eq!(
            canonicalize_lenient(&link.join("missing").join("file")),
            real.join("missing").join("file")
        );

        let cwd = std::env::current_dir()
            .and_then(|cwd| cwd.canonicalize())
            .expect("current_dir");
        assert_eq!(
            canonicalize_lenient(Path::new("missing-relative")),
            cwd.join("missing-relative")
        );
    }

    #[test]
    fn missing_root_anchor_is_the_deepest_existing_ancestor() {
        let tmpdir = testdir();
        let anchor = tmpdir.path().join("anchor");
        std::fs::create_dir(&anchor).expect("create_dir");
        let present = anchor.join("present");
        std::fs::write(&present, "").expect("write");

        assert_eq!(missing_root_anchor(&present), None);
        assert_eq!(missing_root_anchor(&anchor.join("missing")), None);
        assert_eq!(
            missing_root_anchor(&anchor.join("missing").join("file")),
            Some(anchor.clone())
        );
        assert_eq!(
            missing_root_anchor(&anchor.join("missing").join("deeper").join("file")),
            Some(anchor)
        );
        assert_eq!(
            missing_root_anchor(Path::new("/notify-rs-missing/missing/file")),
            None,
            "the root of the file system is never watched for a missing root"
        );
    }

    #[test]
    fn missing_root_anchor_is_never_a_shallow_directory() {
        for root in [
            "/Users/notify-rs-missing/project/file",
            "/private/tmp/notify-rs-missing/missing/file",
        ] {
            assert_eq!(missing_root_anchor(Path::new(root)), None, "{root}");
        }
        let home = std::env::var_os("HOME").expect("HOME");
        let root = Path::new(&home)
            .join("notify-rs-missing")
            .join("missing")
            .join("file");
        assert_eq!(missing_root_anchor(&root), None, "{root:?}");
    }

    #[test]
    fn looking_thread_reports_a_root_although_the_handler_panicked() {
        struct PanicsOnce {
            tx: std::sync::mpsc::Sender<Event>,
            panicked: bool,
        }

        impl EventHandler for PanicsOnce {
            fn handle_event(&mut self, event: Result<Event>) {
                if !self.panicked {
                    self.panicked = true;
                    panic!("the handler panics once");
                }
                self.tx.send(event.expect("event")).expect("send");
            }
        }

        let tmpdir = testdir();
        let roots = [tmpdir.path().join("first"), tmpdir.path().join("second")];
        for root in &roots {
            std::fs::write(root, "").expect("write");
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let handler: Arc<Mutex<dyn EventHandler>> = Arc::new(Mutex::new(PanicsOnce {
            tx,
            panicked: false,
        }));
        let gone = Arc::new(GoneRoots::default());
        for root in &roots {
            assert_eq!(GoneRoots::went(&gone, root, &handler), None);
        }

        // Both roots are there; the first report panics, the second must still come.
        let event = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the looking thread went on after the panic");
        assert_eq!(event.kind, EventKind::Create(CreateKind::File));
        assert_eq!(event.info(), Some("root changed"));
        assert!(roots.contains(&event.paths[0]), "{event:?}");
        assert!(gone.lock().roots.is_empty());

        gone.stop();
        assert!(!gone.lock().looking);
    }

    // A mark that was never used up stops standing for a creation after a moment, and goes.
    #[test]
    fn stale_created_mark_is_ignored_and_dropped() {
        let id = (1, 2);
        let mut gone = GoneRootsState::default();
        let old = Instant::now()
            .checked_sub(GoneRootsState::CREATED_REPORTED_FOR + Duration::from_secs(1))
            .expect("instant");
        gone.created_reported
            .insert(PathBuf::from("/stale"), (old, id));
        gone.created_reported
            .insert(PathBuf::from("/stale-too"), (old, id));
        assert!(!gone.take_created(Path::new("/stale"), Some(id)));
        assert!(!gone.created_reported.contains_key(Path::new("/stale")));

        gone.mark_created(PathBuf::from("/fresh"), id);
        assert!(
            !gone.created_reported.contains_key(Path::new("/stale-too")),
            "{gone:?}"
        );
        assert!(gone.take_created(Path::new("/fresh"), Some(id)));
        assert!(
            !gone.take_created(Path::new("/fresh"), Some(id)),
            "a mark is used once"
        );
    }

    // A mark stands only for the file that was there when the root's coming back was reported.
    #[test]
    fn created_mark_stands_for_the_file_that_was_there() {
        let mut gone = GoneRootsState::default();
        gone.mark_created(PathBuf::from("/root"), (1, 2));
        assert!(
            !gone.take_created(Path::new("/root"), Some((1, 3))),
            "another file"
        );
        assert!(
            !gone.take_created(Path::new("/root"), Some((1, 2))),
            "the mark was used up by the other file"
        );

        gone.mark_created(PathBuf::from("/root"), (1, 2));
        assert!(!gone.take_created(Path::new("/root"), None), "no file");
        assert!(gone.created_reported.is_empty(), "{gone:?}");
    }

    /// The callback's context for watches on `roots`, with a handler that sends what it gets to
    /// the channel returned.
    fn stream_context(
        roots: &[(&Path, WatchMode)],
    ) -> (StreamContextInfo, std::sync::mpsc::Receiver<Result<Event>>) {
        let (tx, rx) = std::sync::mpsc::channel::<Result<Event>>();
        let info = StreamContextInfo {
            event_handler: Arc::new(Mutex::new(tx)),
            watches: roots
                .iter()
                .map(|(root, mode)| (root.to_path_buf(), *mode))
                .collect(),
            gone_roots: Arc::default(),
        };
        (info, rx)
    }

    /// Hands `events` to the callback in one batch, as FSEvents does, and returns the kinds and
    /// infos of what the handler got.
    fn feed(
        info: &StreamContextInfo,
        rx: &std::sync::mpsc::Receiver<Result<Event>>,
        events: &[(&Path, StreamFlags)],
    ) -> Vec<(EventKind, Option<String>)> {
        use std::ffi::CString;

        let paths: Vec<CString> = events
            .iter()
            .map(|(path, _)| CString::new(path.as_os_str().as_bytes()).expect("cstring"))
            .collect();
        let path_ptrs: Vec<*const libc::c_char> = paths.iter().map(|path| path.as_ptr()).collect();
        let flags: Vec<fs::FSEventStreamEventFlags> =
            events.iter().map(|(_, flag)| flag.bits()).collect();
        let ids = vec![0 as fs::FSEventStreamEventId; events.len()];
        unsafe {
            callback_impl(
                std::ptr::null(),
                std::ptr::from_ref(info).cast_mut().cast::<libc::c_void>(),
                events.len(),
                NonNull::new(path_ptrs.as_ptr().cast::<libc::c_void>().cast_mut()).expect("paths"),
                NonNull::new(flags.as_ptr().cast_mut()).expect("flags"),
                NonNull::new(ids.as_ptr().cast_mut()).expect("ids"),
            );
        }
        rx.try_iter()
            .map(|event| {
                let event = event.expect("event");
                (event.kind, event.info().map(str::to_string))
            })
            .collect()
    }

    // The root's first own event after its coming back was reported from the disk uses the mark
    // up; a creation it carries is left out only while the root is still the file reported.
    #[test]
    fn own_event_uses_up_the_mark_of_a_reported_creation() {
        const CREATED: StreamFlags = StreamFlags::ITEM_CREATED.union(StreamFlags::IS_FILE);
        const REMOVED: StreamFlags = StreamFlags::ITEM_REMOVED.union(StreamFlags::IS_FILE);
        const PARENT_CREATED: StreamFlags = StreamFlags::ITEM_CREATED.union(StreamFlags::IS_DIR);

        let tmpdir = testdir();
        let parent = tmpdir.path().join("parent");
        let file = parent.join("file");
        std::fs::create_dir(&parent).expect("create_dir");
        std::fs::write(&file, "").expect("write");
        let (info, rx) = stream_context(&[(&file, WatchMode::non_recursive())]);
        let reported_back = || {
            info.gone_roots
                .lock()
                .roots
                .insert(file.clone(), Went::RootChange);
            assert_eq!(
                feed(&info, &rx, &[(&parent, PARENT_CREATED)]),
                [(
                    EventKind::Create(CreateKind::File),
                    Some("root changed".to_string())
                )]
            );
        };

        // The creation reported from the disk is not reported again.
        reported_back();
        assert_eq!(feed(&info, &rx, &[(&file, CREATED)]), []);

        // A deletion handled once the root is back uses the mark up.
        reported_back();
        std::fs::remove_file(&file).expect("remove");
        std::fs::File::create_new(&file).expect("create");
        assert_eq!(
            feed(&info, &rx, &[(&file, REMOVED), (&file, CREATED)]),
            [
                (EventKind::Remove(RemoveKind::File), None),
                (EventKind::Create(CreateKind::File), None)
            ]
        );

        // A creation of another file than the one reported is a new one.
        reported_back();
        std::fs::remove_file(&file).expect("remove");
        std::fs::File::create_new(&file).expect("create");
        assert_eq!(
            feed(&info, &rx, &[(&file, CREATED.union(REMOVED))]),
            [
                (EventKind::Create(CreateKind::File), None),
                (EventKind::Remove(RemoveKind::File), None)
            ]
        );
    }

    // A root that is a stream path of its own gets a `ROOT_CHANGED` when it goes, and is
    // reported as changed whether or not its own event came first. A root folded into its parent
    // gets its own event only, and the parent's event does not report it again.
    #[test]
    fn root_change_reports_a_root_that_went_by_its_own_event() {
        const DIR_REMOVED: StreamFlags = StreamFlags::ITEM_REMOVED.union(StreamFlags::IS_DIR);
        const FILE_REMOVED: StreamFlags = StreamFlags::ITEM_REMOVED.union(StreamFlags::IS_FILE);

        let tmpdir = testdir();
        let root = tmpdir.path().join("root");
        let parent = tmpdir.path().join("parent");
        let folded = parent.join("folded");
        let (info, rx) = stream_context(&[
            (&root, WatchMode::recursive()),
            (&folded, WatchMode::non_recursive()),
        ]);
        let root_gone = (
            EventKind::Remove(RemoveKind::Any),
            Some("root changed".to_string()),
        );

        assert_eq!(
            feed(
                &info,
                &rx,
                &[(&root, DIR_REMOVED), (&root, StreamFlags::ROOT_CHANGED)]
            ),
            [(EventKind::Remove(RemoveKind::Folder), None), root_gone]
        );
        assert_eq!(feed(&info, &rx, &[(&root, StreamFlags::ROOT_CHANGED)]), []);

        assert_eq!(
            feed(
                &info,
                &rx,
                &[(&folded, FILE_REMOVED), (&parent, DIR_REMOVED)]
            ),
            [(EventKind::Remove(RemoveKind::File), None)]
        );
        info.gone_roots.stop();
    }

    // A looker that is done must not reset `looking` once a root going right after has started
    // the next one: a third could then start, and leave the second out of `stop`'s join.
    #[test]
    fn finished_looker_leaves_the_next_one_looking() {
        let gone = GoneRoots::default();
        gone.lock().looking = true;
        let mut looking = LookingGuard {
            gone: &gone,
            armed: true,
        };

        // A root went during the pass: the looker goes on.
        gone.lock().epoch += 1;
        assert!(!gone.done_looking(0, &mut looking));
        assert!(gone.lock().looking);

        assert!(gone.done_looking(1, &mut looking));
        assert!(!gone.lock().looking);
        // A root goes, and starts the next looker, before this one's guard is dropped.
        gone.lock().looking = true;
        drop(looking);
        assert!(
            gone.lock().looking,
            "the finished looker reset the next one's flag"
        );
    }

    #[test]
    fn stop_wakes_the_looking_thread() {
        let (tx, _rx) = std::sync::mpsc::channel::<crate::Result<Event>>();
        let handler: Arc<Mutex<dyn EventHandler>> = Arc::new(Mutex::new(tx));
        let gone = Arc::new(GoneRoots::default());
        assert_eq!(
            GoneRoots::went(&gone, Path::new("/notify-rs-missing/root"), &handler),
            None
        );
        assert!(gone.lock().looking);
        // Past the first look, into the long sleep before the second one.
        thread::sleep(Duration::from_millis(300));

        let started = Instant::now();
        gone.stop();
        assert!(gone.lock().looker.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "stop waited for the looking thread's sleep: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn does_not_crash_with_empty_path() {
        let mut watcher = FsEventWatcher::new(|_| {}, Config::default()).unwrap();

        let watch_result = watcher.watch(Path::new(""), WatchMode::recursive());
        assert!(
            matches!(
                watch_result,
                Err(Error {
                    kind: ErrorKind::PathNotFound,
                    paths: _
                })
            ),
            "actual: {watch_result:#?}"
        );

        let unwatch_result = watcher.unwatch(Path::new(""));
        assert!(
            matches!(
                unwatch_result,
                Err(Error {
                    kind: ErrorKind::WatchNotFound,
                    paths: _
                })
            ),
            "actual: {unwatch_result:#?}"
        );
    }

    #[test]
    fn create_file() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();
        watcher.watch_recursively(&tmpdir);

        let path = tmpdir.path().join("entry");
        std::fs::File::create_new(&path).expect("create");

        rx.wait_unordered([expected(path).create_file()]);
    }

    #[test]
    fn create_self_file() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");

        watcher.watch_nonrecursively(&path);

        std::fs::File::create_new(&path).expect("create");

        rx.wait_ordered_exact([expected(&path).create_file()]);
    }

    #[test]
    fn write_file() {
        let tmpdir = testdir();

        let path = tmpdir.path().join("entry");
        std::fs::File::create_new(&path).expect("create");

        let (mut watcher, rx) = watcher();

        watcher.watch_recursively(&tmpdir);

        std::fs::write(&path, b"123").expect("write");

        rx.wait_unordered([expected(&path).modify_data_content()]);
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

        rx.wait_unordered([expected(&path).modify_meta_owner()]);
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

        rx.wait_unordered([expected(path).rename_any(), expected(new_path).rename_any()]);
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

        rx.wait_unordered([expected(&path).rename_any()]);

        std::fs::rename(&new_path, &path).expect("rename2");

        rx.wait_unordered([expected(&path).rename_any()]);
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

        rx.wait_unordered([expected(&path).rename_any()]);

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

        rx.wait_unordered([expected(&file).remove_file()]);
    }

    #[test]
    fn delete_self_file() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();
        let file = tmpdir.path().join("file");
        std::fs::write(&file, "").expect("write");

        watcher.watch_nonrecursively(&file);

        std::fs::remove_file(&file).expect("remove");

        rx.wait_unordered([expected(&file).remove_file()]);

        std::fs::write(&file, "").expect("write");

        rx.wait_ordered_exact([expected(&file).create_file()]);
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

        rx.wait_unordered([expected(&file).remove_file()]);

        std::fs::write(&file, "").expect("write");

        // rx.ensure_empty_with_wait(); // TODO: should unwatch
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

        rx.wait_unordered([
            expected(&overwriting_file).create(),
            expected(&overwriting_file).modify_data_content().multiple(),
            expected(&overwriting_file).rename_any(),
            expected(&overwritten_file).rename_any(),
        ]);
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

        rx.wait_unordered([expected(&path).create_folder()]);
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

        rx.wait_unordered([expected(&path).modify_meta_owner()]);
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

        rx.wait_ordered([
            expected(&path).rename_any(),
            expected(&new_path).rename_any(),
        ]);
    }

    #[test]
    fn delete_dir() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        std::fs::create_dir(&path).expect("create_dir");

        watcher.watch_recursively(&tmpdir);
        std::fs::remove_dir(&path).expect("remove");

        rx.wait_unordered([expected(path).remove_folder()]);
    }

    #[test]
    fn delete_self_dir() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        std::fs::create_dir(&path).expect("create_dir");

        watcher.watch_recursively(&tmpdir);
        std::fs::remove_dir(&path).expect("remove");

        rx.wait_unordered([expected(&path).remove_folder()]);

        std::fs::create_dir(&path).expect("create_dir2");

        rx.wait_ordered([expected(&path).create_folder()]);
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

        rx.wait_unordered([expected(&path).remove_folder()]);

        std::fs::create_dir(&path).expect("create_dir2");

        // rx.ensure_empty_with_wait(); // TODO: should unwatch
    }

    #[test]
    fn delete_parent_of_watched_dir() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let parent = tmpdir.path().join("parent");
        let child = parent.join("child");
        std::fs::create_dir_all(&child).expect("create_dir_all");

        watcher.watch_recursively(&child);

        std::fs::remove_dir_all(&parent).expect("remove_dir_all");

        rx.wait_unordered([expected(&child).remove_any()]);
    }

    #[test]
    fn rename_parent_of_watched_dir() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let parent = tmpdir.path().join("parent");
        let child = parent.join("child");
        std::fs::create_dir_all(&child).expect("create_dir_all");

        watcher.watch_recursively(&child);

        let new_parent = tmpdir.path().join("renamed_parent");
        std::fs::rename(&parent, &new_parent).expect("rename");

        rx.wait_unordered([expected(&child).remove_any()]);
    }

    #[test]
    fn rename_parent_of_watched_paths_and_back() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let parent = tmpdir.path().join("parent");
        let dir = parent.join("dir");
        let file = parent.join("file");
        std::fs::create_dir_all(&dir).expect("create_dir_all");
        std::fs::write(&file, "1").expect("write");

        watcher.watch_recursively(&dir);
        watcher.watch_nonrecursively(&file);

        let new_parent = tmpdir.path().join("renamed_parent");
        std::fs::rename(&parent, &new_parent).expect("rename away");
        rx.wait_unordered([expected(&dir).remove_any(), expected(&file).remove_any()]);

        std::fs::rename(&new_parent, &parent).expect("rename back");
        rx.wait_unordered([
            expected(&dir).create_folder(),
            expected(&file).create_file(),
        ]);

        std::fs::write(&file, "2").expect("write");
        rx.wait_unordered([expected(&file).modify_data_content()]);
    }

    // Twelve siblings are folded into their parent: FSEvents watches the parent, and says
    // nothing about the files when the parent goes; the callback does.
    #[test]
    fn rename_parent_of_many_roots_and_back() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let parent = tmpdir.path().join("parent");
        std::fs::create_dir(&parent).expect("create_dir");
        let files: Vec<PathBuf> = (0..12).map(|i| parent.join(format!("file{i}"))).collect();
        for file in &files {
            std::fs::write(file, "1").expect("write");
        }
        {
            let mut paths = watcher.watcher.paths_mut();
            for file in &files {
                paths.add(file, WatchMode::non_recursive()).expect("add");
            }
            paths.commit().expect("commit");
        }
        // FSEvents still reports the writes made just before the stream started.
        let _ = events_within(&rx, SETTLE);

        let new_parent = tmpdir.path().join("renamed_parent");
        std::fs::rename(&parent, &new_parent).expect("rename away");
        rx.wait_unordered_exact(files.iter().map(|file| expected(file).remove_any()))
            .ensure_no_tail();

        std::fs::rename(&new_parent, &parent).expect("rename back");
        rx.wait_unordered_exact(files.iter().map(|file| expected(file).create_file()))
            .ensure_no_tail();
        let events = events_within(&rx, SETTLE);
        assert!(events.is_empty(), "reported twice: {events:#?}");

        std::fs::write(&files[0], "2").expect("write");
        rx.wait_unordered([expected(&files[0]).modify_data_content()]);
    }

    // A root two levels below a missing directory: FSEvents' `WatchRoot` does not follow it,
    // the existing ancestor is watched in its place.
    #[test]
    fn nested_missing_root_follows_its_ancestor() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let anchor = tmpdir.path().join("anchor");
        let file = anchor.join("missing").join("deeper").join("file");
        std::fs::create_dir(&anchor).expect("create_dir");
        watcher.watch_nonrecursively(&file);

        std::fs::create_dir_all(file.parent().expect("parent")).expect("create_dir_all");
        std::fs::File::create_new(&file).expect("create");
        let events = events_within(&rx, SETTLE);
        assert_eq!(creates(&events, &file), 1, "{events:#?}");

        let moved_anchor = tmpdir.path().join("moved_anchor");
        std::fs::rename(&anchor, &moved_anchor).expect("rename away");
        rx.wait_ordered_exact([expected(&file).remove_any()])
            .ensure_no_tail();

        std::fs::rename(&moved_anchor, &anchor).expect("rename back");
        rx.wait_ordered_exact([expected(&file).create_file()])
            .ensure_no_tail();
        let events = events_within(&rx, SETTLE);
        assert!(events.is_empty(), "reported twice: {events:#?}");

        std::fs::write(&file, "2").expect("write");
        rx.wait_unordered([expected(&file).modify_data_content()]);

        // The root has no `ROOT_CHANGED` of its own: its own event may be the only report.
        std::fs::remove_dir_all(&anchor).expect("remove_dir_all");
        let events = events_within(&rx, SETTLE);
        assert!(removes(&events, &file) >= 1, "{events:#?}");
    }

    // A root that appears without `ROOT_CHANGED` must not be reported again by the looking
    // thread, which another root starts up much later.
    #[test]
    fn root_that_appeared_is_not_reported_again_when_another_root_goes() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let anchor = tmpdir.path().join("anchor");
        let nested = anchor.join("missing").join("deeper").join("file");
        let parent = tmpdir.path().join("parent");
        let sibling = parent.join("file");
        std::fs::create_dir(&anchor).expect("create_dir anchor");
        std::fs::create_dir(&parent).expect("create_dir parent");
        std::fs::write(&sibling, "").expect("write");
        watcher.watch_nonrecursively(&nested);
        watcher.watch_nonrecursively(&sibling);

        std::fs::create_dir_all(nested.parent().expect("parent")).expect("create_dir_all");
        std::fs::File::create_new(&nested).expect("create");
        // Past both looks of the thread started when the nested root was missing.
        let events = events_within(&rx, Duration::from_millis(2500));
        assert_eq!(creates(&events, &nested), 1, "{events:#?}");

        let moved_parent = tmpdir.path().join("moved_parent");
        std::fs::rename(&parent, &moved_parent).expect("rename away");
        let events = events_within(&rx, SETTLE);
        assert_eq!(
            root_changes(&events, &sibling),
            [EventKind::Remove(RemoveKind::Any)],
            "{events:#?}"
        );
        assert!(
            events.iter().all(|event| event.paths != [nested.clone()]),
            "the nested root was reported again: {events:#?}"
        );
    }

    #[test]
    fn recreated_root_is_reported_created_once() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let file = tmpdir.path().join("file");
        watcher.watch_nonrecursively(&file);

        std::fs::File::create_new(&file).expect("create");
        let events = events_within(&rx, SETTLE);
        assert_eq!(creates(&events, &file), 1, "{events:#?}");

        std::fs::remove_file(&file).expect("remove");
        let events = events_within(&rx, SETTLE);
        assert!(
            events
                .iter()
                .any(|event| event.kind == EventKind::Remove(RemoveKind::File)),
            "{events:#?}"
        );

        std::fs::File::create_new(&file).expect("create again");
        let events = events_within(&rx, SETTLE);
        assert_eq!(creates(&events, &file), 1, "{events:#?}");
    }

    #[test]
    fn recreated_dir_root_is_reported_created_once() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let dir = tmpdir.path().join("dir");
        watcher.watch_recursively(&dir);

        std::fs::create_dir(&dir).expect("create_dir");
        let events = events_within(&rx, SETTLE);
        assert_eq!(creates(&events, &dir), 1, "{events:#?}");

        std::fs::remove_dir(&dir).expect("remove_dir");
        let events = events_within(&rx, SETTLE);
        assert!(
            events
                .iter()
                .any(|event| event.kind == EventKind::Remove(RemoveKind::Folder)),
            "{events:#?}"
        );

        std::fs::create_dir(&dir).expect("create_dir again");
        let events = events_within(&rx, SETTLE);
        assert_eq!(creates(&events, &dir), 1, "{events:#?}");
    }

    // A root that is not tracked reports its own events only; a `ROOT_CHANGED` that finds it
    // there, as after a recreation or an editor's save, says nothing. One that finds it gone
    // still reports the removal, and may come after the root's own event.
    #[test]
    fn recreated_no_track_root_is_not_reported_as_a_root_change() {
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
        let root_created =
            |events: &[Event]| root_changes(events, &file).iter().any(EventKind::is_create);

        std::fs::remove_file(&file).expect("remove");
        let events = events_within(&rx, SETTLE);
        assert!(removes(&events, &file) >= 1, "{events:#?}");

        // FSEvents may report the creation twice; none of them is a root change.
        std::fs::write(&file, "").expect("write again");
        let events = events_within(&rx, SETTLE);
        assert!(creates(&events, &file) >= 1, "{events:#?}");
        assert!(!root_created(&events), "{events:#?}");

        let saved = tmpdir.path().join("file.saved");
        std::fs::write(&saved, "new").expect("write saved");
        std::fs::rename(&saved, &file).expect("rename over");
        let events = events_within(&rx, SETTLE);
        assert!(
            events
                .iter()
                .any(|event| matches!(event.kind, EventKind::Modify(ModifyKind::Name(_)))),
            "{events:#?}"
        );
        assert_eq!(root_changes(&events, &file), [], "{events:#?}");
    }

    // A root that never was there is not removed when the way its path fails changes.
    #[test]
    fn never_present_root_stays_quiet_while_its_ancestor_changes() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let ancestor = tmpdir.path().join("ancestor");
        std::fs::write(&ancestor, "").expect("write");
        let root = ancestor.join("file");
        watcher.watch_nonrecursively(&root);

        std::fs::remove_file(&ancestor).expect("remove");
        let events = events_within(&rx, SETTLE);
        assert!(events.is_empty(), "{events:#?}");

        // Whichever side sees the root first reports it, `ROOT_CHANGED` or the root's own
        // event; the other stays quiet.
        std::fs::create_dir(&ancestor).expect("create_dir");
        std::fs::File::create_new(&root).expect("create");
        let events = events_within(&rx, SETTLE);
        assert_eq!(creates(&events, &root), 1, "{events:#?}");
    }

    // A root below another root is not a stream path of its own; its parent's rename is.
    #[test]
    fn root_below_a_recursive_root_follows_its_parent() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let outer = tmpdir.path().join("outer");
        let dir = outer.join("dir");
        let file = dir.join("file");
        std::fs::create_dir_all(&dir).expect("create_dir_all");
        std::fs::write(&file, "1").expect("write");
        watcher.watch_recursively(&outer);
        watcher.watch_nonrecursively(&file);

        let renamed_dir = outer.join("renamed_dir");
        std::fs::rename(&dir, &renamed_dir).expect("rename away");
        let events = events_within(&rx, SETTLE);
        assert_eq!(
            root_changes(&events, &file),
            [EventKind::Remove(RemoveKind::Any)],
            "{events:#?}"
        );

        std::fs::rename(&renamed_dir, &dir).expect("rename back");
        let events = events_within(&rx, SETTLE);
        assert_eq!(
            root_changes(&events, &file),
            [EventKind::Create(CreateKind::File)],
            "{events:#?}"
        );
    }

    // A root folded into its parent has no `ROOT_CHANGED` of its own. Once the parent came back,
    // the root's own deletion and creation are each reported once.
    #[test]
    fn folded_root_recreated_after_its_parent_came_back() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let parent = tmpdir.path().join("parent");
        let files = files_without_history(&parent, 12);
        watch_all(&mut watcher, &files, WatchMode::non_recursive());
        let _ = events_within(&rx, SETTLE);

        let moved_parent = tmpdir.path().join("moved_parent");
        std::fs::rename(&parent, &moved_parent).expect("rename away");
        rx.wait_unordered(files.iter().map(|file| expected(file).remove_any()));
        std::fs::rename(&moved_parent, &parent).expect("rename back");
        rx.wait_unordered(files.iter().map(|file| expected(file).create_file()));
        let _ = events_within(&rx, SETTLE);

        assert_recreated_once(&rx, &files[0]);
    }

    // The same for a root below a recursive root, whose watch shares the root's events.
    #[test]
    fn root_below_a_recursive_root_recreated_after_its_parent_came_back() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let outer = tmpdir.path().join("outer");
        let dir = outer.join("dir");
        std::fs::create_dir(&outer).expect("create_dir");
        let file = files_without_history(&dir, 1).remove(0);
        watcher.watch_recursively(&outer);
        watcher.watch_nonrecursively(&file);
        let _ = events_within(&rx, SETTLE);

        let renamed_dir = outer.join("renamed_dir");
        std::fs::rename(&dir, &renamed_dir).expect("rename away");
        let events = events_within(&rx, SETTLE);
        assert_eq!(
            root_changes(&events, &file),
            [EventKind::Remove(RemoveKind::Any)],
            "{events:#?}"
        );
        std::fs::rename(&renamed_dir, &dir).expect("rename back");
        let events = events_within(&rx, SETTLE);
        assert_eq!(
            root_changes(&events, &file),
            [EventKind::Create(CreateKind::File)],
            "{events:#?}"
        );

        assert_recreated_once(&rx, &file);
    }

    // A folded root deleted and made again at once, a moment after its parent came back.
    #[test]
    fn folded_root_made_again_at_once_after_its_parent_came_back() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let parent = tmpdir.path().join("parent");
        let files = files_without_history(&parent, 12);
        watch_all(&mut watcher, &files, WatchMode::non_recursive());
        let _ = events_within(&rx, SETTLE);

        let moved_parent = tmpdir.path().join("moved_parent");
        std::fs::rename(&parent, &moved_parent).expect("rename away");
        rx.wait_unordered(files.iter().map(|file| expected(file).remove_any()));
        std::fs::rename(&moved_parent, &parent).expect("rename back");
        rx.wait_unordered(files.iter().map(|file| expected(file).create_file()));

        assert_made_again_at_once(&rx, &files[0]);
    }

    // The same for a root below a recursive root, whose watch shares the root's events.
    #[test]
    fn root_below_a_recursive_root_made_again_at_once_after_its_parent_came_back() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let outer = tmpdir.path().join("outer");
        let dir = outer.join("dir");
        std::fs::create_dir(&outer).expect("create_dir");
        let file = files_without_history(&dir, 1).remove(0);
        watcher.watch_recursively(&outer);
        watcher.watch_nonrecursively(&file);
        let _ = events_within(&rx, SETTLE);

        let renamed_dir = outer.join("renamed_dir");
        std::fs::rename(&dir, &renamed_dir).expect("rename away");
        rx.wait_unordered([expected(&file).remove_any()]);
        std::fs::rename(&renamed_dir, &dir).expect("rename back");
        rx.wait_unordered([expected(&file).create_file()]);

        assert_made_again_at_once(&rx, &file);
    }

    // The same for a root that is a stream path of its own, whose parent came back.
    #[test]
    fn stream_path_root_made_again_at_once_after_its_parent_came_back() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let parent = tmpdir.path().join("parent");
        let file = files_without_history(&parent, 1).remove(0);
        watcher.watch_nonrecursively(&file);
        let _ = events_within(&rx, SETTLE);

        let moved_parent = tmpdir.path().join("moved_parent");
        std::fs::rename(&parent, &moved_parent).expect("rename away");
        rx.wait_unordered([expected(&file).remove_any()]);
        std::fs::rename(&moved_parent, &parent).expect("rename back");
        rx.wait_unordered([expected(&file).create_file()]);

        assert_made_again_at_once(&rx, &file);
    }

    // The same for a root that is renamed away and back itself.
    #[test]
    fn root_made_again_at_once_after_it_was_renamed_back() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let file = files_without_history(&tmpdir.path().join("dir"), 1).remove(0);
        watcher.watch_nonrecursively(&file);
        let _ = events_within(&rx, SETTLE);

        let moved = file.with_extension("moved");
        std::fs::rename(&file, &moved).expect("rename away");
        let _ = events_within(&rx, SETTLE);
        std::fs::rename(&moved, &file).expect("rename back");

        assert_made_again_at_once(&rx, &file);
    }

    // A folded root deleted by its own event is gone already when its parent goes after it.
    #[test]
    fn deleted_folded_root_is_not_removed_again_when_its_parent_goes() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let parent = tmpdir.path().join("parent");
        let files = files_without_history(&parent, 12);
        watch_all(&mut watcher, &files, WatchMode::non_recursive());
        let _ = events_within(&rx, SETTLE);

        std::fs::remove_file(&files[0]).expect("remove");
        let events = events_within(&rx, SETTLE);
        assert_eq!(removes(&events, &files[0]), 1, "{events:#?}");

        let moved_parent = tmpdir.path().join("moved_parent");
        std::fs::rename(&parent, &moved_parent).expect("rename away");
        let events = events_within(&rx, SETTLE);
        assert_eq!(root_changes(&events, &files[0]), [], "{events:#?}");
        for file in &files[1..] {
            assert_eq!(
                root_changes(&events, file),
                [EventKind::Remove(RemoveKind::Any)],
                "{events:#?}"
            );
        }

        std::fs::rename(&moved_parent, &parent).expect("rename back");
        let events = events_within(&rx, LOOK_SETTLE);
        assert_eq!(root_changes(&events, &files[0]), [], "{events:#?}");

        std::fs::File::create_new(&files[0]).expect("create");
        let events = events_within(&rx, LOOK_SETTLE);
        assert_eq!(creates(&events, &files[0]), 1, "{events:#?}");
    }

    // A parent that goes and comes back at once may be seen either way by the callback; every
    // root is reported as back if it was reported gone, and all of them are still watched.
    #[test]
    fn quick_round_trip_of_the_parent_leaves_the_roots_watched() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let few = tmpdir.path().join("few");
        let many = tmpdir.path().join("many");
        let mut files = files_without_history(&few, 2);
        files.extend(files_without_history(&many, 12));
        watch_all(&mut watcher, &files, WatchMode::non_recursive());
        let _ = events_within(&rx, SETTLE);

        for parent in [&few, &many] {
            let moved = parent.with_extension("moved");
            std::fs::rename(parent, &moved).expect("rename away");
            std::fs::rename(&moved, parent).expect("rename back");
        }
        let events = events_within(&rx, LOOK_SETTLE);
        for file in &files {
            let changes = root_changes(&events, file);
            assert!(
                changes.is_empty()
                    || changes
                        == [
                            EventKind::Remove(RemoveKind::Any),
                            EventKind::Create(CreateKind::File)
                        ],
                "{file:?}: {events:#?}"
            );
        }

        for file in &files {
            std::fs::write(file, "2").expect("write");
        }
        rx.wait_unordered(
            files
                .iter()
                .map(|file| expected(file).modify_data_content()),
        );
    }

    // Anchors are found from the roots known to be gone, so a root whose parent went while the
    // stream ran is anchored at the next rebuild.
    #[test]
    fn root_that_went_is_anchored_at_the_next_rebuild() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let anchor = tmpdir.path().join("anchor");
        let missing = anchor.join("missing");
        let other = tmpdir.path().join("other");
        std::fs::create_dir_all(&missing).expect("create_dir_all");
        let file = files_without_history(&missing.join("deeper"), 1).remove(0);
        std::fs::write(&other, "").expect("write other");
        watcher.watch_nonrecursively(&file);
        assert!(!stream_paths(&watcher.watcher).contains(&anchor));

        std::fs::remove_dir_all(&missing).expect("remove_dir_all");
        rx.wait_unordered([expected(&file).remove_any()]);
        watcher.watch_nonrecursively(&other);
        assert!(
            stream_paths(&watcher.watcher).contains(&anchor),
            "{:?}",
            stream_paths(&watcher.watcher)
        );
        // The new stream still reports what happened just before it started.
        let _ = events_within(&rx, SETTLE);

        std::fs::create_dir_all(file.parent().expect("parent")).expect("create_dir_all");
        std::fs::File::create_new(&file).expect("create");
        let events = events_within(&rx, SETTLE);
        assert_eq!(creates(&events, &file), 1, "{events:#?}");
    }

    // A missing path below a symlinked prefix is watched, and unwatched, by the name FSEvents
    // reports.
    #[test]
    fn missing_root_below_a_symlinked_prefix() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let real = tmpdir.path().join("real");
        let link = tmpdir.path().join("link");
        std::fs::create_dir(&real).expect("create_dir");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let file = real.join("missing").join("file");
        let file_through_link = link.join("missing").join("file");
        watcher.watch_nonrecursively(&file_through_link);

        std::fs::create_dir(file.parent().expect("parent")).expect("create_dir");
        std::fs::File::create_new(&file).expect("create");
        rx.wait_ordered([expected(&file).create_file()]);

        watcher
            .watcher
            .unwatch(&file_through_link)
            .expect("unwatch by the name given to watch");
    }

    // The looking thread holds the handler; dropping the watcher must not leave it running.
    #[test]
    fn drop_stops_the_looking_thread() {
        let tmpdir = testdir();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = FsEventWatcher::new(tx, Config::default()).expect("watcher");

        let parent = tmpdir.path().join("parent");
        let file = parent.join("file");
        std::fs::create_dir(&parent).expect("create_dir");
        std::fs::write(&file, "").expect("write");
        watcher
            .watch(&file, WatchMode::non_recursive())
            .expect("watch");

        std::fs::rename(&parent, tmpdir.path().join("moved_parent")).expect("rename away");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !watcher.gone_roots.lock().roots.contains_key(&file) {
            assert!(Instant::now() < deadline, "the root did not go");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(watcher.gone_roots.lock().looking);
        // Past the first look, into the long sleep before the second one.
        thread::sleep(Duration::from_millis(300));

        let started = Instant::now();
        drop(watcher);
        for _ in rx.try_iter() {}
        let after_drop = rx.recv_timeout(Duration::from_millis(500));
        assert!(
            matches!(
                after_drop,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
            ),
            "the handler was still held after the drop: {after_drop:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
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
            expected(&new_path).rename_any(),
            expected(&new_path2).rename_any(),
        ]);
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

        rx.wait_unordered([expected(path).rename_any()]);
    }

    #[test]
    #[ignore = "https://github.com/notify-rs/notify/issues/729"]
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
            expected(&file1).create_file(),
            expected(&file1).modify_data_content(),
            expected(&file2).modify_data_content(),
            expected(&file1).rename_any(),
            expected(&new_path).rename_any(),
            expected(&new_path).modify_data_content(),
            expected(&new_path).remove_file(),
        ]);
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

        rx.wait_unordered([
            expected(&path).rename_any(),
            expected(&new_path1).rename_any(),
            expected(&new_path2).rename_any(),
        ]);
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

        rx.wait_unordered([expected(&path).modify_meta_any()]);
    }

    #[test]
    fn write_file_non_recursive_watch() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        std::fs::File::create_new(&path).expect("create");

        watcher.watch_nonrecursively(&path);

        std::fs::write(&path, b"123").expect("write");

        rx.wait_unordered([expected(path).modify_data_content()]);
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

        rx.wait_unordered([expected(file).modify_data_content()]);
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

        rx.wait_ordered([expected(&deep).modify_data_any().optional()]);

        watcher.watch_recursively(&path);
        std::fs::File::create_new(&file).expect("create");

        rx.wait_ordered([expected(&file).create_file()]);
    }

    // Replaces a test that watched 4097 paths to provoke an `FSEventStreamStart` failure
    // (https://github.com/fsnotify/fsevents/issues/48). That path count is exactly what
    // closes fd 0, so the test corrupted the process it ran in.
    #[test]
    fn refuses_more_paths_than_fsevents_can_carry() {
        let budget = fsevents_path_budget().expect("path budget");
        if budget > 4096 {
            eprintln!("skipping: RLIMIT_NOFILE leaves a budget of {budget} paths");
            return;
        }

        let tmpdir = testdir();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut watcher = FsEventWatcher::new(tx, Config::default().with_max_fsevent_paths(0))
            .expect("create watcher");

        let mut paths = watcher.paths_mut();
        let depth = (usize::BITS - budget.leading_zeros()).max(1);
        for i in 0..=budget {
            // Use a binary tree so the fork's sibling-path consolidation does not
            // merge the paths before the FSEvents safety check sees them.
            let mut path = tmpdir.path().to_path_buf();
            for bit in (0..depth).rev() {
                path.push(if i & (1 << bit) == 0 { "0" } else { "1" });
            }
            std::fs::create_dir_all(&path).expect("create_dir");
            paths.add(&path, WatchMode::non_recursive()).expect("add");
        }
        let err = paths
            .commit()
            .expect_err("watching more paths than the budget must fail");
        assert!(
            matches!(err.kind, ErrorKind::MaxFilesWatch),
            "expected MaxFilesWatch, got {err:?}"
        );
    }

    #[test]
    fn path_budget_is_shared_across_live_streams() {
        static ACTIVE_PATHS: AtomicUsize = AtomicUsize::new(0);

        let first = FseventsPathReservation::acquire(&ACTIVE_PATHS, 15, 21)
            .expect("first stream must fit within the budget");
        let active_path_count = FseventsPathReservation::acquire(&ACTIVE_PATHS, 15, 21)
            .expect_err("the combined path count must exceed the budget");
        assert_eq!(active_path_count, 15);

        drop(first);

        let second = FseventsPathReservation::acquire(&ACTIVE_PATHS, 15, 21)
            .expect("stopping the first stream must release its paths");
        drop(second);
        assert_eq!(ACTIVE_PATHS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn rename_then_remove_remove_event_must_be_the_last_one() {
        let tmpdir = testdir();
        let (mut watcher, rx) = watcher();

        let path = tmpdir.path().join("entry");
        std::fs::File::create_new(&path).expect("create");

        watcher.watch_recursively(&tmpdir);
        let new_path1 = tmpdir.path().join("renamed1");
        let new_path2 = tmpdir.path().join("renamed2");

        std::fs::rename(&path, &new_path1).expect("rename1");
        std::fs::rename(&new_path1, &new_path2).expect("rename2");

        std::fs::remove_file(&new_path2).expect("remove_file");

        loop {
            let ev = rx.recv();
            if matches!(ev.kind, EventKind::Remove(RemoveKind::File)) {
                assert_eq!(&ev.paths, &[new_path2]);
                break;
            }
        }

        rx.ensure_empty();
    }
}
