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

// ── Copy + Verify (stages 3 and 4) ────────────────────────────────────────

/// Summary of one Copy run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopySummary {
    /// Number of regular files written into the destination.
    pub files_copied: usize,
    /// Total bytes written into the destination.
    pub bytes_copied: u64,
    /// Number of directories created under the destination.
    pub dirs_created: usize,
}

/// Recursively copy every regular file under `src` into `dst`,
/// creating intermediate directories as needed.
///
/// Design constraints from `docs/design/migrate.md`:
/// - Symlinks are **skipped** (cycles + off-tree pointers). Use rsync
///   externally if you need symlink semantics; the migrate target tools
///   (Codex, OpenCode) store regular files only.
/// - File permissions are mirrored (Unix mode bits) so executables and
///   read-only files preserve their character.
/// - **No-overwrite**: `dst` must not exist before the call. The
///   migrate driver enforces this in preflight; this function asserts
///   it again as a defensive belt-and-suspenders.
/// - On any error, `dst` may be left partially populated. The caller
///   is responsible for cleanup (see `cleanup_partial_copy`).
///
/// Equivalent to `cp -r src dst` for the practical AI-tool-data case,
/// minus symlinks. Not a full `rsync` — no delta detection, no
/// resumability. For the v0.4 target (one-shot migration of stable
/// data), this is sufficient.
pub fn copy_tree(src: &Path, dst: &Path) -> Result<CopySummary, MigrateError> {
    if dst.exists() {
        return Err(MigrateError::DestinationExists(dst.to_path_buf()));
    }
    if !src.is_dir() {
        return Err(MigrateError::StageFailed(
            Stage::Copy,
            format!("source `{}` is not a directory", src.display()),
        ));
    }
    let mut summary = CopySummary {
        files_copied: 0,
        bytes_copied: 0,
        dirs_created: 0,
    };

    std::fs::create_dir_all(dst)?;
    summary.dirs_created += 1;

    copy_tree_inner(src, dst, &mut summary)?;
    Ok(summary)
}

fn copy_tree_inner(src: &Path, dst: &Path, summary: &mut CopySummary) -> Result<(), MigrateError> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        // Use `symlink_metadata` so we don't follow symlinks at the
        // type check; that lets us detect (and skip) them explicitly.
        let meta = entry.metadata()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if meta.file_type().is_symlink() {
            // Skip silently. Design doc says symlinks aren't followed;
            // tools whose data dirs contain symlinks need a future
            // `follow_symlinks` opt-in.
            continue;
        }
        if meta.is_dir() {
            std::fs::create_dir_all(&dst_path)?;
            summary.dirs_created += 1;
            copy_tree_inner(&src_path, &dst_path, summary)?;
        } else if meta.is_file() {
            let bytes = std::fs::copy(&src_path, &dst_path)?;
            summary.bytes_copied = summary.bytes_copied.saturating_add(bytes);
            summary.files_copied = summary.files_copied.saturating_add(1);

            // Mirror Unix mode bits so executables stay executable
            // and read-only files stay read-only. On non-Unix this
            // is a no-op (Windows perms are richer and need their
            // own pass when we add Windows support).
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode();
                if let Err(e) =
                    std::fs::set_permissions(&dst_path, std::fs::Permissions::from_mode(mode))
                {
                    return Err(MigrateError::StageFailed(
                        Stage::Copy,
                        format!("set_permissions failed on {}: {e}", dst_path.display()),
                    ));
                }
            }
        }
        // Other file types (block/char devs, FIFOs, sockets) are
        // unexpected in AI-tool data dirs — skip silently.
    }
    Ok(())
}

/// Remove a partially-populated `dst` after a failed copy. Best-effort:
/// errors are swallowed (we're already in a failure path).
pub fn cleanup_partial_copy(dst: &Path) {
    if dst.exists() {
        let _ = std::fs::remove_dir_all(dst);
    }
}

/// Output of a Verify run — symmetric counts/sizes if the migration is
/// good; mismatched fields if something went wrong mid-copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyOutcome {
    pub src_files: usize,
    pub dst_files: usize,
    pub src_bytes: u64,
    pub dst_bytes: u64,
    /// `true` iff src and dst agree on file count *and* total bytes.
    pub matches: bool,
}

/// Walk both `src` and `dst`, comparing file count and total bytes.
/// Returns `Ok(VerifyOutcome)` regardless of whether they match; the
/// caller decides whether to fail the migration on `matches == false`.
pub fn verify_copy(src: &Path, dst: &Path) -> Result<VerifyOutcome, MigrateError> {
    let (src_files, src_bytes) = walk_size(src);
    let (dst_files, dst_bytes) = walk_size(dst);
    Ok(VerifyOutcome {
        src_files,
        dst_files,
        src_bytes,
        dst_bytes,
        matches: src_files == dst_files && src_bytes == dst_bytes,
    })
}

// ── Rewrite (stage 5) ────────────────────────────────────────────────────

/// What happened during Rewrite, so subsequent stages (Resume,
/// Retain, undo) know how to reverse it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RewriteOutcome {
    /// The original source path was renamed aside and a symlink was
    /// installed in its place pointing at `dst`. Tool keeps reading
    /// the canonical path; data lives at the new location.
    SymlinkInstalled {
        /// The original `src` (now a symlink).
        canonical: PathBuf,
        /// Where `canonical` now points to (`dst`).
        target: PathBuf,
        /// The renamed-aside original directory. Empty when the source
        /// hasn't been moved yet (e.g. Retain stage handles renames).
        moved_aside: Option<PathBuf>,
    },
    /// Dry-run; nothing happened. Carried so the result type can still
    /// claim a final stage of Rewrite without lying.
    DryRunSkipped,
    /// Discovery is `Env` or `Config`; not yet implemented this step.
    Deferred {
        /// Free-form reason. Surfaced verbatim in the event log.
        reason: String,
    },
}

/// Install a symlink at `canonical` pointing to `target`, after first
/// renaming `canonical` aside to `canonical.migrated-<unix_seconds>`.
///
/// Used by the Symlink-discovery rewrite branch. The "move aside, then
/// symlink in" sequence preserves the design doc's "never auto-delete"
/// guarantee: the original directory is still on disk at the
/// `.migrated-…` path until the operator runs `sessionguard
/// migrate-cleanup` (a later command).
fn rewrite_via_symlink(canonical: &Path, target: &Path) -> Result<RewriteOutcome, MigrateError> {
    // We expect `canonical` to be a real directory (the original src)
    // and `target` to be the already-copied destination.
    if !canonical.exists() {
        return Err(MigrateError::StageFailed(
            Stage::Rewrite,
            format!(
                "canonical path `{}` doesn't exist; nothing to rewrite",
                canonical.display()
            ),
        ));
    }
    if !target.exists() {
        return Err(MigrateError::StageFailed(
            Stage::Rewrite,
            format!(
                "target `{}` doesn't exist; copy stage must have run first",
                target.display()
            ),
        ));
    }

    // Move the original aside with a timestamped sidecar name. This is
    // the design-doc-prescribed retention pattern.
    let moved_aside = canonical.with_file_name(format!(
        "{}.migrated-{}",
        canonical
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("data"),
        now_unix()
    ));
    if moved_aside.exists() {
        return Err(MigrateError::StageFailed(
            Stage::Rewrite,
            format!(
                "preserved name `{}` already exists; refusing to overwrite. \
                 Move it aside manually and retry.",
                moved_aside.display()
            ),
        ));
    }
    std::fs::rename(canonical, &moved_aside)?;

    // Install symlink. If this fails, undo the rename to leave the
    // operator with their original setup intact.
    #[cfg(unix)]
    let symlink_result = std::os::unix::fs::symlink(target, canonical);
    #[cfg(not(unix))]
    let symlink_result: std::io::Result<()> = Err(std::io::Error::other(
        "symlinks not supported on this platform",
    ));

    match symlink_result {
        Ok(()) => Ok(RewriteOutcome::SymlinkInstalled {
            canonical: canonical.to_path_buf(),
            target: target.to_path_buf(),
            moved_aside: Some(moved_aside),
        }),
        Err(e) => {
            // Symlink failed — restore the original. Best-effort.
            let _ = std::fs::rename(&moved_aside, canonical);
            Err(MigrateError::StageFailed(
                Stage::Rewrite,
                format!("symlink install failed (original restored): {e}"),
            ))
        }
    }
}

/// Undo a [`RewriteOutcome`]. Used when a later stage fails and we
/// need to roll back the symlink dance.
fn undo_rewrite(outcome: &RewriteOutcome) -> Result<(), MigrateError> {
    match outcome {
        RewriteOutcome::SymlinkInstalled {
            canonical,
            moved_aside: Some(aside),
            ..
        } => {
            // Remove the symlink at `canonical`, then rename the
            // preserved directory back.
            if canonical.exists() || canonical.is_symlink() {
                std::fs::remove_file(canonical).or_else(|e| {
                    // `remove_file` doesn't always work on dirs; in our
                    // case `canonical` should always be a symlink, but
                    // be defensive.
                    if canonical.is_dir() {
                        std::fs::remove_dir_all(canonical)
                    } else {
                        Err(e)
                    }
                })?;
            }
            std::fs::rename(aside, canonical)?;
            Ok(())
        }
        RewriteOutcome::SymlinkInstalled {
            moved_aside: None, ..
        }
        | RewriteOutcome::DryRunSkipped
        | RewriteOutcome::Deferred { .. } => Ok(()),
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

    // Stage 3: copy — dry-run enumerates the work; real run actually
    // copies via `copy_tree`. The `NotYetMutating` gate now sits BEFORE
    // Stage::Rewrite rather than here; Copy + Verify are read-only on
    // source and write fully-cleanupable bytes to dst, so it's safe
    // to land them ahead of the rewrite/resume/validate trio.
    if dry_run {
        let summary = describe_copy(src, dst)?;
        record(
            Stage::Copy,
            &format!("dry-run: would {summary}"),
            &mut events,
        );
    } else {
        match copy_tree(src, dst) {
            Ok(s) => {
                record(
                    Stage::Copy,
                    &format!(
                        "copied {} files ({} bytes) into {} directories at {}",
                        s.files_copied,
                        s.bytes_copied,
                        s.dirs_created,
                        dst.display()
                    ),
                    &mut events,
                );
            }
            Err(e) => {
                // Belt-and-suspenders: clean up the partial dst before
                // surfacing the error, so the operator isn't left with
                // an orphaned half-copy.
                cleanup_partial_copy(dst);
                record(
                    Stage::Copy,
                    &format!("copy failed and partial dst removed: {e}"),
                    &mut events,
                );
                return Err(e);
            }
        }
    }

    // Stage 4: verify — under dry-run, sanity-checks the source
    // independently so we'd catch e.g. "src has zero readable files".
    // Under real run, walks both sides and compares {file count,
    // total bytes}. Mismatch is treated as a hard failure: cleanup
    // dst and abort.
    if dry_run {
        let (src_files, src_bytes) = walk_size(src);
        record(
            Stage::Verify,
            &format!("dry-run: source has {src_files} files, {src_bytes} bytes total"),
            &mut events,
        );
    } else {
        match verify_copy(src, dst) {
            Ok(v) if v.matches => {
                record(
                    Stage::Verify,
                    &format!(
                        "verify ok: {} files, {} bytes in both src and dst",
                        v.src_files, v.src_bytes
                    ),
                    &mut events,
                );
            }
            Ok(v) => {
                cleanup_partial_copy(dst);
                let detail = format!(
                    "verify mismatch — src: {} files / {} bytes, \
                     dst: {} files / {} bytes (partial dst removed)",
                    v.src_files, v.src_bytes, v.dst_files, v.dst_bytes
                );
                record(Stage::Verify, &detail, &mut events);
                return Err(MigrateError::StageFailed(Stage::Verify, detail));
            }
            Err(e) => {
                cleanup_partial_copy(dst);
                return Err(e);
            }
        }
    }

    // Stage 5: rewrite — install the symlink / env override / config
    // edit per the layout's discovery branch. This is the first
    // *user-visible* change: after this step, the tool will start
    // reading from `dst`. Resume / Validate / Retain still come next,
    // and the `NotYetMutating` gate now sits BEFORE Stage::Resume —
    // a successful Rewrite isn't enough on its own to call the
    // migration done, but it's safe to land because we can undo it.
    let rewrite_outcome = if dry_run {
        RewriteOutcome::DryRunSkipped
    } else {
        match layout.discovery {
            HomeDirDiscovery::Symlink => match rewrite_via_symlink(src, dst) {
                Ok(o) => o,
                Err(e) => {
                    cleanup_partial_copy(dst);
                    return Err(e);
                }
            },
            HomeDirDiscovery::Env | HomeDirDiscovery::Config => {
                // Env discovery (systemd drop-in or shell rc edit) and
                // Config discovery (write through reconciler adapters)
                // both land in step 6b. Until then, dry-run-only.
                cleanup_partial_copy(dst);
                return Err(MigrateError::StageFailed(
                    Stage::Rewrite,
                    format!(
                        "discovery = {:?} is not yet implemented in this release; \
                         only `Symlink` discovery has a working Rewrite. Use \
                         --dry-run to walk the read-only half of the state machine.",
                        layout.discovery
                    ),
                ));
            }
            HomeDirDiscovery::Compile => {
                // Already rejected in preflight, but be defensive.
                cleanup_partial_copy(dst);
                return Err(MigrateError::CompileBaked);
            }
        }
    };
    let rewrite_detail = match &rewrite_outcome {
        RewriteOutcome::SymlinkInstalled {
            canonical,
            target,
            moved_aside: Some(aside),
        } => format!(
            "installed symlink {} -> {}; preserved original at {}",
            canonical.display(),
            target.display(),
            aside.display()
        ),
        RewriteOutcome::SymlinkInstalled {
            moved_aside: None, ..
        } => "symlink installed (no preserved original recorded)".into(),
        RewriteOutcome::DryRunSkipped => {
            format!(
                "dry-run: would install symlink for {:?} discovery",
                layout.discovery
            )
        }
        RewriteOutcome::Deferred { reason } => format!("rewrite deferred: {reason}"),
    };
    record(Stage::Rewrite, &rewrite_detail, &mut events);

    // Stages 6-8 (Resume / Validate / Retain) are still gated. Once
    // Rewrite has run on a real migration, we need to undo it before
    // returning the gate error — otherwise the operator is left with
    // a symlink pointing at data the tool isn't yet wired up against.
    if !dry_run {
        if let Err(undo_err) = undo_rewrite(&rewrite_outcome) {
            // Best-effort undo: if it fails we record the failure in
            // the event log but still return NotYetMutating; the
            // operator will see both events and know what to fix.
            record(
                Stage::Rewrite,
                &format!("rollback of Rewrite FAILED: {undo_err}"),
                &mut events,
            );
        } else {
            record(
                Stage::Rewrite,
                "rollback ok (Rewrite undone before gate)",
                &mut events,
            );
        }
        cleanup_partial_copy(dst);
        return Err(MigrateError::NotYetMutating);
    }

    // Stages 6–8 are not wired yet. Dry-run returns success at this
    // point with Stage::Rewrite as the terminal so the operator can
    // see the rewrite intent recorded in the event log.
    let result = MigrationResult {
        tool_name: tool.name.clone(),
        src: src.to_path_buf(),
        dst: dst.to_path_buf(),
        dry_run,
        final_stage: Stage::Rewrite,
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
        // With Rewrite landed (step 6), the dry-run terminal stage
        // advances from Verify → Rewrite.
        assert_eq!(result.final_stage, Stage::Rewrite);
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
                Stage::Rewrite,
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

    // ── New in step 5: copy_tree + verify_copy

    #[test]
    fn copy_tree_copies_files_and_subdirs() {
        let src = TempDir::new().unwrap();
        populate(
            src.path(),
            &[
                ("top.txt", b"hello"),
                ("sub/nested.bin", &[0u8; 1024]),
                ("sub/deep/leaf.dat", b"leaf"),
                ("sub/another/x.json", b"{\"k\":\"v\"}"),
            ],
        );
        let dst_dir = TempDir::new().unwrap();
        let dst = dst_dir.path().join("new-tree");

        let summary = copy_tree(src.path(), &dst).unwrap();
        assert_eq!(summary.files_copied, 4);
        assert_eq!(summary.bytes_copied, 5 + 1024 + 4 + 9);
        assert!(summary.dirs_created >= 1);

        // Spot-check content survived
        assert_eq!(
            std::fs::read_to_string(dst.join("top.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("sub/another/x.json")).unwrap(),
            "{\"k\":\"v\"}"
        );
    }

    #[test]
    fn copy_tree_refuses_existing_destination() {
        let src = TempDir::new().unwrap();
        populate(src.path(), &[("a", b"b")]);
        let dst = TempDir::new().unwrap(); // already exists
        let err = copy_tree(src.path(), dst.path()).unwrap_err();
        assert!(matches!(err, MigrateError::DestinationExists(_)));
    }

    #[test]
    fn copy_tree_skips_symlinks() {
        let src = TempDir::new().unwrap();
        populate(src.path(), &[("real.txt", b"data")]);
        // Add a symlink alongside the regular file
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(src.path().join("real.txt"), src.path().join("link.txt"))
                .unwrap();
        }
        let dst_dir = TempDir::new().unwrap();
        let dst = dst_dir.path().join("out");
        let summary = copy_tree(src.path(), &dst).unwrap();
        // Symlink should be skipped on Unix; on other platforms the
        // create above is a no-op so the count is still 1.
        assert_eq!(summary.files_copied, 1, "symlinks must not be followed");
        assert!(dst.join("real.txt").exists());
        assert!(!dst.join("link.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_mirrors_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let src = TempDir::new().unwrap();
        let bin = src.path().join("exec.sh");
        std::fs::write(&bin, b"#!/bin/sh\necho hi\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let dst_dir = TempDir::new().unwrap();
        let dst = dst_dir.path().join("out");
        copy_tree(src.path(), &dst).unwrap();

        let mode = std::fs::metadata(dst.join("exec.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "expected 0755, got {:o}", mode & 0o777);
    }

    #[test]
    fn verify_copy_returns_matches_for_clean_copy() {
        let src = TempDir::new().unwrap();
        populate(
            src.path(),
            &[
                ("a.txt", b"x"),
                ("sub/b.txt", b"yz"),
                ("c.bin", &[0u8; 128]),
            ],
        );
        let dst_dir = TempDir::new().unwrap();
        let dst = dst_dir.path().join("out");
        copy_tree(src.path(), &dst).unwrap();

        let outcome = verify_copy(src.path(), &dst).unwrap();
        assert!(outcome.matches, "verify should match: {outcome:?}");
        assert_eq!(outcome.src_files, outcome.dst_files);
        assert_eq!(outcome.src_bytes, outcome.dst_bytes);
    }

    #[test]
    fn verify_copy_detects_mismatch_when_file_removed() {
        let src = TempDir::new().unwrap();
        populate(src.path(), &[("keep", b"k"), ("drop", b"d")]);
        let dst_dir = TempDir::new().unwrap();
        let dst = dst_dir.path().join("out");
        copy_tree(src.path(), &dst).unwrap();
        // Simulate corruption: remove a file from the dst.
        std::fs::remove_file(dst.join("drop")).unwrap();

        let outcome = verify_copy(src.path(), &dst).unwrap();
        assert!(!outcome.matches, "verify must catch missing dst file");
        assert_eq!(outcome.src_files, 2);
        assert_eq!(outcome.dst_files, 1);
    }

    #[test]
    fn cleanup_partial_copy_removes_dst() {
        let dst_dir = TempDir::new().unwrap();
        let dst = dst_dir.path().join("partial");
        std::fs::create_dir_all(dst.join("nested")).unwrap();
        std::fs::write(dst.join("nested/file"), b"x").unwrap();
        assert!(dst.exists());
        cleanup_partial_copy(&dst);
        assert!(!dst.exists());
    }

    #[test]
    fn cleanup_partial_copy_is_noop_when_dst_absent() {
        // Must not panic when called on a path that was never created.
        let tmp = TempDir::new().unwrap();
        cleanup_partial_copy(&tmp.path().join("never-existed"));
    }

    #[test]
    fn migrate_real_run_now_actually_copies_then_rolls_back() {
        // With the NotYetMutating gate moved to AFTER Verify, a real
        // (non-dry-run) call now performs Copy + Verify successfully,
        // hits the gate at Stage::Rewrite, and rolls back by removing
        // the dst it just populated. The migrate() observer sees
        // NotYetMutating; the FS is left in its original state.
        let t = tool(HomeDirDiscovery::Symlink, None, None);
        let src = TempDir::new().unwrap();
        populate(
            src.path(),
            &[("a.txt", b"first"), ("nested/b.bin", &[0u8; 32])],
        );
        let dst_dir = TempDir::new().unwrap();
        let dst = dst_dir.path().join("not-yet-existing");

        let err = migrate_with(&t, src.path(), &dst, false, &FakeQuiescer::default()).unwrap_err();
        assert!(matches!(err, MigrateError::NotYetMutating), "got {err:?}");
        // Rollback removed the dst the Copy stage created:
        assert!(!dst.exists(), "real-run rollback must remove orphan dst");
        // Source still intact (Rewrite would have moved it aside;
        // rollback then renames it back):
        assert!(src.path().join("a.txt").exists());
        assert!(src.path().join("nested/b.bin").exists());
        // The `.migrated-*` sidecar that Rewrite created should have
        // been undone by the rollback path:
        let leftovers: Vec<_> = std::fs::read_dir(src.path().parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|s| s.contains(".migrated-"))
                    .unwrap_or(false)
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "rollback must remove the .migrated-* sidecar; found: {:?}",
            leftovers.iter().map(|e| e.file_name()).collect::<Vec<_>>()
        );
    }

    // ── New in step 6: Rewrite stage (symlink branch)

    #[cfg(unix)]
    #[test]
    fn rewrite_via_symlink_swaps_dir_for_symlink_and_preserves_original() {
        let parent = TempDir::new().unwrap();
        let canonical = parent.path().join("data");
        let target = parent.path().join("new-home");
        // Set up: canonical is a real dir with one file; target is
        // a separate dir (simulating Copy already ran).
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::write(canonical.join("a"), b"original").unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("a"), b"original").unwrap();

        let outcome = rewrite_via_symlink(&canonical, &target).unwrap();
        match outcome {
            RewriteOutcome::SymlinkInstalled {
                canonical: c,
                target: t,
                moved_aside: Some(aside),
            } => {
                assert_eq!(c, canonical);
                assert_eq!(t, target);
                assert!(aside.exists(), "preserved aside should still exist");
                assert!(aside.is_dir(), "preserved aside should be a directory");
                // canonical is now a symlink
                let meta = std::fs::symlink_metadata(&canonical).unwrap();
                assert!(meta.file_type().is_symlink());
                // Following the symlink reads target/a:
                let read = std::fs::read_to_string(canonical.join("a")).unwrap();
                assert_eq!(read, "original");
            }
            other => panic!("expected SymlinkInstalled, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn rewrite_via_symlink_refuses_when_preserved_name_already_taken() {
        // If someone already migrated and left a .migrated-<ts>
        // sidecar, we refuse to clobber it on a second attempt.
        let parent = TempDir::new().unwrap();
        let canonical = parent.path().join("data");
        let target = parent.path().join("new-home");
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::create_dir_all(&target).unwrap();

        // Pre-create a collision at the timestamped path. The test
        // can't easily predict the exact timestamp `now_unix()`
        // returns, so we manually create a .migrated-<exact-now>
        // sidecar by calling the same time fn and seeding the
        // collision before rewrite runs. Edge case but worth covering.
        let collision = canonical.with_file_name(format!("data.migrated-{}", now_unix()));
        std::fs::create_dir_all(&collision).unwrap();

        let err = rewrite_via_symlink(&canonical, &target).unwrap_err();
        match err {
            MigrateError::StageFailed(Stage::Rewrite, msg) => {
                assert!(msg.contains("preserved name") && msg.contains("already exists"));
            }
            other => panic!("expected StageFailed(Rewrite, ...), got {other:?}"),
        }
        // Original canonical must be untouched:
        assert!(
            canonical.is_dir(),
            "canonical must not be modified on refusal"
        );
    }

    #[cfg(unix)]
    #[test]
    fn undo_rewrite_restores_original_directory() {
        let parent = TempDir::new().unwrap();
        let canonical = parent.path().join("data");
        let target = parent.path().join("new-home");
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::write(canonical.join("a"), b"v1").unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("a"), b"v1").unwrap();

        let outcome = rewrite_via_symlink(&canonical, &target).unwrap();
        // Sanity: canonical is a symlink now
        assert!(std::fs::symlink_metadata(&canonical)
            .unwrap()
            .file_type()
            .is_symlink());

        undo_rewrite(&outcome).unwrap();

        // canonical is a real directory again, with its original file
        let meta = std::fs::symlink_metadata(&canonical).unwrap();
        assert!(meta.is_dir() && !meta.file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(canonical.join("a")).unwrap(), "v1");
    }

    #[test]
    fn migrate_refuses_env_discovery_until_step_6b() {
        // Env-var rewrite branch isn't wired yet (systemd drop-in /
        // shell rc edits land in step 6b). A real-run attempt against
        // a tool with discovery = Env should refuse with a clear
        // message and roll back the copy.
        let t = tool(HomeDirDiscovery::Env, Some("TEST_HOME"), None);
        let src = TempDir::new().unwrap();
        populate(src.path(), &[("a.txt", b"hello")]);
        let dst_dir = TempDir::new().unwrap();
        let dst = dst_dir.path().join("new");

        let err = migrate_with(&t, src.path(), &dst, false, &FakeQuiescer::default()).unwrap_err();
        match err {
            MigrateError::StageFailed(Stage::Rewrite, msg) => {
                assert!(
                    msg.contains("not yet implemented"),
                    "expected discovery=Env refusal, got: {msg}"
                );
            }
            other => panic!("expected StageFailed(Rewrite, ...), got {other:?}"),
        }
        // Rollback removed the copied dst:
        assert!(!dst.exists(), "rollback must remove dst on Env refusal");
    }
}
