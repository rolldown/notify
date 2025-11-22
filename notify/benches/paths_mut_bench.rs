use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use notify::{RecursiveMode, TargetMode, WatchMode, Watcher};
use std::fs::File;
use std::hint::black_box;
use std::path::PathBuf;
use tempfile::TempDir;

/// Helper function to create N temporary files in multiple directories (sometimes nested)
fn create_temp_files(dir: &std::path::Path, count: usize) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(count);

    // Create files in multiple directory structures:
    // - 40% in root directory
    // - 30% in first-level subdirectories (dir1/, dir2/, dir3/)
    // - 30% in nested subdirectories (nested/deep1/, nested/deep2/)

    for i in 0..count {
        let file_path = match i % 10 {
            0..=3 => {
                // Root directory (40%)
                dir.join(format!("file_{}.txt", i))
            }
            4..=6 => {
                // First-level subdirectories (30%)
                let subdir = dir.join(format!("dir{}", i % 3));
                std::fs::create_dir_all(&subdir).expect("Failed to create subdirectory");
                subdir.join(format!("file_{}.txt", i))
            }
            _ => {
                // Nested subdirectories (30%)
                let nested_dir = dir.join("nested").join(format!("deep{}", i % 2));
                std::fs::create_dir_all(&nested_dir).expect("Failed to create nested directory");
                nested_dir.join(format!("file_{}.txt", i))
            }
        };

        File::create(&file_path).expect("Failed to create temp file");
        paths.push(file_path);
    }
    paths
}

/// Benchmark helper: measures paths_mut total time (add all paths + commit)
fn bench_paths_mut<W: Watcher>(watcher: &mut W, paths: &[PathBuf], mode: RecursiveMode) {
    let mut paths_mut = watcher.paths_mut();
    for path in paths {
        paths_mut
            .add(
                path,
                WatchMode {
                    recursive_mode: mode,
                    target_mode: TargetMode::TrackPath,
                },
            )
            .expect("Failed to add path");
    }
    paths_mut.commit().expect("Failed to commit paths");
}

// INotify benchmarks (Linux/Android)
#[cfg(target_os = "linux")]
fn bench_inotify_paths_mut(c: &mut Criterion) {
    use notify::INotifyWatcher;

    let mut group = c.benchmark_group("inotify_paths_mut");

    for file_count in [100, 500, 1000] {
        for mode in [RecursiveMode::NonRecursive, RecursiveMode::Recursive] {
            let mode_str = match mode {
                RecursiveMode::Recursive => "recursive",
                RecursiveMode::NonRecursive => "nonrecursive",
            };

            group.bench_with_input(
                BenchmarkId::new(mode_str, file_count),
                &file_count,
                |b, &count| {
                    let temp_dir = TempDir::new().expect("Failed to create temp dir");
                    let paths = create_temp_files(temp_dir.path(), count);

                    b.iter(|| {
                        use std::hint::black_box;

                        let mut watcher =
                            INotifyWatcher::new(move |_res| {}, notify::Config::default())
                                .expect("Failed to create watcher");

                        bench_paths_mut(&mut watcher, black_box(&paths), mode);
                    });
                },
            );
        }
    }

    group.finish();
}

// FSEvent benchmarks (macOS)
#[cfg(all(target_os = "macos", feature = "macos_fsevent"))]
fn bench_fsevent_paths_mut(c: &mut Criterion) {
    use notify::FsEventWatcher;

    let mut group = c.benchmark_group("fsevent_paths_mut");

    for file_count in [100, 500, 1000] {
        for mode in [RecursiveMode::NonRecursive, RecursiveMode::Recursive] {
            let mode_str = match mode {
                RecursiveMode::Recursive => "recursive",
                RecursiveMode::NonRecursive => "nonrecursive",
            };

            group.bench_with_input(
                BenchmarkId::new(mode_str, file_count),
                &file_count,
                |b, &count| {
                    let temp_dir = TempDir::new().expect("Failed to create temp dir");
                    let paths = create_temp_files(temp_dir.path(), count);

                    b.iter(|| {
                        let mut watcher =
                            FsEventWatcher::new(move |_res| {}, notify::Config::default())
                                .expect("Failed to create watcher");

                        bench_paths_mut(&mut watcher, black_box(&paths), mode);
                    });
                },
            );
        }
    }

    group.finish();
}

// Kqueue benchmarks (macOS with macos_kqueue feature, BSD, iOS)
#[cfg(any(
    all(target_os = "macos", feature = "macos_kqueue"),
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "ios"
))]
fn bench_kqueue_paths_mut(c: &mut Criterion) {
    use notify::KqueueWatcher;

    let mut group = c.benchmark_group("kqueue_paths_mut");

    for file_count in [100, 500, 1000] {
        for mode in [RecursiveMode::NonRecursive, RecursiveMode::Recursive] {
            let mode_str = match mode {
                RecursiveMode::Recursive => "recursive",
                RecursiveMode::NonRecursive => "nonrecursive",
            };

            group.bench_with_input(
                BenchmarkId::new(mode_str, file_count),
                &file_count,
                |b, &count| {
                    let temp_dir = TempDir::new().expect("Failed to create temp dir");
                    let paths = create_temp_files(temp_dir.path(), count);

                    b.iter(|| {
                        let mut watcher =
                            KqueueWatcher::new(move |_res| {}, notify::Config::default())
                                .expect("Failed to create watcher");

                        bench_paths_mut(&mut watcher, black_box(&paths), mode);
                    });
                },
            );
        }
    }

    group.finish();
}

// Windows benchmarks
#[cfg(target_os = "windows")]
fn bench_windows_paths_mut(c: &mut Criterion) {
    use notify::ReadDirectoryChangesWatcher;

    let mut group = c.benchmark_group("windows_paths_mut");

    for file_count in [100, 500, 1000] {
        for mode in [RecursiveMode::NonRecursive, RecursiveMode::Recursive] {
            let mode_str = match mode {
                RecursiveMode::Recursive => "recursive",
                RecursiveMode::NonRecursive => "nonrecursive",
            };

            group.bench_with_input(
                BenchmarkId::new(mode_str, file_count),
                &file_count,
                |b, &count| {
                    let temp_dir = TempDir::new().expect("Failed to create temp dir");
                    let paths = create_temp_files(temp_dir.path(), count);

                    b.iter(|| {
                        let mut watcher = ReadDirectoryChangesWatcher::new(
                            move |_res| {},
                            notify::Config::default(),
                        )
                        .expect("Failed to create watcher");

                        bench_paths_mut(&mut watcher, black_box(&paths), mode);
                    });
                },
            );
        }
    }

    group.finish();
}

// Poll benchmarks (cross-platform)
fn bench_poll_paths_mut(c: &mut Criterion) {
    use notify::PollWatcher;

    let mut group = c.benchmark_group("poll_paths_mut");

    for file_count in [100, 500, 1000] {
        for mode in [RecursiveMode::NonRecursive, RecursiveMode::Recursive] {
            let mode_str = match mode {
                RecursiveMode::Recursive => "recursive",
                RecursiveMode::NonRecursive => "nonrecursive",
            };

            group.bench_with_input(
                BenchmarkId::new(mode_str, file_count),
                &file_count,
                |b, &count| {
                    let temp_dir = TempDir::new().expect("Failed to create temp dir");
                    let paths = create_temp_files(temp_dir.path(), count);

                    b.iter(|| {
                        let mut watcher =
                            PollWatcher::new(move |_res| {}, notify::Config::default())
                                .expect("Failed to create watcher");

                        bench_paths_mut(&mut watcher, black_box(&paths), mode);
                    });
                },
            );
        }
    }

    group.finish();
}

// Configure criterion groups based on platform
#[cfg(target_os = "linux")]
criterion_group!(benches, bench_inotify_paths_mut, bench_poll_paths_mut);

#[cfg(all(
    target_os = "macos",
    feature = "macos_fsevent",
    not(feature = "macos_kqueue")
))]
criterion_group!(benches, bench_fsevent_paths_mut, bench_poll_paths_mut);

#[cfg(all(target_os = "macos", feature = "macos_kqueue"))]
criterion_group!(benches, bench_kqueue_paths_mut, bench_poll_paths_mut);

#[cfg(any(
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "ios"
))]
criterion_group!(benches, bench_kqueue_paths_mut, bench_poll_paths_mut);

#[cfg(target_os = "windows")]
criterion_group!(benches, bench_windows_paths_mut, bench_poll_paths_mut);

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "windows",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "ios"
)))]
criterion_group!(benches, bench_poll_paths_mut);

criterion_main!(benches);
