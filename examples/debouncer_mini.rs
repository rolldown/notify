use std::{path::Path, time::Duration};

use notify::WatchMode;
use notify_debouncer_mini::new_debouncer;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// Example for debouncer mini
fn main() {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("debouncer_mini=trace")),
        )
        .init();
    // emit some events by changing a file
    std::thread::spawn(|| {
        let path = Path::new("test.txt");
        let _ = std::fs::remove_file(path);
        // tracing::info!("running 250ms events");
        for _ in 0..20 {
            tracing::trace!("writing..");
            std::fs::write(path, b"Lorem ipsum").unwrap();
            std::thread::sleep(Duration::from_millis(250));
        }
        // tracing::debug!("waiting 20s");
        std::thread::sleep(Duration::from_millis(20000));
        // tracing::info!("running 3s events");
        for _ in 0..20 {
            // tracing::debug!("writing..");
            std::fs::write(path, b"Lorem ipsum").unwrap();
            std::thread::sleep(Duration::from_millis(3000));
        }
    });

    // setup debouncer
    let (tx, rx) = std::sync::mpsc::channel();

    // No specific tickrate, max debounce time 1 seconds
    let mut debouncer = new_debouncer(Duration::from_secs(1), tx).unwrap();

    debouncer
        .watcher()
        .watch(Path::new("."), WatchMode::recursive())
        .unwrap();

    // print all events, non returning
    for result in rx {
        match result {
            Ok(events) => events
                .iter()
                .for_each(|event| tracing::info!("Event {event:?}")),
            Err(error) => tracing::info!("Error {error:?}"),
        }
    }
}
