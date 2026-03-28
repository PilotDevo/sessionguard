//! Filesystem event watching.
//!
//! Wraps the `notify` crate to watch project root directories for
//! move, rename, and delete events that could affect session artifacts.

use std::path::PathBuf;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::WatchMode;
use crate::error::Result;

/// A filesystem event relevant to SessionGuard.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// A directory was moved/renamed. Contains (old_path, new_path) if available.
    Moved {
        from: Option<PathBuf>,
        to: Option<PathBuf>,
    },
    /// A directory was removed.
    Removed(PathBuf),
    /// A new directory was created (potential move target).
    Created(PathBuf),
}

/// Filesystem watcher that emits session-relevant events.
pub struct FsWatcher {
    _watcher: RecommendedWatcher,
    pub events: mpsc::Receiver<SessionEvent>,
}

impl FsWatcher {
    /// Create a new watcher monitoring the given directories.
    pub fn new(watch_roots: &[PathBuf], _mode: &WatchMode) -> Result<Self> {
        let (tx, rx) = mpsc::channel(256);

        let event_tx = tx.clone();
        let mut watcher =
            notify::recommended_watcher(move |res: std::result::Result<Event, notify::Error>| {
                match res {
                    Ok(event) => {
                        if let Some(session_event) = classify_event(&event) {
                            if event_tx.blocking_send(session_event).is_err() {
                                warn!("event channel closed");
                            }
                        }
                    }
                    Err(e) => warn!("watch error: {e}"),
                }
            })?;

        for root in watch_roots {
            if root.is_dir() {
                info!(path = %root.display(), "watching directory");
                watcher.watch(root, RecursiveMode::Recursive)?;
            } else {
                debug!(path = %root.display(), "skipping non-existent watch root");
            }
        }

        Ok(Self {
            _watcher: watcher,
            events: rx,
        })
    }
}

fn classify_event(event: &Event) -> Option<SessionEvent> {
    match &event.kind {
        EventKind::Create(_) => event
            .paths
            .first()
            .map(|p| SessionEvent::Created(p.clone())),
        EventKind::Remove(_) => event
            .paths
            .first()
            .map(|p| SessionEvent::Removed(p.clone())),
        EventKind::Modify(notify::event::ModifyKind::Name(_)) => {
            let from = event.paths.first().cloned();
            let to = event.paths.get(1).cloned();
            Some(SessionEvent::Moved { from, to })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_returns_none_for_data_change() {
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![PathBuf::from("/test")],
            attrs: Default::default(),
        };
        assert!(classify_event(&event).is_none());
    }
}
