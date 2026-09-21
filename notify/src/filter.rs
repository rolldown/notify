//! Ignoring paths, see [`Config::with_ignored`](crate::Config::with_ignored)

use std::{
    fmt,
    fs::FileType,
    hash::{Hash, Hasher},
    path::Path,
    sync::Arc,
};
use walkdir::WalkDir;

/// The type of a path passed to an [`IgnoreFilter`].
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub enum EntryKind {
    /// The path is a directory.
    Dir,

    /// The path is anything but a directory.
    File,

    /// The watcher cannot tell, for example because the path does not exist (anymore).
    Unknown,
}

impl From<FileType> for EntryKind {
    fn from(file_type: FileType) -> Self {
        if file_type.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::File
        }
    }
}

type Ignored = dyn Fn(&Path, EntryKind) -> bool + Send + Sync;

/// Decides which paths a watcher ignores.
///
/// Installed with [`Config::with_ignored`](crate::Config::with_ignored). The default filter
/// ignores nothing. Clones share the same function, and two filters are equal only if they do.
#[derive(Clone, Default)]
pub struct IgnoreFilter(Option<Arc<Ignored>>);

impl IgnoreFilter {
    /// Creates a filter that ignores the paths for which `ignored` returns `true`.
    pub fn new(ignored: impl Fn(&Path, EntryKind) -> bool + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(ignored)))
    }

    /// Returns whether the filter function returns `true` for `path` itself.
    #[must_use]
    pub fn matches(&self, path: &Path, kind: EntryKind) -> bool {
        self.0.as_ref().is_some_and(|ignored| ignored(path, kind))
    }

    /// Returns whether `path` is ignored: the filter matches `path` or one of its parent
    /// directories.
    #[must_use]
    pub fn is_ignored(&self, path: &Path, kind: EntryKind) -> bool {
        self.0.is_some()
            && (self.matches(path, kind)
                || parents(path).any(|parent| self.matches(parent, EntryKind::Dir)))
    }

    /// Like [`IgnoreFilter::is_ignored`], but looks up the kind of `path` in the file system.
    #[must_use]
    pub fn is_path_ignored(&self, path: &Path) -> bool {
        // no need to look up the kind if nothing is ignored
        self.0.is_some() && {
            let kind = path
                .metadata()
                .map_or(EntryKind::Unknown, |metadata| metadata.file_type().into());
            self.is_ignored(path, kind)
        }
    }

    /// Walks a directory tree without yielding ignored entries or descending into ignored
    /// directories. The root of the walk is not filtered, it is a watched path or the watcher
    /// already asked about it.
    pub(crate) fn walk(
        &self,
        walk_dir: WalkDir,
    ) -> impl Iterator<Item = walkdir::Result<walkdir::DirEntry>> + use<> {
        let filter = self.clone();
        walk_dir.into_iter().filter_entry(move |entry| {
            entry.depth() == 0 || !filter.matches(entry.path(), entry.file_type().into())
        })
    }

    /// Returns whether an event for `path` must be delivered: a watch covers the path and the
    /// path is not ignored.
    ///
    /// This is for the backends that watch recursively inside the kernel, where an event can
    /// come from anywhere below a watch. `watch` returns whether a path is watched, and whether
    /// recursively. A watch covers the watched path itself, its direct children and, if it is
    /// recursive, everything below.
    #[cfg_attr(
        not(any(
            test,
            target_os = "windows",
            all(target_os = "macos", not(feature = "macos_kqueue"))
        )),
        expect(
            dead_code,
            reason = "the other backends only get events from watched directories"
        )
    )]
    pub(crate) fn is_watched(
        &self,
        path: &Path,
        kind: EntryKind,
        watch: impl Fn(&Path) -> Option<bool>,
    ) -> bool {
        let Some(depth) = path.ancestors().enumerate().position(|(depth, ancestor)| {
            watch(ancestor).is_some_and(|is_recursive| is_recursive || depth <= 1)
        }) else {
            return false;
        };
        // An ignored path is never watched, so only the paths below the watch need to be checked.
        depth == 0
            || !(self.matches(path, kind)
                || parents(path)
                    .take(depth - 1)
                    .any(|parent| self.matches(parent, EntryKind::Dir)))
    }
}

/// The parent directories of `path`, nearest first.
fn parents(path: &Path) -> impl Iterator<Item = &Path> {
    // the last ancestor of a relative path is empty
    path.ancestors()
        .skip(1)
        .filter(|parent| !parent.as_os_str().is_empty())
}

impl fmt::Debug for IgnoreFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            Some(_) => "IgnoreFilter(Some(..))",
            None => "IgnoreFilter(None)",
        })
    }
}

impl PartialEq for IgnoreFilter {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        }
    }
}

impl Eq for IgnoreFilter {}

impl Hash for IgnoreFilter {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0
            .as_ref()
            .map(|ignored| Arc::as_ptr(ignored).cast::<()>())
            .hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_modules() -> IgnoreFilter {
        IgnoreFilter::new(|path, kind| {
            kind == EntryKind::Dir && path.file_name().is_some_and(|name| name == "node_modules")
        })
    }

    fn is_watched(filter: &IgnoreFilter, watches: &[(&str, bool)], path: &str) -> bool {
        filter.is_watched(Path::new(path), EntryKind::File, |ancestor| {
            watches
                .iter()
                .find(|(watch, _)| Path::new(watch) == ancestor)
                .map(|(_, is_recursive)| *is_recursive)
        })
    }

    #[test]
    fn default_filter_ignores_nothing() {
        let filter = IgnoreFilter::default();
        assert!(!filter.is_ignored(Path::new("/p/node_modules"), EntryKind::Dir));
        assert!(is_watched(&filter, &[("/p", true)], "/p/node_modules/a.js"));
    }

    #[test]
    fn unwatched_path_is_not_watched() {
        let filter = IgnoreFilter::default();
        assert!(!is_watched(&filter, &[("/p", true)], "/q/a.js"));
        assert!(is_watched(&filter, &[("/p", false)], "/p"));
        assert!(is_watched(&filter, &[("/p", false)], "/p/a.js"));
        assert!(!is_watched(&filter, &[("/p", false)], "/p/src/a.js"));
    }

    #[test]
    fn ignored_directory_hides_everything_below() {
        let filter = node_modules();
        let watches = [("/p", true)];
        assert!(is_watched(&filter, &watches, "/p/src/a.js"));
        assert!(!is_watched(&filter, &watches, "/p/node_modules/a.js"));
        assert!(!is_watched(&filter, &watches, "/p/node_modules/pkg/a.js"));
        assert!(
            !filter.is_watched(Path::new("/p/node_modules"), EntryKind::Dir, |path| {
                (path == Path::new("/p")).then_some(true)
            })
        );
    }

    #[test]
    fn filter_is_only_asked_about_paths_below_a_watch() {
        let filter = IgnoreFilter::new(|path, _| {
            assert!(path.starts_with("/p/src/") && path != Path::new("/p/src/"));
            false
        });
        assert!(is_watched(&filter, &[("/p/src", true)], "/p/src/a/b.js"));
        assert!(is_watched(
            &filter,
            &[("/p/src/a/b.js", false)],
            "/p/src/a/b.js"
        ));
        assert!(!is_watched(&filter, &[("/p/src", true)], "/q/a.js"));
    }

    #[test]
    fn relative_path_has_no_empty_parent() {
        let filter = IgnoreFilter::new(|path, _| path.as_os_str().is_empty());
        assert!(!filter.is_ignored(Path::new("src/a.js"), EntryKind::File));
    }

    #[test]
    fn path_below_an_ignored_directory_is_ignored() {
        let filter = node_modules();
        assert!(filter.is_ignored(Path::new("/p/node_modules"), EntryKind::Dir));
        assert!(filter.is_ignored(Path::new("/p/node_modules/pkg/a.js"), EntryKind::File));
        assert!(!filter.matches(Path::new("/p/node_modules/pkg/a.js"), EntryKind::File));
        assert!(!filter.is_ignored(Path::new("/p/src/a.js"), EntryKind::File));
        // the filter decides based on the kind
        assert!(!filter.is_ignored(Path::new("/p/node_modules"), EntryKind::File));
    }

    #[test]
    fn clones_are_equal() {
        let filter = node_modules();
        assert_eq!(filter, filter.clone());
        assert_ne!(filter, node_modules());
        assert_ne!(filter, IgnoreFilter::default());
        assert_eq!(IgnoreFilter::default(), IgnoreFilter::default());
    }
}

/// The observable behavior must be the same on every backend, so these tests run against the
/// recommended watcher of the platform.
#[cfg(all(test, not(target_family = "wasm")))]
mod watcher_tests {
    use crate::{Config, RecommendedWatcher, Watcher, test::*};
    use std::{fs, path::Path};

    fn is_ignored(path: &Path) -> bool {
        path.file_name().is_some_and(|name| name == "node_modules")
            || path.extension().is_some_and(|extension| extension == "log")
    }

    fn watcher() -> (TestWatcher<RecommendedWatcher>, Receiver) {
        let config = Config::default().with_ignored(|path, _| is_ignored(path));
        channel_with_config(&ChannelConfig::default().with_watcher_config(config))
    }

    /// Returns the paths of all events, which must never contain an ignored path.
    fn event_paths(rx: &mut Receiver, root: &Path) -> Vec<std::path::PathBuf> {
        let paths: Vec<_> = rx.iter().flat_map(|event| event.paths).collect();
        for path in &paths {
            let below_root = path.strip_prefix(root).unwrap_or(path);
            assert!(
                !below_root.ancestors().any(is_ignored),
                "ignored path was reported: {}",
                path.display()
            );
        }
        paths
    }

    #[test]
    fn ignored_paths_are_not_reported() {
        let tmpdir = testdir();
        let root = tmpdir.path();
        fs::create_dir_all(root.join("node_modules/pkg")).expect("create dir");
        fs::create_dir(root.join("src")).expect("create dir");
        fs::create_dir(root.join("lib")).expect("create dir");

        let (mut watcher, mut rx) = watcher();
        watcher.watch_recursively(root);

        // existing ignored directory
        fs::write(root.join("node_modules/pkg/index.js"), "").expect("write");
        // ignored directory created while watching
        fs::create_dir(root.join("src/node_modules")).expect("create dir");
        fs::write(root.join("src/node_modules/index.js"), "").expect("write");
        // ignored file
        fs::write(root.join("src/debug.log"), "").expect("write");
        fs::remove_file(root.join("src/debug.log")).expect("remove");
        fs::remove_dir_all(root.join("src/node_modules")).expect("remove dir");

        // kqueue registers a directory again when a sub directory is added or removed, which may
        // swallow the events of other entries of that directory, so use another one
        fs::write(root.join("lib/index.js"), "").expect("write");

        let paths = event_paths(&mut rx, root);
        assert!(paths.contains(&root.join("lib/index.js")), "{paths:#?}");
    }

    #[test]
    fn rename_to_an_ignored_path_only_reports_the_other_path() {
        let tmpdir = testdir();
        let root = tmpdir.path();
        fs::write(root.join("a.js"), "").expect("write");
        fs::write(root.join("b.log"), "").expect("write");

        let (mut watcher, mut rx) = watcher();
        watcher.watch_recursively(root);

        fs::rename(root.join("a.js"), root.join("a.log")).expect("rename");
        fs::rename(root.join("b.log"), root.join("b.js")).expect("rename");

        let paths = event_paths(&mut rx, root);
        assert!(paths.contains(&root.join("a.js")), "{paths:#?}");
        assert!(paths.contains(&root.join("b.js")), "{paths:#?}");
    }

    #[test]
    fn watching_an_ignored_path_does_nothing() {
        let tmpdir = testdir();
        let root = tmpdir.path();
        let pkg = root.join("node_modules/pkg");
        fs::create_dir_all(&pkg).expect("create dir");
        fs::write(root.join("debug.log"), "").expect("write");

        let (mut watcher, mut rx) = watcher();
        watcher.watch_recursively(root);
        watcher.watch_recursively(root.join("node_modules"));
        watcher.watch_recursively(&pkg);
        watcher.watch_nonrecursively(root.join("debug.log"));

        fs::write(pkg.join("index.js"), "").expect("write");
        fs::write(root.join("debug.log"), "123").expect("write");
        fs::write(root.join("index.js"), "").expect("write");

        let paths = event_paths(&mut rx, root);
        assert!(paths.contains(&root.join("index.js")), "{paths:#?}");

        watcher.watcher.unwatch(&pkg).expect("unwatch");
        watcher
            .watcher
            .unwatch(&root.join("debug.log"))
            .expect("unwatch");
    }
}
