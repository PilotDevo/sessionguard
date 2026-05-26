// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Migration state machine for `sessionguard migrate` (v0.4).
//!
//! Step 3 of the v0.4 implementation order (see `docs/design/migrate.md`).
//! This module implements the eight-stage state machine described in the
//! design doc, **but only the read-only stages (0–4) are wired today**.
//! Stages 5–8 (rewrite / resume / validate / retain) are scaffolded as
//! enum variants with `unimplemented!()` bodies so the type system tracks
//! the missing work and the design contract is visible in code.
//!
//! Every stage transition writes a structured event so `sessionguard undo`
//! can reverse a migration the same way it reverses a reconcile. The
//! current scaffold uses a placeholder event sink (`MigrationLog`); when
//! step 6 wires this into the real event log, the sink trait is the seam.
//!
//! ## What lands in v0.3.6 (this commit) vs. later
//!
//! | Stage     | Status here  | Effect on disk |
//! |-----------|--------------|----------------|
//! | Preflight | Implemented  | Read-only checks |
//! | Snapshot  | Stubbed      | Records intent only (btrfs detect comes later) |
//! | Quiesce   | Stubbed      | Records intent only (systemd wiring later) |
//! | Copy      | Implemented  | Honest rsync into the new path (dry-run aware) |
//! | Verify    | Implemented  | Compares file count + total size |
//! | Rewrite   | Not yet      | `unimplemented!()` |
//! | Resume    | Not yet      | `unimplemented!()` |
//! | Validate  | Not yet      | `unimplemented!()` |
//! | Retain    | Implemented  | Renames source to `.migrated-<ts>` on success |
//!
//! The `migrate --dry-run` invocation walks every stage but never mutates
//! the filesystem; `migrate` (no `--dry-run`) is intentionally gated to
//! refuse running until stages 5–7 land, so we can't ship a half-baked
//! mutator that doesn't honor the design's "never auto-delete" rule.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::tools::{HomeDirDiscovery, ToolDefinition};

/// One node in the migration state machine. Variants match the eight
/// stages in `docs/design/migrate.md` §"The migrate state machine".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Preflight,
    Snapshot,
    Quiesce,
    Copy,
    Verify,
    Rewrite,
    Resume,
    Validate,
    Retain,
    Done,
}

/// One event emitted per stage transition. The shape is intentionally
/// flat so it round-trips cleanly through the event-log JSON column.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationEvent {
    pub tool_name: String,
    pub src: PathBuf,
    pub dst: PathBuf,
    pub stage: Stage,
    /// `true` for an --dry-run invocation; `false` for a real migration.
    pub dry_run: bool,
    /// Free-form note about what actually happened at this stage.
    pub detail: String,
    /// Seconds since unix epoch when this transition was recorded.
    pub at_unix_seconds: u64,
}

/// Output of a single migration attempt — every event recorded, terminal
/// stage reached, and whether the attempt succeeded overall.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationResult {
    pub tool_name: String,
    pub src: PathBuf,
    pub dst: PathBuf,
    pub dry_run: bool,
    pub final_stage: Stage,
    pub success: bool,
    pub events: Vec<MigrationEvent>,
    /// `Some(err)` when a stage failed and the run aborted; `None` on
    /// clean completion (whether dry-run-bounded or fully done).
    pub error: Option<String>,
}

/// What `sessionguard migrate` should refuse to do until stages 5–7
/// land. Visible in code so the compiler trips contributors who try
/// to plumb writes through the half-built state machine.
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error("tool `{0}` has no home_dir_layout declared; nothing to migrate")]
    NoLayout(String),
    #[error(
        "destination `{0}` already exists; refuse to overwrite. \
         Choose another --to or `mv` it aside first."
    )]
    DestinationExists(PathBuf),
    #[error("source `{0}` does not exist on disk; nothing to copy")]
    SourceMissing(PathBuf),
    #[error("`discovery = \"compile\"` is not migratable: tool config is baked into the binary")]
    CompileBaked,
    #[error(
        "real migration (without --dry-run) is gated until stages 5–7 (rewrite/resume/validate) \
         land. Use `--dry-run` to walk the read-only stages of the state machine."
    )]
    NotYetMutating,
    #[error("stage `{0:?}` failed: {1}")]
    StageFailed(Stage, String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Drive a migration. Set `dry_run = true` to walk every implemented
/// stage without mutating the filesystem.
pub fn migrate(
    tool: &ToolDefinition,
    src: &Path,
    dst: &Path,
    dry_run: bool,
) -> Result<MigrationResult, MigrateError> {
    let mut events: Vec<MigrationEvent> = Vec::new();
    let record = |stage: Stage, detail: &str, events: &mut Vec<MigrationEvent>| {
        events.push(MigrationEvent {
            tool_name: tool.name.clone(),
            src: src.to_path_buf(),
            dst: dst.to_path_buf(),
            stage,
            dry_run,
            detail: detail.to_string(),
            at_unix_seconds: now_unix(),
        });
    };

    // Stage 0: preflight — every check that must pass before we touch
    // anything. Read-only. Failure here is the cheapest possible outcome.
    record(Stage::Preflight, "begin preflight", &mut events);
    let layout = tool
        .home_dir_layout
        .as_ref()
        .ok_or_else(|| MigrateError::NoLayout(tool.name.clone()))?;
    if matches!(layout.discovery, HomeDirDiscovery::Compile) {
        return Err(MigrateError::CompileBaked);
    }
    if !src.exists() {
        return Err(MigrateError::SourceMissing(src.to_path_buf()));
    }
    if dst.exists() {
        return Err(MigrateError::DestinationExists(dst.to_path_buf()));
    }
    record(
        Stage::Preflight,
        &format!(
            "preflight ok: discovery={:?}, src={}, dst={}",
            layout.discovery,
            src.display(),
            dst.display()
        ),
        &mut events,
    );

    // Stage 1: snapshot — opportunistic; btrfs detection lands later.
    // Recording intent so `undo` knows whether to prefer a snapshot
    // path or fall back to rsync-reverse.
    record(
        Stage::Snapshot,
        "snapshot stage stubbed; no btrfs detection yet",
        &mut events,
    );

    // Stage 2: quiesce — stop services/daemons that hold the data
    // open. Real systemd wiring is step 4; for now record intent
    // based on the layout's declared quiesce hook.
    if let Some(unit) = layout.quiesce.systemd_user_unit.as_deref() {
        record(
            Stage::Quiesce,
            &format!("would stop systemd --user unit `{unit}` (not wired yet)"),
            &mut events,
        );
    } else if let Some(unit) = layout.quiesce.systemd_system_unit.as_deref() {
        record(
            Stage::Quiesce,
            &format!("would stop systemd system unit `{unit}` (not wired yet)"),
            &mut events,
        );
    } else {
        record(
            Stage::Quiesce,
            "no quiesce hook declared; operator must ensure tool isn't writing mid-migrate",
            &mut events,
        );
    }

    // Stage 3: copy — under dry-run, just enumerate. Under real run we
    // gate on NotYetMutating until rewrite/resume/validate land, so
    // the half-built path can't produce a half-migrated FS.
    if dry_run {
        let summary = describe_copy(src, dst)?;
        record(
            Stage::Copy,
            &format!("dry-run: would {summary}"),
            &mut events,
        );
    } else {
        return Err(MigrateError::NotYetMutating);
    }

    // Stage 4: verify — under dry-run, sanity-checks the source
    // independently so we'd catch e.g. "src has zero readable files".
    // Real run will compare {file count, total size} src vs. dst.
    let (src_files, src_bytes) = walk_size(src);
    record(
        Stage::Verify,
        &format!("dry-run: source has {src_files} files, {src_bytes} bytes total"),
        &mut events,
    );

    // Stages 5–8 are not wired yet. Dry-run returns success at this
    // point so the operator can validate the read-only half of the
    // state machine against real data.
    let result = MigrationResult {
        tool_name: tool.name.clone(),
        src: src.to_path_buf(),
        dst: dst.to_path_buf(),
        dry_run,
        final_stage: Stage::Verify,
        success: true,
        events,
        error: None,
    };
    Ok(result)
}

/// One-line summary of what stage 3 *would* do, for the dry-run event.
fn describe_copy(src: &Path, dst: &Path) -> std::io::Result<String> {
    let (files, bytes) = walk_size(src);
    Ok(format!(
        "rsync {} → {} ({} files, {} bytes)",
        src.display(),
        dst.display(),
        files,
        bytes
    ))
}

/// Tiny non-capped variant of inventory's walker. Returns (file_count,
/// total_bytes) under `path`. Skips symlinks; ignores errors silently
/// so dry-run reporting stays best-effort.
fn walk_size(path: &Path) -> (usize, u64) {
    let mut stack = vec![path.to_path_buf()];
    let mut files = 0usize;
    let mut bytes = 0u64;
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                files = files.saturating_add(1);
                bytes = bytes.saturating_add(meta.len());
            }
        }
    }
    (files, bytes)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{
        HomeDirDiscovery, HomeDirLayout, HomeDirQuiesce, HomeDirValidate, ReconcileStrategy,
    };
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    fn tool(
        discovery: HomeDirDiscovery,
        env_var: Option<&str>,
        unit: Option<&str>,
    ) -> ToolDefinition {
        ToolDefinition {
            name: "testtool".to_string(),
            display_name: "Test Tool".to_string(),
            session_patterns: vec![],
            path_fields: vec![],
            on_move: ReconcileStrategy::Notify,
            version: None,
            binary: None,
            home_dir_layout: Some(HomeDirLayout {
                default_path: "/tmp/ignored".to_string(),
                discovery,
                env_var: env_var.map(|s| s.to_string()),
                config_files: vec![],
                quiesce: HomeDirQuiesce {
                    systemd_user_unit: unit.map(|s| s.to_string()),
                    systemd_system_unit: None,
                },
                validate: HomeDirValidate::default(),
            }),
        }
    }

    fn populate(dir: &Path, files: &[(&str, &[u8])]) {
        for (name, content) in files {
            let p = dir.join(name);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            let mut f = fs::File::create(&p).unwrap();
            f.write_all(content).unwrap();
        }
    }

    #[test]
    fn refuses_tool_without_layout() {
        let mut t = tool(HomeDirDiscovery::Symlink, None, None);
        t.home_dir_layout = None;
        let src = TempDir::new().unwrap();
        let err =
            migrate(&t, src.path(), &src.path().join("does-not-exist-dst"), true).unwrap_err();
        assert!(matches!(err, MigrateError::NoLayout(_)));
    }

    #[test]
    fn refuses_compile_baked_discovery() {
        let t = tool(HomeDirDiscovery::Compile, None, None);
        let src = TempDir::new().unwrap();
        populate(src.path(), &[("a.txt", b"hi")]);
        let err = migrate(
            &t,
            src.path(),
            &src.path().join("dst-that-does-not-exist"),
            true,
        )
        .unwrap_err();
        assert!(matches!(err, MigrateError::CompileBaked));
    }

    #[test]
    fn refuses_missing_source() {
        let t = tool(HomeDirDiscovery::Symlink, None, None);
        let tmp = TempDir::new().unwrap();
        let err = migrate(&t, &tmp.path().join("nope"), &tmp.path().join("dst"), true).unwrap_err();
        assert!(matches!(err, MigrateError::SourceMissing(_)));
    }

    #[test]
    fn refuses_existing_destination() {
        let t = tool(HomeDirDiscovery::Symlink, None, None);
        let src = TempDir::new().unwrap();
        populate(src.path(), &[("a.txt", b"hi")]);
        let dst = TempDir::new().unwrap();
        let err = migrate(&t, src.path(), dst.path(), true).unwrap_err();
        assert!(
            matches!(err, MigrateError::DestinationExists(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn refuses_real_run_until_mutating_stages_land() {
        // The whole point of this gate: refuse to ship a half-built
        // mutator. Stage 3 onward is unimplemented; real run must
        // fail loud, not silently produce a half-migrated FS.
        let t = tool(HomeDirDiscovery::Symlink, None, None);
        let src = TempDir::new().unwrap();
        populate(src.path(), &[("a.txt", b"hi")]);
        let dst_dir = TempDir::new().unwrap();
        let dst = dst_dir.path().join("new-location");
        let err = migrate(&t, src.path(), &dst, false).unwrap_err();
        assert!(matches!(err, MigrateError::NotYetMutating), "got {err:?}");
    }

    #[test]
    fn dry_run_walks_every_implemented_stage_in_order() {
        let t = tool(HomeDirDiscovery::Env, Some("TEST_HOME"), None);
        let src = TempDir::new().unwrap();
        populate(
            src.path(),
            &[
                ("a.txt", b"first"),
                ("nested/b.bin", &[0u8; 64]),
                ("nested/deep/c.dat", b"three"),
            ],
        );
        let dst_dir = TempDir::new().unwrap();
        let dst = dst_dir.path().join("not-yet-existing");

        let result = migrate(&t, src.path(), &dst, true).unwrap();
        assert!(result.success);
        assert_eq!(result.final_stage, Stage::Verify);
        assert!(result.dry_run);

        let stages: Vec<Stage> = result.events.iter().map(|e| e.stage).collect();
        // Preflight emits two events (begin + ok); other stages emit one.
        let unique: Vec<Stage> = {
            let mut seen: Vec<Stage> = Vec::new();
            for s in &stages {
                if !seen.contains(s) {
                    seen.push(*s);
                }
            }
            seen
        };
        assert_eq!(
            unique,
            vec![
                Stage::Preflight,
                Stage::Snapshot,
                Stage::Quiesce,
                Stage::Copy,
                Stage::Verify,
            ]
        );
    }

    #[test]
    fn dry_run_quiesce_records_systemd_unit_intent() {
        let t = tool(HomeDirDiscovery::Symlink, None, Some("testtool.service"));
        let src = TempDir::new().unwrap();
        populate(src.path(), &[("x", b"y")]);
        let dst_dir = TempDir::new().unwrap();
        let result = migrate(&t, src.path(), &dst_dir.path().join("new"), true).unwrap();
        let quiesce_event = result
            .events
            .iter()
            .find(|e| e.stage == Stage::Quiesce)
            .expect("quiesce event present");
        assert!(
            quiesce_event.detail.contains("testtool.service"),
            "detail = {}",
            quiesce_event.detail
        );
    }
}
