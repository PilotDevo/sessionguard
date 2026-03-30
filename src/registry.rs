// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! SQLite registry mapping projects to their AI session artifacts.
//!
//! The registry is the source of truth for which projects are tracked
//! and what session artifacts they contain.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::Result;

/// A tracked project entry in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub id: i64,
    pub path: PathBuf,
    pub tools_detected: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A session artifact entry associated with a project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionArtifact {
    pub id: i64,
    pub project_id: i64,
    pub tool_name: String,
    pub artifact_path: PathBuf,
    pub created_at: String,
}

/// The SQLite-backed project/session registry.
pub struct Registry {
    conn: Connection,
}

impl Registry {
    /// Open the registry at the default data directory location.
    pub fn open_default() -> Result<Self> {
        let data_dir = Config::data_dir();
        std::fs::create_dir_all(&data_dir)?;
        let db_path = data_dir.join("registry.db");
        Self::open(&db_path)
    }

    /// Open (or create) the registry at a specific path.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        let registry = Self { conn };
        registry.migrate()?;
        Ok(registry)
    }

    /// Open an in-memory registry (for testing).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let registry = Self { conn };
        registry.migrate()?;
        Ok(registry)
    }

    /// Run schema migrations.
    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS projects (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                path        TEXT NOT NULL UNIQUE,
                created_at  TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS session_artifacts (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                tool_name   TEXT NOT NULL,
                artifact_path TEXT NOT NULL,
                created_at  TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(project_id, tool_name, artifact_path)
            );

            CREATE INDEX IF NOT EXISTS idx_artifacts_project
                ON session_artifacts(project_id);
            CREATE INDEX IF NOT EXISTS idx_projects_path
                ON projects(path);
            ",
        )?;
        Ok(())
    }

    /// Register a project directory. Returns the project ID.
    pub fn register_project(&self, path: &Path) -> Result<i64> {
        let path_str = path.to_string_lossy();
        self.conn.execute(
            "INSERT INTO projects (path) VALUES (?1)
             ON CONFLICT(path) DO UPDATE SET updated_at = datetime('now')",
            params![path_str.as_ref()],
        )?;
        let id = self.conn.query_row(
            "SELECT id FROM projects WHERE path = ?1",
            params![path_str.as_ref()],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Remove a project and its artifacts from the registry.
    pub fn unregister_project(&self, path: &Path) -> Result<()> {
        let path_str = path.to_string_lossy();
        self.conn.execute(
            "DELETE FROM projects WHERE path = ?1",
            params![path_str.as_ref()],
        )?;
        Ok(())
    }

    /// Record a session artifact for a project.
    pub fn add_artifact(
        &self,
        project_id: i64,
        tool_name: &str,
        artifact_path: &Path,
    ) -> Result<()> {
        let artifact_str = artifact_path.to_string_lossy();
        self.conn.execute(
            "INSERT OR REPLACE INTO session_artifacts (project_id, tool_name, artifact_path)
             VALUES (?1, ?2, ?3)",
            params![project_id, tool_name, artifact_str.as_ref()],
        )?;
        Ok(())
    }

    /// Update all path references when a project moves.
    ///
    /// Note: the daemon's move pipeline currently uses `register_project` +
    /// `unregister_project` instead. This method is available for callers
    /// that prefer an atomic in-place update.
    pub fn update_project_path(&self, old_path: &Path, new_path: &Path) -> Result<()> {
        let old_str = old_path.to_string_lossy();
        let new_str = new_path.to_string_lossy();
        self.conn.execute(
            "UPDATE projects SET path = ?1, updated_at = datetime('now') WHERE path = ?2",
            params![new_str.as_ref(), old_str.as_ref()],
        )?;
        Ok(())
    }

    /// List all tracked projects.
    pub fn list_projects(&self) -> Result<Vec<ProjectEntry>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, path, created_at, updated_at FROM projects ORDER BY path")?;
        let rows = stmt.query_map([], |row| {
            Ok(ProjectEntry {
                id: row.get(0)?,
                path: PathBuf::from(row.get::<_, String>(1)?),
                tools_detected: Vec::new(), // populated separately
                created_at: row.get(2)?,
                updated_at: row.get(3)?,
            })
        })?;
        let mut projects = Vec::new();
        for row in rows {
            projects.push(row?);
        }
        Ok(projects)
    }

    /// Get artifacts for a project.
    pub fn get_artifacts(&self, project_id: i64) -> Result<Vec<SessionArtifact>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, tool_name, artifact_path, created_at
             FROM session_artifacts WHERE project_id = ?1",
        )?;
        let rows = stmt.query_map(params![project_id], |row| {
            Ok(SessionArtifact {
                id: row.get(0)?,
                project_id: row.get(1)?,
                tool_name: row.get(2)?,
                artifact_path: PathBuf::from(row.get::<_, String>(3)?),
                created_at: row.get(4)?,
            })
        })?;
        let mut artifacts = Vec::new();
        for row in rows {
            artifacts.push(row?);
        }
        Ok(artifacts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_round_trip() {
        let reg = Registry::open_in_memory().unwrap();
        let id = reg.register_project(Path::new("/tmp/my-project")).unwrap();
        reg.add_artifact(
            id,
            "claude_code",
            Path::new("/tmp/my-project/.claude/settings.json"),
        )
        .unwrap();

        let projects = reg.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].path, PathBuf::from("/tmp/my-project"));

        let artifacts = reg.get_artifacts(id).unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].tool_name, "claude_code");
        assert_eq!(
            artifacts[0].artifact_path,
            PathBuf::from("/tmp/my-project/.claude/settings.json")
        );
    }

    #[test]
    fn registry_deduplicates_artifacts() {
        let reg = Registry::open_in_memory().unwrap();
        let id = reg.register_project(Path::new("/tmp/test")).unwrap();
        let path = Path::new("/tmp/test/.claude/settings.json");
        reg.add_artifact(id, "claude_code", path).unwrap();
        reg.add_artifact(id, "claude_code", path).unwrap(); // duplicate

        let artifacts = reg.get_artifacts(id).unwrap();
        assert_eq!(artifacts.len(), 1, "duplicates should be deduplicated");
    }

    #[test]
    fn registry_update_path() {
        let reg = Registry::open_in_memory().unwrap();
        reg.register_project(Path::new("/old/path")).unwrap();
        reg.update_project_path(Path::new("/old/path"), Path::new("/new/path"))
            .unwrap();

        let projects = reg.list_projects().unwrap();
        assert_eq!(projects[0].path, PathBuf::from("/new/path"));
    }
}
