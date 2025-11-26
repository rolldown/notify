use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use file_id::{FileId, get_file_id};
use notify::{RecursiveMode, WatchMode};
use walkdir::WalkDir;

/// The interface of a file ID cache.
///
/// This trait can be implemented for an existing cache, if it already holds `FileId`s.
pub trait FileIdCache {
    /// Get a `FileId` from the cache for a given `path`.
    ///
    /// If the path is not cached, `None` should be returned and there should not be any attempt to read the file ID from disk.
    fn cached_file_id(&self, path: &Path) -> Option<impl AsRef<FileId>>;

    /// Add a new path to the cache or update its value.
    ///
    /// This will be called if a new file or directory is created or if an existing file is overridden.
    fn add_path(&mut self, path: &Path, watch_mode: WatchMode);

    /// Remove a path from the cache.
    ///
    /// This will be called if a file or directory is deleted.
    fn remove_path(&mut self, path: &Path);

    /// Re-scan all `root_paths`.
    ///
    /// This will be called if the notification back-end has dropped events.
    /// The root paths are passed as argument, so the implementer doesn't have to store them.
    ///
    /// The default implementation calls `add_path` for each root path.
    fn rescan(&mut self, root_paths: &[(PathBuf, WatchMode)]) {
        for (path, watch_mode) in root_paths {
            self.add_path(path, *watch_mode);
        }
    }
}

/// A cache to hold the file system IDs of all watched files.
///
/// The file ID cache uses unique file IDs provided by the file system and is used to stitch together
/// rename events in case the notification back-end doesn't emit rename cookies.
#[derive(Debug, Clone, Default)]
pub struct FileIdMap {
    paths: HashMap<PathBuf, FileId>,
}

impl FileIdMap {
    /// Construct an empty cache.
    #[must_use]
    pub fn new() -> Self {
        FileIdMap::default()
    }

    fn dir_scan_depth(is_recursive: bool) -> usize {
        if is_recursive { usize::MAX } else { 1 }
    }
}

impl FileIdCache for FileIdMap {
    fn cached_file_id(&self, path: &Path) -> Option<impl AsRef<FileId>> {
        self.paths.get(path)
    }

    fn add_path(&mut self, path: &Path, watch_mode: WatchMode) {
        let is_recursive = watch_mode.recursive_mode == RecursiveMode::Recursive;

        for (path, file_id) in WalkDir::new(path)
            .follow_links(true)
            .max_depth(Self::dir_scan_depth(is_recursive))
            .into_iter()
            .filter_map(|entry| {
                let path = entry.ok()?.into_path();
                let file_id = get_file_id(&path).ok()?;
                Some((path, file_id))
            })
        {
            self.paths.insert(path, file_id);
        }
    }

    fn remove_path(&mut self, path: &Path) {
        self.paths.retain(|p, _| !p.starts_with(path));
    }
}

/// An implementation of the `FileIdCache` trait that doesn't hold any data.
///
/// This pseudo cache can be used to disable the file tracking using file system IDs.
#[derive(Debug, Clone, Default)]
pub struct NoCache;

impl NoCache {
    /// Construct an empty cache.
    #[must_use]
    pub fn new() -> Self {
        NoCache
    }
}

impl FileIdCache for NoCache {
    fn cached_file_id(&self, _path: &Path) -> Option<impl AsRef<FileId>> {
        Option::<&FileId>::None
    }

    fn add_path(&mut self, _path: &Path, _watch_mode: WatchMode) {}

    fn remove_path(&mut self, _path: &Path) {}
}

/// The recommended file ID cache implementation for the current platform
#[cfg(any(target_os = "linux", target_os = "android"))]
pub type RecommendedCache = NoCache;
/// The recommended file ID cache implementation for the current platform
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub type RecommendedCache = FileIdMap;
