use crate::FileIdCache;
use file_id::{FileId, get_file_id};
use notify::{EntryKind, RecursiveMode, WatchMode};
use rustc_hash::FxBuildHasher;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use walkdir::WalkDir;

type IgnoredFilter = Arc<dyn Fn(&Path, EntryKind) -> bool + Send + Sync>;

/// A cache to hold the file system IDs of all watched files.
///
/// The file ID cache uses unique file IDs provided by the file system and is used to stitch together
/// rename events in case the notification back-end doesn't emit rename cookies.
#[derive(Clone, Default)]
pub struct FileIdMap {
    paths: HashMap<PathBuf, FileId, FxBuildHasher>,
    ignored: Option<IgnoredFilter>,
}

impl std::fmt::Debug for FileIdMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileIdMap")
            .field("paths", &self.paths)
            .field("ignored", &self.ignored.is_some())
            .finish()
    }
}

impl FileIdMap {
    /// Construct an empty cache.
    #[must_use]
    pub fn new() -> Self {
        FileIdMap::default()
    }

    /// Construct a cache that excludes paths for which `ignored` returns `true`.
    ///
    /// Matching directories are pruned before their descendants are visited. Ancestors of a scan
    /// root are checked as directories so a direct scan cannot re-enter an ignored subtree. The
    /// predicate has the same signature as [`notify::Config::with_ignored`].
    ///
    /// Paths retain the absolute or relative form of the root passed to [`FileIdCache::add_path`].
    /// Pass absolute roots when sharing matching logic with [`notify::Config::with_ignored`], whose
    /// predicate receives absolute paths. [`EntryKind::Unknown`] is used for a scan root when its
    /// metadata is unavailable. The predicate must remain stable for the lifetime of the cache;
    /// recreate the cache to change the filtering behavior.
    #[must_use]
    pub fn new_with_ignored(
        ignored: impl Fn(&Path, EntryKind) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            ignored: Some(Arc::new(ignored)),
            ..Self::default()
        }
    }

    fn dir_scan_depth(is_recursive: bool) -> usize {
        if is_recursive { usize::MAX } else { 1 }
    }

    fn is_ignored(&self, path: &Path, kind: EntryKind) -> bool {
        self.ignored
            .as_ref()
            .is_some_and(|ignored| ignored(path, kind))
    }

    fn is_ignored_path(&self, path: &Path, kind: EntryKind) -> bool {
        self.is_ignored(path, kind)
            || path
                .ancestors()
                .skip(1)
                .any(|parent| self.is_ignored(parent, EntryKind::Dir))
    }

    fn entry_kind(path: &Path) -> EntryKind {
        match path.metadata() {
            Ok(metadata) if metadata.is_dir() => EntryKind::Dir,
            Ok(_) => EntryKind::File,
            Err(_) => EntryKind::Unknown,
        }
    }

    fn add_entries(&mut self, entries: impl Iterator<Item = walkdir::Result<walkdir::DirEntry>>) {
        for (path, file_id) in entries.filter_map(|entry| {
            let path = entry.ok()?.into_path();
            let file_id = get_file_id(&path).ok()?;
            Some((path, file_id))
        }) {
            self.paths.insert(path, file_id);
        }
    }
}

impl FileIdCache for FileIdMap {
    fn cached_file_id(&self, path: &Path) -> Option<impl AsRef<FileId>> {
        self.paths.get(path)
    }

    fn add_path(&mut self, path: &Path, watch_mode: WatchMode) {
        if self.ignored.is_some() && self.is_ignored_path(path, Self::entry_kind(path)) {
            return;
        }

        let is_recursive = watch_mode.recursive_mode == RecursiveMode::Recursive;
        let entries = WalkDir::new(path)
            .follow_links(true)
            .max_depth(Self::dir_scan_depth(is_recursive))
            .into_iter();

        if let Some(ignored) = self.ignored.clone() {
            self.add_entries(entries.filter_entry(move |entry| {
                entry.depth() == 0 || {
                    let kind = if entry.file_type().is_dir() {
                        EntryKind::Dir
                    } else {
                        EntryKind::File
                    };
                    !ignored(entry.path(), kind)
                }
            }));
        } else {
            self.add_entries(entries);
        }
    }

    fn remove_path(&mut self, path: &Path) {
        self.paths.retain(|p, _| !p.starts_with(path));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use notify::TargetMode;

    use super::*;

    fn recursive() -> WatchMode {
        WatchMode {
            recursive_mode: RecursiveMode::Recursive,
            target_mode: TargetMode::TrackPath,
        }
    }

    #[test]
    fn recursive_scan_prunes_ignored_subtrees() {
        let dir = tempfile::tempdir().unwrap();
        let ignored_dir = dir.path().join("ignored");
        let ignored_file = ignored_dir.join("nested/file.txt");
        let visible_file = dir.path().join("visible.txt");
        std::fs::create_dir_all(ignored_file.parent().unwrap()).unwrap();
        std::fs::write(&ignored_file, "ignored").unwrap();
        std::fs::write(&visible_file, "visible").unwrap();

        let visited = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
        let visited_by_filter = Arc::clone(&visited);
        let mut cache = FileIdMap::new_with_ignored(move |path, kind| {
            visited_by_filter.lock().unwrap().push(path.to_path_buf());
            kind == EntryKind::Dir && path.file_name().is_some_and(|name| name == "ignored")
        });

        cache.add_path(dir.path(), recursive());

        assert!(cache.cached_file_id(&visible_file).is_some());
        assert!(cache.cached_file_id(&ignored_dir).is_none());
        assert!(cache.cached_file_id(&ignored_file).is_none());
        let visited = visited.lock().unwrap();
        assert_eq!(
            visited
                .iter()
                .filter(|path| path.as_path() == dir.path())
                .count(),
            1
        );
        assert!(
            visited
                .iter()
                .all(|path| { path == &ignored_dir || !path.starts_with(&ignored_dir) })
        );
    }

    #[test]
    fn direct_paths_and_rescans_respect_ignored_filter() {
        let dir = tempfile::tempdir().unwrap();
        let ignored_dir = dir.path().join("node_modules");
        let ignored_child = ignored_dir.join("pkg/index.js");
        let ignored_file = dir.path().join("ignored.txt");
        let visible_file = dir.path().join("visible.txt");
        std::fs::create_dir_all(ignored_child.parent().unwrap()).unwrap();
        for path in [&ignored_child, &ignored_file, &visible_file] {
            std::fs::write(path, "contents").unwrap();
        }

        let ignored_file_for_filter = ignored_file.clone();
        let mut cache = FileIdMap::new_with_ignored(move |path, kind| {
            (kind == EntryKind::Dir && path.file_name().is_some_and(|name| name == "node_modules"))
                || (kind == EntryKind::File && path == ignored_file_for_filter)
        });

        cache.add_path(&ignored_file, WatchMode::non_recursive());
        cache.add_path(ignored_child.parent().unwrap(), recursive());
        assert!(cache.cached_file_id(&ignored_file).is_none());
        assert!(cache.cached_file_id(&ignored_child).is_none());

        cache.rescan(&[(dir.path().to_path_buf(), recursive())]);
        assert!(cache.cached_file_id(&visible_file).is_some());
        assert!(cache.cached_file_id(&ignored_file).is_none());
        assert!(cache.cached_file_id(&ignored_child).is_none());
    }
}
