use notify::WatchMode;
use notify_debouncer_full::new_debouncer;
use std::{path::Path, time::Duration};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// Example for notify-debouncer-full
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

    // Create a new debounced file watcher with a timeout of 2 seconds.
    // The tickrate will be selected automatically, as well as the underlying watch implementation.
    let mut debouncer = new_debouncer(Duration::from_secs(2), None, tx)?;

    // Add a path to be watched. All files and directories at that path and
    // below will be monitored for changes.
    debouncer.watch(path.as_ref(), WatchMode::recursive())?;

    // print all events and errors
    for result in rx {
        match result {
            Ok(events) => events.iter().for_each(|event| tracing::info!("{event:?}")),
            Err(errors) => errors.iter().for_each(|error| tracing::error!("{error:?}")),
        }
    }

    Ok(())
}
