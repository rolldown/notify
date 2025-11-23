//! Configuration types

use std::time::Duration;

/// Indicates how the path should be watched
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub struct WatchMode {
    /// Indicates whether to watch sub-directories as well
    pub recursive_mode: RecursiveMode,
    /// Indicates what happens when the relationship of the physical entity and the file path changes
    pub target_mode: TargetMode,
}

impl WatchMode {
    /// Creates a WatchMode that watches directories recursively and tracks the file path
    pub fn recursive() -> Self {
        Self {
            recursive_mode: RecursiveMode::Recursive,
            target_mode: TargetMode::TrackPath,
        }
    }

    /// Creates a WatchMode that watches only the provided directory and tracks the file path
    pub fn non_recursive() -> Self {
        Self {
            recursive_mode: RecursiveMode::NonRecursive,
            target_mode: TargetMode::TrackPath,
        }
    }
}

/// Indicates whether only the provided directory or its sub-directories as well should be watched
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum RecursiveMode {
    /// Watch all sub-directories as well, including directories created after installing the watch
    Recursive,

    /// Watch only the provided directory
    NonRecursive,
}

impl RecursiveMode {
    pub(crate) fn is_recursive(&self) -> bool {
        match *self {
            RecursiveMode::Recursive => true,
            RecursiveMode::NonRecursive => false,
        }
    }
}

/// Indicates what happens when the relationship of the physical entity and the file path changes
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum TargetMode {
    /// Tracks the file path.
    ///
    /// If the underlying physical entity (inode/File ID) at this path is replaced
    /// (e.g., by a move/rename operation), the watch continues to monitor the new entity
    /// that now occupies the path.
    ///
    /// TODO: This is not yet implemented. Currently, all backends behave as NoTrack.
    TrackPath,

    /// Does not track the file path, nor the physical entity.
    ///
    /// If the underlying physical entity (inode/File ID) is replaced
    /// (e.g., by a move/rename operation), the watch stops monitoring.
    NoTrack,
}

/// Watcher Backend configuration
///
/// This contains multiple settings that may relate to only one specific backend,
/// such as to correctly configure each backend regardless of what is selected during runtime.
///
/// ```rust
/// # use std::time::Duration;
/// # use notify::Config;
/// let config = Config::default()
///     .with_poll_interval(Duration::from_secs(2))
///     .with_compare_contents(true);
/// ```
///
/// Some options can be changed during runtime, others have to be set when creating the watcher backend.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub struct Config {
    /// See [Config::with_poll_interval]
    poll_interval: Option<Duration>,

    /// See [Config::with_compare_contents]
    compare_contents: bool,

    follow_symlinks: bool,
}

impl Config {
    /// For the [`PollWatcher`](crate::PollWatcher) backend.
    ///
    /// Interval between each re-scan attempt. This can be extremely expensive for large
    /// file trees so it is recommended to measure and tune accordingly.
    ///
    /// The default poll frequency is 30 seconds.
    ///
    /// This will enable automatic polling, overwriting [`with_manual_polling()`](Config::with_manual_polling).
    pub fn with_poll_interval(mut self, dur: Duration) -> Self {
        // TODO: v7.0 break signature to option
        self.poll_interval = Some(dur);
        self
    }

    /// Returns current setting
    pub fn poll_interval(&self) -> Option<Duration> {
        // Changed Signature to Option
        self.poll_interval
    }

    /// For the [`PollWatcher`](crate::PollWatcher) backend.
    ///
    /// Disable automatic polling. Requires calling [`crate::PollWatcher::poll()`] manually.
    ///
    /// This will disable automatic polling, overwriting [`with_poll_interval()`](Config::with_poll_interval).
    pub fn with_manual_polling(mut self) -> Self {
        self.poll_interval = None;
        self
    }

    /// For the [`PollWatcher`](crate::PollWatcher) backend.
    ///
    /// Optional feature that will evaluate the contents of changed files to determine if
    /// they have indeed changed using a fast hashing algorithm.  This is especially important
    /// for pseudo filesystems like those on Linux under /sys and /proc which are not obligated
    /// to respect any other filesystem norms such as modification timestamps, file sizes, etc.
    /// By enabling this feature, performance will be significantly impacted as all files will
    /// need to be read and hashed at each `poll_interval`.
    ///
    /// This can't be changed during runtime. Off by default.
    pub fn with_compare_contents(mut self, compare_contents: bool) -> Self {
        self.compare_contents = compare_contents;
        self
    }

    /// Returns current setting
    pub fn compare_contents(&self) -> bool {
        self.compare_contents
    }

    /// For the [INotifyWatcher](crate::INotifyWatcher), [KqueueWatcher](crate::KqueueWatcher),
    /// and [PollWatcher](crate::PollWatcher).
    ///
    /// Determine if symbolic links should be followed when recursively watching a directory.
    ///
    /// This can't be changed during runtime. On by default.
    pub fn with_follow_symlinks(mut self, follow_symlinks: bool) -> Self {
        self.follow_symlinks = follow_symlinks;
        self
    }

    /// Returns current setting
    pub fn follow_symlinks(&self) -> bool {
        self.follow_symlinks
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            poll_interval: Some(Duration::from_secs(30)),
            compare_contents: false,
            follow_symlinks: true,
        }
    }
}
