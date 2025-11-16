# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
