// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Structured event log for reconciliation actions.
//!
//! Every path rewrite is logged for auditability and undo capability. The
//! schema records enough information to reverse an action without needing
//! the originating tool definition — format is stored alongside the old/new
//! values so the correct adapter can be invoked during undo.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::Result;

/// A single reconciliation action that was performed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcileAction {
    pub tool_name: String,
    pub file_path: PathBuf,
    pub field: String,
    /// Format of the artifact file — required for undo to route to the right
    /// adapter (`json`, `toml`, or anything else → text).
    pub format: String,
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
    pub format: String,
    pub old_value: String,
    pub new_value: String,
    /// `Some(timestamp)` if this action has been undone via `sessionguard undo`.
    pub undone_at: Option<String>,
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
        // Step 1: ensure the base table exists. Fresh DBs get the full schema
        // here; pre-existing DBs are untouched (CREATE IF NOT EXISTS no-ops).
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS events (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp   TEXT NOT NULL DEFAULT (datetime('now')),
                tool_name   TEXT NOT NULL,
                file_path   TEXT NOT NULL,
                field       TEXT NOT NULL,
                format      TEXT NOT NULL DEFAULT 'text',
                old_value   TEXT NOT NULL,
                new_value   TEXT NOT NULL,
                undone_at   TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_events_timestamp ON events(timestamp);
            ",
        )?;

        // Step 2: idempotent column-adds for event logs created by v0.2.x
        // before `format` and `undone_at` existed. Must run BEFORE any index
        // that references these columns — otherwise the index creation
        // fails on old DBs and aborts the whole migration.
        self.add_column_if_missing("events", "format", "TEXT NOT NULL DEFAULT 'text'")?;
        self.add_column_if_missing("events", "undone_at", "TEXT")?;

        // Step 3: now safe to create indexes on the new columns.
        self.conn
            .execute_batch("CREATE INDEX IF NOT EXISTS idx_events_undone ON events(undone_at);")?;
        Ok(())
    }

    /// Add a column to a table if it doesn't already exist.
    ///
    /// Relies on SQLite's ALTER TABLE ADD COLUMN returning a "duplicate
    /// column name" error when the column already exists. `pragma_table_info`
    /// with bound parameters is unreliable across SQLite versions, so we
    /// trap the error instead.
    fn add_column_if_missing(&self, table: &str, column: &str, decl: &str) -> Result<()> {
        let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {decl}");
        match self.conn.execute(&sql, []) {
            Ok(_) => Ok(()),
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                if msg.contains("duplicate column") || msg.contains("already exists") {
                    Ok(())
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Record a reconciliation action.
    pub fn record(&self, action: &ReconcileAction) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events (tool_name, file_path, field, format, old_value, new_value)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                action.tool_name,
                action.file_path.to_string_lossy().as_ref(),
                action.field,
                action.format,
                action.old_value,
                action.new_value,
            ],
        )?;
        Ok(())
    }

    /// Get the most recent N log entries (regardless of undone state).
    ///
    /// Ordered by `id DESC` (insertion order) rather than `timestamp`, because
    /// SQLite's `datetime('now')` only has 1-second resolution and would
    /// produce non-deterministic ordering for events in the same second.
    pub fn recent(&self, limit: usize) -> Result<Vec<LogEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, timestamp, tool_name, file_path, field, format,
                    old_value, new_value, undone_at
             FROM events ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], row_to_entry)?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
    }

    /// Get the most recent N entries that have NOT yet been undone.
    /// Used by `sessionguard undo` to find candidate actions to reverse.
    pub fn recent_pending_undo(&self, limit: usize) -> Result<Vec<LogEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, timestamp, tool_name, file_path, field, format,
                    old_value, new_value, undone_at
             FROM events WHERE undone_at IS NULL
             ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], row_to_entry)?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
    }

    /// Fetch a specific event by id. Returns `Ok(None)` if no row matches.
    pub fn get(&self, id: i64) -> Result<Option<LogEntry>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, timestamp, tool_name, file_path, field, format,
                        old_value, new_value, undone_at
                 FROM events WHERE id = ?1",
                params![id],
                row_to_entry,
            )
            .optional()?;
        Ok(row)
    }

    /// Mark an event as undone with the current timestamp. Idempotent — a
    /// second call is a no-op.
    pub fn mark_undone(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE events SET undone_at = datetime('now')
             WHERE id = ?1 AND undone_at IS NULL",
            params![id],
        )?;
        Ok(())
    }

    /// Count total events.
    pub fn count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
        Ok(count as usize)
    }
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<LogEntry> {
    Ok(LogEntry {
        id: row.get(0)?,
        timestamp: row.get(1)?,
        tool_name: row.get(2)?,
        file_path: PathBuf::from(row.get::<_, String>(3)?),
        field: row.get(4)?,
        format: row.get(5)?,
        old_value: row.get(6)?,
        new_value: row.get(7)?,
        undone_at: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_action() -> ReconcileAction {
        ReconcileAction {
            tool_name: "claude_code".to_string(),
            file_path: PathBuf::from(".claude/settings.json"),
            field: "project_path".to_string(),
            format: "json".to_string(),
            old_value: "/old/path".to_string(),
            new_value: "/new/path".to_string(),
        }
    }

    #[test]
    fn event_log_round_trip() {
        let log = EventLog::open_in_memory().unwrap();
        log.record(&sample_action()).unwrap();

        let entries = log.recent(10).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tool_name, "claude_code");
        assert_eq!(entries[0].format, "json");
        assert_eq!(entries[0].old_value, "/old/path");
        assert!(entries[0].undone_at.is_none());
    }

    #[test]
    fn mark_undone_hides_from_pending() {
        let log = EventLog::open_in_memory().unwrap();
        log.record(&sample_action()).unwrap();
        let pending_before = log.recent_pending_undo(10).unwrap();
        assert_eq!(pending_before.len(), 1);
        let id = pending_before[0].id;

        log.mark_undone(id).unwrap();

        let pending_after = log.recent_pending_undo(10).unwrap();
        assert!(pending_after.is_empty(), "undone events should not appear");

        // But still visible via `recent()` and `get()`
        assert_eq!(log.recent(10).unwrap().len(), 1);
        let got = log.get(id).unwrap().unwrap();
        assert!(got.undone_at.is_some());
    }

    #[test]
    fn mark_undone_is_idempotent() {
        let log = EventLog::open_in_memory().unwrap();
        log.record(&sample_action()).unwrap();
        let id = log.recent(1).unwrap()[0].id;
        log.mark_undone(id).unwrap();
        let first_ts = log.get(id).unwrap().unwrap().undone_at;
        log.mark_undone(id).unwrap();
        let second_ts = log.get(id).unwrap().unwrap().undone_at;
        assert_eq!(first_ts, second_ts, "undone_at must not change on repeat");
    }

    #[test]
    fn get_returns_none_for_missing_id() {
        let log = EventLog::open_in_memory().unwrap();
        assert!(log.get(999).unwrap().is_none());
    }
}
