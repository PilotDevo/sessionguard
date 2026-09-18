// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Store re-keying — the actual reconcile for home-dir session stores.
//!
//! The v0.1–v0.3 reconciler rewrites path references *inside a project*. But
//! the three assistants that matter keep no project path inside the project:
//! Claude Code, Codex and OpenCode key their sessions from a store under
//! `$HOME`. So when a project moves, rewriting in-project files reconciles
//! nothing — the store still points at the old path and every session for that
//! project is orphaned. This module closes that gap: given the same
//! `[tool.session_store]` declaration [`crate::sessions`] reads, it re-keys the
//! store from the old project path to the new one.
//!
//! Per layout (the design's table, `docs/design/session-store-model.md`):
//!
//! | Layout | Operation |
//! | --- | --- |
//! | `encoded_dir` | rename `<old-encoding>` → `<new-encoding>`, **and** rewrite the key field inside the session files |
//! | `jsonl_field` | rewrite the key field in matching files |
//! | `sqlite_column` | `UPDATE <table> SET <path_column>` for matching rows |
//!
//! The `encoded_dir` row is two operations rather than one because the store
//! is keyed twice: by the directory name *and*, since v0.8.1, by the `cwd` a
//! transcript records inside it — which the census trusts over the name.
//! Renaming alone would leave the recorded path pointing at the old location,
//! and the census would read the old path straight back out of the renamed
//! directory. Both keys move or neither does.
//!
//! **Everything here is planned before it is applied.** [`plan`] performs no
//! mutation and returns exactly what would change; `--dry-run` prints that
//! plan and stops. [`apply`] executes it and hands back the inverse, which is
//! recorded in the event log so `undo` can reverse it. Writes are atomic
//! (temp sibling + rename, via [`crate::reconciler::atomic_write`]) and value
//! rewrites are whole-token, so a project path can never be corrupted into a
//! partially-rewritten one.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::sessions::encode_project_path;
use crate::tools::{safe_ident, SessionStore};

/// Cap on files rewritten in one store, mirroring the census walk's bound.
const REKEY_WALK_CAP: usize = 50_000;

#[derive(Debug, thiserror::Error)]
pub enum RekeyError {
    #[error(
        "tool `{tool}`: refusing to re-key — the destination store `{dest}` already exists. \
         Re-keying into it would merge two projects' session histories with no way to tell \
         them apart afterwards; move or remove it first."
    )]
    DestinationExists { tool: String, dest: String },
    #[error(
        "tool `{tool}`: refusing to re-key — the session database `{db}` is locked, which \
         means the tool is running. Quit it and retry; writing under it risks a torn store."
    )]
    StoreLocked { tool: String, db: String },
    #[error("tool `{tool}`: re-key failed on `{path}`: {detail}")]
    Failed {
        tool: String,
        path: String,
        detail: String,
    },
}

/// One reversible step of a re-key. Serialized into the event log as the undo
/// plan, so the shape is a compatibility surface — add variants, don't
/// repurpose them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RekeyAction {
    /// Rename a whole store directory (`encoded_dir`).
    RenameDir { from: PathBuf, to: PathBuf },
    /// Replace every occurrence of one JSON string token in a file. `from`
    /// and `to` are the raw path values; the token actually matched is the
    /// JSON-encoded form (quoted and escaped), so a longer path that merely
    /// starts with `from` cannot match.
    RewriteJsonValue {
        file: PathBuf,
        from: String,
        to: String,
        occurrences: usize,
    },
    /// `UPDATE <table> SET <column> = to WHERE <column> = from`.
    UpdateSqliteRows {
        db: PathBuf,
        table: String,
        column: String,
        from: String,
        to: String,
        rows: usize,
    },
}

impl RekeyAction {
    /// The step that reverses this one. Paths are deliberately unchanged:
    /// a file rewritten after its directory was renamed is reversed *before*
    /// the directory is renamed back (see [`RekeyPlan::inverse`]), so it is
    /// still at the post-rename location when the inverse runs.
    pub fn inverse(&self) -> RekeyAction {
        match self {
            RekeyAction::RenameDir { from, to } => RekeyAction::RenameDir {
                from: to.clone(),
                to: from.clone(),
            },
            RekeyAction::RewriteJsonValue {
                file,
                from,
                to,
                occurrences,
            } => RekeyAction::RewriteJsonValue {
                file: file.clone(),
                from: to.clone(),
                to: from.clone(),
                occurrences: *occurrences,
            },
            RekeyAction::UpdateSqliteRows {
                db,
                table,
                column,
                from,
                to,
                rows,
            } => RekeyAction::UpdateSqliteRows {
                db: db.clone(),
                table: table.clone(),
                column: column.clone(),
                from: to.clone(),
                to: from.clone(),
                rows: *rows,
            },
        }
    }

    /// One line of `--dry-run` output.
    pub fn describe(&self) -> String {
        match self {
            RekeyAction::RenameDir { from, to } => format!(
                "rename store dir {} -> {}",
                from.display(),
                to.file_name().unwrap_or_default().to_string_lossy()
            ),
            RekeyAction::RewriteJsonValue {
                file, occurrences, ..
            } => format!(
                "rewrite {occurrences} path reference(s) in {}",
                file.display()
            ),
            RekeyAction::UpdateSqliteRows {
                db, table, rows, ..
            } => format!("update {rows} row(s) in {}::{table}", db.display()),
        }
    }
}

/// What a re-key would do to one tool's store. Empty `actions` means the store
/// holds nothing for this project — not an error, just nothing to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RekeyPlan {
    pub tool: String,
    pub old_path: String,
    pub new_path: String,
    pub actions: Vec<RekeyAction>,
}

impl RekeyPlan {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// The plan that undoes this one: every action inverted, in reverse order.
    /// Order matters — the forward plan renames the directory first and then
    /// rewrites files under the new name, so the inverse must rewrite them
    /// back *before* renaming the directory away from under them.
    pub fn inverse(&self) -> RekeyPlan {
        RekeyPlan {
            tool: self.tool.clone(),
            old_path: self.new_path.clone(),
            new_path: self.old_path.clone(),
            actions: self.actions.iter().rev().map(|a| a.inverse()).collect(),
        }
    }
}

/// The JSON token a path value appears as inside a JSONL line: quoted and
/// escaped exactly as `serde_json` would write it. Matching this token rather
/// than the bare path is what makes replacement boundary-safe — `"/a/b"`
/// cannot match inside `"/a/bc"`, because the closing quote is part of the
/// needle.
fn json_token(value: &str) -> String {
    serde_json::Value::String(value.to_string()).to_string()
}

/// Plan the re-key of one tool's store from `old_path` to `new_path`.
/// Read-only: nothing is mutated here, and the returned plan is exactly what
/// [`apply`] will do.
pub fn plan(
    home: &Path,
    tool: &str,
    store: &SessionStore,
    old_path: &Path,
    new_path: &Path,
) -> Result<RekeyPlan, RekeyError> {
    let old = old_path.display().to_string();
    let new = new_path.display().to_string();
    let mut actions = Vec::new();

    match store {
        SessionStore::EncodedDir {
            path,
            separator,
            key_glob,
            key_field,
        } => {
            let base = crate::sessions::expand_home(home, path);
            let old_dir = base.join(encode_project_path(&old, separator));
            if !old_dir.is_dir() {
                return Ok(RekeyPlan {
                    tool: tool.into(),
                    old_path: old,
                    new_path: new,
                    actions,
                });
            }
            let new_dir = base.join(encode_project_path(&new, separator));
            if new_dir != old_dir && new_dir.exists() {
                return Err(RekeyError::DestinationExists {
                    tool: tool.into(),
                    dest: new_dir.display().to_string(),
                });
            }
            if new_dir != old_dir {
                actions.push(RekeyAction::RenameDir {
                    from: old_dir.clone(),
                    to: new_dir.clone(),
                });
            }
            // The recorded `cwd` is the other half of the key — see the
            // module docs. Files are named at their POST-rename location,
            // since the rename is applied first.
            // Gated on the hint being DECLARED (both halves; `validate`
            // enforces both-or-neither). The field's name isn't needed to do
            // the rewrite — matching the path's JSON token catches the key
            // wherever it sits, including a dotted one like `payload.cwd`,
            // and any sibling field holding the same project path, which
            // should follow the project too.
            if let (Some(glob), Some(_)) = (key_glob, key_field) {
                if let Ok(pattern) = glob::Pattern::new(glob) {
                    for entry in std::fs::read_dir(&old_dir)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .take(REKEY_WALK_CAP)
                    {
                        let Ok(meta) = entry.metadata() else { continue };
                        if !meta.is_file() || !pattern.matches(&entry.file_name().to_string_lossy())
                        {
                            continue;
                        }
                        let occurrences = count_json_token(&entry.path(), &old);
                        if occurrences > 0 {
                            let name = entry.file_name();
                            actions.push(RekeyAction::RewriteJsonValue {
                                file: new_dir.join(&name),
                                from: old.clone(),
                                to: new.clone(),
                                occurrences,
                            });
                        }
                    }
                }
            }
        }
        SessionStore::JsonlField { path, glob, .. } => {
            let base = crate::sessions::expand_home(home, path);
            for (file, _) in crate::sessions::walk_store_files(&base, glob, REKEY_WALK_CAP).0 {
                let occurrences = count_json_token(&file, &old);
                if occurrences > 0 {
                    actions.push(RekeyAction::RewriteJsonValue {
                        file,
                        from: old.clone(),
                        to: new.clone(),
                        occurrences,
                    });
                }
            }
        }
        SessionStore::SqliteColumn {
            path,
            table,
            path_column,
            ..
        } => {
            let db = crate::sessions::expand_home(home, path);
            if !db.is_file() || !safe_ident(table) || !safe_ident(path_column) {
                return Ok(RekeyPlan {
                    tool: tool.into(),
                    old_path: old,
                    new_path: new,
                    actions,
                });
            }
            let rows = count_sqlite_rows(tool, &db, table, path_column, &old)?;
            if rows > 0 {
                actions.push(RekeyAction::UpdateSqliteRows {
                    db,
                    table: table.clone(),
                    column: path_column.clone(),
                    from: old.clone(),
                    to: new.clone(),
                    rows,
                });
            }
        }
    }

    Ok(RekeyPlan {
        tool: tool.into(),
        old_path: old,
        new_path: new,
        actions,
    })
}

/// How many times the JSON token for `value` appears in a file. Counting up
/// front is what lets `--dry-run` state the blast radius honestly.
fn count_json_token(file: &Path, value: &str) -> usize {
    let Ok(content) = std::fs::read_to_string(file) else {
        return 0;
    };
    content.matches(&json_token(value)).count()
}

fn count_sqlite_rows(
    tool: &str,
    db: &Path,
    table: &str,
    column: &str,
    value: &str,
) -> Result<usize, RekeyError> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| RekeyError::Failed {
                tool: tool.into(),
                path: db.display().to_string(),
                detail: e.to_string(),
            })?;
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?1");
    conn.query_row(&sql, [value], |r| r.get::<_, i64>(0))
        .map(|n| n.max(0) as usize)
        .map_err(|e| RekeyError::Failed {
            tool: tool.into(),
            path: db.display().to_string(),
            detail: e.to_string(),
        })
}

/// A failed [`apply`], with the means to recover from it.
#[derive(Debug)]
pub struct RekeyFailure {
    /// Why the re-key stopped.
    pub error: RekeyError,
    /// The plan that reverses the actions which DID complete — exactly the
    /// completed prefix, so the caller rolls back what happened rather than
    /// guessing.
    pub rollback: RekeyPlan,
}

/// Execute a plan, returning the plan that undoes it.
///
/// Actions run in order and stop at the first failure; anything already done
/// stays done, and the returned-on-error inverse covers exactly the completed
/// prefix, so the caller can roll back precisely what happened rather than
/// guessing.
pub fn apply(plan: &RekeyPlan) -> Result<RekeyPlan, Box<RekeyFailure>> {
    let mut done: Vec<RekeyAction> = Vec::new();
    for action in &plan.actions {
        if let Err(e) = apply_action(&plan.tool, action) {
            let rollback = RekeyPlan {
                tool: plan.tool.clone(),
                old_path: plan.new_path.clone(),
                new_path: plan.old_path.clone(),
                actions: done.iter().rev().map(|a| a.inverse()).collect(),
            };
            return Err(Box::new(RekeyFailure { error: e, rollback }));
        }
        done.push(action.clone());
    }
    Ok(plan.inverse())
}

fn apply_action(tool: &str, action: &RekeyAction) -> Result<(), RekeyError> {
    match action {
        RekeyAction::RenameDir { from, to } => {
            if to.exists() {
                return Err(RekeyError::DestinationExists {
                    tool: tool.into(),
                    dest: to.display().to_string(),
                });
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).map_err(|e| RekeyError::Failed {
                    tool: tool.into(),
                    path: parent.display().to_string(),
                    detail: e.to_string(),
                })?;
            }
            std::fs::rename(from, to).map_err(|e| RekeyError::Failed {
                tool: tool.into(),
                path: from.display().to_string(),
                detail: e.to_string(),
            })
        }
        RekeyAction::RewriteJsonValue { file, from, to, .. } => {
            let content = std::fs::read_to_string(file).map_err(|e| RekeyError::Failed {
                tool: tool.into(),
                path: file.display().to_string(),
                detail: e.to_string(),
            })?;
            let rewritten = content.replace(&json_token(from), &json_token(to));
            if rewritten == content {
                return Ok(());
            }
            crate::reconciler::atomic_write(file, &rewritten).map_err(|e| RekeyError::Failed {
                tool: tool.into(),
                path: file.display().to_string(),
                detail: e.to_string(),
            })
        }
        RekeyAction::UpdateSqliteRows {
            db,
            table,
            column,
            from,
            to,
            ..
        } => {
            if !safe_ident(table) || !safe_ident(column) {
                return Err(RekeyError::Failed {
                    tool: tool.into(),
                    path: db.display().to_string(),
                    detail: "unsafe SQL identifier in recorded plan".into(),
                });
            }
            let conn = rusqlite::Connection::open(db).map_err(|e| RekeyError::Failed {
                tool: tool.into(),
                path: db.display().to_string(),
                detail: e.to_string(),
            })?;
            // Fail fast instead of blocking the daemon behind a live tool.
            let _ = conn.busy_timeout(std::time::Duration::from_millis(2_000));
            let sql = format!("UPDATE {table} SET {column} = ?1 WHERE {column} = ?2");
            conn.execute(&sql, [to, from]).map_err(|e| {
                if is_locked(&e) {
                    RekeyError::StoreLocked {
                        tool: tool.into(),
                        db: db.display().to_string(),
                    }
                } else {
                    RekeyError::Failed {
                        tool: tool.into(),
                        path: db.display().to_string(),
                        detail: e.to_string(),
                    }
                }
            })?;
            Ok(())
        }
    }
}

fn is_locked(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked,
                ..
            },
            _
        )
    )
}

/// A re-key's recorded undo plan, as read back from the event log.
///
/// v0.9.0 wrote ONE ROW PER STORE, each holding a bare [`RekeyPlan`]; since
/// v0.9.1 one invocation is one row holding every store's plan, in replay
/// order. Both shapes are accepted so an event log written by v0.9.0 stays
/// undoable after an upgrade — the rows are the operator's only route back
/// from a mutation, so silently failing to parse one is not an option.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum RecordedUndo {
    /// v0.9.1+: every store from one invocation.
    Many(Vec<RekeyPlan>),
    /// v0.9.0: a single store.
    One(Box<RekeyPlan>),
}

impl RecordedUndo {
    /// The plans to replay, in order.
    pub fn plans(self) -> Vec<RekeyPlan> {
        match self {
            RecordedUndo::Many(v) => v,
            RecordedUndo::One(p) => vec![*p],
        }
    }
}

/// What happened to one tool's store during [`rekey_all`].
#[derive(Debug)]
pub struct RekeyReport {
    pub tool: String,
    /// The plan — the actions it would take, or took.
    pub plan: RekeyPlan,
    /// True once the plan was applied (false for a dry run, an empty plan, or
    /// a failure).
    pub applied: bool,
    /// Event-log row id of the recorded undo plan, when one was applied.
    pub log_id: Option<i64>,
    /// Why this store was skipped or failed. A refusal (destination exists, a
    /// locked database) lands here — the other stores still proceed, because
    /// re-keying two of three is strictly better than re-keying none, and the
    /// operator is told exactly which one needs their attention.
    pub error: Option<String>,
}

/// Re-key every declared store from `old_path` to `new_path`, recording each
/// applied plan in `event_log` so `undo` can reverse it.
///
/// Stores are independent: a refusal on one is reported and the rest continue.
/// A failure *midway through* one store's plan rolls that store back to where
/// it started, so no store is ever left half-re-keyed.
pub fn rekey_all(
    home: &Path,
    stores: &[(String, SessionStore)],
    old_path: &Path,
    new_path: &Path,
    event_log: Option<&crate::event_log::EventLog>,
    dry_run: bool,
) -> Vec<RekeyReport> {
    let mut reports = Vec::new();
    let mut undos: Vec<RekeyPlan> = Vec::new();
    for (tool, store) in stores {
        let plan = match plan(home, tool, store, old_path, new_path) {
            Ok(p) => p,
            Err(e) => {
                reports.push(RekeyReport {
                    tool: tool.clone(),
                    plan: RekeyPlan {
                        tool: tool.clone(),
                        old_path: old_path.display().to_string(),
                        new_path: new_path.display().to_string(),
                        actions: Vec::new(),
                    },
                    applied: false,
                    log_id: None,
                    error: Some(e.to_string()),
                });
                continue;
            }
        };
        if plan.is_empty() || dry_run {
            reports.push(RekeyReport {
                tool: tool.clone(),
                plan,
                applied: false,
                log_id: None,
                error: None,
            });
            continue;
        }

        match apply(&plan) {
            Ok(undo) => {
                // Recorded once for the whole invocation, after the loop —
                // see the note there.
                undos.push(undo);
                reports.push(RekeyReport {
                    tool: tool.clone(),
                    plan,
                    applied: true,
                    log_id: None,
                    error: None,
                });
            }
            Err(failure) => {
                // Put this store back the way we found it. A half-re-keyed
                // store is the one outcome worse than not re-keying at all.
                let rolled_back = apply(&failure.rollback).is_ok();
                let e = &failure.error;
                let detail = if rolled_back {
                    format!("{e} (this store was rolled back, nothing changed)")
                } else {
                    format!(
                        "{e} — AND the rollback also failed; this store is partly re-keyed. \
                         Inspect {} before using it.",
                        plan.old_path
                    )
                };
                reports.push(RekeyReport {
                    tool: tool.clone(),
                    plan,
                    applied: false,
                    log_id: None,
                    error: Some(detail),
                });
            }
        }
    }

    // ONE event-log row for the whole invocation, carrying every store's
    // undo plan — not one row per store.
    //
    // The unit of undo must match the unit of action. v0.9.0 recorded a row
    // per store, so a single `rekey` of a project with Claude Code + Codex +
    // OpenCode history wrote three rows and a bare `undo` reversed only the
    // last one: Codex back at the old path, Claude Code still at the new one.
    // That is a split-brain project across tools — precisely what re-keying
    // exists to prevent — and nothing told the operator two more undos were
    // needed. Caught by `scripts/rekey-dogfood.sh`.
    //
    // Stored in the order they must be REPLAYED (reverse of application), so
    // undo is a straight walk of the list.
    if !undos.is_empty() {
        undos.reverse();
        let tools: Vec<&str> = reports
            .iter()
            .filter(|r| r.applied)
            .map(|r| r.tool.as_str())
            .collect();
        let label = tools.join(", ");
        let log_id = event_log.and_then(|log| match serde_json::to_string(&undos) {
            Ok(blob) => log
                .record_rekey(
                    &label,
                    &old_path.display().to_string(),
                    &new_path.display().to_string(),
                    &blob,
                )
                .map_err(|e| {
                    tracing::warn!(
                        tools = %label,
                        error = %e,
                        "re-key applied but NOT recorded; `undo` cannot reverse it"
                    );
                })
                .ok(),
            Err(e) => {
                tracing::warn!(tools = %label, error = %e, "could not serialize undo plan");
                None
            }
        });
        for r in reports.iter_mut().filter(|r| r.applied) {
            r.log_id = log_id;
        }
    }

    // Record what was DECIDED for every store — including the ones that had
    // nothing to do. Done here rather than in the callers so the CLI and the
    // daemon are observed identically; a decision only one entry point records
    // is a blind spot in whichever path the operator happens to use.
    if !dry_run {
        for r in &reports {
            let outcome = match (&r.error, r.applied) {
                (Some(e), _) => crate::activity::Outcome::Refused { reason: e.clone() },
                (None, true) => crate::activity::Outcome::acted(
                    r.plan.actions.len(),
                    crate::activity::NoOpReason::NoStoreForProject,
                ),
                (None, false) => crate::activity::Outcome::NoOp {
                    reason: crate::activity::NoOpReason::NoStoreForProject,
                },
            };
            crate::activity::ActivityRecord::new(crate::activity::ActivityKind::Rekey, outcome)
                .tool(&r.tool)
                .project(new_path.display().to_string())
                .emit(event_log);
        }
    }
    reports
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::TimeUnit;
    use tempfile::TempDir;

    fn claude_store() -> SessionStore {
        SessionStore::EncodedDir {
            path: "~/.claude/projects".into(),
            separator: "-".into(),
            key_glob: Some("*.jsonl".into()),
            key_field: Some("cwd".into()),
        }
    }

    /// Build a Claude Code store directory for `project` holding one
    /// transcript that records it, exactly as the real tool does.
    fn seed_claude(home: &Path, project: &Path) -> PathBuf {
        let enc = encode_project_path(&project.display().to_string(), "-");
        let dir = home.join(".claude/projects").join(enc);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("s1.jsonl"),
            format!(
                "{{\"type\":\"summary\",\"summary\":\"x\"}}\n\
                 {{\"type\":\"user\",\"cwd\":\"{p}\",\"sessionId\":\"abc\"}}\n\
                 {{\"type\":\"assistant\",\"cwd\":\"{p}\"}}\n",
                p = project.display()
            ),
        )
        .unwrap();
        dir
    }

    #[test]
    fn encoded_dir_rekey_moves_both_keys_and_the_census_follows() {
        // The whole point: after a move, `sessions` must report the NEW path
        // with the same session, and nothing at the old one. Renaming the
        // directory alone would fail this — the census reads `cwd` from
        // inside the transcript and would report the old path right back.
        let home = TempDir::new().unwrap();
        let old = home.path().join("work/app");
        let new = home.path().join("elsewhere/app");
        std::fs::create_dir_all(&old).unwrap();
        let old_dir = seed_claude(home.path(), &old);

        // The project itself moves, then we re-key.
        std::fs::create_dir_all(new.parent().unwrap()).unwrap();
        std::fs::rename(&old, &new).unwrap();

        let plan = plan(home.path(), "claude_code", &claude_store(), &old, &new).unwrap();
        assert!(!plan.is_empty(), "a move with sessions must plan work");
        let undo = apply(&plan).expect("apply succeeds");

        assert!(!old_dir.exists(), "old store dir is gone");
        let stores = vec![("claude_code".to_string(), claude_store())];
        let groups = crate::sessions::census(home.path(), &stores, false);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].project_path,
            new.display().to_string(),
            "census must report the new path"
        );
        assert!(
            !groups[0].orphaned,
            "the re-keyed project is live, not orphaned"
        );
        assert_eq!(
            groups[0].tools["claude_code"].count, 1,
            "the session survived"
        );

        // ...and the whole thing reverses.
        apply(&undo).expect("undo succeeds");
        let back = crate::sessions::census(home.path(), &stores, false);
        assert_eq!(back[0].project_path, old.display().to_string());
        assert!(old_dir.exists(), "original store dir restored");
    }

    #[test]
    fn rekey_refuses_when_the_destination_store_already_exists() {
        // Two projects' histories must never be merged into one directory.
        let home = TempDir::new().unwrap();
        let old = home.path().join("work/app");
        let new = home.path().join("work/other");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        let old_dir = seed_claude(home.path(), &old);
        let new_dir = seed_claude(home.path(), &new);

        let e = plan(home.path(), "claude_code", &claude_store(), &old, &new).unwrap_err();
        assert!(matches!(e, RekeyError::DestinationExists { .. }), "{e}");
        assert!(e.to_string().contains("merge"), "error must say why: {e}");
        assert!(old_dir.exists() && new_dir.exists(), "nothing was touched");
    }

    #[test]
    fn rewrite_is_whole_token_so_a_sibling_prefix_path_is_untouched() {
        // `/work/app` must not rewrite inside `/work/app-two`. The JSON token
        // includes the closing quote, which is what makes this safe.
        let home = TempDir::new().unwrap();
        let old = home.path().join("work/app");
        let new = home.path().join("work/moved");
        std::fs::create_dir_all(&old).unwrap();
        let dir = seed_claude(home.path(), &old);
        let sibling = format!("{}-two", old.display());
        std::fs::write(
            dir.join("s2.jsonl"),
            format!(
                "{{\"cwd\":\"{s}\"}}\n{{\"cwd\":\"{o}\",\"file\":\"{o}/src/main.rs\"}}\n",
                s = sibling,
                o = old.display()
            ),
        )
        .unwrap();

        let plan = plan(home.path(), "claude_code", &claude_store(), &old, &new).unwrap();
        apply(&plan).expect("apply");

        let enc = encode_project_path(&new.display().to_string(), "-");
        let moved = home
            .path()
            .join(".claude/projects")
            .join(enc)
            .join("s2.jsonl");
        let content = std::fs::read_to_string(&moved).unwrap();
        assert!(
            content.contains(&format!("\"cwd\":\"{sibling}\"")),
            "the sibling project's path must survive verbatim: {content}"
        );
        assert!(
            content.contains(&format!("\"cwd\":\"{}\"", new.display())),
            "the moved project's key must be rewritten: {content}"
        );
        assert!(
            content.contains(&format!("\"{}/src/main.rs\"", old.display())),
            "a historical file reference is a record of what happened, not a key: {content}"
        );
    }

    #[test]
    fn jsonl_field_store_rekeys_matching_files_only() {
        let home = TempDir::new().unwrap();
        let old = home.path().join("proj/a");
        let new = home.path().join("proj/b");
        let sessions = home.path().join(".codex/sessions/2026/07");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("mine.jsonl"),
            format!("{{\"cwd\":\"{}\"}}\n", old.display()),
        )
        .unwrap();
        std::fs::write(
            sessions.join("other.jsonl"),
            "{\"cwd\":\"/somewhere/else\"}\n",
        )
        .unwrap();

        let store = SessionStore::JsonlField {
            path: "~/.codex/sessions".into(),
            glob: "**/*.jsonl".into(),
            key_field: "cwd".into(),
            fallback_field: None,
        };
        let plan = plan(home.path(), "codex", &store, &old, &new).unwrap();
        assert_eq!(plan.actions.len(), 1, "only the matching file is planned");
        apply(&plan).expect("apply");

        assert!(std::fs::read_to_string(sessions.join("mine.jsonl"))
            .unwrap()
            .contains(&new.display().to_string()));
        assert_eq!(
            std::fs::read_to_string(sessions.join("other.jsonl")).unwrap(),
            "{\"cwd\":\"/somewhere/else\"}\n",
            "an unrelated session must be byte-identical"
        );
    }

    #[test]
    fn sqlite_store_rekeys_matching_rows_and_reverses() {
        let home = TempDir::new().unwrap();
        let dbdir = home.path().join(".local/share/opencode");
        std::fs::create_dir_all(&dbdir).unwrap();
        let db = dbdir.join("opencode.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (directory TEXT, time_updated INTEGER, time_archived INTEGER);
             INSERT INTO session VALUES ('/p/old', 1, NULL);
             INSERT INTO session VALUES ('/p/old', 2, NULL);
             INSERT INTO session VALUES ('/p/untouched', 3, NULL);",
        )
        .unwrap();
        drop(conn);

        let store = SessionStore::SqliteColumn {
            path: "~/.local/share/opencode/opencode.db".into(),
            table: "session".into(),
            path_column: "directory".into(),
            updated_column: Some("time_updated".into()),
            updated_unit: TimeUnit::Ms,
            archived_column: Some("time_archived".into()),
        };
        let p = plan(
            home.path(),
            "opencode",
            &store,
            Path::new("/p/old"),
            Path::new("/p/new"),
        )
        .unwrap();
        match &p.actions[..] {
            [RekeyAction::UpdateSqliteRows { rows, .. }] => assert_eq!(*rows, 2),
            other => panic!("expected one row update, got {other:?}"),
        }

        let undo = apply(&p).expect("apply");
        let count = |dir: &str| {
            let c = rusqlite::Connection::open(&db).unwrap();
            c.query_row(
                "SELECT COUNT(*) FROM session WHERE directory = ?1",
                [dir],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };
        assert_eq!(count("/p/new"), 2);
        assert_eq!(count("/p/old"), 0);
        assert_eq!(count("/p/untouched"), 1, "other projects untouched");

        apply(&undo).expect("undo");
        assert_eq!(count("/p/old"), 2, "undo restores the original key");
        assert_eq!(count("/p/new"), 0);
    }

    #[test]
    fn one_invocation_records_one_undo_covering_every_store() {
        // v0.9.0 wrote a row per store, so a bare `undo` reversed only the
        // last one and left the project split-brain across tools. One
        // invocation must be one undo.
        let home = TempDir::new().unwrap();
        let old = home.path().join("work/app");
        std::fs::create_dir_all(&old).unwrap();
        seed_claude(home.path(), &old);
        let codex = home.path().join(".codex/sessions");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(
            codex.join("r.jsonl"),
            format!("{{\"cwd\":\"{}\"}}\n", old.display()),
        )
        .unwrap();

        let stores = vec![
            ("claude_code".to_string(), claude_store()),
            (
                "codex".to_string(),
                SessionStore::JsonlField {
                    path: "~/.codex/sessions".into(),
                    glob: "**/*.jsonl".into(),
                    key_field: "cwd".into(),
                    fallback_field: None,
                },
            ),
        ];
        let log = crate::event_log::EventLog::open_in_memory().unwrap();
        let new = home.path().join("work/moved");
        let reports = rekey_all(home.path(), &stores, &old, &new, Some(&log), false);

        let applied: Vec<_> = reports.iter().filter(|r| r.applied).collect();
        assert_eq!(applied.len(), 2, "both stores re-keyed");
        assert_eq!(
            log.recent_rekeys(10).unwrap().len(),
            1,
            "one invocation must record exactly ONE undo row"
        );
        let ids: std::collections::HashSet<_> = applied.iter().map(|r| r.log_id).collect();
        assert_eq!(ids.len(), 1, "every store reports the same undo id");

        // Replaying that single row must put BOTH stores back.
        let entry = log.latest_pending_rekey().unwrap().unwrap();
        assert!(entry.tool_name.contains("claude_code") && entry.tool_name.contains("codex"));
        let plans: RecordedUndo = serde_json::from_str(&entry.undo_plan).unwrap();
        let plans = plans.plans();
        assert_eq!(plans.len(), 2);
        for plan in &plans {
            apply(plan).expect("undo applies");
        }
        let groups = crate::sessions::census(home.path(), &stores, false);
        assert!(
            groups
                .iter()
                .all(|g| g.project_path != new.display().to_string()),
            "nothing may remain at the new path after a full undo"
        );
        let back = groups
            .iter()
            .find(|g| g.project_path == old.display().to_string())
            .expect("both stores back at the original path");
        assert_eq!(back.tools.len(), 2, "both tools reversed, not just one");
    }

    #[test]
    fn a_v0_9_0_single_plan_undo_row_still_replays() {
        // Compatibility: rows written by v0.9.0 hold a bare RekeyPlan object,
        // not an array. Those rows are the operator's only route back from a
        // mutation, so an upgrade must not strand them.
        let single = serde_json::json!({
            "tool": "claude_code",
            "old_path": "/new",
            "new_path": "/old",
            "actions": []
        })
        .to_string();
        let recorded: RecordedUndo = serde_json::from_str(&single).expect("v0.9.0 shape parses");
        let plans = recorded.plans();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].tool, "claude_code");

        // ...and the v0.9.1 array shape still parses as many.
        let many = serde_json::json!([{
            "tool": "codex", "old_path": "/new", "new_path": "/old", "actions": []
        }])
        .to_string();
        let recorded: RecordedUndo = serde_json::from_str(&many).unwrap();
        assert_eq!(recorded.plans().len(), 1);
    }

    #[test]
    fn a_store_with_nothing_for_this_project_plans_no_work() {
        let home = TempDir::new().unwrap();
        let p = plan(
            home.path(),
            "claude_code",
            &claude_store(),
            &home.path().join("never/used"),
            &home.path().join("somewhere"),
        )
        .unwrap();
        assert!(p.is_empty());
    }

    #[test]
    fn a_failed_action_returns_the_inverse_of_only_what_completed() {
        // Partial application must be precisely reversible: the caller gets
        // the undo for the prefix that ran, not for the whole plan.
        let home = TempDir::new().unwrap();
        let old = home.path().join("work/app");
        std::fs::create_dir_all(&old).unwrap();
        let dir = seed_claude(home.path(), &old);

        let bogus = RekeyPlan {
            tool: "claude_code".into(),
            old_path: old.display().to_string(),
            new_path: "/x".into(),
            actions: vec![
                RekeyAction::RenameDir {
                    from: dir.clone(),
                    to: dir.with_file_name("renamed"),
                },
                RekeyAction::RenameDir {
                    from: PathBuf::from("/nonexistent/source"),
                    to: PathBuf::from("/nonexistent/dest"),
                },
            ],
        };
        let failure = apply(&bogus).expect_err("second action must fail");
        assert!(
            matches!(failure.error, RekeyError::Failed { .. }),
            "{}",
            failure.error
        );
        assert_eq!(
            failure.rollback.actions.len(),
            1,
            "only the completed step is reversible"
        );
        apply(&failure.rollback).expect("the partial undo applies");
        assert!(dir.exists(), "the first rename was rolled back");
    }
}
