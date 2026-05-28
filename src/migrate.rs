// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Migration state machine for `sessionguard migrate` (v0.4).
//!
//! Implements the nine-stage state machine from `docs/design/migrate.md`,
//! end-to-end: real migrations now run to completion through
//! Preflight → Snapshot → Quiesce → Copy → Verify → Rewrite →
//! Resume → Validate → Retain → Done.
//!
//! ## Stage status
//!
//! | Stage     | Status      | Effect on disk |
//! |-----------|-------------|----------------|
//! | Preflight | Implemented | Read-only checks |
//! | Snapshot  | Stubbed     | Records intent only (btrfs detect comes later) |
//! | Quiesce   | Implemented | `systemctl stop` via [`Quiescer`] |
//! | Copy      | Implemented | Recursive copy into the new path |
//! | Verify    | Implemented | Compares file count + total size |
//! | Rewrite   | Implemented | Symlink / config edit / systemd drop-in per discovery |
//! | Resume    | Implemented | `systemctl start` via [`Quiescer`] |
//! | Validate  | Implemented | Runs `validate.command` with timeout |
//! | Retain    | Implemented | Renames source to `.migrated-<ts>` (never auto-deletes) |
//! | Done      | Terminal    | — |
//!
//! Any post-Verify failure rolls back every preceding side-effect:
//! dst removed, drop-ins uninstalled, config backups restored,
//! symlink-sidecars renamed back. The "never auto-delete the source"
//! design rule means even on success the original lives on at
//! `<src>.migrated-<unix>` until the operator decides to clean it up.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::tools::{HomeDirConfigFile, HomeDirDiscovery, ToolDefinition};

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
    /// What Rewrite did, retained so `sessionguard undo` can reverse it.
    /// `None` on a dry-run or a run that aborted before Rewrite.
    #[serde(default)]
    pub rewrite_outcome: Option<RewriteOutcome>,
    /// Where the source ended up after Retain (the `.migrated-<unix>`
    /// sidecar). `undo` renames this back to `src`. `None` for dry-runs
    /// and for runs where the source was never moved aside.
    #[serde(default)]
    pub retained_at: Option<PathBuf>,
}

/// The minimal, self-contained set of facts `sessionguard undo` needs
/// to reverse a completed migration. Serialized to JSON and stored in
/// the event log's `migrations` table; deserialized at undo time and
/// fed to [`undo_migration`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationUndo {
    pub tool_name: String,
    pub src: PathBuf,
    pub dst: PathBuf,
    /// How Rewrite mutated the system, so we can invert it.
    pub rewrite_outcome: RewriteOutcome,
    /// The `.migrated-<unix>` path the source was renamed to (Config/Env
    /// discovery). `None` for Symlink discovery, where `rewrite_outcome`
    /// already carries the moved-aside path.
    pub retained_at: Option<PathBuf>,
}

impl MigrationResult {
    /// Build the undo plan for a successful real migration, or `None`
    /// if this result isn't reversible (dry-run, failure, or a rewrite
    /// outcome that left nothing to undo).
    pub fn undo_plan(&self) -> Option<MigrationUndo> {
        if self.dry_run || !self.success {
            return None;
        }
        let rewrite_outcome = self.rewrite_outcome.clone()?;
        if matches!(
            rewrite_outcome,
            RewriteOutcome::DryRunSkipped | RewriteOutcome::Deferred { .. }
        ) {
            return None;
        }
        Some(MigrationUndo {
            tool_name: self.tool_name.clone(),
            src: self.src.clone(),
            dst: self.dst.clone(),
            rewrite_outcome,
            retained_at: self.retained_at.clone(),
        })
    }
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

// ── EnvWriter: abstraction over "set the env var the tool reads" ─────────
//
// Used by the `HomeDirDiscovery::Env` rewrite branch. The default
// implementation drops a `Environment=<VAR>=<value>` override into the
// tool's systemd unit (user or system scope, mirroring Quiesce), then
// runs `systemctl daemon-reload`. Tests substitute a fake so they don't
// depend on the host having systemd or a real unit registered.
//
// Design constraint: if the layout declares `discovery = "env"` but
// has no systemd unit attached, we *refuse* the rewrite — there's no
// safe automatic place to set the env var system-wide, and we don't
// want to silently edit shell rc files. The operator's preflight
// message tells them to declare a unit or set the var manually.
//
// Drop-in convention (matches systemd docs and what most operators
// expect to find when troubleshooting):
//   user:   ~/.config/systemd/user/<unit>.d/sessionguard-migrate.conf
//   system: /etc/systemd/system/<unit>.d/sessionguard-migrate.conf

/// Record of an env-rewrite install, used both for the result event
/// log and to roll the override back during undo. Tagged so the
/// JSON shape is self-describing for dashboards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvOverrideRecord {
    /// Systemd unit scope: `"user"` or `"system"`.
    pub scope: String,
    /// Unit name (e.g. `opencode.service`).
    pub unit: String,
    /// The drop-in file that was written.
    pub drop_in_path: PathBuf,
    /// Env-var name that was set.
    pub env_var: String,
    /// Value the env var was set to (the new data dir).
    pub value: String,
}

/// Pluggable env-writer backend so unit tests can verify Env-rewrite
/// behaviour without spawning real `systemctl` or writing into the
/// operator's `~/.config/systemd/user`.
pub trait EnvWriter {
    /// Install an env override for the layout's declared systemd unit.
    /// Returns the record so undo can find the drop-in to remove.
    /// On `dry_run = true`, returns `Ok(None)` and writes nothing.
    fn install(
        &self,
        layout: &crate::tools::HomeDirLayout,
        env_var: &str,
        value: &str,
        dry_run: bool,
    ) -> Result<Option<EnvOverrideRecord>, MigrateError>;

    /// Remove a previously-installed drop-in. Best-effort: missing
    /// files are not an error (undo may run twice).
    fn uninstall(&self, record: &EnvOverrideRecord, dry_run: bool) -> Result<(), MigrateError>;
}

/// Default real-systemd implementation. Writes the drop-in under the
/// declared scope's standard location and runs `systemctl daemon-reload`.
pub struct SystemdEnvWriter;

impl EnvWriter for SystemdEnvWriter {
    fn install(
        &self,
        layout: &crate::tools::HomeDirLayout,
        env_var: &str,
        value: &str,
        dry_run: bool,
    ) -> Result<Option<EnvOverrideRecord>, MigrateError> {
        if dry_run {
            return Ok(None);
        }
        let (scope, unit) = pick_unit(layout)?;
        let drop_in_dir = drop_in_dir_for(&scope, &unit)?;
        std::fs::create_dir_all(&drop_in_dir)?;
        let drop_in_path = drop_in_dir.join("sessionguard-migrate.conf");
        if drop_in_path.exists() {
            return Err(MigrateError::StageFailed(
                Stage::Rewrite,
                format!(
                    "drop-in `{}` already exists; another migrate may be in flight \
                     or an earlier one didn't clean up. Remove it manually and retry.",
                    drop_in_path.display()
                ),
            ));
        }
        let body = format!("[Service]\nEnvironment={env_var}={value}\n");
        std::fs::write(&drop_in_path, body)?;
        let args: Vec<&str> = if scope == "user" {
            vec!["--user", "daemon-reload"]
        } else {
            vec!["daemon-reload"]
        };
        if let Err(e) = run_systemctl(&args) {
            // daemon-reload failed — undo our drop-in to leave the
            // operator's unit graph untouched.
            let _ = std::fs::remove_file(&drop_in_path);
            return Err(e);
        }
        Ok(Some(EnvOverrideRecord {
            scope,
            unit,
            drop_in_path,
            env_var: env_var.into(),
            value: value.into(),
        }))
    }

    fn uninstall(&self, record: &EnvOverrideRecord, dry_run: bool) -> Result<(), MigrateError> {
        if dry_run {
            return Ok(());
        }
        if record.drop_in_path.exists() {
            std::fs::remove_file(&record.drop_in_path)?;
        }
        let args: Vec<&str> = if record.scope == "user" {
            vec!["--user", "daemon-reload"]
        } else {
            vec!["daemon-reload"]
        };
        // daemon-reload may fail if systemd isn't running; we've
        // already removed the file, so swallow non-fatal errors.
        let _ = run_systemctl(&args);
        Ok(())
    }
}

/// Pick the (scope, unit) pair from a layout. Prefers user over system
/// (same convention as Quiescer). Returns a clear error when neither
/// is declared — env discovery without a unit isn't supported.
fn pick_unit(layout: &crate::tools::HomeDirLayout) -> Result<(String, String), MigrateError> {
    if let Some(u) = layout.quiesce.systemd_user_unit.as_deref() {
        return Ok(("user".into(), u.into()));
    }
    if let Some(u) = layout.quiesce.systemd_system_unit.as_deref() {
        return Ok(("system".into(), u.into()));
    }
    Err(MigrateError::StageFailed(
        Stage::Rewrite,
        "discovery = \"env\" requires `quiesce.systemd_user_unit` or \
         `quiesce.systemd_system_unit` to be declared in the layout. \
         Without a unit there is no safe place to set the env var; \
         set the var manually in your shell rc and re-run with \
         --dry-run to walk the rest of the state machine."
            .into(),
    ))
}

/// Path to the systemd drop-in directory for `<scope>/<unit>.d`.
fn drop_in_dir_for(scope: &str, unit: &str) -> Result<PathBuf, MigrateError> {
    if scope == "user" {
        let home = std::env::var_os("HOME").ok_or_else(|| {
            MigrateError::StageFailed(
                Stage::Rewrite,
                "HOME is unset; cannot resolve user-scope systemd drop-in directory".into(),
            )
        })?;
        Ok(PathBuf::from(home)
            .join(".config/systemd/user")
            .join(format!("{unit}.d")))
    } else {
        Ok(PathBuf::from("/etc/systemd/system").join(format!("{unit}.d")))
    }
}

/// Rewrite via env: install a systemd drop-in pointing the tool's
/// declared env var at `dst`. Validates the layout has an `env_var`
/// before doing anything.
fn rewrite_via_env(
    layout: &crate::tools::HomeDirLayout,
    dst: &Path,
    env_writer: &dyn EnvWriter,
) -> Result<RewriteOutcome, MigrateError> {
    let env_var = layout.env_var.as_deref().ok_or_else(|| {
        MigrateError::StageFailed(
            Stage::Rewrite,
            "discovery = \"env\" but `env_var` is not declared in the layout".into(),
        )
    })?;
    let value = dst.to_string_lossy().into_owned();
    let record = env_writer.install(layout, env_var, &value, false)?;
    match record {
        Some(r) => Ok(RewriteOutcome::EnvOverridden { record: r }),
        None => Err(MigrateError::StageFailed(
            Stage::Rewrite,
            "env writer returned no record on a non-dry-run install".into(),
        )),
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
    /// One or more config files were rewritten in place, with backups
    /// taken first. Each entry is `(original_file, backup_path)` so
    /// undo can restore by renaming the backup back over the original.
    ConfigEdited { backups: Vec<ConfigBackup> },
    /// A systemd drop-in was installed setting the tool's data-dir
    /// env var to the new location. Undo removes the drop-in file
    /// and runs `daemon-reload` again.
    EnvOverridden { record: EnvOverrideRecord },
    /// Discovery branch isn't wired yet. Carried for forward-compat;
    /// no current variant uses it but keeping the enum non-exhaustive
    /// for future discovery modes (e.g. `Manifest`, `Plist`).
    Deferred {
        /// Free-form reason. Surfaced verbatim in the event log.
        reason: String,
    },
}

/// One config-file backup pair. `original` is the file we rewrote;
/// `backup` is the timestamped sidecar copy we made first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigBackup {
    pub original: PathBuf,
    pub backup: PathBuf,
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

/// Rewrite every config file in `config_files` so the data-dir field
/// points from `src` to `dst`. Backs each file up first (timestamped
/// `.sessionguard-backup-<unix>` sidecar); on any per-file failure,
/// every backup taken so far is restored and the error is returned —
/// no partial rewrites escape this function.
///
/// Used by the `HomeDirDiscovery::Config` rewrite branch. Reuses the
/// reconciler's adapter dispatch (`json` / `toml` / text fallback)
/// via `pub(crate) reconciler::rewrite_field`.
fn rewrite_via_config(
    config_files: &[HomeDirConfigFile],
    src: &Path,
    dst: &Path,
) -> Result<RewriteOutcome, MigrateError> {
    use crate::tools::PathFieldSpec;

    if config_files.is_empty() {
        return Err(MigrateError::StageFailed(
            Stage::Rewrite,
            "discovery = Config but no config_files declared".into(),
        ));
    }

    let pairs = vec![(
        src.to_string_lossy().into_owned(),
        dst.to_string_lossy().into_owned(),
    )];
    let stamp = now_unix();
    let mut backups: Vec<ConfigBackup> = Vec::new();

    for cf in config_files {
        let file_path = crate::inventory::expand_home(&cf.file);
        if !file_path.exists() {
            // Restore anything we already touched, then fail loud.
            restore_config_backups(&backups);
            return Err(MigrateError::StageFailed(
                Stage::Rewrite,
                format!(
                    "config file `{}` does not exist; cannot rewrite \
                     data-dir reference for discovery=Config",
                    file_path.display()
                ),
            ));
        }
        let backup_path = file_path.with_file_name(format!(
            "{}.sessionguard-backup-{stamp}",
            file_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("config")
        ));
        if backup_path.exists() {
            restore_config_backups(&backups);
            return Err(MigrateError::StageFailed(
                Stage::Rewrite,
                format!(
                    "backup path `{}` already exists; refusing to clobber. \
                     Move it aside manually and retry.",
                    backup_path.display()
                ),
            ));
        }
        // Copy (not rename) so the in-place rewrite still happens on
        // the original inode. Tools watching the config via mtime see
        // a single edit, not a delete+create.
        if let Err(e) = std::fs::copy(&file_path, &backup_path) {
            restore_config_backups(&backups);
            return Err(MigrateError::StageFailed(
                Stage::Rewrite,
                format!(
                    "failed to back up `{}` → `{}`: {e}",
                    file_path.display(),
                    backup_path.display()
                ),
            ));
        }
        backups.push(ConfigBackup {
            original: file_path.clone(),
            backup: backup_path.clone(),
        });

        // Synthesise a PathFieldSpec for the reconciler adapter.
        let spec = PathFieldSpec {
            file: String::new(),
            field: cf.field.clone(),
            format: cf.format.clone(),
        };
        let changed = crate::reconciler::rewrite_field(&file_path, &spec, &pairs).map_err(|e| {
            // Reconciler failed mid-write — restore everything.
            restore_config_backups(&backups);
            MigrateError::StageFailed(
                Stage::Rewrite,
                format!(
                    "config rewrite failed on `{}` (field `{}`, format `{}`): {e}",
                    file_path.display(),
                    cf.field,
                    cf.format
                ),
            )
        })?;
        if !changed {
            // Field wasn't present, or didn't carry the src prefix.
            // That's a misconfigured layout — fail loud rather than
            // pretend the rewrite happened.
            restore_config_backups(&backups);
            return Err(MigrateError::StageFailed(
                Stage::Rewrite,
                format!(
                    "config file `{}` field `{}` did not contain `{}`; \
                     check the home_dir_layout config_files declaration",
                    file_path.display(),
                    cf.field,
                    src.display()
                ),
            ));
        }
    }

    Ok(RewriteOutcome::ConfigEdited { backups })
}

/// Best-effort restore of a list of config backups. Used when a later
/// step in `rewrite_via_config` fails and we need to back out every
/// edit we made earlier in the same pass.
fn restore_config_backups(backups: &[ConfigBackup]) {
    for b in backups {
        let _ = std::fs::rename(&b.backup, &b.original);
    }
}

/// Undo a [`RewriteOutcome`]. Used when a later stage fails and we
/// need to roll back the symlink / config edit / env override.
///
/// Takes a `dyn EnvWriter` so the `EnvOverridden` branch can be
/// undone through the same systemd-aware abstraction the install
/// used. Tests pass their fake; production callers pass
/// `&SystemdEnvWriter`.
fn undo_rewrite(outcome: &RewriteOutcome, env_writer: &dyn EnvWriter) -> Result<(), MigrateError> {
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
        RewriteOutcome::ConfigEdited { backups } => {
            // Restore each backup over its original — last-write-wins.
            // Errors here mean the operator's config is now half-rewritten;
            // surface the first one so they can hand-fix.
            for b in backups {
                if !b.backup.exists() {
                    // Already restored or never created; skip.
                    continue;
                }
                std::fs::rename(&b.backup, &b.original).map_err(|e| {
                    MigrateError::StageFailed(
                        Stage::Rewrite,
                        format!(
                            "undo failed: could not restore `{}` from backup `{}`: {e}",
                            b.original.display(),
                            b.backup.display()
                        ),
                    )
                })?;
            }
            Ok(())
        }
        RewriteOutcome::EnvOverridden { record } => env_writer.uninstall(record, false),
        RewriteOutcome::SymlinkInstalled {
            moved_aside: None, ..
        }
        | RewriteOutcome::DryRunSkipped
        | RewriteOutcome::Deferred { .. } => Ok(()),
    }
}

// ── Undo a completed migration ───────────────────────────────────────────

/// What [`undo_migration`] did, for reporting to the operator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationUndoReport {
    /// `true` when invoked with `--dry-run` — no side effects taken.
    pub dry_run: bool,
    /// Human-readable steps performed (or that would be performed).
    pub steps: Vec<String>,
}

/// Reverse a completed migration recorded in the event log.
///
/// The inverse of the forward state machine, in dependency order:
/// 1. **Quiesce** — stop whatever came up reading `dst`.
/// 2. **Reverse Rewrite** — remove the symlink / restore config backups
///    / uninstall the systemd drop-in (via [`undo_rewrite`]).
/// 3. **Restore source** — for Config/Env discovery, rename the
///    `.migrated-<unix>` sidecar back to `src` (Symlink discovery already
///    restored `src` in step 2).
/// 4. **Remove dst** — the now-orphaned copy at the new location.
/// 5. **Resume** — start the unit back up.
///
/// `dry_run` walks every step without touching the system. Failures in
/// the destructive middle are surfaced; the source is never deleted, so
/// the worst case still leaves the operator with recoverable data.
pub fn undo_migration(
    plan: &MigrationUndo,
    layout: &crate::tools::HomeDirLayout,
    quiescer: &dyn Quiescer,
    env_writer: &dyn EnvWriter,
    dry_run: bool,
) -> Result<MigrationUndoReport, MigrateError> {
    let mut steps: Vec<String> = Vec::new();

    // 1. Quiesce — stop the service so nothing is reading `dst` while we
    //    pull it out from under it. Capture the resume action for step 5.
    let (_q, resume_action) = quiescer.quiesce(layout, dry_run)?;
    steps.push(match &resume_action {
        ResumeAction::StartUnit { scope, unit } => {
            format!("quiesced {scope} unit `{unit}` (will resume after)")
        }
        ResumeAction::None => "no unit to quiesce".into(),
    });

    // 2. Reverse the Rewrite.
    if dry_run {
        steps.push(format!(
            "dry-run: would reverse rewrite ({})",
            describe_rewrite_for_undo(&plan.rewrite_outcome)
        ));
    } else {
        undo_rewrite(&plan.rewrite_outcome, env_writer)?;
        steps.push(format!(
            "reversed rewrite ({})",
            describe_rewrite_for_undo(&plan.rewrite_outcome)
        ));
    }

    // 3. Restore the source for Config/Env discovery. Only act when the
    //    source slot is free (Symlink discovery already restored it in
    //    step 2, leaving `src` present).
    if let Some(sidecar) = &plan.retained_at {
        if dry_run {
            steps.push(format!(
                "dry-run: would rename {} back to {}",
                sidecar.display(),
                plan.src.display()
            ));
        } else if !plan.src.exists() && sidecar.exists() {
            std::fs::rename(sidecar, &plan.src)?;
            steps.push(format!(
                "restored source {} from {}",
                plan.src.display(),
                sidecar.display()
            ));
        } else {
            steps.push(format!(
                "source {} already present; left sidecar {} in place",
                plan.src.display(),
                sidecar.display()
            ));
        }
    }

    // 4. Remove the orphaned copy at `dst`.
    if dry_run {
        steps.push(format!(
            "dry-run: would remove copy at {}",
            plan.dst.display()
        ));
    } else if plan.dst.exists() {
        std::fs::remove_dir_all(&plan.dst).map_err(|e| {
            MigrateError::StageFailed(
                Stage::Copy,
                format!("undo: failed to remove dst `{}`: {e}", plan.dst.display()),
            )
        })?;
        steps.push(format!("removed copy at {}", plan.dst.display()));
    } else {
        steps.push(format!("copy at {} already gone", plan.dst.display()));
    }

    // 5. Resume — bring the service back up against the restored source.
    quiescer.resume(&resume_action, dry_run)?;
    steps.push(match (&resume_action, dry_run) {
        (ResumeAction::StartUnit { scope, unit }, false) => {
            format!("resumed {scope} unit `{unit}`")
        }
        (ResumeAction::StartUnit { scope, unit }, true) => {
            format!("dry-run: would resume {scope} unit `{unit}`")
        }
        (ResumeAction::None, _) => "nothing to resume".into(),
    });

    Ok(MigrationUndoReport { dry_run, steps })
}

/// One-liner describing a rewrite outcome for undo reporting.
fn describe_rewrite_for_undo(outcome: &RewriteOutcome) -> String {
    match outcome {
        RewriteOutcome::SymlinkInstalled { canonical, .. } => {
            format!(
                "remove symlink {} and restore original",
                canonical.display()
            )
        }
        RewriteOutcome::ConfigEdited { backups } => {
            format!("restore {} config backup(s)", backups.len())
        }
        RewriteOutcome::EnvOverridden { record } => {
            format!(
                "uninstall systemd drop-in {}",
                record.drop_in_path.display()
            )
        }
        RewriteOutcome::DryRunSkipped => "nothing (dry-run rewrite)".into(),
        RewriteOutcome::Deferred { reason } => format!("nothing (deferred: {reason})"),
    }
}

// ── Validate (stage 7) ────────────────────────────────────────────────────

/// Outcome of running the layout's optional `validate.command`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidateOutcome {
    /// No command declared in the layout; nothing to do.
    Skipped,
    /// Command ran and exited zero within the timeout.
    Ok { command: String, took_ms: u128 },
}

/// Run the layout's `validate.command` (if any). Returns an error if
/// the command exits non-zero, times out, or fails to spawn. The
/// timeout defaults to 10 seconds when `timeout_seconds` is unset.
fn run_validate(validate: &crate::tools::HomeDirValidate) -> Result<ValidateOutcome, MigrateError> {
    if validate.command.is_empty() {
        return Ok(ValidateOutcome::Skipped);
    }
    let timeout = std::time::Duration::from_secs(validate.timeout_seconds.unwrap_or(10));
    let start = std::time::Instant::now();
    let argv0 = &validate.command[0];
    let argv_rest = &validate.command[1..];
    let mut child = std::process::Command::new(argv0)
        .args(argv_rest)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            MigrateError::StageFailed(
                Stage::Validate,
                format!("failed to spawn `{}`: {e}", validate.command.join(" ")),
            )
        })?;

    // Poll-loop with sleep — keeps us off the tokio runtime so this
    // stays a pure sync function reachable from non-async callers.
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(ValidateOutcome::Ok {
                        command: validate.command.join(" "),
                        took_ms: start.elapsed().as_millis(),
                    });
                } else {
                    let code = status.code().unwrap_or(-1);
                    return Err(MigrateError::StageFailed(
                        Stage::Validate,
                        format!("validate `{}` exited {code}", validate.command.join(" ")),
                    ));
                }
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    return Err(MigrateError::StageFailed(
                        Stage::Validate,
                        format!(
                            "validate `{}` timed out after {}s",
                            validate.command.join(" "),
                            timeout.as_secs()
                        ),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                return Err(MigrateError::StageFailed(
                    Stage::Validate,
                    format!("failed to poll validate child: {e}"),
                ));
            }
        }
    }
}

// ── Retain (stage 8) ─────────────────────────────────────────────────────

/// What Retain did with the source dir. Stored on the result so a
/// future `sessionguard migrate-cleanup` command can find the
/// preserved sidecar and delete it once the operator's happy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RetainOutcome {
    /// Symlink discovery already moved the source aside in Rewrite.
    PreservedInRewrite { aside: PathBuf },
    /// Config/Env discovery: Retain renamed src → src.migrated-<unix>.
    RenamedAside { from: PathBuf, to: PathBuf },
    /// No source preservation applicable (e.g. partial recovery path).
    Nothing,
    /// Dry-run; nothing happened.
    DryRunSkipped,
}

/// Preserve `src` per the design's "never auto-delete" rule. The
/// behaviour depends on whether Rewrite already moved it aside.
fn retain_source(
    src: &Path,
    rewrite_outcome: &RewriteOutcome,
) -> Result<RetainOutcome, MigrateError> {
    if let RewriteOutcome::SymlinkInstalled {
        moved_aside: Some(aside),
        ..
    } = rewrite_outcome
    {
        return Ok(RetainOutcome::PreservedInRewrite {
            aside: aside.clone(),
        });
    }
    // For Config / Env rewrites, src is still a real directory. Rename
    // it aside with a timestamped sidecar name so the tool can't be
    // tricked into reading the old copy if its config is reverted by
    // hand.
    if !src.exists() {
        return Ok(RetainOutcome::Nothing);
    }
    let aside = src.with_file_name(format!(
        "{}.migrated-{}",
        src.file_name().and_then(|s| s.to_str()).unwrap_or("data"),
        now_unix()
    ));
    if aside.exists() {
        return Err(MigrateError::StageFailed(
            Stage::Retain,
            format!(
                "preserved name `{}` already exists; refusing to overwrite",
                aside.display()
            ),
        ));
    }
    std::fs::rename(src, &aside)?;
    Ok(RetainOutcome::RenamedAside {
        from: src.to_path_buf(),
        to: aside,
    })
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
/// Uses the default [`SystemdQuiescer`] + [`SystemdEnvWriter`]; pass
/// custom backends via [`migrate_with`] for tests or alternate
/// strategies.
pub fn migrate(
    tool: &ToolDefinition,
    src: &Path,
    dst: &Path,
    dry_run: bool,
) -> Result<MigrationResult, MigrateError> {
    migrate_with_backends(tool, src, dst, dry_run, &SystemdQuiescer, &SystemdEnvWriter)
}

/// Like [`migrate`] but with an injectable [`Quiescer`]. Uses the
/// default [`SystemdEnvWriter`]. Existing tests built before the
/// env-rewrite trait landed continue to compile against this signature.
pub fn migrate_with(
    tool: &ToolDefinition,
    src: &Path,
    dst: &Path,
    dry_run: bool,
    quiescer: &dyn Quiescer,
) -> Result<MigrationResult, MigrateError> {
    migrate_with_backends(tool, src, dst, dry_run, quiescer, &SystemdEnvWriter)
}

/// Fully-injectable migration driver. Tests use this with both fakes
/// to exercise Env-discovery rewrite without spawning real `systemctl`
/// or writing into the operator's `~/.config/systemd/user`.
pub fn migrate_with_backends(
    tool: &ToolDefinition,
    src: &Path,
    dst: &Path,
    dry_run: bool,
    quiescer: &dyn Quiescer,
    env_writer: &dyn EnvWriter,
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
            HomeDirDiscovery::Config => match rewrite_via_config(&layout.config_files, src, dst) {
                Ok(o) => o,
                Err(e) => {
                    cleanup_partial_copy(dst);
                    return Err(e);
                }
            },
            HomeDirDiscovery::Env => match rewrite_via_env(layout, dst, env_writer) {
                Ok(o) => o,
                Err(e) => {
                    cleanup_partial_copy(dst);
                    return Err(e);
                }
            },
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
        RewriteOutcome::ConfigEdited { backups } => {
            let names: Vec<String> = backups
                .iter()
                .map(|b| b.original.display().to_string())
                .collect();
            format!(
                "rewrote {} config file(s) [{}]; backups taken alongside each",
                backups.len(),
                names.join(", ")
            )
        }
        RewriteOutcome::EnvOverridden { record } => format!(
            "installed systemd {scope} drop-in for `{unit}` at {path} \
             setting {var}={value}",
            scope = record.scope,
            unit = record.unit,
            path = record.drop_in_path.display(),
            var = record.env_var,
            value = record.value
        ),
        RewriteOutcome::DryRunSkipped => match layout.discovery {
            HomeDirDiscovery::Symlink => format!(
                "dry-run: would install symlink {} -> {} and preserve original",
                src.display(),
                dst.display()
            ),
            HomeDirDiscovery::Config => {
                let n = layout.config_files.len();
                let files: Vec<String> = layout
                    .config_files
                    .iter()
                    .map(|cf| format!("{} (field `{}`, {})", cf.file, cf.field, cf.format))
                    .collect();
                format!(
                    "dry-run: would rewrite {n} config file(s) [{}]",
                    files.join(", ")
                )
            }
            HomeDirDiscovery::Env => {
                let var = layout.env_var.as_deref().unwrap_or("<unset>");
                let unit = layout
                    .quiesce
                    .systemd_user_unit
                    .as_deref()
                    .map(|u| format!("--user {u}"))
                    .or_else(|| {
                        layout
                            .quiesce
                            .systemd_system_unit
                            .as_deref()
                            .map(|u| format!("--system {u}"))
                    })
                    .unwrap_or_else(|| "<no unit declared — real run would refuse>".into());
                format!(
                    "dry-run: would install systemd drop-in for {unit} setting {var}={}",
                    dst.display()
                )
            }
            HomeDirDiscovery::Compile => {
                "dry-run: discovery=compile is not migratable (rejected at preflight)".into()
            }
        },
        RewriteOutcome::Deferred { reason } => format!("rewrite deferred: {reason}"),
    };
    record(Stage::Rewrite, &rewrite_detail, &mut events);

    // Stage 6: resume — bring back up whatever Quiesce stopped. Same
    // [`Quiescer`] handles both ends. Dry-run still walks this stage
    // but takes no system action.
    match quiescer.resume(&resume_action, dry_run) {
        Ok(()) => {
            let detail = match (&resume_action, dry_run) {
                (ResumeAction::StartUnit { scope, unit }, false) => {
                    format!("started systemd {scope} unit `{unit}`")
                }
                (ResumeAction::StartUnit { scope, unit }, true) => {
                    format!("dry-run: would start systemd {scope} unit `{unit}`")
                }
                (ResumeAction::None, _) => "no resume action declared; nothing to start".into(),
            };
            record(Stage::Resume, &detail, &mut events);
        }
        Err(e) => {
            // Resume failure is recoverable: undo Rewrite, clean up
            // dst, surface the error. The operator's daemons may be
            // left stopped (since Quiesce ran), and that's the
            // honest state to report.
            if let Err(undo_err) = undo_rewrite(&rewrite_outcome, env_writer) {
                record(
                    Stage::Resume,
                    &format!("Resume failed AND undo of Rewrite failed: {undo_err}"),
                    &mut events,
                );
            }
            cleanup_partial_copy(dst);
            return Err(e);
        }
    }

    // Stage 7: validate — optional post-rewrite health check. If the
    // layout declares a `validate.command`, run it (bounded by
    // `timeout_seconds`, default 10s) and expect exit 0. Anything
    // else means the migration didn't take and we should unwind.
    if dry_run {
        if layout.validate.command.is_empty() {
            record(
                Stage::Validate,
                "dry-run: no validate command declared",
                &mut events,
            );
        } else {
            record(
                Stage::Validate,
                &format!(
                    "dry-run: would run `{}` (timeout {}s)",
                    layout.validate.command.join(" "),
                    layout.validate.timeout_seconds.unwrap_or(10)
                ),
                &mut events,
            );
        }
    } else {
        match run_validate(&layout.validate) {
            Ok(ValidateOutcome::Skipped) => {
                record(
                    Stage::Validate,
                    "no validate command declared; skipping",
                    &mut events,
                );
            }
            Ok(ValidateOutcome::Ok { command, took_ms }) => {
                record(
                    Stage::Validate,
                    &format!("validate `{command}` exited 0 in {took_ms}ms"),
                    &mut events,
                );
            }
            Err(e) => {
                // Validate failed — full rollback. Stop the service
                // again so the operator isn't left with a half-broken
                // tool, undo the rewrite, clean up dst.
                let _ = quiescer.quiesce(layout, dry_run);
                if let Err(undo_err) = undo_rewrite(&rewrite_outcome, env_writer) {
                    record(
                        Stage::Validate,
                        &format!("Validate failed AND undo of Rewrite failed: {undo_err}"),
                        &mut events,
                    );
                } else {
                    record(
                        Stage::Validate,
                        &format!("validate failed: {e}; rolled back Rewrite"),
                        &mut events,
                    );
                }
                cleanup_partial_copy(dst);
                return Err(e);
            }
        }
    }

    // Stage 8: retain — preserve the source per the "never auto-delete"
    // rule. For Symlink discovery the Rewrite stage already moved the
    // original aside, so Retain is a no-op recording the existing
    // `moved_aside` path. For Config / Env, src is still a real
    // directory; rename it to `<src>.migrated-<unix>` so the tool
    // can never accidentally start reading the old copy.
    let retain_outcome = if dry_run {
        RetainOutcome::DryRunSkipped
    } else {
        retain_source(src, &rewrite_outcome)?
    };
    let retain_detail = match &retain_outcome {
        RetainOutcome::PreservedInRewrite { aside } => {
            format!(
                "source already preserved during Rewrite at {}",
                aside.display()
            )
        }
        RetainOutcome::RenamedAside { from, to } => format!(
            "renamed source {} → {} (operator-driven cleanup later)",
            from.display(),
            to.display()
        ),
        RetainOutcome::Nothing => "source preservation not applicable".into(),
        RetainOutcome::DryRunSkipped => "dry-run: would rename source aside".into(),
    };
    record(Stage::Retain, &retain_detail, &mut events);

    // Stage 9: done — terminal event so dashboards can stop polling.
    record(Stage::Done, "migration complete", &mut events);

    // For Config/Env discovery the source was renamed aside in Retain;
    // capture that path so `undo` can move it back. Symlink discovery
    // carries its moved-aside path inside `rewrite_outcome` instead, so
    // we leave `retained_at` None there to avoid double-restoring.
    let retained_at = match &retain_outcome {
        RetainOutcome::RenamedAside { to, .. } => Some(to.clone()),
        _ => None,
    };

    Ok(MigrationResult {
        tool_name: tool.name.clone(),
        src: src.to_path_buf(),
        dst: dst.to_path_buf(),
        dry_run,
        final_stage: Stage::Done,
        success: true,
        events,
        error: None,
        resume_action,
        rewrite_outcome: Some(rewrite_outcome),
        retained_at,
    })
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

    /// Fake env writer that records every install/uninstall in a
    /// shared scratch dir provided at construction. Lets tests inspect
    /// the exact drop-in contents without touching real systemd.
    struct FakeEnvWriter {
        scratch: PathBuf,
        installs: std::sync::Mutex<Vec<EnvOverrideRecord>>,
        uninstalls: std::sync::Mutex<Vec<EnvOverrideRecord>>,
    }

    impl FakeEnvWriter {
        fn new(scratch: PathBuf) -> Self {
            Self {
                scratch,
                installs: std::sync::Mutex::new(Vec::new()),
                uninstalls: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn installs(&self) -> Vec<EnvOverrideRecord> {
            self.installs.lock().unwrap().clone()
        }
        fn uninstalls(&self) -> Vec<EnvOverrideRecord> {
            self.uninstalls.lock().unwrap().clone()
        }
    }

    impl EnvWriter for FakeEnvWriter {
        fn install(
            &self,
            layout: &crate::tools::HomeDirLayout,
            env_var: &str,
            value: &str,
            dry_run: bool,
        ) -> Result<Option<EnvOverrideRecord>, MigrateError> {
            if dry_run {
                return Ok(None);
            }
            // Mirror SystemdEnvWriter's unit-pick logic but write into
            // a scratch dir so tests can inspect the file.
            let (scope, unit) = pick_unit(layout)?;
            let drop_in_dir = self.scratch.join(format!("{scope}-{unit}.d"));
            std::fs::create_dir_all(&drop_in_dir)?;
            let drop_in_path = drop_in_dir.join("sessionguard-migrate.conf");
            if drop_in_path.exists() {
                return Err(MigrateError::StageFailed(
                    Stage::Rewrite,
                    format!("fake drop-in already exists at {}", drop_in_path.display()),
                ));
            }
            std::fs::write(
                &drop_in_path,
                format!("[Service]\nEnvironment={env_var}={value}\n"),
            )?;
            let record = EnvOverrideRecord {
                scope,
                unit,
                drop_in_path,
                env_var: env_var.into(),
                value: value.into(),
            };
            self.installs.lock().unwrap().push(record.clone());
            Ok(Some(record))
        }

        fn uninstall(&self, record: &EnvOverrideRecord, dry_run: bool) -> Result<(), MigrateError> {
            if dry_run {
                return Ok(());
            }
            if record.drop_in_path.exists() {
                std::fs::remove_file(&record.drop_in_path)?;
            }
            self.uninstalls.lock().unwrap().push(record.clone());
            Ok(())
        }
    }

    /// Convenience constructor: a FakeEnvWriter rooted in a temp dir
    /// the test owns. Returns (writer, tempdir-handle) so the dir
    /// outlives the writer.
    fn fake_env_writer() -> (FakeEnvWriter, TempDir) {
        let dir = TempDir::new().unwrap();
        let w = FakeEnvWriter::new(dir.path().to_path_buf());
        (w, dir)
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

    // (Removed `refuses_real_run_until_mutating_stages_land` — the
    // NotYetMutating gate is gone now that Resume + Validate + Retain
    // are wired. Real migrations run end-to-end. See the per-discovery
    // end-to-end success tests below.)

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
        // With Resume/Validate/Retain wired (step 7), dry-run now
        // walks to the terminal Stage::Done.
        assert_eq!(result.final_stage, Stage::Done);
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
                Stage::Resume,
                Stage::Validate,
                Stage::Retain,
                Stage::Done,
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
        // After step 7, dry-run walks Resume too — so the fake sees
        // quiesce + resume = 2 calls, both flagged dry_run=true.
        assert_eq!(calls.len(), 2, "quiesce+resume called once each: {calls:?}");
        assert!(calls[0].contains("quiesce(dry_run=true"));
        assert!(calls[1].contains("resume(") && calls[1].contains("dry_run=true"));
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

    #[cfg(unix)]
    #[test]
    fn migrate_real_run_symlink_discovery_completes_end_to_end() {
        // With Resume/Validate/Retain wired (step 7) and the gate gone,
        // a real (non-dry-run) call now COMPLETES instead of unwinding.
        // The dst is populated, the canonical path is a symlink, the
        // original src has been preserved at `<src>.migrated-<ts>`,
        // and the final stage is Done.
        let t = tool(HomeDirDiscovery::Symlink, None, None);
        // Own a parent dir explicitly so the .migrated-* sidecar scan
        // doesn't pick up siblings from concurrent tests in /tmp.
        let parent = TempDir::new().unwrap();
        let src_path = parent.path().join("src-dir");
        std::fs::create_dir_all(&src_path).unwrap();
        std::fs::write(src_path.join("a.txt"), b"first").unwrap();
        std::fs::create_dir_all(src_path.join("nested")).unwrap();
        std::fs::write(src_path.join("nested/b.bin"), [0u8; 32]).unwrap();
        let dst = parent.path().join("new-home");

        let result = migrate_with(&t, &src_path, &dst, false, &FakeQuiescer::default()).unwrap();
        assert!(result.success);
        assert_eq!(result.final_stage, Stage::Done);

        // dst was populated and survives
        assert!(dst.join("a.txt").exists());
        assert!(dst.join("nested/b.bin").exists());

        // canonical src is now a symlink to dst
        let meta = std::fs::symlink_metadata(&src_path).unwrap();
        assert!(meta.file_type().is_symlink());
        let link_target = std::fs::read_link(&src_path).unwrap();
        assert_eq!(link_target, dst);

        // The `.migrated-*` sidecar from Rewrite still exists — design
        // doc says we never auto-delete.
        let sidecars: Vec<_> = std::fs::read_dir(parent.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|s| s.starts_with("src-dir.migrated-"))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(
            sidecars.len(),
            1,
            "exactly one .migrated-* sidecar should remain (never auto-deleted)"
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

        let (ew, _scratch) = fake_env_writer();
        undo_rewrite(&outcome, &ew).unwrap();

        // canonical is a real directory again, with its original file
        let meta = std::fs::symlink_metadata(&canonical).unwrap();
        assert!(meta.is_dir() && !meta.file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(canonical.join("a")).unwrap(), "v1");
    }

    #[test]
    fn migrate_refuses_env_discovery_when_no_unit_declared() {
        // discovery=Env with `env_var` set but NO systemd unit declared
        // → preflight/copy/verify succeed, then Rewrite refuses loudly.
        // Operator gets actionable instructions, not silent dotfile edit.
        let t = tool(HomeDirDiscovery::Env, Some("TEST_HOME"), None);
        let src = TempDir::new().unwrap();
        populate(src.path(), &[("a.txt", b"hello")]);
        let dst_dir = TempDir::new().unwrap();
        let dst = dst_dir.path().join("new");

        let (ew, _scratch) = fake_env_writer();
        let err = migrate_with_backends(&t, src.path(), &dst, false, &FakeQuiescer::default(), &ew)
            .unwrap_err();
        match err {
            MigrateError::StageFailed(Stage::Rewrite, msg) => {
                assert!(
                    msg.contains("requires `quiesce.systemd_user_unit`"),
                    "expected unit-required refusal, got: {msg}"
                );
            }
            other => panic!("expected StageFailed(Rewrite, ...), got {other:?}"),
        }
        // Rollback removed the copied dst:
        assert!(!dst.exists(), "rollback must remove dst on Env refusal");
    }

    // ── New in step 6b-config: Config discovery via reconciler adapters

    /// Build a Config-discovery tool whose data-dir is named in a JSON
    /// config file. Returns (tool, config_file_path).
    fn config_tool_json(config_path: &Path, field: &str, data_dir: &str) -> ToolDefinition {
        ToolDefinition {
            name: "configtool".into(),
            display_name: "Config Tool".into(),
            session_patterns: vec![],
            path_fields: vec![],
            on_move: ReconcileStrategy::Notify,
            version: None,
            binary: None,
            home_dir_layout: Some(HomeDirLayout {
                default_path: data_dir.to_string(),
                discovery: HomeDirDiscovery::Config,
                env_var: None,
                config_files: vec![crate::tools::HomeDirConfigFile {
                    file: config_path.display().to_string(),
                    field: field.into(),
                    format: "json".into(),
                }],
                quiesce: HomeDirQuiesce::default(),
                validate: HomeDirValidate::default(),
            }),
        }
    }

    #[test]
    fn rewrite_via_config_edits_field_and_backs_up_original() {
        let tmp = TempDir::new().unwrap();
        let data_old = tmp.path().join("data-old");
        let data_new = tmp.path().join("data-new");
        let cfg = tmp.path().join("config.json");
        std::fs::create_dir_all(&data_old).unwrap();
        std::fs::create_dir_all(&data_new).unwrap();
        std::fs::write(
            &cfg,
            format!(
                r#"{{"data_dir": "{}", "other": "untouched"}}"#,
                data_old.display()
            ),
        )
        .unwrap();

        let cf = crate::tools::HomeDirConfigFile {
            file: cfg.display().to_string(),
            field: "data_dir".into(),
            format: "json".into(),
        };
        let outcome = rewrite_via_config(&[cf], &data_old, &data_new).unwrap();

        match &outcome {
            RewriteOutcome::ConfigEdited { backups } => {
                assert_eq!(backups.len(), 1);
                assert_eq!(backups[0].original, cfg);
                assert!(backups[0].backup.exists(), "backup should be on disk");
                // Backup content == pre-rewrite original
                let backup_content = std::fs::read_to_string(&backups[0].backup).unwrap();
                assert!(backup_content.contains(&data_old.display().to_string()));
            }
            other => panic!("expected ConfigEdited, got {other:?}"),
        }

        // The live config now names the NEW dir.
        let live: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(live["data_dir"], data_new.display().to_string());
        assert_eq!(live["other"], "untouched");
    }

    #[test]
    fn rewrite_via_config_fails_loud_when_field_missing() {
        let tmp = TempDir::new().unwrap();
        let data_old = tmp.path().join("data-old");
        let data_new = tmp.path().join("data-new");
        let cfg = tmp.path().join("config.json");
        std::fs::create_dir_all(&data_old).unwrap();
        std::fs::create_dir_all(&data_new).unwrap();
        // Field exists but doesn't contain the src path → no-op rewrite.
        std::fs::write(&cfg, r#"{"data_dir": "/some/unrelated/path"}"#).unwrap();

        let cf = crate::tools::HomeDirConfigFile {
            file: cfg.display().to_string(),
            field: "data_dir".into(),
            format: "json".into(),
        };
        let err = rewrite_via_config(&[cf], &data_old, &data_new).unwrap_err();
        match err {
            MigrateError::StageFailed(Stage::Rewrite, msg) => {
                assert!(
                    msg.contains("did not contain"),
                    "expected 'did not contain' refusal, got: {msg}"
                );
            }
            other => panic!("expected StageFailed(Rewrite,...), got {other:?}"),
        }

        // Live config untouched, no orphan backups
        let live = std::fs::read_to_string(&cfg).unwrap();
        assert!(live.contains("/some/unrelated/path"));
        let siblings: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|s| s.contains(".sessionguard-backup-"))
                    .unwrap_or(false)
            })
            .collect();
        assert!(
            siblings.is_empty(),
            "no backup sidecars must remain on failure"
        );
    }

    #[test]
    fn rewrite_via_config_restores_earlier_files_when_later_one_fails() {
        // Two config files declared. First one rewrites cleanly; second
        // is missing → whole op should fail and the first file must be
        // restored to its pre-rewrite state.
        let tmp = TempDir::new().unwrap();
        let data_old = tmp.path().join("data-old");
        let data_new = tmp.path().join("data-new");
        std::fs::create_dir_all(&data_old).unwrap();
        std::fs::create_dir_all(&data_new).unwrap();

        let cfg1 = tmp.path().join("first.json");
        let original1 = format!(r#"{{"data_dir": "{}"}}"#, data_old.display());
        std::fs::write(&cfg1, &original1).unwrap();

        let cfg2_missing = tmp.path().join("does-not-exist.json");

        let cfs = vec![
            crate::tools::HomeDirConfigFile {
                file: cfg1.display().to_string(),
                field: "data_dir".into(),
                format: "json".into(),
            },
            crate::tools::HomeDirConfigFile {
                file: cfg2_missing.display().to_string(),
                field: "data_dir".into(),
                format: "json".into(),
            },
        ];

        let err = rewrite_via_config(&cfs, &data_old, &data_new).unwrap_err();
        assert!(matches!(err, MigrateError::StageFailed(Stage::Rewrite, _)));

        // cfg1 must be restored: contents back to the OLD path.
        let restored = std::fs::read_to_string(&cfg1).unwrap();
        let v: serde_json::Value = serde_json::from_str(&restored).unwrap();
        assert_eq!(
            v["data_dir"],
            data_old.display().to_string(),
            "first file must be restored to pre-rewrite state"
        );

        // No backup sidecars left dangling
        let siblings: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|s| s.contains(".sessionguard-backup-"))
                    .unwrap_or(false)
            })
            .collect();
        assert!(
            siblings.is_empty(),
            "no backup sidecars must remain after rollback"
        );
    }

    #[test]
    fn undo_rewrite_restores_config_backups() {
        let tmp = TempDir::new().unwrap();
        let data_old = tmp.path().join("data-old");
        let data_new = tmp.path().join("data-new");
        let cfg = tmp.path().join("config.json");
        std::fs::create_dir_all(&data_old).unwrap();
        std::fs::create_dir_all(&data_new).unwrap();
        let original = format!(r#"{{"data_dir": "{}"}}"#, data_old.display());
        std::fs::write(&cfg, &original).unwrap();

        let cf = crate::tools::HomeDirConfigFile {
            file: cfg.display().to_string(),
            field: "data_dir".into(),
            format: "json".into(),
        };
        let outcome = rewrite_via_config(&[cf], &data_old, &data_new).unwrap();

        // Sanity: live config now names the new dir.
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["data_dir"], data_new.display().to_string());

        // Undo restores to the pre-rewrite state.
        let (ew, _scratch) = fake_env_writer();
        undo_rewrite(&outcome, &ew).unwrap();
        let restored = std::fs::read_to_string(&cfg).unwrap();
        let v: serde_json::Value = serde_json::from_str(&restored).unwrap();
        assert_eq!(v["data_dir"], data_old.display().to_string());

        // Backup sidecar consumed by the rename
        let siblings: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|s| s.contains(".sessionguard-backup-"))
                    .unwrap_or(false)
            })
            .collect();
        assert!(
            siblings.is_empty(),
            "undo should consume the backup sidecar"
        );
    }

    #[test]
    fn migrate_with_config_discovery_completes_end_to_end() {
        // End-to-end: real (non-dry) migrate with discovery=Config.
        // After step 7 the driver runs to completion: config rewritten,
        // src renamed aside by Retain, dst populated, final_stage=Done.
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("data-src");
        let dst = tmp.path().join("data-dst");
        let cfg = tmp.path().join("tool.json");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a"), b"hello").unwrap();
        let original = format!(r#"{{"data_dir": "{}"}}"#, src.display());
        std::fs::write(&cfg, &original).unwrap();

        let t = config_tool_json(&cfg, "data_dir", &src.display().to_string());
        let result = migrate_with(&t, &src, &dst, false, &FakeQuiescer::default()).unwrap();
        assert!(result.success);
        assert_eq!(result.final_stage, Stage::Done);

        // dst populated
        assert!(dst.join("a").exists());
        // config file points at NEW location now
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["data_dir"], dst.display().to_string());
        // src renamed aside (Retain stage)
        assert!(!src.exists(), "src should be renamed aside by Retain");
        let sidecars: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|s| s.starts_with("data-src.migrated-"))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(
            sidecars.len(),
            1,
            "exactly one src sidecar should remain (never auto-deleted)"
        );
        // The reconciler's backup of the config file was consumed when
        // the rewrite succeeded — i.e. it survives because no rollback
        // happened. Confirm at least one .sessionguard-backup-* exists.
        let backups: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|s| s.contains(".sessionguard-backup-"))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(
            backups.len(),
            1,
            "config backup sidecar should remain (kept for undo)"
        );
    }

    #[test]
    fn migrate_config_discovery_refuses_when_no_config_files_declared() {
        // discovery = Config but config_files is empty → preflight ok
        // (read-only), Quiesce ok, Copy ok, Verify ok, then Rewrite
        // refuses loudly. Tests the "misconfigured layout" guard.
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a"), b"x").unwrap();

        let t = ToolDefinition {
            name: "noconfigfiles".into(),
            display_name: "No Config Files".into(),
            session_patterns: vec![],
            path_fields: vec![],
            on_move: ReconcileStrategy::Notify,
            version: None,
            binary: None,
            home_dir_layout: Some(HomeDirLayout {
                default_path: src.display().to_string(),
                discovery: HomeDirDiscovery::Config,
                env_var: None,
                config_files: vec![], // empty!
                quiesce: HomeDirQuiesce::default(),
                validate: HomeDirValidate::default(),
            }),
        };

        let err = migrate_with(&t, &src, &dst, false, &FakeQuiescer::default()).unwrap_err();
        match err {
            MigrateError::StageFailed(Stage::Rewrite, msg) => {
                assert!(
                    msg.contains("no config_files declared"),
                    "expected empty-config_files refusal, got: {msg}"
                );
            }
            other => panic!("expected StageFailed(Rewrite,...), got {other:?}"),
        }
        assert!(!dst.exists(), "rollback must remove orphan dst");
    }

    // ── New in step 6b-env: Env discovery via systemd drop-in ────────────

    #[test]
    fn rewrite_via_env_writes_drop_in_with_correct_contents() {
        // Direct unit test of the env rewrite helper. Layout has
        // user-scope unit + env_var declared; the fake env writer
        // captures the install and lets us inspect the on-disk
        // drop-in file.
        let dst = TempDir::new().unwrap();
        let layout = HomeDirLayout {
            default_path: "/ignored".into(),
            discovery: HomeDirDiscovery::Env,
            env_var: Some("OPENCODE_HOME".into()),
            config_files: vec![],
            quiesce: HomeDirQuiesce {
                systemd_user_unit: Some("opencode.service".into()),
                systemd_system_unit: None,
            },
            validate: HomeDirValidate::default(),
        };
        let (ew, _scratch) = fake_env_writer();
        let outcome = rewrite_via_env(&layout, dst.path(), &ew).unwrap();

        match &outcome {
            RewriteOutcome::EnvOverridden { record } => {
                assert_eq!(record.scope, "user");
                assert_eq!(record.unit, "opencode.service");
                assert_eq!(record.env_var, "OPENCODE_HOME");
                assert_eq!(record.value, dst.path().display().to_string());
                let body = std::fs::read_to_string(&record.drop_in_path).unwrap();
                assert!(body.contains("[Service]"));
                assert!(body.contains(&format!(
                    "Environment=OPENCODE_HOME={}",
                    dst.path().display()
                )));
            }
            other => panic!("expected EnvOverridden, got {other:?}"),
        }

        let installs = ew.installs();
        assert_eq!(installs.len(), 1, "fake should have recorded one install");
    }

    #[test]
    fn rewrite_via_env_prefers_user_over_system_scope() {
        // Both scopes declared; user wins (cheaper, no sudo).
        let dst = TempDir::new().unwrap();
        let layout = HomeDirLayout {
            default_path: "/ignored".into(),
            discovery: HomeDirDiscovery::Env,
            env_var: Some("FOO".into()),
            config_files: vec![],
            quiesce: HomeDirQuiesce {
                systemd_user_unit: Some("user.service".into()),
                systemd_system_unit: Some("system.service".into()),
            },
            validate: HomeDirValidate::default(),
        };
        let (ew, _scratch) = fake_env_writer();
        let outcome = rewrite_via_env(&layout, dst.path(), &ew).unwrap();
        if let RewriteOutcome::EnvOverridden { record } = outcome {
            assert_eq!(record.scope, "user");
            assert_eq!(record.unit, "user.service");
        } else {
            panic!("expected EnvOverridden");
        }
    }

    #[test]
    fn rewrite_via_env_falls_back_to_system_when_only_system_declared() {
        let dst = TempDir::new().unwrap();
        let layout = HomeDirLayout {
            default_path: "/ignored".into(),
            discovery: HomeDirDiscovery::Env,
            env_var: Some("FOO".into()),
            config_files: vec![],
            quiesce: HomeDirQuiesce {
                systemd_user_unit: None,
                systemd_system_unit: Some("nginx.service".into()),
            },
            validate: HomeDirValidate::default(),
        };
        let (ew, _scratch) = fake_env_writer();
        let outcome = rewrite_via_env(&layout, dst.path(), &ew).unwrap();
        if let RewriteOutcome::EnvOverridden { record } = outcome {
            assert_eq!(record.scope, "system");
            assert_eq!(record.unit, "nginx.service");
        } else {
            panic!("expected EnvOverridden");
        }
    }

    #[test]
    fn rewrite_via_env_refuses_when_env_var_missing() {
        // Layout declares discovery=Env but no env_var → caller bug,
        // not operator bug. Refuse loudly.
        let dst = TempDir::new().unwrap();
        let layout = HomeDirLayout {
            default_path: "/ignored".into(),
            discovery: HomeDirDiscovery::Env,
            env_var: None,
            config_files: vec![],
            quiesce: HomeDirQuiesce {
                systemd_user_unit: Some("x.service".into()),
                systemd_system_unit: None,
            },
            validate: HomeDirValidate::default(),
        };
        let (ew, _scratch) = fake_env_writer();
        let err = rewrite_via_env(&layout, dst.path(), &ew).unwrap_err();
        match err {
            MigrateError::StageFailed(Stage::Rewrite, msg) => {
                assert!(
                    msg.contains("`env_var` is not declared"),
                    "expected env_var-missing message, got: {msg}"
                );
            }
            other => panic!("expected StageFailed, got {other:?}"),
        }
    }

    #[test]
    fn undo_rewrite_removes_env_drop_in() {
        let dst = TempDir::new().unwrap();
        let layout = HomeDirLayout {
            default_path: "/ignored".into(),
            discovery: HomeDirDiscovery::Env,
            env_var: Some("BAR".into()),
            config_files: vec![],
            quiesce: HomeDirQuiesce {
                systemd_user_unit: Some("bar.service".into()),
                systemd_system_unit: None,
            },
            validate: HomeDirValidate::default(),
        };
        let (ew, _scratch) = fake_env_writer();
        let outcome = rewrite_via_env(&layout, dst.path(), &ew).unwrap();
        let drop_in = if let RewriteOutcome::EnvOverridden { ref record } = outcome {
            record.drop_in_path.clone()
        } else {
            panic!("expected EnvOverridden");
        };
        assert!(drop_in.exists(), "drop-in should exist before undo");

        undo_rewrite(&outcome, &ew).unwrap();
        assert!(!drop_in.exists(), "drop-in should be removed by undo");
        assert_eq!(ew.uninstalls().len(), 1, "fake should record one uninstall");
    }

    #[test]
    fn migrate_with_env_discovery_completes_end_to_end() {
        // End-to-end: real (non-dry) migrate with discovery=Env.
        // After step 7 the driver runs to completion: drop-in stays,
        // src renamed aside, dst populated, final_stage=Done.
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a"), b"hi").unwrap();

        let t = tool(
            HomeDirDiscovery::Env,
            Some("MYTOOL_HOME"),
            Some("mytool.service"),
        );

        let (ew, _scratch) = fake_env_writer();
        let result =
            migrate_with_backends(&t, &src, &dst, false, &FakeQuiescer::default(), &ew).unwrap();
        assert!(result.success);
        assert_eq!(result.final_stage, Stage::Done);

        // dst populated
        assert!(dst.join("a").exists());
        // drop-in survives (no rollback)
        assert_eq!(ew.installs().len(), 1, "one install during Rewrite");
        assert!(
            ew.uninstalls().is_empty(),
            "no uninstall on a successful run"
        );
        assert!(ew.installs()[0].drop_in_path.exists());
        // src renamed aside by Retain
        assert!(!src.exists());
        let sidecars: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|s| s.starts_with("src.migrated-"))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(sidecars.len(), 1, "exactly one src sidecar should remain");
    }

    // ── New in step 7: Resume / Validate / Retain stages

    #[test]
    fn validate_runs_command_and_succeeds_on_exit_zero() {
        let v = crate::tools::HomeDirValidate {
            command: vec!["true".into()],
            timeout_seconds: Some(5),
        };
        let outcome = run_validate(&v).unwrap();
        match outcome {
            ValidateOutcome::Ok { command, .. } => assert_eq!(command, "true"),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn validate_fails_on_nonzero_exit() {
        let v = crate::tools::HomeDirValidate {
            command: vec!["false".into()],
            timeout_seconds: Some(5),
        };
        let err = run_validate(&v).unwrap_err();
        match err {
            MigrateError::StageFailed(Stage::Validate, msg) => {
                assert!(msg.contains("exited"));
            }
            other => panic!("expected StageFailed(Validate, ...), got {other:?}"),
        }
    }

    #[test]
    fn validate_times_out_on_long_running_command() {
        // sleep 30s but timeout at 1s — should kill + report timeout.
        let v = crate::tools::HomeDirValidate {
            command: vec!["sleep".into(), "30".into()],
            timeout_seconds: Some(1),
        };
        let start = std::time::Instant::now();
        let err = run_validate(&v).unwrap_err();
        let elapsed = start.elapsed().as_secs();
        assert!(
            elapsed < 5,
            "should kill within timeout window, took {elapsed}s"
        );
        match err {
            MigrateError::StageFailed(Stage::Validate, msg) => {
                assert!(msg.contains("timed out"), "got: {msg}");
            }
            other => panic!("expected StageFailed timeout, got {other:?}"),
        }
    }

    #[test]
    fn validate_skipped_when_no_command_declared() {
        let v = crate::tools::HomeDirValidate::default();
        let outcome = run_validate(&v).unwrap();
        assert_eq!(outcome, ValidateOutcome::Skipped);
    }

    #[test]
    fn validate_failure_rolls_back_full_migration() {
        // Build a Symlink-discovery tool with a validate command that
        // fails. The driver should run all the way through Rewrite +
        // Resume, hit Validate, fail it, and roll back: dst gone,
        // symlink replaced by original directory, no .migrated- sidecars.
        let parent = TempDir::new().unwrap();
        let src = parent.path().join("src");
        let dst = parent.path().join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a"), b"x").unwrap();

        let mut t = tool(HomeDirDiscovery::Symlink, None, None);
        if let Some(ref mut layout) = t.home_dir_layout {
            layout.validate = crate::tools::HomeDirValidate {
                command: vec!["false".into()],
                timeout_seconds: Some(5),
            };
        }

        let err = migrate_with(&t, &src, &dst, false, &FakeQuiescer::default()).unwrap_err();
        assert!(
            matches!(err, MigrateError::StageFailed(Stage::Validate, _)),
            "got {err:?}"
        );

        // dst removed
        assert!(!dst.exists(), "rollback must remove dst on Validate fail");
        // src is a real dir again (Rewrite-undo renamed sidecar back)
        let meta = std::fs::symlink_metadata(&src).unwrap();
        assert!(meta.is_dir() && !meta.file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(src.join("a")).unwrap(), "x");
        // No leftover .migrated-* sidecars
        let sidecars: Vec<_> = std::fs::read_dir(parent.path())
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
            sidecars.is_empty(),
            "Validate-failure rollback must remove .migrated-* sidecars: {:?}",
            sidecars.iter().map(|e| e.file_name()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn retain_source_renames_src_for_config_discovery() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("data");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a"), b"v").unwrap();
        // Pretend Rewrite did Config-edits, not symlink-move. Construct
        // a corresponding outcome with no `moved_aside`.
        let outcome = RewriteOutcome::ConfigEdited { backups: vec![] };
        let r = retain_source(&src, &outcome).unwrap();
        match r {
            RetainOutcome::RenamedAside { from, to } => {
                assert_eq!(from, src);
                assert!(to.exists(), "renamed-aside path should exist");
                assert!(!src.exists(), "original src should be gone");
                assert!(to
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .contains(".migrated-"));
            }
            other => panic!("expected RenamedAside, got {other:?}"),
        }
    }

    #[test]
    fn retain_source_is_noop_when_symlink_rewrite_already_preserved() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("data");
        let aside = tmp.path().join("data.migrated-99");
        std::fs::create_dir_all(&aside).unwrap();
        let outcome = RewriteOutcome::SymlinkInstalled {
            canonical: src.clone(),
            target: tmp.path().join("dst"),
            moved_aside: Some(aside.clone()),
        };
        let r = retain_source(&src, &outcome).unwrap();
        match r {
            RetainOutcome::PreservedInRewrite { aside: a } => assert_eq!(a, aside),
            other => panic!("expected PreservedInRewrite, got {other:?}"),
        }
    }

    // ── Dry-run rewrite-detail strings are discovery-aware ──────────────

    fn dry_run_rewrite_detail(t: &ToolDefinition) -> String {
        let src = TempDir::new().unwrap();
        populate(src.path(), &[("a", b"x")]);
        let dst_parent = TempDir::new().unwrap();
        let dst = dst_parent.path().join("dst");
        let (ew, _scratch) = fake_env_writer();
        let result =
            migrate_with_backends(t, src.path(), &dst, true, &FakeQuiescer::default(), &ew)
                .unwrap();
        result
            .events
            .iter()
            .find(|e| e.stage == Stage::Rewrite)
            .expect("rewrite event present")
            .detail
            .clone()
    }

    #[test]
    fn dry_run_detail_for_symlink_discovery_mentions_symlink() {
        let t = tool(HomeDirDiscovery::Symlink, None, None);
        let d = dry_run_rewrite_detail(&t);
        assert!(
            d.contains("would install symlink") && d.contains(" -> "),
            "got: {d}"
        );
    }

    #[test]
    fn dry_run_detail_for_config_discovery_mentions_config_files() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("c.json");
        std::fs::write(&cfg, "{}").unwrap();
        let t = config_tool_json(&cfg, "data_dir", "/old");
        let d = dry_run_rewrite_detail(&t);
        assert!(
            d.contains("would rewrite") && d.contains("config file"),
            "got: {d}"
        );
        assert!(d.contains("data_dir"), "should name the field: {d}");
    }

    #[test]
    fn dry_run_detail_for_env_discovery_mentions_systemd_drop_in() {
        let t = tool(
            HomeDirDiscovery::Env,
            Some("MYTOOL_HOME"),
            Some("mytool.service"),
        );
        let d = dry_run_rewrite_detail(&t);
        assert!(d.contains("systemd drop-in"), "got: {d}");
        assert!(d.contains("--user mytool.service"), "got: {d}");
        assert!(d.contains("MYTOOL_HOME="), "got: {d}");
    }

    #[test]
    fn dry_run_detail_for_env_discovery_without_unit_warns_real_run_would_refuse() {
        // Tool with discovery=Env but no systemd unit declared — dry-run
        // should call out that a real run would refuse, so the operator
        // doesn't think this is ready to ship.
        let t = tool(HomeDirDiscovery::Env, Some("X_HOME"), None);
        let d = dry_run_rewrite_detail(&t);
        assert!(d.contains("no unit declared"), "got: {d}");
        assert!(d.contains("would refuse"), "got: {d}");
    }

    // ── Undo of a completed migration ────────────────────────────────

    #[test]
    fn undo_plan_is_none_for_dry_run() {
        let t = tool(HomeDirDiscovery::Symlink, None, None);
        let parent = TempDir::new().unwrap();
        let src = parent.path().join("src-dir");
        std::fs::create_dir_all(&src).unwrap();
        let dst = parent.path().join("new-home");
        let q = FakeQuiescer::default();
        let (ew, _s) = fake_env_writer();
        let result = migrate_with_backends(&t, &src, &dst, true, &q, &ew).unwrap();
        assert!(
            result.undo_plan().is_none(),
            "a dry-run migration is not reversible"
        );
    }

    #[cfg(unix)]
    #[test]
    fn undo_migration_symlink_round_trips() {
        let t = tool(HomeDirDiscovery::Symlink, None, None);
        let parent = TempDir::new().unwrap();
        let src = parent.path().join("src-dir");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), b"payload").unwrap();
        let dst = parent.path().join("new-home");

        let q = FakeQuiescer::default();
        let (ew, _scratch) = fake_env_writer();
        let result = migrate_with_backends(&t, &src, &dst, false, &q, &ew).unwrap();
        assert!(result.success);
        // Sanity: src is a symlink to dst after a successful migrate.
        assert!(std::fs::symlink_metadata(&src)
            .unwrap()
            .file_type()
            .is_symlink());

        let plan = result
            .undo_plan()
            .expect("a successful real migration should be reversible");
        let layout = t.home_dir_layout.as_ref().unwrap();
        let report = undo_migration(&plan, layout, &q, &ew, false).unwrap();
        assert!(!report.dry_run);

        // src is a real directory again with original content; the
        // orphaned copy at dst is removed.
        let meta = std::fs::symlink_metadata(&src).unwrap();
        assert!(meta.is_dir() && !meta.file_type().is_symlink());
        assert_eq!(
            std::fs::read_to_string(src.join("a.txt")).unwrap(),
            "payload"
        );
        assert!(!dst.exists(), "copy at dst should be removed by undo");
    }

    #[test]
    fn undo_migration_config_round_trips() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("data-src");
        let dst = tmp.path().join("data-dst");
        let cfg = tmp.path().join("tool.json");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a"), b"hello").unwrap();
        std::fs::write(&cfg, format!(r#"{{"data_dir": "{}"}}"#, src.display())).unwrap();

        let t = config_tool_json(&cfg, "data_dir", &src.display().to_string());
        let q = FakeQuiescer::default();
        let (ew, _scratch) = fake_env_writer();
        let result = migrate_with_backends(&t, &src, &dst, false, &q, &ew).unwrap();
        assert!(result.success);
        assert!(!src.exists(), "src renamed aside by Retain after migrate");

        let plan = result.undo_plan().expect("config migration is reversible");
        let layout = t.home_dir_layout.as_ref().unwrap();
        undo_migration(&plan, layout, &q, &ew, false).unwrap();

        // src restored from the .migrated sidecar with original content.
        assert!(src.exists(), "src should be restored by undo");
        assert_eq!(std::fs::read_to_string(src.join("a")).unwrap(), "hello");
        // config points back to the original src.
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["data_dir"], src.display().to_string());
        // the copy at dst is removed.
        assert!(!dst.exists(), "copy at dst should be removed by undo");
    }

    #[cfg(unix)]
    #[test]
    fn undo_migration_dry_run_takes_no_action() {
        let t = tool(HomeDirDiscovery::Symlink, None, None);
        let parent = TempDir::new().unwrap();
        let src = parent.path().join("src-dir");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), b"payload").unwrap();
        let dst = parent.path().join("new-home");

        let q = FakeQuiescer::default();
        let (ew, _scratch) = fake_env_writer();
        let result = migrate_with_backends(&t, &src, &dst, false, &q, &ew).unwrap();
        let plan = result.undo_plan().unwrap();
        let layout = t.home_dir_layout.as_ref().unwrap();

        let report = undo_migration(&plan, layout, &q, &ew, true).unwrap();
        assert!(report.dry_run);
        // Nothing changed: src is still a symlink, dst still present.
        assert!(std::fs::symlink_metadata(&src)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(dst.exists(), "dry-run undo must not remove dst");
    }
}
