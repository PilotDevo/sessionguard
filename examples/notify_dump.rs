// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT
//
// Diagnostic: dump every raw notify event for a watched directory.
//
// Usage:
//   cargo run --example notify_dump -- <dir-to-watch>
//
// Run it, then `mv` / `touch` / `rm` things inside the watched dir in
// another terminal. Each event prints its EventKind plus all paths
// plus the event attrs (incl. tracker cookie if any).

use std::env;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: notify_dump <dir-to-watch>");
        std::process::exit(2);
    }
    let dir = PathBuf::from(&args[1]);
    if !dir.is_dir() {
        eprintln!("not a directory: {}", dir.display());
        std::process::exit(2);
    }

    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;
    watcher.watch(&dir, RecursiveMode::Recursive)?;
    eprintln!("watching {} (recursive). Ctrl-C to quit.\n", dir.display());

    let mut n = 0usize;
    loop {
        match rx.recv_timeout(Duration::from_secs(3600)) {
            Ok(Ok(ev)) => {
                n += 1;
                println!(
                    "[{:03}] kind={:?}\n      paths={:?}\n      tracker={:?} flag={:?}",
                    n,
                    ev.kind,
                    ev.paths,
                    ev.attrs.tracker(),
                    ev.attrs.flag()
                );
            }
            Ok(Err(e)) => eprintln!("watch error: {e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(e) => {
                eprintln!("channel closed: {e}");
                break;
            }
        }
    }
    Ok(())
}
