// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Structured event log for reconciliation actions.
//!
//! Every path rewrite, symlink update, and session migration is logged
//! for auditability and undo capability.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::Result;

/// A single reconciliation action that was performed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcileAction {
    pub tool_name: String,
    pub file_path: PathBuf,
    pub field: String,
    pub old_value: String,
    pub new_value: String,
}

/// A logged reconciliation event with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: i64,
    pub timestamp: String,
    pub tool_name: String,
    pub file_path: PathBuf,
    pub field: String,
    pub old_value: String,
    pub new_value: String,
}

/// Structured event log backed by SQLite.
pub struct EventLog {
    conn: Connection,
}

impl EventLog {
    /// Open the event log at the default data directory.
    pub fn open_default() -> Result<Self> {
        let data_dir = Config::data_dir();
        std::fs::create_dir_all(&data_dir)?;
        Self::open(&data_dir.join("event_log.db"))
    }

    /// Open (or create) the event log at a specific path.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        let log = Self { conn };
        log.migrate()?;
        Ok(log)
    }

    /// Open an in-memory event log (for testing).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let log = Self { conn };
        log.migrate()?;
        Ok(log)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS events (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp   TEXT NOT NULL DEFAULT (datetime('now')),
                tool_name   TEXT NOT NULL,
                file_path   TEXT NOT NULL,
                field       TEXT NOT NULL,
                old_value   TEXT NOT NULL,
                new_value   TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_events_timestamp ON events(timestamp);
            ",
        )?;
        Ok(())
    }

    /// Record a reconciliation action.
    pub fn record(&self, action: &ReconcileAction) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events (tool_name, file_path, field, old_value, new_value)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                action.tool_name,
                action.file_path.to_string_lossy().as_ref(),
                action.field,
                action.old_value,
                action.new_value,
            ],
        )?;
        Ok(())
    }

    /// Get the most recent N log entries.
    ///
    /// Ordered by `id DESC` (insertion order) rather than `timestamp`, because
    /// SQLite's `datetime('now')` only has 1-second resolution and would
    /// produce non-deterministic ordering for events in the same second.
    pub fn recent(&self, limit: usize) -> Result<Vec<LogEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, timestamp, tool_name, file_path, field, old_value, new_value
             FROM events ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(LogEntry {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                tool_name: row.get(2)?,
                file_path: PathBuf::from(row.get::<_, String>(3)?),
                field: row.get(4)?,
                old_value: row.get(5)?,
                new_value: row.get(6)?,
            })
        })?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
    }

    /// Count total events.
    pub fn count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
        Ok(count as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_log_round_trip() {
        let log = EventLog::open_in_memory().unwrap();
        let action = ReconcileAction {
            tool_name: "claude_code".to_string(),
            file_path: PathBuf::from(".claude/settings.json"),
            field: "project_path".to_string(),
            old_value: "/old/path".to_string(),
            new_value: "/new/path".to_string(),
        };
        log.record(&action).unwrap();

        let entries = log.recent(10).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tool_name, "claude_code");
        assert_eq!(entries[0].old_value, "/old/path");
    }
}
