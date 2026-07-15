// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

use super::*;
use crate::tools::{
    HomeDirDiscovery, HomeDirLayout, HomeDirQuiesce, HomeDirValidate, ReconcileStrategy,
};
use std::fs;
use std::io::Write;
use tempfile::TempDir;

fn tool(discovery: HomeDirDiscovery, env_var: Option<&str>, unit: Option<&str>) -> ToolDefinition {
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
    let err = migrate(&t, src.path(), &src.path().join("does-not-exist-dst"), true).unwrap_err();
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

#[test]
fn unit_absent_failures_are_classified_benign() {
    // The cases that mean "this host doesn't run the tool under that
    // unit" — benign, migration proceeds.
    assert!(is_unit_absent_failure(Some(5), ""));
    assert!(is_unit_absent_failure(
        Some(1),
        "Failed to stop opencode.service: Unit opencode.service not loaded."
    ));
    assert!(is_unit_absent_failure(
        Some(1),
        "Unit codex.service not found."
    ));
    assert!(is_unit_absent_failure(
        None,
        "Unit file opencode.service does not exist."
    ));
    // case-insensitive
    assert!(is_unit_absent_failure(Some(1), "NO SUCH UNIT"));
}

#[test]
fn real_failures_are_not_classified_benign() {
    // These must still abort the migration — they are not "the unit
    // simply isn't here".
    assert!(!is_unit_absent_failure(
        Some(1),
        "Failed to stop opencode.service: Access denied"
    ));
    assert!(!is_unit_absent_failure(
        Some(1),
        "Failed to connect to bus: No such file or directory"
    ));
    assert!(!is_unit_absent_failure(None, "permission denied"));
}

#[test]
fn unit_absent_outcome_serialises_with_tagged_repr() {
    // Event log / dashboard consume this as JSON.
    let o = QuiesceOutcome::UnitAbsent {
        scope: "user".into(),
        unit: "opencode.service".into(),
    };
    let j = serde_json::to_value(&o).unwrap();
    assert_eq!(j["kind"], "unit_absent");
    assert_eq!(j["scope"], "user");
    assert_eq!(j["unit"], "opencode.service");
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

#[cfg(unix)]
#[test]
fn copy_tree_recreates_relative_symlink() {
    let src = TempDir::new().unwrap();
    populate(src.path(), &[("real.txt", b"data")]);
    // A relative symlink — recreated verbatim.
    std::os::unix::fs::symlink("real.txt", src.path().join("link.txt")).unwrap();

    let dst_dir = TempDir::new().unwrap();
    let dst = dst_dir.path().join("out");
    let summary = copy_tree(src.path(), &dst).unwrap();

    assert_eq!(summary.files_copied, 1, "only the regular file is a file");
    assert_eq!(summary.symlinks_copied, 1, "the symlink is recreated");
    let link = dst.join("link.txt");
    let meta = std::fs::symlink_metadata(&link).unwrap();
    assert!(meta.file_type().is_symlink(), "link must remain a symlink");
    assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("real.txt"));
    // It resolves to the copied file, not the original.
    assert_eq!(std::fs::read_to_string(&link).unwrap(), "data");
}

#[cfg(unix)]
#[test]
fn copy_tree_remaps_absolute_symlink_into_source() {
    let src = TempDir::new().unwrap();
    populate(src.path(), &[("real.txt", b"payload")]);
    // An ABSOLUTE symlink pointing into the source root — must be
    // remapped to the destination so it resolves post-migrate.
    std::os::unix::fs::symlink(src.path().join("real.txt"), src.path().join("abs.txt")).unwrap();

    let dst_dir = TempDir::new().unwrap();
    let dst = dst_dir.path().join("out");
    let summary = copy_tree(src.path(), &dst).unwrap();

    assert_eq!(summary.symlinks_copied, 1);
    let link = dst.join("abs.txt");
    let target = std::fs::read_link(&link).unwrap();
    assert_eq!(
        target,
        dst.join("real.txt"),
        "absolute target into src must be rebased onto dst"
    );
    assert_eq!(std::fs::read_to_string(&link).unwrap(), "payload");
}

#[cfg(unix)]
#[test]
fn copy_tree_recreates_symlinked_directory_and_dangling_link() {
    // The two cases the audit flagged as silently droppable.
    let src = TempDir::new().unwrap();
    std::fs::create_dir_all(src.path().join("realdir")).unwrap();
    std::fs::write(src.path().join("realdir/inner.txt"), b"x").unwrap();
    // symlink TO a directory (not followed — recreated as a link)
    std::os::unix::fs::symlink("realdir", src.path().join("dirlink")).unwrap();
    // dangling symlink (target doesn't exist)
    std::os::unix::fs::symlink("nowhere", src.path().join("dangling")).unwrap();

    let dst_dir = TempDir::new().unwrap();
    let dst = dst_dir.path().join("out");
    let summary = copy_tree(src.path(), &dst).unwrap();

    // realdir is copied as a dir (1 file inside); dirlink + dangling are links.
    assert_eq!(summary.files_copied, 1);
    assert_eq!(
        summary.symlinks_copied, 2,
        "dir-link and dangling recreated"
    );
    let dirlink = std::fs::symlink_metadata(dst.join("dirlink")).unwrap();
    assert!(dirlink.file_type().is_symlink(), "dir-link stays a symlink");
    assert_eq!(
        std::fs::read_link(dst.join("dirlink")).unwrap(),
        Path::new("realdir")
    );
    let dangling = std::fs::symlink_metadata(dst.join("dangling")).unwrap();
    assert!(dangling.file_type().is_symlink(), "dangling link recreated");
    assert!(!dst.join("dangling").exists(), "dangling stays dangling");

    // Verify must report a clean match (regular-file payload identical;
    // symlinks excluded from the size walk on both sides).
    let v = verify_copy(src.path(), &dst).unwrap();
    assert!(v.matches, "verify should match: {v:?}");
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
fn verify_copy_catches_same_total_swap() {
    // Two files whose sizes are swapped at dst: identical file COUNT and
    // identical TOTAL bytes — the old totals-only verify passed this. The
    // per-file manifest must fail it and name the offenders.
    let src = TempDir::new().unwrap();
    populate(src.path(), &[("a", b"12345"), ("b", b"xyz")]);
    let dst_dir = TempDir::new().unwrap();
    let dst = dst_dir.path().join("out");
    copy_tree(src.path(), &dst).unwrap();
    std::fs::write(dst.join("a"), b"xyz").unwrap();
    std::fs::write(dst.join("b"), b"12345").unwrap();

    let outcome = verify_copy(src.path(), &dst).unwrap();
    assert!(
        !outcome.matches,
        "per-file verify must catch a same-total size swap"
    );
    assert!(
        !outcome.mismatches.is_empty(),
        "mismatches should name the offending files"
    );
}

#[cfg(unix)]
#[test]
fn verify_copy_accepts_remapped_in_tree_symlink() {
    // copy_tree remaps an absolute in-tree symlink target from src → dst; the
    // literal targets differ on the two sides, but that IS the correct copy —
    // verify must not flag it.
    let src = TempDir::new().unwrap();
    populate(src.path(), &[("real.txt", b"hello")]);
    std::os::unix::fs::symlink(src.path().join("real.txt"), src.path().join("link.txt")).unwrap();
    let dst_dir = TempDir::new().unwrap();
    let dst = dst_dir.path().join("out");
    copy_tree(src.path(), &dst).unwrap();

    let outcome = verify_copy(src.path(), &dst).unwrap();
    assert!(
        outcome.matches,
        "remapped in-tree symlink is correct, not a mismatch: {:?}",
        outcome.mismatches
    );
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

#[test]
fn migrate_resumes_the_service_when_a_stage_aborts() {
    // A unit quiesced before Copy must be brought back up even if the migration
    // fails after Quiesce — otherwise the operator's service is left stopped.
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    populate(&src, &[("a.txt", b"hi")]);
    let dst = tmp.path().join("dst");

    // Config discovery whose config file is invalid JSON → Copy/Verify succeed,
    // then Rewrite fails. Declare a quiesce unit so Quiesce actually stops it.
    let bad_cfg = tmp.path().join("config.json");
    std::fs::write(&bad_cfg, "NOT VALID JSON {{{").unwrap();
    let mut t = config_tool_json(&bad_cfg, "data_dir", &src.display().to_string());
    t.home_dir_layout
        .as_mut()
        .unwrap()
        .quiesce
        .systemd_user_unit = Some("svc.service".into());

    let fake = FakeQuiescer::default();
    let (ew, _scratch) = fake_env_writer();
    let err = migrate_with_backends(&t, &src, &dst, false, &fake, &ew).unwrap_err();
    assert!(matches!(err, MigrateError::StageFailed(Stage::Rewrite, _)));

    let calls = fake.calls();
    assert!(
        calls.iter().any(|c| c.starts_with("quiesce")),
        "should have quiesced"
    );
    assert!(
        calls.iter().any(|c| c.starts_with("resume")),
        "must resume the unit after aborting; calls: {calls:?}"
    );
    assert!(!dst.exists(), "aborted migration should leave no dst");
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
        migrate_with_backends(t, src.path(), &dst, true, &FakeQuiescer::default(), &ew).unwrap();
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

// ── migrate-cleanup ──────────────────────────────────────────────

#[cfg(unix)]
#[test]
fn cleanup_symlink_removes_sidecar_but_keeps_live_data() {
    let t = tool(HomeDirDiscovery::Symlink, None, None);
    let parent = TempDir::new().unwrap();
    let src = parent.path().join("src-dir");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.txt"), b"payload-1234").unwrap();
    let dst = parent.path().join("new-home");

    let q = FakeQuiescer::default();
    let (ew, _s) = fake_env_writer();
    let result = migrate_with_backends(&t, &src, &dst, false, &q, &ew).unwrap();
    let plan = result.undo_plan().unwrap();

    // The preserved path is the moved-aside original.
    let preserved = plan.preserved_paths();
    assert_eq!(preserved.len(), 1);
    assert!(preserved[0].exists());

    // Dry-run reports a non-zero reclaim and removes nothing.
    let dry = cleanup_migration(&plan, true).unwrap();
    assert!(dry.dry_run);
    assert!(dry.total_bytes() > 0);
    assert!(preserved[0].exists(), "dry-run must not delete");

    // Live run deletes the sidecar; the symlink + dst stay intact.
    let report = cleanup_migration(&plan, false).unwrap();
    assert!(report.items.iter().all(|i| i.removed));
    assert!(!preserved[0].exists(), "sidecar should be gone");
    assert!(std::fs::symlink_metadata(&src)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(dst.join("a.txt").exists(), "live data must survive cleanup");
}

#[test]
fn cleanup_config_removes_sidecar_and_backups() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("data-src");
    let dst = tmp.path().join("data-dst");
    let cfg = tmp.path().join("tool.json");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a"), b"hi").unwrap();
    std::fs::write(&cfg, format!(r#"{{"data_dir": "{}"}}"#, src.display())).unwrap();

    let t = config_tool_json(&cfg, "data_dir", &src.display().to_string());
    let q = FakeQuiescer::default();
    let (ew, _s) = fake_env_writer();
    let result = migrate_with_backends(&t, &src, &dst, false, &q, &ew).unwrap();
    let plan = result.undo_plan().unwrap();

    // Preserved: the renamed-aside src sidecar + the config backup.
    let preserved = plan.preserved_paths();
    assert_eq!(preserved.len(), 2, "src sidecar + one config backup");
    assert!(preserved.iter().all(|p| p.exists()));

    cleanup_migration(&plan, false).unwrap();
    assert!(
        preserved.iter().all(|p| !p.exists()),
        "all preserved artifacts removed"
    );
    // The config file itself (live) is untouched and still points at dst.
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    assert_eq!(v["data_dir"], dst.display().to_string());
}

#[test]
fn cleanup_is_idempotent_when_already_gone() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("data-src");
    let dst = tmp.path().join("data-dst");
    let cfg = tmp.path().join("tool.json");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a"), b"hi").unwrap();
    std::fs::write(&cfg, format!(r#"{{"data_dir": "{}"}}"#, src.display())).unwrap();
    let t = config_tool_json(&cfg, "data_dir", &src.display().to_string());
    let q = FakeQuiescer::default();
    let (ew, _s) = fake_env_writer();
    let result = migrate_with_backends(&t, &src, &dst, false, &q, &ew).unwrap();
    let plan = result.undo_plan().unwrap();

    cleanup_migration(&plan, false).unwrap();
    // Second pass: everything already gone, reports existed=false, no error.
    let again = cleanup_migration(&plan, false).unwrap();
    assert!(again.items.iter().all(|i| !i.existed && !i.removed));
    assert_eq!(again.total_bytes(), 0);
}
