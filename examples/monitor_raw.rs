use notify::{Config, RecommendedWatcher, WatchMode, Watcher};
use std::path::Path;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

fn main() {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let path = std::env::args()
        .nth(1)
        .expect("Argument 1 needs to be a path");

    tracing::info!("Watching {path}");

    if let Err(error) = watch(path) {
        tracing::error!("Error: {error:?}");
    }
}

fn watch<P: AsRef<Path>>(path: P) -> notify::Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();

    // Automatically select the best implementation for your platform.
    // You can also access each implementation directly e.g. INotifyWatcher.
    let mut watcher = RecommendedWatcher::new(tx, Config::default())?;

    // Add a path to be watched. All files and directories at that path and
    // below will be monitored for changes.
    watcher.watch(path.as_ref(), WatchMode::recursive())?;

    for res in rx {
        match res {
            Ok(event) => tracing::info!("Change: {event:?}"),
            Err(error) => tracing::error!("Error: {error:?}"),
        }
    }

    Ok(())
}
