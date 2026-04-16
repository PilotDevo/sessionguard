// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Filesystem event watching.
//!
//! Wraps the `notify` crate to watch project root directories for
//! move, rename, and delete events that could affect session artifacts.

use std::path::PathBuf;

use notify::event::{ModifyKind, RenameMode};
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
    ///
    /// `_mode` is reserved for future debouncing / aggressiveness tuning
    /// (see `WatchMode`). It is currently accepted but ignored — the
    /// `notify::RecommendedWatcher` runs in its default configuration.
    /// Tracked as a v0.3 enhancement.
    pub fn new(watch_roots: &[PathBuf], _mode: &WatchMode) -> Result<Self> {
        let (tx, rx) = mpsc::channel(256);

        let event_tx = tx.clone();
        let mut watcher =
            notify::recommended_watcher(move |res: std::result::Result<Event, notify::Error>| {
                match res {
                    Ok(event) => {
                        if let Some(session_event) = classify_event(&event) {
                            // `try_send` — never block the notify worker thread. If the
                            // consumer is backed up, drop the event and log. Using
                            // `blocking_send` here risks deadlock: the notify thread is
                            // synchronous and holds OS-level watch handles.
                            if let Err(e) = event_tx.try_send(session_event) {
                                use tokio::sync::mpsc::error::TrySendError;
                                match e {
                                    TrySendError::Full(_) => {
                                        warn!("event channel full — dropping event");
                                    }
                                    TrySendError::Closed(_) => {
                                        warn!("event channel closed");
                                    }
                                }
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

/// Map a raw `notify` event to a [`SessionEvent`].
///
/// Rename semantics differ across platforms:
/// - **macOS (FSEvents)** typically emits `RenameMode::Both` with paths[0]=from
///   and paths[1]=to for an atomic rename within the same volume.
/// - **Linux (inotify)** emits `RenameMode::From` and `RenameMode::To` as two
///   *separate* events linked by a cookie. Pairing them requires a TTL cache
///   keyed on `event.attrs.tracker()` — currently a known gap; the half-events
///   are surfaced as `Created`/`Removed` so the caller at least sees them.
/// - **`RenameMode::Any`** with two paths is treated like `Both`; with one,
///   like a half-event.
///
/// Tracked as a v0.3 enhancement: full cookie-based rename pairing.
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
        EventKind::Modify(ModifyKind::Name(mode)) => classify_rename(mode, &event.paths),
        _ => None,
    }
}

/// Decide how to surface a rename event given its `RenameMode` and path list.
fn classify_rename(mode: &RenameMode, paths: &[PathBuf]) -> Option<SessionEvent> {
    match mode {
        RenameMode::Both => {
            let from = paths.first().cloned();
            let to = paths.get(1).cloned();
            if from.is_some() && to.is_some() {
                Some(SessionEvent::Moved { from, to })
            } else {
                None
            }
        }
        RenameMode::From => paths.first().cloned().map(SessionEvent::Removed),
        RenameMode::To => paths.first().cloned().map(SessionEvent::Created),
        RenameMode::Any | RenameMode::Other => {
            if paths.len() >= 2 {
                Some(SessionEvent::Moved {
                    from: Some(paths[0].clone()),
                    to: Some(paths[1].clone()),
                })
            } else {
                paths.first().cloned().map(SessionEvent::Created)
            }
        }
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

    #[test]
    fn classify_rename_both_emits_full_move() {
        let paths = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let ev = classify_rename(&RenameMode::Both, &paths);
        match ev {
            Some(SessionEvent::Moved { from, to }) => {
                assert_eq!(from, Some(PathBuf::from("/a")));
                assert_eq!(to, Some(PathBuf::from("/b")));
            }
            other => panic!("expected Moved, got {other:?}"),
        }
    }

    #[test]
    fn classify_rename_from_emits_removed() {
        let paths = vec![PathBuf::from("/a")];
        let ev = classify_rename(&RenameMode::From, &paths);
        assert!(matches!(ev, Some(SessionEvent::Removed(p)) if p == PathBuf::from("/a")));
    }

    #[test]
    fn classify_rename_to_emits_created() {
        let paths = vec![PathBuf::from("/b")];
        let ev = classify_rename(&RenameMode::To, &paths);
        assert!(matches!(ev, Some(SessionEvent::Created(p)) if p == PathBuf::from("/b")));
    }
}
