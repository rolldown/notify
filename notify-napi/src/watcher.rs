use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::{Status, bindgen_prelude::*};
use napi_derive::napi;
use notify::{
    Config, Event, EventKind, PollWatcher, RecommendedWatcher, WatchMode,
    event::{CreateKind, ModifyKind, RemoveKind, RenameMode},
};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer_opt};

use crate::{IgnoredGlobs, ignored_globs, notify_error, path_string};

#[napi(object)]
pub struct WatcherEvent {
    pub kind: String,
    pub path: Option<String>,
    pub is_dir: Option<bool>,
    pub error: Option<String>,
}

#[napi(object)]
pub struct WatcherOptions {
    pub use_polling: Option<bool>,
    pub poll_interval: Option<u32>,
    pub follow_symlinks: Option<bool>,
    pub ignored: Option<Vec<String>>,
}

type WatcherCallback = ThreadsafeFunction<WatcherEvent, (), FnArgs<(WatcherEvent,)>, Status, false>;
type RecommendedDebouncer = Debouncer<RecommendedWatcher, RecommendedCache>;
type PollDebouncer = Debouncer<PollWatcher, RecommendedCache>;

enum Backend {
    Recommended(RecommendedDebouncer),
    Poll(PollDebouncer),
}

impl Backend {
    fn watch(&mut self, path: &Path) -> notify::Result<()> {
        match self {
            Self::Recommended(watcher) => watcher.watch(path, WatchMode::recursive()),
            Self::Poll(watcher) => watcher.watch(path, WatchMode::recursive()),
        }
    }

    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        match self {
            Self::Recommended(watcher) => watcher.unwatch(path),
            Self::Poll(watcher) => watcher.unwatch(path),
        }
    }
}

#[derive(Default)]
struct WatchState {
    excluded: HashSet<PathBuf>,
    roots: HashMap<PathBuf, bool>,
}

impl WatchState {
    fn root_for(&self, path: &Path) -> Option<&Path> {
        self.roots
            .iter()
            .filter(|(root, is_dir)| path == root.as_path() || (**is_dir && path.starts_with(root)))
            .min_by_key(|(root, _)| root.components().count())
            .map(|(root, _)| root.as_path())
    }

    fn includes(&self, path: &Path, ignored: &IgnoredGlobs) -> bool {
        let Some(root) = self.root_for(path) else {
            return false;
        };
        if self.excluded.iter().any(|root| path.starts_with(root)) || ignored.is_match(path) {
            return false;
        }
        for ancestor in path.ancestors().skip(1) {
            if !ancestor.starts_with(root) {
                break;
            }
            if ignored.is_match(ancestor) && !ignored.is_excluded(path) {
                return false;
            }
        }
        true
    }
}

#[napi]
pub struct Watcher {
    inner: Mutex<Option<Backend>>,
    callback: Arc<Mutex<Option<WatcherCallback>>>,
    ignored: Arc<IgnoredGlobs>,
    state: Arc<Mutex<WatchState>>,
    canonicalize_paths: bool,
}

#[napi]
impl Watcher {
    #[napi(constructor)]
    pub fn new(
        callback: Function<'_, FnArgs<(WatcherEvent,)>, ()>,
        options: Option<WatcherOptions>,
    ) -> Result<Self> {
        let callback = std::mem::ManuallyDrop::new(callback);
        let callback: WatcherCallback = callback
            .build_threadsafe_function::<WatcherEvent>()
            .build_callback(|context| {
                Ok(FnArgs {
                    data: (context.value,),
                })
            })?;
        let callback = Arc::new(Mutex::new(Some(callback)));
        let options = options.unwrap_or(WatcherOptions {
            use_polling: None,
            poll_interval: None,
            follow_symlinks: None,
            ignored: None,
        });
        let ignored = Arc::new(ignored_globs(
            options.ignored.as_deref().unwrap_or_default(),
        )?);
        let follow_symlinks = options.follow_symlinks.unwrap_or(true);
        let state = Arc::new(Mutex::new(WatchState::default()));

        let mut config = Config::default().with_follow_symlinks(follow_symlinks);
        if let Some(interval) = options.poll_interval {
            config = config.with_poll_interval(Duration::from_millis(u64::from(interval)));
        }
        if !ignored.is_empty() && !ignored.has_exclusions() {
            let ignored = Arc::clone(&ignored);
            config = config.with_ignored(move |path, _kind| ignored.is_match(path));
        }

        let timeout = Duration::from_millis(5);
        let use_polling = options.use_polling.unwrap_or(false) || cfg!(target_family = "wasm");
        let canonicalize_paths = follow_symlinks
            || (!use_polling
                && <RecommendedWatcher as notify::Watcher>::kind() == notify::WatcherKind::Fsevent);
        if use_polling && options.poll_interval.is_none() {
            config = config.with_poll_interval(Duration::from_millis(100));
        }
        let handler = event_handler(
            Arc::clone(&state),
            Arc::clone(&callback),
            Arc::clone(&ignored),
        );
        let backend = if use_polling {
            Backend::Poll(
                new_debouncer_opt::<_, PollWatcher, RecommendedCache>(
                    timeout,
                    None,
                    handler,
                    RecommendedCache::new(),
                    config,
                )
                .map_err(|error| notify_error(&error))?,
            )
        } else {
            Backend::Recommended(
                new_debouncer_opt::<_, RecommendedWatcher, RecommendedCache>(
                    timeout,
                    None,
                    handler,
                    RecommendedCache::new(),
                    config,
                )
                .map_err(|error| notify_error(&error))?,
            )
        };

        Ok(Self {
            inner: Mutex::new(Some(backend)),
            callback,
            ignored,
            state,
            canonicalize_paths,
        })
    }

    #[napi]
    pub fn add(&self, paths: Vec<String>) -> Result<()> {
        let mut backend = self.lock_backend()?;
        let backend = backend
            .as_mut()
            .ok_or_else(|| Error::new(Status::GenericFailure, "watcher is closed"))?;
        for path in paths {
            let path = self.normalize_path(&path)?;
            let mut state = self
                .state
                .lock()
                .map_err(|_| Error::new(Status::GenericFailure, "watch state lock poisoned"))?;
            if !state.roots.keys().any(|root| path.starts_with(root)) {
                backend.watch(&path).map_err(|error| notify_error(&error))?;
                state.roots.insert(path.clone(), path.is_dir());
            }
            state
                .excluded
                .retain(|excluded| !path.starts_with(excluded));
        }
        Ok(())
    }

    #[napi]
    pub fn remove(&self, paths: Vec<String>) -> Result<()> {
        let mut backend = self.lock_backend()?;
        let backend = backend
            .as_mut()
            .ok_or_else(|| Error::new(Status::GenericFailure, "watcher is closed"))?;
        for path in paths {
            let path = self.normalize_path(&path)?;
            let mut state = self
                .state
                .lock()
                .map_err(|_| Error::new(Status::GenericFailure, "watch state lock poisoned"))?;
            let roots = state
                .roots
                .keys()
                .filter(|root| root.starts_with(&path))
                .cloned()
                .collect::<Vec<_>>();
            for root in roots {
                backend
                    .unwatch(&root)
                    .map_err(|error| notify_error(&error))?;
                state.roots.remove(&root);
            }
            state.excluded.insert(path);
        }
        Ok(())
    }

    #[napi]
    pub fn is_watched(
        &self,
        path: String,
        is_ignored: Option<Function<'_, String, bool>>,
    ) -> Result<bool> {
        if self.lock_backend()?.is_none() {
            return Ok(false);
        }
        let absolute = absolute_path(&path)?;
        let canonical = self.normalize_path(&path)?;
        let state = self
            .state
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "watch state lock poisoned"))?;
        // Polling reports followed links beneath a directory root under their
        // alias. A symlink supplied as a root uses its canonical path instead.
        let path = if state.root_for(&absolute).is_some() {
            absolute
        } else {
            canonical
        };
        if !state.includes(&path, &self.ignored) {
            return Ok(false);
        }
        drop(state);
        // Evaluate JS ignores on the normalized path, on the caller's thread.
        match is_ignored {
            Some(is_ignored) => Ok(!is_ignored.call(path_string(path))?),
            None => Ok(true),
        }
    }

    #[napi]
    pub fn close(&self) -> Result<()> {
        self.callback
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "callback lock poisoned"))?
            .take();
        self.lock_backend()?.take();
        Ok(())
    }

    #[napi(js_name = "ref")]
    #[allow(deprecated)]
    pub fn refer(&self, env: &Env) -> Result<()> {
        self.callback
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "callback lock poisoned"))?
            .as_mut()
            .ok_or_else(|| Error::new(Status::GenericFailure, "watcher is closed"))?
            .refer(env)
    }

    #[napi]
    #[allow(deprecated)]
    pub fn unref(&self, env: &Env) -> Result<()> {
        self.callback
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "callback lock poisoned"))?
            .as_mut()
            .ok_or_else(|| Error::new(Status::GenericFailure, "watcher is closed"))?
            .unref(env)
    }
}

impl Watcher {
    fn lock_backend(&self) -> Result<std::sync::MutexGuard<'_, Option<Backend>>> {
        self.inner
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "watcher lock poisoned"))
    }

    fn normalize_path(&self, path: &str) -> Result<PathBuf> {
        let path = absolute_path(path)?;
        Ok(if self.canonicalize_paths {
            canonicalize_path(&path)
        } else {
            path
        })
    }
}

fn absolute_path(path: &str) -> Result<PathBuf> {
    let path = PathBuf::from(path);
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut absolute = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                absolute.pop();
            }
            _ => absolute.push(component.as_os_str()),
        }
    }
    Ok(absolute)
}

fn event_handler(
    state: Arc<Mutex<WatchState>>,
    callback: Arc<Mutex<Option<WatcherCallback>>>,
    ignored: Arc<IgnoredGlobs>,
) -> impl FnMut(DebounceEventResult) + Send + 'static {
    move |result| {
        let output = match result {
            Ok(events) => {
                let Ok(state) = state.lock() else {
                    return;
                };
                events
                    .iter()
                    .flat_map(|event| translate_event(event))
                    .filter(|event| {
                        event
                            .path
                            .as_ref()
                            .is_some_and(|path| state.includes(Path::new(path), &ignored))
                    })
                    .collect::<Vec<_>>()
            }
            Err(errors) => errors
                .into_iter()
                .map(|error| WatcherEvent {
                    kind: "error".to_string(),
                    path: error.paths.first().map(path_string),
                    is_dir: None,
                    error: Some(error.to_string()),
                })
                .collect(),
        };
        let Ok(callback) = callback.lock() else {
            return;
        };
        if let Some(callback) = callback.as_ref() {
            for event in output {
                let _ = callback.call(event, ThreadsafeFunctionCallMode::NonBlocking);
            }
        }
    }
}

fn translate_event(event: &Event) -> Vec<WatcherEvent> {
    // libuv and Node.js fs.watch() do not reconstruct missed native events.
    // Likewise, ignore notify's rescan requests after event loss.
    if event.need_rescan() {
        return Vec::new();
    }

    let rename_directory = if event.kind == EventKind::Modify(ModifyKind::Name(RenameMode::Both)) {
        event.paths.get(1).map(|path| path.is_dir())
    } else {
        None
    };
    event
        .paths
        .iter()
        .enumerate()
        .filter_map(|(index, path)| {
            let (kind, is_dir) = match event.kind {
                EventKind::Create(kind) => {
                    let is_dir =
                        kind == CreateKind::Folder || (kind != CreateKind::File && path.is_dir());
                    (if is_dir { "addDir" } else { "add" }, is_dir)
                }
                EventKind::Remove(kind) => {
                    let is_dir = kind == RemoveKind::Folder;
                    (if is_dir { "unlinkDir" } else { "unlink" }, is_dir)
                }
                EventKind::Modify(ModifyKind::Name(mode)) => {
                    let is_dir = rename_directory.unwrap_or_else(|| path.is_dir());
                    let removed = match mode {
                        RenameMode::From => true,
                        RenameMode::To => false,
                        RenameMode::Both => index == 0,
                        _ => !path.exists(),
                    };
                    (
                        match (removed, is_dir) {
                            (true, true) => "unlinkDir",
                            (true, false) => "unlink",
                            (false, true) => "addDir",
                            (false, false) => "add",
                        },
                        is_dir,
                    )
                }
                EventKind::Any => ("change", path.is_dir()),
                EventKind::Modify(_) if !path.is_dir() => ("change", false),
                _ => return None,
            };
            Some(WatcherEvent {
                kind: kind.to_string(),
                path: Some(path_string(path)),
                is_dir: Some(is_dir),
                error: None,
            })
        })
        .collect()
}

fn canonicalize_path(path: &Path) -> PathBuf {
    if let Ok(path) = path.canonicalize() {
        return path;
    }
    let mut current = path;
    let mut suffix = Vec::new();
    while let Some(parent) = current.parent() {
        if let Some(name) = current.file_name() {
            suffix.push(name.to_os_string());
        }
        if let Ok(mut canonical) = parent.canonicalize() {
            for name in suffix.iter().rev() {
                canonical.push(name);
            }
            return canonical;
        }
        current = parent;
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rescan_requests_do_not_emit_file_changes() {
        for kind in [
            EventKind::Any,
            EventKind::Modify(ModifyKind::Any),
            EventKind::Other,
        ] {
            let event = Event::new(kind)
                .set_flag(notify::event::Flag::Rescan)
                .add_path(PathBuf::from("/project"));
            assert!(translate_event(&event).is_empty());
        }
    }

    #[test]
    fn modify_and_remove_events_map_to_change_and_unlink() {
        let path = PathBuf::from("/missing/entry.js");
        let changed =
            translate_event(&Event::new(EventKind::Modify(ModifyKind::Any)).add_path(path.clone()));
        assert_eq!(changed[0].kind, "change");
        let removed = translate_event(
            &Event::new(EventKind::Remove(RemoveKind::File)).add_path(path.clone()),
        );
        assert_eq!(removed[0].kind, "unlink");
        let removed_directory =
            translate_event(&Event::new(EventKind::Remove(RemoveKind::Folder)).add_path(path));
        assert_eq!(removed_directory[0].kind, "unlinkDir");
    }

    #[test]
    fn paired_renames_emit_both_sides_in_order() {
        let events = translate_event(
            &Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(PathBuf::from("/missing/before.js"))
                .add_path(PathBuf::from("/missing/after.js")),
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "unlink");
        assert_eq!(events[1].kind, "add");
    }
}
