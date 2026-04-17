// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Filesystem event watching.
//!
//! Wraps the `notify` crate to watch project root directories for
//! move, rename, and delete events that could affect session artifacts.
//!
//! ## Rename handling
//!
//! Moves arrive from `notify` differently across platforms — and in most
//! cases *not* as a single atomic `Moved` event:
//!
//! - **macOS (FSEvents)** emits two consecutive `Modify(Name(Any))` events
//!   (source then destination), each with 1 path and no tracker cookie.
//! - **Linux (inotify)** emits separate `Modify(Name(From))` + `Modify(Name(To))`
//!   events linked by a `tracker()` cookie.
//! - **`RenameMode::Both`** (rare; some platforms/conditions) carries both
//!   paths in a single event.
//!
//! The [`RenameBuffer`] pairs half-events into [`SessionEvent::Moved`] by
//! matching cookies when present, falling back to FIFO order within a
//! short TTL window when cookies are absent (macOS).

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use notify::event::{ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::WatchMode;
use crate::error::Result;

/// How long a half-rename event waits in the pairing buffer before being
/// dropped. notify typically emits the two halves within a few ms, so
/// 500ms is a comfortable upper bound that still drops truly orphaned
/// halves reasonably quickly.
const RENAME_PAIRING_TTL: Duration = Duration::from_millis(500);

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

        // The pairing buffer is accessed only from notify's single worker
        // thread, but `Mutex` is used to satisfy the `Send + Sync` bounds
        // on the callback closure. Contention is effectively zero.
        let buffer = Mutex::new(RenameBuffer::new(RENAME_PAIRING_TTL));
        let event_tx = tx.clone();

        let mut watcher =
            notify::recommended_watcher(move |res: std::result::Result<Event, notify::Error>| {
                match res {
                    Ok(event) => {
                        let mut buf = buffer.lock().unwrap();
                        let emitted = classify_event(&event, &mut buf);
                        drop(buf);
                        for session_event in emitted {
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

// ── Rename pairing ───────────────────────────────────────────────────────────

/// One buffered half of a rename, waiting to be paired with its other half.
#[derive(Debug)]
struct PendingRename {
    path: PathBuf,
    cookie: Option<usize>,
    when: Instant,
}

/// TTL-bounded buffer that pairs rename half-events into `Moved` events.
///
/// Invariant: entries older than `ttl` are pruned on every operation.
#[derive(Debug)]
struct RenameBuffer {
    pending: Vec<PendingRename>,
    ttl: Duration,
}

impl RenameBuffer {
    fn new(ttl: Duration) -> Self {
        Self {
            pending: Vec::new(),
            ttl,
        }
    }

    fn prune(&mut self) {
        let now = Instant::now();
        self.pending
            .retain(|p| now.duration_since(p.when) <= self.ttl);
    }

    /// Buffer a half-rename event, or emit `Moved` if a matching pending
    /// half is found.
    ///
    /// Matching strategy:
    /// - If both the incoming event and a pending entry carry a cookie
    ///   and the cookies match — pair by cookie (Linux path).
    /// - Else if the incoming and pending entries both have no cookie —
    ///   pair the oldest pending with the incoming (macOS FIFO path).
    /// - Else buffer the incoming for a future match.
    fn observe_half(&mut self, path: PathBuf, cookie: Option<usize>) -> Option<SessionEvent> {
        self.prune();

        // Try cookie match first (Linux)
        if cookie.is_some() {
            if let Some(i) = self.pending.iter().position(|p| p.cookie == cookie) {
                let from = self.pending.remove(i);
                return Some(SessionEvent::Moved {
                    from: Some(from.path),
                    to: Some(path),
                });
            }
        }

        // Fall back to FIFO pairing among cookie-less entries (macOS)
        if cookie.is_none() {
            if let Some(i) = self.pending.iter().position(|p| p.cookie.is_none()) {
                let from = self.pending.remove(i);
                return Some(SessionEvent::Moved {
                    from: Some(from.path),
                    to: Some(path),
                });
            }
        }

        // No pair found — buffer this half.
        self.pending.push(PendingRename {
            path,
            cookie,
            when: Instant::now(),
        });
        None
    }
}

// ── Event classification ─────────────────────────────────────────────────────

/// Map a raw `notify` event into zero or more [`SessionEvent`]s.
///
/// Most events map 1:1. Rename half-events (most renames in practice) are
/// buffered in `buf` and may emit either 0 or 1 paired `Moved` events.
fn classify_event(event: &Event, buf: &mut RenameBuffer) -> Vec<SessionEvent> {
    match &event.kind {
        EventKind::Create(_) => event
            .paths
            .first()
            .cloned()
            .map(SessionEvent::Created)
            .into_iter()
            .collect(),
        EventKind::Remove(_) => event
            .paths
            .first()
            .cloned()
            .map(SessionEvent::Removed)
            .into_iter()
            .collect(),
        EventKind::Modify(ModifyKind::Name(mode)) => classify_rename(mode, event, buf),
        _ => Vec::new(),
    }
}

fn classify_rename(mode: &RenameMode, event: &Event, buf: &mut RenameBuffer) -> Vec<SessionEvent> {
    match mode {
        // Some platforms provide both paths in a single atomic event.
        RenameMode::Both => {
            if event.paths.len() >= 2 {
                vec![SessionEvent::Moved {
                    from: Some(event.paths[0].clone()),
                    to: Some(event.paths[1].clone()),
                }]
            } else {
                Vec::new()
            }
        }
        // Half-events: buffer for pairing.
        RenameMode::From | RenameMode::To | RenameMode::Any | RenameMode::Other => {
            let Some(path) = event.paths.first().cloned() else {
                return Vec::new();
            };
            let cookie = event.attrs.tracker();
            match buf.observe_half(path, cookie) {
                Some(ev) => vec![ev],
                None => Vec::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn half_event(mode: RenameMode, path: &str, cookie: Option<usize>) -> Event {
        let mut attrs = notify::event::EventAttributes::new();
        if let Some(c) = cookie {
            attrs.set_tracker(c);
        }
        Event {
            kind: EventKind::Modify(ModifyKind::Name(mode)),
            paths: vec![PathBuf::from(path)],
            attrs,
        }
    }

    #[test]
    fn classify_returns_empty_for_data_change() {
        let mut buf = RenameBuffer::new(RENAME_PAIRING_TTL);
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![PathBuf::from("/test")],
            attrs: Default::default(),
        };
        assert!(classify_event(&event, &mut buf).is_empty());
    }

    #[test]
    fn classify_rename_both_emits_full_move() {
        let mut buf = RenameBuffer::new(RENAME_PAIRING_TTL);
        let event = Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            paths: vec![PathBuf::from("/a"), PathBuf::from("/b")],
            attrs: Default::default(),
        };
        let out = classify_event(&event, &mut buf);
        assert_eq!(out.len(), 1);
        match &out[0] {
            SessionEvent::Moved { from, to } => {
                assert_eq!(from, &Some(PathBuf::from("/a")));
                assert_eq!(to, &Some(PathBuf::from("/b")));
            }
            other => panic!("expected Moved, got {other:?}"),
        }
    }

    #[test]
    fn macos_pair_two_any_half_events_into_moved() {
        let mut buf = RenameBuffer::new(RENAME_PAIRING_TTL);
        // First half: the rename-away side, 1 path, no cookie.
        let first = classify_event(&half_event(RenameMode::Any, "/old", None), &mut buf);
        assert!(
            first.is_empty(),
            "first half should be buffered, not emitted"
        );
        // Second half: the rename-into side, arrives right after.
        let second = classify_event(&half_event(RenameMode::Any, "/new", None), &mut buf);
        assert_eq!(second.len(), 1);
        match &second[0] {
            SessionEvent::Moved { from, to } => {
                assert_eq!(from, &Some(PathBuf::from("/old")));
                assert_eq!(to, &Some(PathBuf::from("/new")));
            }
            other => panic!("expected Moved, got {other:?}"),
        }
    }

    #[test]
    fn linux_pair_from_and_to_by_cookie() {
        let mut buf = RenameBuffer::new(RENAME_PAIRING_TTL);
        let first = classify_event(&half_event(RenameMode::From, "/src", Some(42)), &mut buf);
        assert!(first.is_empty());
        let second = classify_event(&half_event(RenameMode::To, "/dst", Some(42)), &mut buf);
        assert_eq!(second.len(), 1);
        match &second[0] {
            SessionEvent::Moved { from, to } => {
                assert_eq!(from, &Some(PathBuf::from("/src")));
                assert_eq!(to, &Some(PathBuf::from("/dst")));
            }
            other => panic!("expected Moved, got {other:?}"),
        }
    }

    #[test]
    fn linux_unpaired_cookie_does_not_match_different_cookie() {
        let mut buf = RenameBuffer::new(RENAME_PAIRING_TTL);
        let _ = classify_event(&half_event(RenameMode::From, "/a", Some(1)), &mut buf);
        // Different cookie — should buffer, not pair.
        let out = classify_event(&half_event(RenameMode::To, "/b", Some(2)), &mut buf);
        assert!(out.is_empty(), "mismatched cookies must not pair");
    }

    #[test]
    fn expired_half_does_not_pair_with_late_arrival() {
        let mut buf = RenameBuffer::new(Duration::from_millis(1));
        let _ = classify_event(&half_event(RenameMode::Any, "/a", None), &mut buf);
        std::thread::sleep(Duration::from_millis(5));
        // First half expired; second half should be buffered, not paired.
        let out = classify_event(&half_event(RenameMode::Any, "/b", None), &mut buf);
        assert!(
            out.is_empty(),
            "second half should be buffered after first expires"
        );
    }
}
