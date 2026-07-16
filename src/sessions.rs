// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Per-project session census across the tools' home-directory stores.
//!
//! Where `inventory` reports store-level totals ("codex: 2.5 GB"), this module
//! flips the axis to the question operators actually ask: *"for each PROJECT,
//! which assistants have sessions, how many, and how fresh?"* — including
//! sessions whose project directory no longer exists (**orphans**), which is
//! the first signal for cleaning up or archiving stale session data.
//!
//! Store formats (mirrors what the dashboard's Activity tab pioneered; this
//! module is now the single source of truth and the dashboard consumes it):
//! - **Claude Code**: `~/.claude/projects/<encoded>/` — dir name is the
//!   project path with `/` → `-`. Segments may themselves contain hyphens,
//!   so decoding DFS-validates against the real filesystem.
//! - **Codex**: `~/.codex/sessions/**/*.jsonl` — `cwd` in the first JSON line.
//! - **OpenCode**: `~/.local/share/opencode/opencode.db` — SQLite, read-only;
//!   `session.directory` carries the project dir.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// Cap on files visited per store walk, so a pathological store can't spin.
const SESSION_WALK_CAP: usize = 50_000;

/// One tool's sessions for one project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ToolSessions {
    /// Number of session files (Claude/Codex) or session rows (OpenCode).
    pub count: usize,
    /// Total bytes of the session files (0 for OpenCode — its sessions share
    /// one database file, so per-session sizes aren't attributable).
    pub bytes: u64,
    /// Most recent activity, unix seconds.
    pub last_active_unix: u64,
}

/// All known sessions for one project directory, across tools.
#[derive(Debug, Clone, Serialize)]
pub struct SessionGroup {
    /// The project directory the sessions belong to. When `decoded` is false
    /// this is the raw encoded store name (best effort — still listed rather
    /// than hidden).
    pub project_path: String,
    /// Whether `project_path` was confidently resolved to a real path form.
    pub decoded: bool,
    /// True when the project directory no longer exists on disk — the
    /// sessions are orphaned (candidates for archive/cleanup).
    pub orphaned: bool,
    /// Per-tool session summaries, keyed by tool name.
    pub tools: BTreeMap<String, ToolSessions>,
}

/// Census every known home-dir session store under `home`, grouped by
/// project directory. Read-only.
pub fn census(home: &Path) -> Vec<SessionGroup> {
    let mut groups: BTreeMap<String, (bool, BTreeMap<String, ToolSessions>)> = BTreeMap::new();

    let mut absorb = |path: String, decoded: bool, tool: &str, add: ToolSessions| {
        let entry = groups
            .entry(path)
            .or_insert_with(|| (decoded, BTreeMap::new()));
        // Once any source confidently decodes the path, keep that.
        entry.0 |= decoded;
        let t = entry.1.entry(tool.to_string()).or_default();
        t.count += add.count;
        t.bytes += add.bytes;
        t.last_active_unix = t.last_active_unix.max(add.last_active_unix);
    };

    claude_sessions(home, &mut absorb);
    codex_sessions(home, &mut absorb);
    opencode_sessions(home, &mut absorb);

    groups
        .into_iter()
        .map(|(project_path, (decoded, tools))| {
            let orphaned = decoded && !Path::new(&project_path).exists();
            SessionGroup {
                project_path,
                decoded,
                orphaned,
                tools,
            }
        })
        .collect()
}

/// Decode a Claude Code project directory name (e.g.
/// `-Users-devo-Droco-side-projects-ai-session-track`) into a real filesystem
/// path by DFS-validating each candidate segment split against the actual
/// filesystem. Segments can legitimately contain hyphens, so naive `-` → `/`
/// replacement breaks; we bias toward MORE path separators (shorter segments)
/// and only collapse hyphens into a segment when no split alternative exists
/// on disk. Returns `(path, decoded_ok)`; on failure the encoded name is
/// returned unchanged so callers can still display the entry.
pub fn decode_claude_project_dir(name: &str) -> (String, bool) {
    let Some(rest) = name.strip_prefix('-') else {
        return (name.to_string(), false);
    };
    let parts: Vec<&str> = rest.split('-').collect();

    fn walk(base: &Path, remaining: &[&str]) -> Option<PathBuf> {
        if remaining.is_empty() {
            return Some(base.to_path_buf());
        }
        for k in 1..=remaining.len() {
            let segment = remaining[..k].join("-");
            let candidate = base.join(&segment);
            if candidate.is_dir() {
                if let Some(found) = walk(&candidate, &remaining[k..]) {
                    return Some(found);
                }
            }
        }
        None
    }

    match walk(Path::new("/"), &parts) {
        Some(p) => (p.display().to_string(), true),
        None => (name.to_string(), false),
    }
}

/// `~/.claude/projects/<encoded>/` — one subdir per project; each file inside
/// is session state (transcripts, todos, …).
fn claude_sessions(home: &Path, absorb: &mut impl FnMut(String, bool, &str, ToolSessions)) {
    let base = home.join(".claude/projects");
    let Ok(entries) = std::fs::read_dir(&base) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let (path, decoded) = decode_claude_project_dir(&name);

        let mut count = 0usize;
        let mut bytes = 0u64;
        let mut last = 0u64;
        if let Ok(files) = std::fs::read_dir(&dir) {
            for f in files.flatten() {
                let Ok(meta) = f.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                count += 1;
                bytes += meta.len();
                last = last.max(unix_mtime(&meta));
            }
        }
        if count == 0 {
            continue;
        }
        absorb(
            path,
            decoded,
            "claude_code",
            ToolSessions {
                count,
                bytes,
                last_active_unix: last,
            },
        );
    }
}

/// `~/.codex/sessions/**/*.jsonl` — the first JSON line carries `cwd` (or
/// `payload.cwd` in newer layouts).
fn codex_sessions(home: &Path, absorb: &mut impl FnMut(String, bool, &str, ToolSessions)) {
    let base = home.join(".codex/sessions");
    if !base.is_dir() {
        return;
    }
    let mut stack = vec![base];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(p);
                continue;
            }
            if !meta.is_file() || p.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            visited += 1;
            if visited > SESSION_WALK_CAP {
                return;
            }
            let Some(cwd) = codex_cwd(&p) else { continue };
            absorb(
                cwd,
                true,
                "codex",
                ToolSessions {
                    count: 1,
                    bytes: meta.len(),
                    last_active_unix: unix_mtime(&meta),
                },
            );
        }
    }
}

/// Extract `cwd` (or `payload.cwd`) from the first line of a Codex session
/// file, reading only a bounded prefix.
fn codex_cwd(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(f).read_line(&mut line).ok()?;
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    v.get("cwd")
        .and_then(|c| c.as_str())
        .or_else(|| {
            v.get("payload")
                .and_then(|p| p.get("cwd"))
                .and_then(|c| c.as_str())
        })
        .map(|s| s.to_string())
}

/// `~/.local/share/opencode/opencode.db` — read-only SQLite;
/// `session.directory` names the project dir, timestamps are unix-ms.
/// Any error (schema drift, lock, missing) degrades to "no rows".
fn opencode_sessions(home: &Path, absorb: &mut impl FnMut(String, bool, &str, ToolSessions)) {
    let db = home.join(".local/share/opencode/opencode.db");
    if !db.is_file() {
        return;
    }
    let Ok(conn) =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return;
    };
    let Ok(mut stmt) =
        conn.prepare("SELECT directory, time_updated FROM session WHERE time_archived IS NULL")
    else {
        return;
    };
    let rows = stmt.query_map([], |row| {
        let dir: Option<String> = row.get(0)?;
        let updated_ms: Option<i64> = row.get(1)?;
        Ok((dir, updated_ms))
    });
    let Ok(rows) = rows else { return };
    for row in rows.flatten() {
        let (Some(dir), updated_ms) = row else {
            continue;
        };
        if dir.is_empty() {
            continue;
        }
        absorb(
            dir,
            true,
            "opencode",
            ToolSessions {
                count: 1,
                bytes: 0,
                last_active_unix: (updated_ms.unwrap_or(0) / 1000).max(0) as u64,
            },
        );
    }
}

fn unix_mtime(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn decode_resolves_hyphenated_segments_against_filesystem() {
        // Build a real tree containing a hyphenated segment, then encode it
        // the way Claude Code does and confirm the DFS decoder recovers it.
        let root = TempDir::new().unwrap();
        let project = root.path().join("work/side-projects/myapp");
        std::fs::create_dir_all(&project).unwrap();

        let encoded = project.display().to_string().replace('/', "-");
        let (decoded, ok) = decode_claude_project_dir(&encoded);
        assert!(ok, "should decode against the real filesystem");
        assert_eq!(decoded, project.display().to_string());
    }

    #[test]
    fn decode_returns_encoded_name_when_unresolvable() {
        let (out, ok) = decode_claude_project_dir("-no-such-root-anywhere-zzz");
        assert!(!ok);
        assert_eq!(out, "-no-such-root-anywhere-zzz");
    }

    #[test]
    fn census_groups_claude_and_codex_by_project_and_flags_orphans() {
        let home = TempDir::new().unwrap();
        // A live project (exists on disk) with a Claude store dir.
        let live = home.path().join("proj/alpha");
        std::fs::create_dir_all(&live).unwrap();
        let enc = live.display().to_string().replace('/', "-");
        let store = home.path().join(".claude/projects").join(&enc);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("s1.jsonl"), b"{}").unwrap();
        std::fs::write(store.join("s2.jsonl"), b"{}").unwrap();

        // A Codex session pointing at a project dir that DOESN'T exist.
        let gone = home.path().join("proj/deleted");
        let codex = home.path().join(".codex/sessions/2026/07");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(
            codex.join("rollout-1.jsonl"),
            format!("{{\"cwd\": \"{}\"}}\n", gone.display()),
        )
        .unwrap();

        let groups = census(home.path());
        let live_group = groups
            .iter()
            .find(|g| g.project_path == live.display().to_string())
            .expect("live project present");
        assert!(!live_group.orphaned);
        assert!(live_group.decoded);
        assert_eq!(live_group.tools["claude_code"].count, 2);

        let orphan = groups
            .iter()
            .find(|g| g.project_path == gone.display().to_string())
            .expect("codex project present");
        assert!(
            orphan.orphaned,
            "nonexistent project dir must flag orphaned"
        );
        assert_eq!(orphan.tools["codex"].count, 1);
    }

    #[test]
    fn census_reads_opencode_db_read_only() {
        let home = TempDir::new().unwrap();
        let dbdir = home.path().join(".local/share/opencode");
        std::fs::create_dir_all(&dbdir).unwrap();
        let conn = rusqlite::Connection::open(dbdir.join("opencode.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (directory TEXT, time_updated INTEGER, time_archived INTEGER);
             INSERT INTO session VALUES ('/tmp/oc-proj', 1752600000000, NULL);
             INSERT INTO session VALUES ('/tmp/oc-proj', 1752610000000, NULL);
             INSERT INTO session VALUES ('/tmp/archived', 1752600000000, 1752600000001);",
        )
        .unwrap();
        drop(conn);

        let groups = census(home.path());
        let g = groups
            .iter()
            .find(|g| g.project_path == "/tmp/oc-proj")
            .expect("opencode rows surfaced");
        assert_eq!(g.tools["opencode"].count, 2, "archived rows excluded");
        assert_eq!(g.tools["opencode"].last_active_unix, 1_752_610_000);
    }
}
