use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{RecvTimeoutError, sync_channel},
    },
    time::Duration,
};

use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::{Status, bindgen_prelude::*};
use napi_derive::napi;
use notify::{Config, EventKind, PollWatcher, RecommendedWatcher, WatchMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer_opt};
use walkdir::WalkDir;

use crate::{IgnoredGlobs, ignored_globs, notify_error, path_string};

#[napi(object)]
pub struct WatcherEvent {
    pub kind: String,
    pub path: Option<String>,
    pub is_dir: Option<bool>,
    pub error: Option<String>,
}

#[napi(object)]
pub struct WatchedEntry {
    pub path: String,
    pub is_dir: bool,
}

#[napi(object)]
pub struct WatcherOptions {
    pub use_polling: Option<bool>,
    pub poll_interval: Option<u32>,
    pub follow_symlinks: Option<bool>,
    pub ignored: Option<Vec<String>>,
}

type WatcherCallback = ThreadsafeFunction<WatcherEvent, (), FnArgs<(WatcherEvent,)>, Status, false>;
type IgnoreFunction<'a> = Function<'a, FnArgs<(String,)>, bool>;
type IgnoreFunctionRef = FunctionRef<FnArgs<(String,)>, bool>;
type IgnoreCallback = ThreadsafeFunction<String, bool, FnArgs<(String,)>, Status, false>;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
}

impl EntryKind {
    fn is_dir(self) -> bool {
        self == Self::Directory
    }
}

struct WatchState {
    entries: HashMap<PathBuf, EntryKind>,
    excluded: HashSet<PathBuf>,
    roots: HashSet<PathBuf>,
}

impl WatchState {
    fn reconcile(
        &mut self,
        path: &Path,
        snapshot: ScanSnapshot,
        emit_events: bool,
        change_existing: bool,
    ) -> Vec<WatcherEvent> {
        if self.is_excluded(path) {
            return Vec::new();
        }

        let ScanSnapshot {
            mut entries,
            parent,
            mut errors,
        } = snapshot;
        let previous = self
            .entries
            .iter()
            .filter(|(known, _)| known.starts_with(path))
            .map(|(known, kind)| (known.clone(), *kind))
            .collect::<HashMap<_, _>>();
        let was_directory = previous.get(path) == Some(&EntryKind::Directory);
        let mut removed = previous.iter().collect::<Vec<_>>();
        removed.sort_by_key(|(known, _)| std::cmp::Reverse(known.components().count()));
        for (known, old_kind) in removed {
            let new_kind = entries.get(known).copied();
            if new_kind != Some(*old_kind) && emit_events {
                errors.push(path_event(
                    if old_kind.is_dir() {
                        "unlinkDir"
                    } else {
                        "unlink"
                    },
                    known,
                    old_kind.is_dir(),
                ));
            }
            self.entries.remove(known);
        }

        let mut current = entries.drain().collect::<Vec<_>>();
        current.sort_by_key(|(known, _)| known.components().count());
        for (known, kind) in current {
            let old_kind = previous.get(&known).copied();
            self.entries.insert(known.clone(), kind);
            if emit_events && old_kind != Some(kind) {
                errors.push(path_event(
                    if kind.is_dir() { "addDir" } else { "add" },
                    &known,
                    kind.is_dir(),
                ));
            } else if emit_events && change_existing && known == path && kind == EntryKind::File {
                errors.push(path_event("change", &known, false));
            }
        }
        if !was_directory && let Some(parent) = parent {
            self.entries.entry(parent).or_insert(EntryKind::Directory);
        }
        errors
    }

    fn forget(&mut self, path: &Path) {
        self.entries.retain(|known, _| !known.starts_with(path));
    }

    fn prune_entries(&mut self) {
        self.entries.retain(|known, _| {
            self.roots
                .iter()
                .any(|root| known.starts_with(root) || root.starts_with(known))
        });
    }

    fn is_excluded(&self, path: &Path) -> bool {
        self.excluded.iter().any(|root| path.starts_with(root))
    }

    fn add_root(&mut self, path: PathBuf) {
        if self.roots.iter().any(|root| path.starts_with(root)) {
            return;
        }
        self.roots.retain(|root| !root.starts_with(&path));
        self.roots.insert(path);
    }
}

struct ScanSnapshot {
    entries: HashMap<PathBuf, EntryKind>,
    parent: Option<PathBuf>,
    errors: Vec<WatcherEvent>,
}

#[derive(Clone)]
struct IgnoreMatcher {
    globs: Arc<IgnoredGlobs>,
    callback: Arc<Mutex<Option<IgnoreCallback>>>,
    closed: Arc<AtomicBool>,
}

impl IgnoreMatcher {
    fn matches(&self, path: &Path, direct: Option<&IgnoreFunction<'_>>) -> Result<bool> {
        if self.globs.is_excluded(path) {
            return Ok(false);
        }
        if self.globs.is_included(path) {
            return Ok(true);
        }
        self.matches_callback(path, direct)
    }

    fn matches_ancestor(
        &self,
        ancestor: &Path,
        path: &Path,
        direct: Option<&IgnoreFunction<'_>>,
    ) -> Result<bool> {
        if self.globs.is_included(ancestor)
            && !self.globs.is_excluded(ancestor)
            && !self.globs.is_excluded(path)
        {
            return Ok(true);
        }
        self.matches_callback(ancestor, direct)
    }

    fn matches_callback(&self, path: &Path, direct: Option<&IgnoreFunction<'_>>) -> Result<bool> {
        let path = path_string(path);
        if let Some(function) = direct {
            return function.call(FnArgs { data: (path,) });
        }
        let (sender, receiver) = sync_channel(1);
        let status = {
            let callback = self
                .callback
                .lock()
                .map_err(|_| Error::new(Status::GenericFailure, "ignore callback lock poisoned"))?;
            let Some(callback) = callback.as_ref() else {
                return Ok(false);
            };
            callback.call_with_return_value(
                path,
                ThreadsafeFunctionCallMode::NonBlocking,
                move |result, _env| {
                    let _ = sender.send(result.unwrap_or(false));
                    Ok(())
                },
            )
        };
        if status != Status::Ok {
            return Ok(false);
        }
        loop {
            match receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(ignored) => return Ok(ignored),
                Err(RecvTimeoutError::Timeout) if !self.closed.load(Ordering::Acquire) => {}
                Err(_) => return Ok(false),
            }
        }
    }

    #[allow(deprecated)]
    fn refer(&self, env: &Env) -> Result<()> {
        if let Some(callback) = self
            .callback
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "ignore callback lock poisoned"))?
            .as_mut()
        {
            callback.refer(env)?;
        }
        Ok(())
    }

    #[allow(deprecated)]
    fn unref(&self, env: &Env) -> Result<()> {
        if let Some(callback) = self
            .callback
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "ignore callback lock poisoned"))?
            .as_mut()
        {
            callback.unref(env)?;
        }
        Ok(())
    }

    fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::Release);
        self.callback
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "ignore callback lock poisoned"))?
            .take();
        Ok(())
    }
}

#[derive(Clone)]
struct Scanner {
    ignored: IgnoreMatcher,
    follow_symlinks: bool,
}

impl Scanner {
    fn is_ignored(
        &self,
        path: &Path,
        root: &Path,
        direct: Option<&IgnoreFunction<'_>>,
    ) -> Result<bool> {
        if self.ignored.matches(path, direct)? {
            return Ok(true);
        }
        let mut candidate = path;
        while candidate != root {
            let Some(parent) = candidate.parent() else {
                break;
            };
            candidate = parent;
            if self.ignored.matches_ancestor(candidate, path, direct)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn scan(
        &self,
        path: &Path,
        root: &Path,
        excluded: &HashSet<PathBuf>,
        direct: Option<&IgnoreFunction<'_>>,
    ) -> Result<ScanSnapshot> {
        let is_excluded =
            |candidate: &Path| excluded.iter().any(|root| candidate.starts_with(root));
        if is_excluded(path) || self.is_ignored(path, root, direct)? {
            return Ok(ScanSnapshot {
                entries: HashMap::new(),
                parent: None,
                errors: Vec::new(),
            });
        }

        let mut entries = HashMap::new();
        let mut errors = Vec::new();
        let has_exclusions = self.ignored.globs.has_exclusions();
        let walker = WalkDir::new(path)
            .follow_links(self.follow_symlinks)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|entry| {
                entry.depth() == 0
                    || (!is_excluded(entry.path())
                        && (has_exclusions
                            || !self.ignored.matches(entry.path(), direct).unwrap_or(false)))
            });
        for entry in walker {
            match entry {
                Ok(entry) => {
                    if has_exclusions
                        && self.is_ignored(entry.path(), path, direct).unwrap_or(false)
                    {
                        continue;
                    }
                    let kind = if entry.file_type().is_dir() {
                        EntryKind::Directory
                    } else {
                        EntryKind::File
                    };
                    entries.insert(entry.path().to_path_buf(), kind);
                }
                Err(error) if path.exists() => errors.push(error_event(error.to_string())),
                Err(_) => {}
            }
        }
        let parent = if entries.get(path) == Some(&EntryKind::File) {
            path.parent()
                .filter(|parent| {
                    parent.is_dir()
                        && !is_excluded(parent)
                        && !self.ignored.matches(parent, direct).unwrap_or(false)
                })
                .map(Path::to_path_buf)
        } else {
            None
        };
        Ok(ScanSnapshot {
            entries,
            parent,
            errors,
        })
    }
}

#[napi]
pub struct Watcher {
    inner: Mutex<Option<Backend>>,
    callback: Arc<Mutex<Option<WatcherCallback>>>,
    ignored_function: Mutex<Option<IgnoreFunctionRef>>,
    scanner: Scanner,
    state: Arc<Mutex<WatchState>>,
    native_roots: Mutex<HashSet<PathBuf>>,
    canonicalize_paths: bool,
}

#[napi]
impl Watcher {
    #[napi(constructor)]
    pub fn new(
        callback: Function<'_, FnArgs<(WatcherEvent,)>, ()>,
        options: Option<WatcherOptions>,
        ignored_function: Option<Function<'_, FnArgs<(String,)>, bool>>,
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
        let ignored_function_ref = ignored_function
            .as_ref()
            .map(Function::create_ref)
            .transpose()?;
        let ignored_callback = ignored_function
            .map(|function| {
                function
                    .build_threadsafe_function::<String>()
                    .build_callback(|context| {
                        Ok(FnArgs {
                            data: (context.value,),
                        })
                    })
            })
            .transpose()?;
        let ignored = IgnoreMatcher {
            globs: Arc::clone(&ignored),
            callback: Arc::new(Mutex::new(ignored_callback)),
            closed: Arc::new(AtomicBool::new(false)),
        };
        let follow_symlinks = options.follow_symlinks.unwrap_or(true);
        let scanner = Scanner {
            ignored,
            follow_symlinks,
        };
        let state = Arc::new(Mutex::new(WatchState {
            entries: HashMap::new(),
            excluded: HashSet::new(),
            roots: HashSet::new(),
        }));

        let mut config = Config::default();
        if let Some(interval) = options.poll_interval {
            config = config.with_poll_interval(Duration::from_millis(u64::from(interval)));
        }
        if let Some(follow_symlinks) = options.follow_symlinks {
            config = config.with_follow_symlinks(follow_symlinks);
        }
        if !scanner.ignored.globs.is_empty() && !scanner.ignored.globs.has_exclusions() {
            let ignored = Arc::clone(&scanner.ignored.globs);
            config = config.with_ignored(move |path, _kind| ignored.is_match(path));
        }

        let timeout = Duration::from_millis(5);
        let use_polling = options.use_polling.unwrap_or(false) || cfg!(target_family = "wasm");
        if use_polling && options.poll_interval.is_none() {
            config = config.with_poll_interval(Duration::from_millis(100));
        }
        let backend = if use_polling {
            Backend::Poll(
                new_debouncer_opt::<_, PollWatcher, RecommendedCache>(
                    timeout,
                    None,
                    event_handler(Arc::clone(&state), Arc::clone(&callback), scanner.clone()),
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
                    event_handler(Arc::clone(&state), Arc::clone(&callback), scanner.clone()),
                    RecommendedCache::new(),
                    config,
                )
                .map_err(|error| notify_error(&error))?,
            )
        };

        Ok(Self {
            inner: Mutex::new(Some(backend)),
            callback,
            ignored_function: Mutex::new(ignored_function_ref),
            scanner,
            state,
            native_roots: Mutex::new(HashSet::new()),
            canonicalize_paths: follow_symlinks,
        })
    }

    #[napi]
    pub fn add(
        &self,
        env: &Env,
        paths: Vec<String>,
        emit_initial: bool,
    ) -> Result<Vec<WatcherEvent>> {
        let paths = paths
            .into_iter()
            .map(|path| {
                let path = PathBuf::from(path);
                if self.canonicalize_paths {
                    canonicalize_path(&path)
                } else {
                    path
                }
            })
            .collect::<Vec<_>>();
        {
            let mut backend = self.lock_backend()?;
            let backend = backend
                .as_mut()
                .ok_or_else(|| Error::new(Status::GenericFailure, "watcher is closed"))?;
            let mut native_roots = self
                .native_roots
                .lock()
                .map_err(|_| Error::new(Status::GenericFailure, "watch roots lock poisoned"))?;

            for path in &paths {
                if !native_roots.iter().any(|root| path.starts_with(root)) {
                    backend.watch(path).map_err(|error| notify_error(&error))?;
                    native_roots.insert(path.clone());
                }
            }
        }

        let ignored_function = self
            .ignored_function
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "ignore function lock poisoned"))?;
        let ignored_function = ignored_function
            .as_ref()
            .map(|function| function.borrow_back(env))
            .transpose()?;
        let mut events = Vec::new();
        for path in paths {
            let mut excluded = self
                .state
                .lock()
                .map_err(|_| Error::new(Status::GenericFailure, "watch state lock poisoned"))?
                .excluded
                .clone();
            excluded.retain(|excluded| !path.starts_with(excluded));
            let snapshot = self
                .scanner
                .scan(&path, &path, &excluded, ignored_function.as_ref())?;
            let mut state = self
                .state
                .lock()
                .map_err(|_| Error::new(Status::GenericFailure, "watch state lock poisoned"))?;
            state
                .excluded
                .retain(|excluded| !path.starts_with(excluded));
            state.add_root(path.clone());
            events.extend(state.reconcile(&path, snapshot, emit_initial, false));
        }
        Ok(events)
    }

    #[napi]
    pub fn remove(&self, paths: Vec<String>) -> Result<()> {
        let paths = paths
            .into_iter()
            .map(|path| {
                let path = PathBuf::from(path);
                if self.canonicalize_paths {
                    canonicalize_path(&path)
                } else {
                    path
                }
            })
            .collect::<Vec<_>>();
        {
            let mut backend = self.lock_backend()?;
            let backend = backend
                .as_mut()
                .ok_or_else(|| Error::new(Status::GenericFailure, "watcher is closed"))?;
            let mut native_roots = self
                .native_roots
                .lock()
                .map_err(|_| Error::new(Status::GenericFailure, "watch roots lock poisoned"))?;
            for path in &paths {
                let removed = native_roots
                    .iter()
                    .filter(|root| root.starts_with(path))
                    .cloned()
                    .collect::<Vec<_>>();
                for root in removed {
                    backend
                        .unwatch(&root)
                        .map_err(|error| notify_error(&error))?;
                    native_roots.remove(&root);
                }
            }
        }

        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "watch state lock poisoned"))?;
        for path in paths {
            state.excluded.insert(path.clone());
            state.roots.retain(|root| !root.starts_with(&path));
            state.forget(&path);
            state.prune_entries();
        }
        Ok(())
    }

    #[napi]
    pub fn entries(&self) -> Result<Vec<WatchedEntry>> {
        let state = self
            .state
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "watch state lock poisoned"))?;
        Ok(state
            .entries
            .iter()
            .map(|(path, kind)| WatchedEntry {
                path: path_string(path),
                is_dir: kind.is_dir(),
            })
            .collect())
    }

    #[napi]
    pub fn close(&self) -> Result<()> {
        self.scanner.ignored.close()?;
        self.lock_backend()?.take();
        self.callback
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "callback lock poisoned"))?
            .take();
        self.ignored_function
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "ignore function lock poisoned"))?
            .take();
        self.state
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "watch state lock poisoned"))?
            .entries
            .clear();
        Ok(())
    }

    #[napi(js_name = "ref")]
    #[allow(deprecated)]
    pub fn refer(&self, env: &Env) -> Result<()> {
        let mut callback = self
            .callback
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "callback lock poisoned"))?;
        callback
            .as_mut()
            .ok_or_else(|| Error::new(Status::GenericFailure, "watcher is closed"))?
            .refer(env)?;
        self.scanner.ignored.refer(env)
    }

    #[napi]
    #[allow(deprecated)]
    pub fn unref(&self, env: &Env) -> Result<()> {
        let mut callback = self
            .callback
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "callback lock poisoned"))?;
        callback
            .as_mut()
            .ok_or_else(|| Error::new(Status::GenericFailure, "watcher is closed"))?
            .unref(env)?;
        self.scanner.ignored.unref(env)
    }
}

impl Watcher {
    fn lock_backend(&self) -> Result<std::sync::MutexGuard<'_, Option<Backend>>> {
        self.inner
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "watcher lock poisoned"))
    }
}

fn event_handler(
    state: Arc<Mutex<WatchState>>,
    callback: Arc<Mutex<Option<WatcherCallback>>>,
    scanner: Scanner,
) -> impl FnMut(DebounceEventResult) + Send + 'static {
    move |result| {
        let output = match result {
            Ok(events) => {
                let mut output = Vec::new();
                for event in events {
                    if event.need_rescan() {
                        output.extend(rescan(&state, &scanner));
                        continue;
                    }
                    if !matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    ) {
                        continue;
                    }
                    for path in &event.paths {
                        let Ok((excluded, root)) = state.lock().map(|state| {
                            let root = state
                                .roots
                                .iter()
                                .filter(|root| path.starts_with(root))
                                .max_by_key(|root| root.components().count())
                                .cloned()
                                .unwrap_or_else(|| path.clone());
                            (state.excluded.clone(), root)
                        }) else {
                            return;
                        };
                        if excluded.iter().any(|root| path.starts_with(root)) {
                            continue;
                        }
                        let snapshot = match scanner.scan(path, &root, &excluded, None) {
                            Ok(snapshot) => snapshot,
                            Err(error) => {
                                output.push(error_event(error.to_string()));
                                continue;
                            }
                        };
                        let Ok(mut state) = state.lock() else {
                            return;
                        };
                        output.extend(state.reconcile(path, snapshot, true, true));
                    }
                }
                output
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
        let Some(callback) = callback.as_ref() else {
            return;
        };
        for event in output {
            let _ = callback.call(event, ThreadsafeFunctionCallMode::NonBlocking);
        }
    }
}

fn rescan(state: &Mutex<WatchState>, scanner: &Scanner) -> Vec<WatcherEvent> {
    let Ok(state_guard) = state.lock() else {
        return Vec::new();
    };
    let roots = state_guard.roots.iter().cloned().collect::<Vec<_>>();
    let excluded = state_guard.excluded.clone();
    drop(state_guard);

    let mut output = Vec::new();
    for root in roots {
        let existing_files = match state.lock() {
            Ok(state) => state
                .entries
                .iter()
                .filter(|(path, kind)| path.starts_with(&root) && **kind == EntryKind::File)
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>(),
            Err(_) => return output,
        };
        let snapshot = match scanner.scan(&root, &root, &excluded, None) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                output.push(error_event(error.to_string()));
                continue;
            }
        };
        let Ok(mut state) = state.lock() else {
            return output;
        };
        output.extend(state.reconcile(&root, snapshot, true, false));
        for path in existing_files {
            if state.entries.get(&path) == Some(&EntryKind::File) {
                output.push(path_event("change", &path, false));
            }
        }
    }
    output
}

fn path_event(kind: &str, path: &Path, is_dir: bool) -> WatcherEvent {
    WatcherEvent {
        kind: kind.to_string(),
        path: Some(path_string(path)),
        is_dir: Some(is_dir),
        error: None,
    }
}

fn error_event(error: String) -> WatcherEvent {
    WatcherEvent {
        kind: "error".to_string(),
        path: None,
        is_dir: None,
        error: Some(error),
    }
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
    use std::{fs, time::SystemTime};

    use super::*;

    #[test]
    fn rescan_reconciles_the_watched_snapshot() {
        let root = std::env::temp_dir().join(format!(
            "rolldown-notify-rescan-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let removed = root.join("removed.js");
        let added = root.join("added.js");
        let changed = root.join("changed.js");
        fs::write(&removed, "removed").unwrap();
        fs::write(&changed, "before").unwrap();

        let scanner = Scanner {
            ignored: IgnoreMatcher {
                globs: Arc::new(ignored_globs(&[]).unwrap()),
                callback: Arc::new(Mutex::new(None)),
                closed: Arc::new(AtomicBool::new(false)),
            },
            follow_symlinks: true,
        };
        let mut state = WatchState {
            entries: HashMap::new(),
            excluded: HashSet::new(),
            roots: HashSet::from([root.clone()]),
        };
        state.add_root(removed.clone());
        assert_eq!(state.roots, HashSet::from([root.clone()]));
        state.roots.clear();
        state.add_root(removed.clone());
        state.add_root(root.clone());
        assert_eq!(state.roots, HashSet::from([root.clone()]));
        let snapshot = scanner.scan(&root, &root, &HashSet::new(), None).unwrap();
        state.reconcile(&root, snapshot, false, false);
        fs::remove_file(&removed).unwrap();
        fs::write(&added, "added").unwrap();
        fs::write(&changed, "after").unwrap();

        let state = Mutex::new(state);
        let events = rescan(&state, &scanner);
        assert!(events.iter().any(|event| {
            event.kind == "unlink" && event.path.as_deref() == Some(path_string(&removed).as_str())
        }));
        assert!(events.iter().any(|event| {
            event.kind == "add" && event.path.as_deref() == Some(path_string(&added).as_str())
        }));
        assert!(events.iter().any(|event| {
            event.kind == "change" && event.path.as_deref() == Some(path_string(&changed).as_str())
        }));
        assert!(!state.lock().unwrap().entries.contains_key(&removed));
        assert_eq!(
            state.lock().unwrap().entries.get(&added),
            Some(&EntryKind::File)
        );

        fs::remove_dir_all(&root).unwrap();
        let events = rescan(&state, &scanner);
        assert!(events.iter().any(|event| {
            event.kind == "unlinkDir" && event.path.as_deref() == Some(path_string(&root).as_str())
        }));
        assert!(state.lock().unwrap().entries.is_empty());

        let _ = fs::remove_dir_all(root);
    }
}
