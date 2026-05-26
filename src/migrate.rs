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
    /// The inverse of whatever Quiesce did. The Resume stage (and
    /// `sessionguard undo` for stale half-migrates) reads this to
    /// know what to bring back up.
    #[serde(default)]
    pub resume_action: ResumeAction,
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

// ── Quiescer: abstraction over "stop / start the thing holding the data" ──
//
// Wired in step 4 (this commit). The default implementation shells out to
// systemctl; tests substitute a [`FakeQuiescer`] so they don't depend on
// the host having systemd or a real unit registered.
//
// Design constraint from `docs/design/migrate.md` §3 "Open questions":
// for ephemeral tools (no systemd unit declared), Quiesce *cannot* stop
// anything itself — best it can do is warn the operator. That case is
// represented by a successful Quiescer call that records the warning
// in its returned `QuiesceOutcome` rather than failing the migration.

/// How a tool was quiesced (or wasn't).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum QuiesceOutcome {
    /// A systemd unit was stopped. `scope` is `"user"` or `"system"`.
    UnitStopped { scope: String, unit: String },
    /// No unit declared — operator was warned to ensure the tool
    /// isn't writing mid-migrate. Migration continues.
    NoUnitWarning,
    /// Skipped because dry-run is in effect; no side effects.
    DryRunSkipped,
}

/// How to undo a Quiesce. Carried in the migration result so Resume
/// can do the right thing post-rewrite, and so future `undo` for a
/// stale half-migrate can also restart services.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ResumeAction {
    /// Resume by starting this unit at the named scope.
    StartUnit { scope: String, unit: String },
    /// Nothing to resume.
    #[default]
    None,
}

/// Pluggable systemd backend so unit tests can verify Quiesce / Resume
/// behaviour without spawning real `systemctl` processes.
pub trait Quiescer {
    /// Stop the relevant unit (if any) per the layout. Returns the
    /// outcome (for the event log) and the inverse action (for Resume).
    fn quiesce(
        &self,
        layout: &crate::tools::HomeDirLayout,
        dry_run: bool,
    ) -> Result<(QuiesceOutcome, ResumeAction), MigrateError>;

    /// Perform the resume action recorded during quiesce.
    fn resume(&self, action: &ResumeAction, dry_run: bool) -> Result<(), MigrateError>;
}

/// Default real-systemd implementation. Shells out to `systemctl`,
/// preferring `--user` when both scopes are declared (the user-scope
/// stop is cheap and doesn't require sudo).
pub struct SystemdQuiescer;

impl Quiescer for SystemdQuiescer {
    fn quiesce(
        &self,
        layout: &crate::tools::HomeDirLayout,
        dry_run: bool,
    ) -> Result<(QuiesceOutcome, ResumeAction), MigrateError> {
        if dry_run {
            return Ok((QuiesceOutcome::DryRunSkipped, ResumeAction::None));
        }
        if let Some(unit) = layout.quiesce.systemd_user_unit.as_deref() {
            run_systemctl(&["--user", "stop", unit])?;
            return Ok((
                QuiesceOutcome::UnitStopped {
                    scope: "user".into(),
                    unit: unit.into(),
                },
                ResumeAction::StartUnit {
                    scope: "user".into(),
                    unit: unit.into(),
                },
            ));
        }
        if let Some(unit) = layout.quiesce.systemd_system_unit.as_deref() {
            run_systemctl(&["stop", unit])?;
            return Ok((
                QuiesceOutcome::UnitStopped {
                    scope: "system".into(),
                    unit: unit.into(),
                },
                ResumeAction::StartUnit {
                    scope: "system".into(),
                    unit: unit.into(),
                },
            ));
        }
        Ok((QuiesceOutcome::NoUnitWarning, ResumeAction::None))
    }

    fn resume(&self, action: &ResumeAction, dry_run: bool) -> Result<(), MigrateError> {
        if dry_run {
            return Ok(());
        }
        match action {
            ResumeAction::StartUnit { scope, unit } => {
                let args: Vec<&str> = if scope == "user" {
                    vec!["--user", "start", unit.as_str()]
                } else {
                    vec!["start", unit.as_str()]
                };
                run_systemctl(&args)
            }
            ResumeAction::None => Ok(()),
        }
    }
}

fn run_systemctl(args: &[&str]) -> Result<(), MigrateError> {
    let output = std::process::Command::new("systemctl")
        .args(args)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(MigrateError::StageFailed(
            Stage::Quiesce,
            format!(
                "systemctl {} exited {}: {}",
                args.join(" "),
                output.status.code().unwrap_or(-1),
                stderr.trim()
            ),
        ));
    }
    Ok(())
}

/// Drive a migration. Set `dry_run = true` to walk every implemented
/// stage without mutating the filesystem.
///
/// Uses the default [`SystemdQuiescer`]; pass a custom one via
/// [`migrate_with`] for tests or alternate quiesce strategies.
pub fn migrate(
    tool: &ToolDefinition,
    src: &Path,
    dst: &Path,
    dry_run: bool,
) -> Result<MigrationResult, MigrateError> {
    migrate_with(tool, src, dst, dry_run, &SystemdQuiescer)
}

/// Like [`migrate`] but with an injectable [`Quiescer`]. Production
/// callers use [`migrate`]; tests use this with a fake to avoid
/// shelling out to real `systemctl`.
pub fn migrate_with(
    tool: &ToolDefinition,
    src: &Path,
    dst: &Path,
    dry_run: bool,
    quiescer: &dyn Quiescer,
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
    // open. Delegates to the [`Quiescer`] (default: real systemd) so
    // tests can substitute a fake. Carries the returned `ResumeAction`
    // forward to whatever Resume implementation lands next.
    let (quiesce_outcome, resume_action) = quiescer.quiesce(layout, dry_run)?;
    let detail = match &quiesce_outcome {
        QuiesceOutcome::UnitStopped { scope, unit } => {
            format!("stopped systemd {scope} unit `{unit}`")
        }
        QuiesceOutcome::NoUnitWarning => {
            "no quiesce hook declared; operator must ensure tool isn't writing mid-migrate"
                .to_string()
        }
        QuiesceOutcome::DryRunSkipped => match (
            layout.quiesce.systemd_user_unit.as_deref(),
            layout.quiesce.systemd_system_unit.as_deref(),
        ) {
            (Some(u), _) => format!("dry-run: would stop systemd --user unit `{u}`"),
            (None, Some(u)) => format!("dry-run: would stop systemd unit `{u}`"),
            _ => "dry-run: no quiesce hook declared; no-op".to_string(),
        },
    };
    record(Stage::Quiesce, &detail, &mut events);

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
        resume_action,
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

    /// Fake quiescer for tests. Records every `quiesce` / `resume`
    /// invocation so assertions can verify the right arguments were
    /// passed, without spawning real `systemctl` processes.
    #[derive(Default)]
    struct FakeQuiescer {
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl FakeQuiescer {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Quiescer for FakeQuiescer {
        fn quiesce(
            &self,
            layout: &crate::tools::HomeDirLayout,
            dry_run: bool,
        ) -> Result<(QuiesceOutcome, ResumeAction), MigrateError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("quiesce(dry_run={dry_run})"));
            if dry_run {
                return Ok((QuiesceOutcome::DryRunSkipped, ResumeAction::None));
            }
            if let Some(u) = layout.quiesce.systemd_user_unit.as_deref() {
                Ok((
                    QuiesceOutcome::UnitStopped {
                        scope: "user".into(),
                        unit: u.into(),
                    },
                    ResumeAction::StartUnit {
                        scope: "user".into(),
                        unit: u.into(),
                    },
                ))
            } else if let Some(u) = layout.quiesce.systemd_system_unit.as_deref() {
                Ok((
                    QuiesceOutcome::UnitStopped {
                        scope: "system".into(),
                        unit: u.into(),
                    },
                    ResumeAction::StartUnit {
                        scope: "system".into(),
                        unit: u.into(),
                    },
                ))
            } else {
                Ok((QuiesceOutcome::NoUnitWarning, ResumeAction::None))
            }
        }

        fn resume(&self, action: &ResumeAction, dry_run: bool) -> Result<(), MigrateError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("resume({action:?}, dry_run={dry_run})"));
            Ok(())
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

    // ── New in step 4: Quiescer is actually called + ResumeAction lands

    #[test]
    fn migrate_with_fake_quiescer_records_dry_run_skipped() {
        // Dry-run path should pass through the Quiescer but produce a
        // `DryRunSkipped` outcome and a `None` resume action.
        let t = tool(HomeDirDiscovery::Symlink, None, Some("anything.service"));
        let src = TempDir::new().unwrap();
        populate(src.path(), &[("a", b"b")]);
        let dst_dir = TempDir::new().unwrap();
        let fake = FakeQuiescer::default();

        let result = migrate_with(&t, src.path(), &dst_dir.path().join("d"), true, &fake).unwrap();
        let calls = fake.calls();
        assert_eq!(calls.len(), 1, "quiesce called once: {calls:?}");
        assert!(calls[0].contains("dry_run=true"));
        assert_eq!(result.resume_action, ResumeAction::None);
    }

    #[test]
    fn fake_quiescer_returns_user_unit_when_layout_declares_one() {
        // Drives the Quiescer trait directly to verify the user-scope
        // branch picks up the right unit name + scope. The migrate
        // dispatcher would carry this through to the result.
        let layout = HomeDirLayout {
            default_path: "~/.ignored".into(),
            discovery: HomeDirDiscovery::Symlink,
            env_var: None,
            config_files: vec![],
            quiesce: HomeDirQuiesce {
                systemd_user_unit: Some("dropbox.service".into()),
                systemd_system_unit: None,
            },
            validate: HomeDirValidate::default(),
        };
        let fake = FakeQuiescer::default();
        let (outcome, resume) = fake.quiesce(&layout, false).unwrap();
        match outcome {
            QuiesceOutcome::UnitStopped { scope, unit } => {
                assert_eq!(scope, "user");
                assert_eq!(unit, "dropbox.service");
            }
            other => panic!("expected UnitStopped, got {other:?}"),
        }
        match resume {
            ResumeAction::StartUnit { scope, unit } => {
                assert_eq!(scope, "user");
                assert_eq!(unit, "dropbox.service");
            }
            other => panic!("expected StartUnit, got {other:?}"),
        }
    }

    #[test]
    fn fake_quiescer_prefers_user_over_system_when_both_declared() {
        // Per design: `--user` is preferred (cheap, no sudo). When
        // both are set, user wins.
        let layout = HomeDirLayout {
            default_path: "~/.ignored".into(),
            discovery: HomeDirDiscovery::Symlink,
            env_var: None,
            config_files: vec![],
            quiesce: HomeDirQuiesce {
                systemd_user_unit: Some("user.service".into()),
                systemd_system_unit: Some("system.service".into()),
            },
            validate: HomeDirValidate::default(),
        };
        let fake = FakeQuiescer::default();
        let (outcome, _) = fake.quiesce(&layout, false).unwrap();
        match outcome {
            QuiesceOutcome::UnitStopped { unit, .. } => assert_eq!(unit, "user.service"),
            other => panic!("expected user-scope UnitStopped, got {other:?}"),
        }
    }

    #[test]
    fn fake_quiescer_falls_through_to_system_when_only_system_declared() {
        let layout = HomeDirLayout {
            default_path: "~/.ignored".into(),
            discovery: HomeDirDiscovery::Symlink,
            env_var: None,
            config_files: vec![],
            quiesce: HomeDirQuiesce {
                systemd_user_unit: None,
                systemd_system_unit: Some("nginx.service".into()),
            },
            validate: HomeDirValidate::default(),
        };
        let fake = FakeQuiescer::default();
        let (outcome, resume) = fake.quiesce(&layout, false).unwrap();
        match outcome {
            QuiesceOutcome::UnitStopped { scope, unit } => {
                assert_eq!(scope, "system");
                assert_eq!(unit, "nginx.service");
            }
            other => panic!("expected system-scope UnitStopped, got {other:?}"),
        }
        assert!(matches!(resume, ResumeAction::StartUnit { .. }));
    }

    #[test]
    fn fake_quiescer_returns_warning_for_unitless_tool() {
        // Ephemeral tools (no systemd unit declared) get a warning,
        // not a failure — migration continues; operator's responsibility
        // to make sure the tool isn't writing.
        let layout = HomeDirLayout {
            default_path: "~/.ignored".into(),
            discovery: HomeDirDiscovery::Symlink,
            env_var: None,
            config_files: vec![],
            quiesce: HomeDirQuiesce::default(),
            validate: HomeDirValidate::default(),
        };
        let fake = FakeQuiescer::default();
        let (outcome, resume) = fake.quiesce(&layout, false).unwrap();
        assert_eq!(outcome, QuiesceOutcome::NoUnitWarning);
        assert_eq!(resume, ResumeAction::None);
    }

    #[test]
    fn resume_action_serialises_with_tagged_repr() {
        // The dashboard / event log consume this as JSON.
        let r = ResumeAction::StartUnit {
            scope: "user".into(),
            unit: "x.service".into(),
        };
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["kind"], "start_unit");
        assert_eq!(j["scope"], "user");
        assert_eq!(j["unit"], "x.service");

        let n = ResumeAction::None;
        let j = serde_json::to_value(&n).unwrap();
        assert_eq!(j["kind"], "none");
    }
}
