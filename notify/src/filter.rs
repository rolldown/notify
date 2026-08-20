use crate::config::{Config, EntryKind, IgnoredFilter};
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "ios",
    all(target_os = "macos", feature = "macos_kqueue")
))]
use crate::event::{CreateKind, EventKind, RemoveKind};
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "windows",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "ios"
))]
use rustc_hash::FxBuildHasher;
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "windows",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "ios"
))]
use std::collections::HashMap;
#[cfg(any(
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "ios",
    all(target_os = "macos", feature = "macos_kqueue")
))]
use std::fs;
use std::path::Path;
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "windows",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "ios"
))]
use std::path::PathBuf;

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "ios",
    all(target_os = "macos", feature = "macos_kqueue")
))]
use crate::Event;

#[derive(Clone, Default)]
pub(crate) struct IgnoreFilter {
    filter: Option<IgnoredFilter>,
}

impl std::fmt::Debug for IgnoreFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IgnoreFilter")
            .field("active", &self.is_active())
            .finish()
    }
}

/// Cached results of ancestor-chain checks for one event batch.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "windows",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "ios"
))]
pub(crate) type AncestorMemo = HashMap<PathBuf, bool, FxBuildHasher>;

impl IgnoreFilter {
    pub(crate) fn new(config: &Config) -> Self {
        Self {
            filter: config.ignored().cloned(),
        }
    }

    /// Whether a filter is installed.
    #[inline]
    pub(crate) fn is_active(&self) -> bool {
        self.filter.is_some()
    }

    /// Return whether a path is ignored.
    #[inline]
    pub(crate) fn is_ignored(&self, path: &Path, kind: EntryKind) -> bool {
        match &self.filter {
            Some(f) => f(path, kind),
            None => false,
        }
    }

    /// Return the entry path unless ignored.
    #[cfg(any(
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly",
        target_os = "ios",
        all(target_os = "macos", feature = "macos_kqueue")
    ))]
    pub(crate) fn keep_dir_entry(&self, entry: &fs::DirEntry) -> Option<PathBuf> {
        let path = entry.path();
        let Some(filter) = &self.filter else {
            return Some(path);
        };
        let kind = match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => EntryKind::Dir,
            Ok(_) => EntryKind::File,
            Err(_) => EntryKind::Unknown,
        };
        (!filter(&path, kind)).then_some(path)
    }

    /// Return whether a path or one of its parent directories is ignored.
    pub(crate) fn is_ignored_path(&self, path: &Path, kind: EntryKind) -> bool {
        if self.filter.is_none() {
            return false;
        }
        self.is_ignored(path, kind)
            || path
                .ancestors()
                .skip(1)
                .any(|parent| self.is_ignored(parent, EntryKind::Dir))
    }

    /// Drop an event when every path is ignored.
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly",
        target_os = "ios",
        all(target_os = "macos", feature = "macos_kqueue")
    ))]
    pub(crate) fn is_ignored_event(&self, event: &Event, memo: &mut AncestorMemo) -> bool {
        if self.filter.is_none() || event.paths.is_empty() {
            return false;
        }
        let kind = entry_kind_of_event(event.kind);
        event
            .paths
            .iter()
            .all(|path| self.is_ignored_event_path(path, kind, memo))
    }

    /// Check a path and its parent directories.
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "windows",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly",
        target_os = "ios"
    ))]
    pub(crate) fn is_ignored_event_path(
        &self,
        path: &Path,
        kind: EntryKind,
        memo: &mut AncestorMemo,
    ) -> bool {
        if self.filter.is_none() {
            return false;
        }
        if self.is_ignored(path, kind) {
            return true;
        }
        match path.parent() {
            Some(parent) => self.dir_chain_ignored(parent, memo),
            None => false,
        }
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "windows",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly",
        target_os = "ios"
    ))]
    fn dir_chain_ignored(&self, dir: &Path, memo: &mut AncestorMemo) -> bool {
        if let Some(&cached) = memo.get(dir) {
            return cached;
        }
        let f = self.filter.as_ref().expect("checked by callers");
        let ignored = f(dir, EntryKind::Dir)
            || match dir.parent() {
                Some(parent) => self.dir_chain_ignored(parent, memo),
                None => false,
            };
        memo.insert(dir.to_path_buf(), ignored);
        ignored
    }
}

/// Infer the path kind from a walk entry.
pub(crate) fn entry_kind_of_walkdir(entry: &walkdir::DirEntry) -> EntryKind {
    if entry.file_type().is_dir() {
        EntryKind::Dir
    } else {
        EntryKind::File
    }
}

/// Infer the current path kind.
pub(crate) fn entry_kind_of_path(path: &Path) -> EntryKind {
    match path.metadata() {
        Ok(metadata) if metadata.is_dir() => EntryKind::Dir,
        Ok(_) => EntryKind::File,
        Err(_) => EntryKind::Unknown,
    }
}

/// Infer the path kind from an event kind.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "ios",
    all(target_os = "macos", feature = "macos_kqueue")
))]
pub(crate) fn entry_kind_of_event(kind: EventKind) -> EntryKind {
    match kind {
        EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder) => {
            EntryKind::Dir
        }
        EventKind::Create(CreateKind::File) | EventKind::Remove(RemoveKind::File) => {
            EntryKind::File
        }
        _ => EntryKind::Unknown,
    }
}

#[cfg(all(
    test,
    any(
        target_os = "linux",
        target_os = "android",
        target_os = "windows",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly",
        target_os = "ios"
    )
))]
mod tests {
    use super::*;

    fn filter() -> IgnoreFilter {
        IgnoreFilter::new(&Config::default().with_ignored(|path, kind| {
            kind == EntryKind::Dir && path.file_name().is_some_and(|name| name == "node_modules")
        }))
    }

    #[test]
    fn ignored_directory_applies_to_descendants() {
        assert!(filter().is_ignored_path(
            Path::new("/project/node_modules/pkg/index.js"),
            EntryKind::File,
        ));
    }

    #[test]
    fn inactive_filter_ignores_nothing() {
        assert!(!IgnoreFilter::default().is_ignored_event_path(
            Path::new("/project/file.js"),
            EntryKind::Unknown,
            &mut AncestorMemo::default(),
        ));
    }

    #[test]
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly",
        target_os = "ios",
        all(target_os = "macos", feature = "macos_kqueue")
    ))]
    fn event_is_dropped_only_when_all_paths_are_ignored() {
        let filter = filter();
        let ignored = PathBuf::from("/project/node_modules/pkg/index.js");
        let visible = PathBuf::from("/project/src/index.js");
        let mut memo = AncestorMemo::default();

        let partly_visible = Event::new(EventKind::Any)
            .add_path(ignored.clone())
            .add_path(visible);
        assert!(!filter.is_ignored_event(&partly_visible, &mut memo));

        let fully_ignored = Event::new(EventKind::Any)
            .add_path(ignored)
            .add_path(PathBuf::from("/project/node_modules/pkg/other.js"));
        assert!(filter.is_ignored_event(&fully_ignored, &mut memo));
    }
}
