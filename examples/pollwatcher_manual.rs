use notify::{Config, PollWatcher, WatchMode, Watcher};
use std::path::Path;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

// Example for the PollWatcher with manual polling.
// Call with cargo run -p examples --example pollwatcher_manual -- path/to/watch
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
    // use the PollWatcher and disable automatic polling
    let mut watcher = PollWatcher::new(tx, Config::default().with_manual_polling())?;

    // Add a path to be watched. All files and directories at that path and
    // below will be monitored for changes.
    watcher.watch(path.as_ref(), WatchMode::recursive())?;

    // run event receiver on a different thread, we want this one for user input
    std::thread::spawn(move || {
        for res in rx {
            match res {
                Ok(event) => println!("changed: {:?}", event),
                Err(e) => println!("watch error: {:?}", e),
            }
        }
    });

    // wait for any input and poll
    loop {
        println!("Press enter to poll for changes");
        let mut buffer = String::new();
        std::io::stdin().read_line(&mut buffer)?;
        println!("polling..");
        // manually poll for changes, received by the spawned thread
        watcher.poll().unwrap();
    }
}
