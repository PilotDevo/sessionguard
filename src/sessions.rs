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
//! Store layouts are *declarations*, not hardcoded readers: each tool's
//! `[tool.session_store]` (see [`crate::tools::SessionStore`]) names one of
//! three data-bound shapes, and [`census`] dispatches to the matching reader
//! with that declaration's bindings. Adding or repointing a store is a TOML
//! change, not a recompile:
//! - **`encoded_dir`** (Claude Code): one directory per project; the
//!   directory name is the project path with each path separator collapsed
//!   into `separator`. Segments may themselves contain hyphens, so decoding
//!   DFS-validates each candidate split against the real filesystem.
//! - **`jsonl_field`** (Codex): a tree of JSONL files; the project path is a
//!   field (optionally dotted, e.g. `payload.cwd`) on the first line.
//! - **`sqlite_column`** (OpenCode): rows in a SQLite table, opened
//!   read-only; a column carries the project dir and an optional column
//!   flags archived rows.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::tools::{SessionStore, TimeUnit};

/// Cap on files visited per store walk, so a pathological store can't spin.
const SESSION_WALK_CAP: usize = 50_000;

/// How confidently a store entry was resolved to a real project path.
/// Ordered most-confident first so `min` keeps the best decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecodeConfidence {
    Exact,
    Inferred,
    Unresolved,
}

/// One tool's sessions for one project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGroup {
    /// The project directory the sessions belong to. At `Inferred`
    /// confidence this is a best-effort naive decode rather than a
    /// filesystem-validated path; at `Unresolved` it is the raw encoded
    /// store name (still listed rather than hidden). See
    /// [`DecodeConfidence`].
    pub project_path: String,
    /// Whether `project_path` was confidently resolved to a real path form.
    pub decoded: bool,
    /// How confidently `project_path` was resolved (see [`DecodeConfidence`]).
    pub confidence: DecodeConfidence,
    /// True when the project directory no longer exists on disk — the
    /// sessions are orphaned (candidates for archive/cleanup).
    pub orphaned: bool,
    /// The host this census was taken on. Always `"local"` for now — a
    /// future fleet census will populate this from remote sources.
    pub host: String,
    /// Per-tool session summaries, keyed by tool name.
    pub tools: BTreeMap<String, ToolSessions>,
}

/// Expand a leading `~` against `home`. Declarations use `~` so the same TOML
/// works for any user and any census root (including a mounted remote home).
fn expand(home: &Path, raw: &str) -> PathBuf {
    match raw.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if raw == "~" => home.to_path_buf(),
        None => PathBuf::from(raw),
    }
}

/// SQLite identifiers come from a TOML file, so they are validated rather than
/// interpolated blindly — a malicious or fat-fingered declaration must not be
/// able to inject SQL.
fn safe_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Census every declared session store under `home`, grouped by project
/// directory. Read-only. `stores` pairs each tool name with the store
/// declaration to read it with (typically `ToolRegistry::all()` filtered to
/// tools that declare `session_store`).
pub fn census(home: &Path, stores: &[(String, SessionStore)]) -> Vec<SessionGroup> {
    let mut groups: BTreeMap<String, (DecodeConfidence, BTreeMap<String, ToolSessions>)> =
        BTreeMap::new();

    let mut absorb = |path: String, conf: DecodeConfidence, tool: &str, add: ToolSessions| {
        let entry = groups
            .entry(path)
            .or_insert_with(|| (conf, BTreeMap::new()));
        // Keep the most confident decode any source produced.
        if conf < entry.0 {
            entry.0 = conf;
        }
        let t = entry.1.entry(tool.to_string()).or_default();
        t.count += add.count;
        t.bytes += add.bytes;
        t.last_active_unix = t.last_active_unix.max(add.last_active_unix);
    };

    for (tool, store) in stores {
        match store {
            SessionStore::EncodedDir { path, separator } => {
                read_encoded_dir(&expand(home, path), separator, tool, &mut absorb)
            }
            SessionStore::JsonlField {
                path,
                glob,
                key_field,
                fallback_field,
            } => read_jsonl_field(
                &expand(home, path),
                glob,
                key_field,
                fallback_field.as_deref(),
                tool,
                &mut absorb,
            ),
            SessionStore::SqliteColumn {
                path,
                table,
                path_column,
                updated_column,
                updated_unit,
                archived_column,
            } => read_sqlite_column(
                &expand(home, path),
                table,
                path_column,
                updated_column.as_deref(),
                *updated_unit,
                archived_column.as_deref(),
                tool,
                &mut absorb,
            ),
        }
    }

    groups
        .into_iter()
        .map(|(project_path, (confidence, tools))| {
            let exists = Path::new(&project_path).exists();
            SessionGroup {
                orphaned: matches!(
                    confidence,
                    DecodeConfidence::Exact | DecodeConfidence::Inferred
                ) && !exists,
                decoded: confidence == DecodeConfidence::Exact,
                project_path,
                confidence,
                host: "local".to_string(),
                tools,
            }
        })
        .collect()
}

/// Decode an encoded project directory name (e.g.
/// `-Users-devo-Droco-side-projects-ai-session-track`) into a real filesystem
/// path. Segments can legitimately contain the separator, so naive
/// `separator` → `/` replacement is ambiguous on its own; three-step
/// algorithm:
///
/// 1. DFS-validate candidate splits against the real filesystem, biasing
///    toward MORE path separators (shorter segments) and only collapsing the
///    separator into a segment when no split alternative exists on disk. If
///    this consumes the whole name, the path is `Exact`.
/// 2. Otherwise the project directory itself is most likely gone. Find the
///    *deepest ancestor* the DFS above DID validate, then fold everything
///    after it into ONE final segment (rejoined with `separator`) rather
///    than guessing further internal structure — e.g. ancestor
///    `/home/devo/projects` with leftover parts `["amarillo", "project"]`
///    becomes `/home/devo/projects/amarillo-project`. This gets the common
///    shape exactly right (one deleted leaf under a surviving parent,
///    regardless of hyphens in the leaf's own name) and is reported
///    `Inferred`. It is still a best guess, not a validated fact: if an
///    ancestor directory is *also* gone (a deeper, multi-segment deletion),
///    more than the true leaf gets folded into that one segment, and the
///    proposed path can be wrong — `Inferred` means "best guess," not
///    "confirmed."
/// 3. If not even the first segment matches a real directory, there is
///    nothing on disk to anchor a guess to; fall back to the fully naive
///    `separator` → `/` decode, still `Inferred` — better than an opaque
///    encoded name.
/// 4. `name` not starting with `separator` at all stays `Unresolved`: there
///    is nothing path-shaped to propose, so the raw name is returned
///    unchanged.
pub fn decode_claude_project_dir(name: &str, separator: &str) -> (String, DecodeConfidence) {
    let Some(rest) = name.strip_prefix(separator) else {
        return (name.to_string(), DecodeConfidence::Unresolved);
    };
    let parts: Vec<&str> = rest.split(separator).collect();

    fn walk(base: &Path, remaining: &[&str], separator: &str) -> Option<PathBuf> {
        if remaining.is_empty() {
            return Some(base.to_path_buf());
        }
        for k in 1..=remaining.len() {
            let segment = remaining[..k].join(separator);
            let candidate = base.join(&segment);
            if candidate.is_dir() {
                if let Some(found) = walk(&candidate, &remaining[k..], separator) {
                    return Some(found);
                }
            }
        }
        None
    }

    if let Some(found) = walk(Path::new("/"), &parts, separator) {
        return (found.display().to_string(), DecodeConfidence::Exact);
    }

    // Full DFS failed. Find the deepest ancestor it DID validate along any
    // explored branch (same split candidates as `walk`, but tracking the
    // best partial match instead of requiring full consumption).
    fn deepest_match(base: &Path, remaining: &[&str], separator: &str) -> (PathBuf, usize) {
        let mut best = (base.to_path_buf(), 0usize);
        for k in 1..=remaining.len() {
            let segment = remaining[..k].join(separator);
            let candidate = base.join(&segment);
            if candidate.is_dir() {
                let (deeper_base, deeper_count) =
                    deepest_match(&candidate, &remaining[k..], separator);
                let total = k + deeper_count;
                if total > best.1 {
                    best = (deeper_base, total);
                }
            }
        }
        best
    }

    let (ancestor, consumed) = deepest_match(Path::new("/"), &parts, separator);
    if consumed == 0 {
        // Not even the first segment matched a real directory.
        return (format!("/{}", parts.join("/")), DecodeConfidence::Inferred);
    }
    let leaf = parts[consumed..].join(separator);
    (
        ancestor.join(leaf).display().to_string(),
        DecodeConfidence::Inferred,
    )
}

/// `encoded_dir` layout: one subdir per project under `base`; each file
/// inside is session state (transcripts, todos, …).
fn read_encoded_dir(
    base: &Path,
    separator: &str,
    tool: &str,
    absorb: &mut impl FnMut(String, DecodeConfidence, &str, ToolSessions),
) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let (path, confidence) = decode_claude_project_dir(&name, separator);

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
            confidence,
            tool,
            ToolSessions {
                count,
                bytes,
                last_active_unix: last,
            },
        );
    }
}

/// `jsonl_field` layout: a tree of JSONL files under `base` matching
/// `pattern`; `key_field` (optionally dotted, e.g. `payload.cwd`) on the
/// first line of each file carries the project path, with `fallback_field`
/// tried when `key_field` is absent (older/newer schema variants).
fn read_jsonl_field(
    base: &Path,
    pattern: &str,
    key_field: &str,
    fallback_field: Option<&str>,
    tool: &str,
    absorb: &mut impl FnMut(String, DecodeConfidence, &str, ToolSessions),
) {
    if !base.is_dir() {
        return;
    }
    let full = format!("{}/{}", base.display(), pattern);
    let Ok(paths) = glob::glob(&full) else {
        return;
    };
    for (visited, entry) in paths.enumerate() {
        if visited > SESSION_WALK_CAP {
            return;
        }
        let Ok(p) = entry else { continue };
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Some(cwd) = jsonl_key(&p, key_field, fallback_field) else {
            continue;
        };
        absorb(
            cwd,
            DecodeConfidence::Exact,
            tool,
            ToolSessions {
                count: 1,
                bytes: meta.len(),
                last_active_unix: unix_mtime(&meta),
            },
        );
    }
}

/// Extract `key_field` (or `fallback_field`) from the first line of a JSONL
/// file, reading only a bounded prefix.
fn jsonl_key(path: &Path, key_field: &str, fallback_field: Option<&str>) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(f).read_line(&mut line).ok()?;
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    json_field(&v, key_field).or_else(|| fallback_field.and_then(|f| json_field(&v, f)))
}

/// Look up a dotted field path (e.g. `payload.cwd`) inside a JSON value.
fn json_field(v: &serde_json::Value, field: &str) -> Option<String> {
    let mut cur = v;
    for part in field.split('.') {
        cur = cur.get(part)?;
    }
    cur.as_str().map(str::to_string)
}

/// `sqlite_column` layout: rows in a SQLite table at `db`, opened read-only;
/// `path_column` names the project dir, `updated_column` (in `updated_unit`)
/// tracks recency, and rows with a non-null `archived_column` are excluded.
/// Table/column names come from a TOML declaration, so they are validated
/// with [`safe_ident`] before ever being interpolated into a query — an
/// unsafe identifier is never built into SQL; when the table or the path
/// column itself is unsafe there is no safe query to run, so the whole store
/// is skipped.
#[allow(clippy::too_many_arguments)]
fn read_sqlite_column(
    db: &Path,
    table: &str,
    path_column: &str,
    updated_column: Option<&str>,
    updated_unit: TimeUnit,
    archived_column: Option<&str>,
    tool: &str,
    absorb: &mut impl FnMut(String, DecodeConfidence, &str, ToolSessions),
) {
    if !db.is_file() {
        return;
    }
    if !safe_ident(table) || !safe_ident(path_column) {
        tracing::warn!(
            tool,
            "session_store declares unsafe SQL identifiers; skipping"
        );
        return;
    }
    let mut sql = format!("SELECT {path_column}");
    match updated_column {
        Some(c) if safe_ident(c) => sql.push_str(&format!(", {c}")),
        _ => sql.push_str(", NULL"),
    }
    sql.push_str(&format!(" FROM {table}"));
    if let Some(a) = archived_column.filter(|a| safe_ident(a)) {
        sql.push_str(&format!(" WHERE {a} IS NULL"));
    }

    let Ok(conn) =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return;
    };
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return;
    };
    let rows = stmt.query_map([], |row| {
        let dir: Option<String> = row.get(0)?;
        let updated: Option<i64> = row.get(1)?;
        Ok((dir, updated))
    });
    let Ok(rows) = rows else { return };
    for row in rows.flatten() {
        let (Some(dir), updated) = row else {
            continue;
        };
        if dir.is_empty() {
            continue;
        }
        let raw = updated.unwrap_or(0).max(0) as u64;
        let last_active_unix = match updated_unit {
            TimeUnit::Ms => raw / 1000,
            TimeUnit::S => raw,
        };
        absorb(
            dir,
            DecodeConfidence::Exact,
            tool,
            ToolSessions {
                count: 1,
                bytes: 0,
                last_active_unix,
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

    /// The builtin store declarations, expressed as data — mirrors what the
    /// TOMLs will declare in Task 3.
    fn test_stores() -> Vec<(String, SessionStore)> {
        vec![
            (
                "claude_code".into(),
                SessionStore::EncodedDir {
                    path: "~/.claude/projects".into(),
                    separator: "-".into(),
                },
            ),
            (
                "codex".into(),
                SessionStore::JsonlField {
                    path: "~/.codex/sessions".into(),
                    glob: "**/*.jsonl".into(),
                    key_field: "cwd".into(),
                    fallback_field: Some("payload.cwd".into()),
                },
            ),
            (
                "opencode".into(),
                SessionStore::SqliteColumn {
                    path: "~/.local/share/opencode/opencode.db".into(),
                    table: "session".into(),
                    path_column: "directory".into(),
                    updated_column: Some("time_updated".into()),
                    updated_unit: TimeUnit::Ms,
                    archived_column: Some("time_archived".into()),
                },
            ),
        ]
    }

    #[test]
    fn decode_resolves_hyphenated_segments_against_filesystem() {
        // Build a real tree containing a hyphenated segment, then encode it
        // the way Claude Code does and confirm the DFS decoder recovers it.
        let root = TempDir::new().unwrap();
        let project = root.path().join("work/side-projects/myapp");
        std::fs::create_dir_all(&project).unwrap();

        let encoded = project.display().to_string().replace('/', "-");
        let (decoded, confidence) = decode_claude_project_dir(&encoded, "-");
        assert_eq!(confidence, DecodeConfidence::Exact);
        assert_eq!(decoded, project.display().to_string());
    }

    #[test]
    fn decode_falls_back_to_naive_decode_when_dfs_fails() {
        // No such root exists anywhere, so DFS can't validate any split — the
        // naive `separator` -> `/` decode is proposed instead, at `Inferred`
        // confidence, rather than the opaque encoded name.
        let (out, confidence) = decode_claude_project_dir("-no-such-root-anywhere-zzz", "-");
        assert_eq!(confidence, DecodeConfidence::Inferred);
        assert_eq!(out, "/no/such/root/anywhere/zzz");
    }

    #[test]
    fn decode_is_unresolved_when_name_has_no_leading_separator() {
        // Nothing path-shaped to propose at all: the raw name is returned
        // unchanged, at `Unresolved` confidence.
        let (out, confidence) = decode_claude_project_dir("not-even-path-shaped", "_");
        assert_eq!(confidence, DecodeConfidence::Unresolved);
        assert_eq!(out, "not-even-path-shaped");
    }

    #[test]
    fn decode_folds_hyphenated_leaf_onto_deepest_surviving_ancestor() {
        // Fix round 1: the naive full-substitution fallback got this wrong
        // — it can't tell a literal hyphen inside a directory name from a
        // separator hyphen, so it would split a deleted "amarillo-project"
        // leaf into "amarillo/project". The corrected fallback instead runs
        // DFS as far as the filesystem allows (here: all the way through
        // the surviving "projects" parent), then folds whatever's left
        // ("amarillo", "project") into ONE final segment rejoined with the
        // separator — recovering the hyphenated leaf name exactly.
        let root = TempDir::new().unwrap();
        let parent = root.path().join("home/devo/projects");
        std::fs::create_dir_all(&parent).unwrap();
        let gone = parent.join("amarillo-project"); // deleted: never created

        let encoded = gone.display().to_string().replace('/', "-");
        let (decoded, confidence) = decode_claude_project_dir(&encoded, "-");
        assert_eq!(confidence, DecodeConfidence::Inferred);
        assert_eq!(decoded, gone.display().to_string());
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

        let groups = census(home.path(), &test_stores());
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

        let groups = census(home.path(), &test_stores());
        let g = groups
            .iter()
            .find(|g| g.project_path == "/tmp/oc-proj")
            .expect("opencode rows surfaced");
        assert_eq!(g.tools["opencode"].count, 2, "archived rows excluded");
        assert_eq!(g.tools["opencode"].last_active_unix, 1_752_610_000);
    }

    #[test]
    fn census_is_driven_by_declarations_not_hardcoded_paths() {
        // A store declared at a NON-default path must be read; a hardcoded
        // reader would find nothing here.
        let home = TempDir::new().unwrap();
        let live = home.path().join("proj/alpha");
        std::fs::create_dir_all(&live).unwrap();
        let enc = live.display().to_string().replace('/', "-");
        let store = home.path().join("custom/store").join(&enc);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("s1.jsonl"), b"{}").unwrap();

        let stores = vec![(
            "claude_code".to_string(),
            SessionStore::EncodedDir {
                path: "~/custom/store".into(),
                separator: "-".into(),
            },
        )];
        let groups = census(home.path(), &stores);
        assert_eq!(groups.len(), 1, "declared custom path must be read");
        assert_eq!(groups[0].tools["claude_code"].count, 1);
    }

    #[test]
    fn sqlite_column_skips_whole_store_when_table_or_path_column_is_unsafe() {
        // `table`/`path_column` have no safe fallback to substitute — an
        // unsafe identifier there means there is no safe query to run at
        // all, so the whole store is skipped rather than ever interpolating
        // it into SQL.
        let home = TempDir::new().unwrap();
        let dbdir = home.path().join(".local/share/opencode");
        std::fs::create_dir_all(&dbdir).unwrap();
        let conn = rusqlite::Connection::open(dbdir.join("opencode.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (directory TEXT, time_updated INTEGER, time_archived INTEGER);
             INSERT INTO session VALUES ('/tmp/oc-proj', 1752600000000, NULL);",
        )
        .unwrap();
        drop(conn);

        let unsafe_table = vec![(
            "opencode".to_string(),
            SessionStore::SqliteColumn {
                path: "~/.local/share/opencode/opencode.db".into(),
                table: "session; DROP TABLE session;--".into(),
                path_column: "directory".into(),
                updated_column: Some("time_updated".into()),
                updated_unit: TimeUnit::Ms,
                archived_column: Some("time_archived".into()),
            },
        )];
        assert!(
            census(home.path(), &unsafe_table).is_empty(),
            "unsafe table identifier must skip the whole store without building a query"
        );

        let unsafe_path_column = vec![(
            "opencode".to_string(),
            SessionStore::SqliteColumn {
                path: "~/.local/share/opencode/opencode.db".into(),
                table: "session".into(),
                path_column: "directory, (SELECT 1)".into(),
                updated_column: Some("time_updated".into()),
                updated_unit: TimeUnit::Ms,
                archived_column: Some("time_archived".into()),
            },
        )];
        assert!(
            census(home.path(), &unsafe_path_column).is_empty(),
            "unsafe path_column identifier must skip the whole store without building a query"
        );
    }

    #[test]
    fn sqlite_column_degrades_gracefully_when_optional_columns_are_unsafe() {
        // Unlike `table`/`path_column`, the optional `updated_column` and
        // `archived_column` DO have a safe fallback: they are simply never
        // interpolated (substituted with `NULL` / the `WHERE` clause is
        // dropped) rather than aborting the whole store. Prove the store
        // still reads — including the row that would have been excluded had
        // the archived-column filter actually applied.
        let home = TempDir::new().unwrap();
        let dbdir = home.path().join(".local/share/opencode");
        std::fs::create_dir_all(&dbdir).unwrap();
        let conn = rusqlite::Connection::open(dbdir.join("opencode.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (directory TEXT, time_updated INTEGER, time_archived INTEGER);
             INSERT INTO session VALUES ('/tmp/oc-proj', 1752600000000, NULL);
             INSERT INTO session VALUES ('/tmp/archived', 1752600000000, 1752600000001);",
        )
        .unwrap();
        drop(conn);

        let stores = vec![(
            "opencode".to_string(),
            SessionStore::SqliteColumn {
                path: "~/.local/share/opencode/opencode.db".into(),
                table: "session".into(),
                path_column: "directory".into(),
                updated_column: Some("time_updated; --".into()),
                updated_unit: TimeUnit::Ms,
                archived_column: Some("time_archived OR 1=1".into()),
            },
        )];

        let groups = census(home.path(), &stores);
        assert!(
            groups.iter().any(|g| g.project_path == "/tmp/oc-proj"),
            "safe rows must still be read when only optional columns are unsafe"
        );
        assert!(
            groups.iter().any(|g| g.project_path == "/tmp/archived"),
            "unsafe archived_column must degrade to 'no filter', not skip the store \
             (the row that would have been excluded must still surface)"
        );
        let live = groups
            .iter()
            .find(|g| g.project_path == "/tmp/oc-proj")
            .unwrap();
        assert_eq!(
            live.tools["opencode"].last_active_unix, 0,
            "unsafe updated_column must fall back to the NULL substitution, not the real column"
        );
    }

    #[test]
    fn deleted_claude_project_is_inferred_and_orphaned() {
        // Today this is impossible: decode DFS-validates against the live
        // filesystem, so a DELETED project never decodes and therefore never
        // reports as orphaned. Inferred confidence is what makes Claude Code
        // orphan detection work at all.
        //
        // Fix round 1: the task brief's literal fixture used "deleted-app"
        // as the leaf segment, but its own naive-fallback algorithm
        // (`separator` -> `/` split-then-join over the WHOLE name) can't
        // distinguish that leaf's literal hyphen from a path-separator
        // hyphen — "deleted-app" round-trips to "deleted/app", not
        // "deleted-app". The corrected algorithm fixes this generally by
        // running DFS as far as the filesystem allows and folding only the
        // truly-unvalidated tail into one segment — which requires a real
        // surviving ancestor to anchor to. So unlike the brief's literal
        // fixture, this test creates `work/` (the project's parent) without
        // creating `work/deleted-app` (the project itself, which is gone).
        let home = TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join("work")).unwrap();
        let gone = home.path().join("work/deleted-app");
        let enc = gone.display().to_string().replace('/', "-");
        let store = home.path().join(".claude/projects").join(&enc);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("s.jsonl"), b"{}").unwrap();

        let groups = census(home.path(), &test_stores());
        let g = groups
            .iter()
            .find(|g| g.project_path == gone.display().to_string())
            .expect("DFS-plus-fold decode should propose the deleted path");
        assert_eq!(g.confidence, DecodeConfidence::Inferred);
        assert!(g.orphaned, "a deleted project must report as orphaned");
        assert!(!g.decoded, "compat field: only Exact counts as decoded");
    }

    #[test]
    fn deleted_claude_project_directly_under_home_is_inferred_and_orphaned() {
        // Simpler coverage alongside the hyphenated-leaf case above: no
        // intermediate parent to fold across, so this also exercises the
        // "no ancestor beyond the trivial root chain" shape without any
        // separator ambiguity in play.
        let home = TempDir::new().unwrap();
        let gone = home.path().join("deleted_project");
        let enc = gone.display().to_string().replace('/', "-");
        let store = home.path().join(".claude/projects").join(&enc);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("s.jsonl"), b"{}").unwrap();

        let groups = census(home.path(), &test_stores());
        let g = groups
            .iter()
            .find(|g| g.project_path == gone.display().to_string())
            .expect("decode should propose the deleted path");
        assert_eq!(g.confidence, DecodeConfidence::Inferred);
        assert!(g.orphaned, "a deleted project must report as orphaned");
        assert!(!g.decoded, "compat field: only Exact counts as decoded");
    }

    #[test]
    fn live_project_decodes_exact() {
        let home = TempDir::new().unwrap();
        let live = home.path().join("work/app");
        std::fs::create_dir_all(&live).unwrap();
        let enc = live.display().to_string().replace('/', "-");
        let store = home.path().join(".claude/projects").join(&enc);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("s.jsonl"), b"{}").unwrap();

        let groups = census(home.path(), &test_stores());
        assert_eq!(groups[0].confidence, DecodeConfidence::Exact);
        assert!(groups[0].decoded);
        assert!(!groups[0].orphaned);
    }
}
