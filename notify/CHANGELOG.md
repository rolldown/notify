# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [9.0.0](https://github.com/rolldown/notify/compare/rolldown-notify-v8.2.4...rolldown-notify-v9.0.0) - 2025-11-23

### Added

- implement `TargetMode::TrackPath` for kqueue ([#25](https://github.com/rolldown/notify/pull/25))
- implement `TargetMode::TrackPath` for fsevent ([#27](https://github.com/rolldown/notify/pull/27))
- implement `TargetMode::TrackPath` for Windows ([#23](https://github.com/rolldown/notify/pull/23))
- implement `TargetMode::TrackPath` for inotify ([#22](https://github.com/rolldown/notify/pull/22))
- [**breaking**] change `Watcher::watch` to take `WatchMode` instead of `RecursiveMode` ([#21](https://github.com/rolldown/notify/pull/21))

### Other

- update TargetMode comment ([#36](https://github.com/rolldown/notify/pull/36))
- add optional expected events to reduce flakiness ([#31](https://github.com/rolldown/notify/pull/31))
- add `TargetMode` related tests for polling watcher ([#30](https://github.com/rolldown/notify/pull/30))
- wait a short period before checking whether no events were received ([#29](https://github.com/rolldown/notify/pull/29))
- add optional expected events to reduce flakiness ([#28](https://github.com/rolldown/notify/pull/28))

## [8.2.4](https://github.com/rolldown/notify/compare/rolldown-notify-v8.2.3...rolldown-notify-v8.2.4) - 2025-11-23

### Fixed

- watch hardlinks correctly for inotify backend ([#20](https://github.com/rolldown/notify/pull/20))
- prevent hanging on file additions in recursive watches for inotify backend ([#18](https://github.com/rolldown/notify/pull/18))

### Other

- merge adjacent `if` and `match` statements ([#24](https://github.com/rolldown/notify/pull/24))

## [8.2.3](https://github.com/rolldown/notify/compare/rolldown-notify-v8.2.2...rolldown-notify-v8.2.3) - 2025-11-21

### Fixed

- align the behavior of kqueue backend more with others ([#16](https://github.com/rolldown/notify/pull/16))

## [8.2.2](https://github.com/rolldown/notify/compare/rolldown-notify-v8.2.1...rolldown-notify-v8.2.2) - 2025-11-21

### Fixed

- remove watch handles after file deletion for inotify ([#15](https://github.com/rolldown/notify/pull/15))
- avoid watching file under a directory that is watched for inotify backend ([#14](https://github.com/rolldown/notify/pull/14))

### Other

- verify watch handles for kqueue ([#13](https://github.com/rolldown/notify/pull/13))
- verify watch handles for Windows backend ([#12](https://github.com/rolldown/notify/pull/12))
- verify watch handles for inotify ([#11](https://github.com/rolldown/notify/pull/11))
- separate watch_handles from watchers for inotify backend ([#9](https://github.com/rolldown/notify/pull/9))

## [8.2.1](https://github.com/rolldown/notify/compare/rolldown-notify-v8.2.0...rolldown-notify-v8.2.1) - 2025-11-16

### Fixed

- emit `remove` event if add watch fails due to non-existing path for kqueue watcher ([#6](https://github.com/rolldown/notify/pull/6))
- throw fsevents stream start error properly ([#4](https://github.com/rolldown/notify/pull/4))

### Other

- add kqueue tests ([#5](https://github.com/rolldown/notify/pull/5))
- reuse the same `ReadDirectoryChangesW` handle for watching a file in the same directory ([#3](https://github.com/rolldown/notify/pull/3))
- migrate to rust edition 2024 ([#2](https://github.com/rolldown/notify/pull/2))
- add benchmark for .paths_mut ([#1](https://github.com/rolldown/notify/pull/1))
- fix test failure with macOS kqueue
- add test helpers and tests ([#728](https://github.com/rolldown/notify/pull/728))
- `FsEventWatcher` crashes when dealing with empty path ([#718](https://github.com/rolldown/notify/pull/718))
- update rust toolchain to 1.90.0
