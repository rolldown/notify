//! Ignoring paths, see [`Config::with_ignored`](crate::Config::with_ignored)

use std::{fmt, fs::FileType, path::Path, sync::Arc};
use walkdir::WalkDir;

/// The type of a path passed to an [`IgnoreFilter`].
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub enum EntryKind {
    /// The path is a directory.
    Dir,

    /// The path is anything but a directory.
    File,

    /// The watcher cannot tell, for example because the path does not exist.
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

/// The ignore filter of a watcher, see [`Config::with_ignored`](crate::Config::with_ignored).
///
/// The default filter ignores nothing. Clones share the same filter function.
#[derive(Clone, Default)]
pub struct IgnoreFilter(Option<Arc<Ignored>>);

impl IgnoreFilter {
    /// Creates a filter that ignores the paths for which `ignored` returns `true`.
    pub fn new(ignored: impl Fn(&Path, EntryKind) -> bool + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(ignored)))
    }

    /// Returns whether the filter ignores `path`.
    #[must_use]
    pub fn is_ignored(&self, path: &Path, kind: EntryKind) -> bool {
        self.0.as_ref().is_some_and(|ignored| ignored(path, kind))
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

    /// Walks a directory tree, skipping ignored entries and everything inside them.
    ///
    /// The root of the walk is not filtered, the caller has already checked it.
    pub(crate) fn walk(
        &self,
        walk_dir: WalkDir,
    ) -> impl Iterator<Item = walkdir::Result<walkdir::DirEntry>> + use<> {
        let filter = self.clone();
        walk_dir.into_iter().filter_entry(move |entry| {
            entry.depth() == 0 || !filter.is_ignored(entry.path(), entry.file_type().into())
        })
    }
}

impl fmt::Debug for IgnoreFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            Some(_) => "IgnoreFilter(Some(..))",
            None => "IgnoreFilter(None)",
        })
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    use crate::test::testdir;
    use std::{
        fs,
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    #[test]
    fn default_filter_ignores_nothing() {
        let filter = IgnoreFilter::default();
        assert!(!filter.is_ignored(Path::new("/p/node_modules"), EntryKind::Dir));
        assert!(!filter.is_path_ignored(Path::new("/p/node_modules/a.js")));
    }

    #[test]
    fn is_path_ignored_looks_up_the_kind() {
        let tmpdir = testdir();
        let root = tmpdir.path();
        fs::create_dir(root.join("dir")).expect("create dir");
        fs::write(root.join("file"), "").expect("write");

        let asked = Arc::new(Mutex::new(Vec::new()));
        let filter = IgnoreFilter::new({
            let asked = Arc::clone(&asked);
            move |path, kind| {
                asked.lock().unwrap().push((path.to_path_buf(), kind));
                true
            }
        });
        assert!(filter.is_path_ignored(&root.join("dir")));
        assert!(filter.is_path_ignored(&root.join("file")));
        assert!(filter.is_path_ignored(&root.join("missing")));

        assert_eq!(
            *asked.lock().unwrap(),
            [
                (root.join("dir"), EntryKind::Dir),
                (root.join("file"), EntryKind::File),
                (root.join("missing"), EntryKind::Unknown),
            ]
        );
    }

    #[test]
    fn walk_does_not_descend_into_ignored_directories() {
        let tmpdir = testdir();
        let root = tmpdir.path();
        fs::create_dir_all(root.join("node_modules/pkg")).expect("create dir");
        fs::write(root.join("node_modules/pkg/index.js"), "").expect("write");
        fs::create_dir(root.join("src")).expect("create dir");
        fs::write(root.join("src/index.js"), "").expect("write");

        let asked = Arc::new(Mutex::new(Vec::new()));
        let filter = IgnoreFilter::new({
            let asked = Arc::clone(&asked);
            move |path, _| {
                asked.lock().unwrap().push(path.to_path_buf());
                path.file_name().is_some_and(|name| name == "node_modules")
            }
        });

        let mut walked: Vec<PathBuf> = filter
            .walk(WalkDir::new(root))
            .map(|entry| entry.expect("walk").into_path())
            .collect();
        walked.sort();
        assert_eq!(
            walked,
            [
                root.to_path_buf(),
                root.join("src"),
                root.join("src/index.js")
            ]
        );

        // the root is not filtered, and the entries of an ignored directory are never seen
        let mut asked = std::mem::take(&mut *asked.lock().unwrap());
        asked.sort();
        assert_eq!(
            asked,
            [
                root.join("node_modules"),
                root.join("src"),
                root.join("src/index.js")
            ]
        );
    }
}

/// The observable behavior must be the same on every backend, so these tests run against the
/// recommended watcher of the platform.
#[cfg(all(test, not(target_family = "wasm")))]
mod watcher_tests {
    use crate::{RecommendedWatcher, RecursiveMode, TargetMode, WatchMode, Watcher, test::*};
    use std::{fs, path::Path};

    fn watcher() -> (TestWatcher<RecommendedWatcher>, Receiver) {
        ignoring_channel()
    }

    /// Returns the paths of all events, which must never contain an ignored path.
    fn event_paths(rx: &mut Receiver, root: &Path) -> Vec<std::path::PathBuf> {
        let paths: Vec<_> = rx.iter().flat_map(|event| event.paths).collect();
        for path in &paths {
            let below_root = path.strip_prefix(root).unwrap_or(path);
            assert!(
                !is_ignored(below_root),
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

        // kqueue re-registers a directory when a sub directory comes or goes and may miss other
        // events in it meanwhile, so use a different directory
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
        let no_track = WatchMode {
            recursive_mode: RecursiveMode::NonRecursive,
            target_mode: TargetMode::NoTrack,
        };
        watcher.watch(root.join("node_modules/missing"), no_track);

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
