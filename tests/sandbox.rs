// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Sandbox integration tests.
//!
//! Creates realistic project structures with AI tool session artifacts
//! and exercises the full CLI against them.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Build a `sessionguard` command with an isolated data directory so tests
/// don't share (or pollute) the real user registry.
fn cmd(data_dir: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("sessionguard").unwrap();
    c.env("SESSIONGUARD_DATA_DIR", data_dir);
    c
}

/// Create a fake project with Claude Code artifacts.
fn create_claude_project(root: &std::path::Path, name: &str) -> std::path::PathBuf {
    let project = root.join(name);
    fs::create_dir_all(project.join(".claude")).unwrap();
    fs::write(
        project.join("CLAUDE.md"),
        "# CLAUDE.md\nThis project uses Rust.",
    )
    .unwrap();
    fs::write(project.join(".claudeignore"), "target/\n").unwrap();
    fs::write(
        project.join(".claude/settings.json"),
        format!(
            r#"{{"project_path": "{}","model": "opus"}}"#,
            project.display()
        ),
    )
    .unwrap();
    // Fake source file
    fs::write(project.join("main.rs"), "fn main() {}").unwrap();
    project
}

/// Create a fake project with Cursor artifacts.
fn create_cursor_project(root: &std::path::Path, name: &str) -> std::path::PathBuf {
    let project = root.join(name);
    fs::create_dir_all(project.join(".cursor/rules")).unwrap();
    fs::write(project.join(".cursorignore"), "node_modules/\n").unwrap();
    fs::write(
        project.join(".cursor/state.json"),
        format!(r#"{{"project_root": "{}"}}"#, project.display()),
    )
    .unwrap();
    fs::write(
        project.join(".cursor/rules/style.md"),
        "Use TypeScript strict mode.",
    )
    .unwrap();
    fs::write(project.join("index.ts"), "console.log('hello')").unwrap();
    project
}

/// Create a project with both Claude and Cursor artifacts.
fn create_multi_tool_project(root: &std::path::Path, name: &str) -> std::path::PathBuf {
    let project = create_claude_project(root, name);
    fs::create_dir_all(project.join(".cursor")).unwrap();
    fs::write(project.join(".cursorignore"), "target/\n").unwrap();
    fs::write(
        project.join(".cursor/state.json"),
        format!(r#"{{"project_root": "{}"}}"#, project.display()),
    )
    .unwrap();
    project
}

/// Create a project with no AI artifacts (control case).
fn create_plain_project(root: &std::path::Path, name: &str) -> std::path::PathBuf {
    let project = root.join(name);
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("README.md"), "# Plain Project").unwrap();
    project
}

#[test]
fn sandbox_scan_detects_tools() {
    let sandbox = TempDir::new().unwrap();
    create_claude_project(sandbox.path(), "my-rust-app");
    create_cursor_project(sandbox.path(), "my-ts-app");
    create_plain_project(sandbox.path(), "no-ai-project");

    cmd(sandbox.path())
        .args(["scan", &sandbox.path().to_string_lossy()])
        .assert()
        .success()
        .stdout(predicate::str::contains("my-rust-app"))
        .stdout(predicate::str::contains("Claude Code"))
        .stdout(predicate::str::contains("my-ts-app"))
        .stdout(predicate::str::contains("Cursor"))
        .stdout(predicate::str::is_match("no-ai-project").unwrap().not());
}

#[test]
fn sandbox_watch_registers_project() {
    let sandbox = TempDir::new().unwrap();
    let project = create_claude_project(sandbox.path(), "watched-project");

    cmd(sandbox.path())
        .args(["watch", &project.to_string_lossy()])
        .assert()
        .success()
        .stdout(predicate::str::contains("watching"))
        .stdout(predicate::str::contains("Claude Code"));
}

#[test]
fn sandbox_status_shows_watched() {
    let sandbox = TempDir::new().unwrap();
    let project = create_claude_project(sandbox.path(), "status-test");

    // Register it first
    cmd(sandbox.path())
        .args(["watch", &project.to_string_lossy()])
        .assert()
        .success();

    // Now check status
    cmd(sandbox.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("status-test"));
}

#[test]
fn sandbox_simulate_shows_affected_artifacts() {
    let sandbox = TempDir::new().unwrap();
    let project = create_claude_project(sandbox.path(), "sim-project");
    let dest = sandbox.path().join("sim-project-renamed");

    cmd(sandbox.path())
        .args([
            "simulate",
            "mv",
            &project.to_string_lossy(),
            &dest.to_string_lossy(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Claude Code"))
        .stdout(predicate::str::contains("would rewrite"));
}

#[test]
fn sandbox_simulate_no_artifacts() {
    let sandbox = TempDir::new().unwrap();
    let project = create_plain_project(sandbox.path(), "plain-project");
    let dest = sandbox.path().join("plain-renamed");

    cmd(sandbox.path())
        .args([
            "simulate",
            "mv",
            &project.to_string_lossy(),
            &dest.to_string_lossy(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("no AI session artifacts"));
}

#[test]
fn sandbox_doctor_detects_stale_paths() {
    let sandbox = TempDir::new().unwrap();
    let project = create_claude_project(sandbox.path(), "doctor-test");

    // Register the project
    cmd(sandbox.path())
        .args(["watch", &project.to_string_lossy()])
        .assert()
        .success();

    // Delete the project directory
    fs::remove_dir_all(&project).unwrap();

    // Doctor should detect the stale path
    cmd(sandbox.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("WARN"))
        .stdout(predicate::str::contains("no longer exists"));
}

#[test]
fn sandbox_doctor_clean_dry_run_does_not_mutate() {
    // Register a project, delete its dir, then run --clean --dry-run.
    // The stale entry must still be flagged on a subsequent doctor run.
    let sandbox = TempDir::new().unwrap();
    let project = create_claude_project(sandbox.path(), "dryrun-test");

    cmd(sandbox.path())
        .args(["watch", &project.to_string_lossy()])
        .assert()
        .success();
    fs::remove_dir_all(&project).unwrap();

    cmd(sandbox.path())
        .args(["doctor", "--clean", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry-run"))
        .stdout(predicate::str::contains("[DRY]"));

    // Re-run plain doctor — entry should still be there.
    cmd(sandbox.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("dryrun-test"))
        .stdout(predicate::str::contains("no longer exists"));
}

#[test]
fn sandbox_doctor_clean_removes_stale_entries() {
    // Register two projects, delete one, run --clean.
    // The deleted one must disappear from `status`; the live one must remain.
    let sandbox = TempDir::new().unwrap();
    let live = create_claude_project(sandbox.path(), "clean-live");
    let stale = create_claude_project(sandbox.path(), "clean-stale");

    cmd(sandbox.path())
        .args(["watch", &live.to_string_lossy()])
        .assert()
        .success();
    cmd(sandbox.path())
        .args(["watch", &stale.to_string_lossy()])
        .assert()
        .success();
    fs::remove_dir_all(&stale).unwrap();

    cmd(sandbox.path())
        .args(["doctor", "--clean"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[DEL]"))
        .stdout(predicate::str::contains("clean-stale"))
        .stdout(predicate::str::contains("removed 1 stale"));

    // After cleanup, status should still show the live project
    // and NOT show the stale one.
    cmd(sandbox.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("clean-live"))
        .stdout(predicate::str::contains("clean-stale").not());
}

#[test]
fn sandbox_unwatch_removes_project() {
    let sandbox = TempDir::new().unwrap();
    let project = create_claude_project(sandbox.path(), "unwatch-test");
    // canonicalize to match what `watch` stores (macOS /var → /private/var)
    let canonical = fs::canonicalize(&project).unwrap();

    // Watch it
    cmd(sandbox.path())
        .args(["watch", &project.to_string_lossy()])
        .assert()
        .success();

    // Verify it's registered
    cmd(sandbox.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("unwatch-test"));

    // Unwatch it using the canonical path
    cmd(sandbox.path())
        .args(["unwatch", &canonical.to_string_lossy()])
        .assert()
        .success()
        .stdout(predicate::str::contains("unwatched"));

    // Status should no longer show this project
    cmd(sandbox.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("unwatch-test").not());
}

#[test]
fn sandbox_export_import_round_trip() {
    let sandbox = TempDir::new().unwrap();
    let project = create_multi_tool_project(sandbox.path(), "export-test");
    let export_file = sandbox.path().join("export.json");

    // Watch the project
    cmd(sandbox.path())
        .args(["watch", &project.to_string_lossy()])
        .assert()
        .success();

    // Export
    cmd(sandbox.path())
        .args(["export", "-o", &export_file.to_string_lossy()])
        .assert()
        .success()
        .stdout(predicate::str::contains("exported"));

    // Verify export file exists and contains project data
    let content = fs::read_to_string(&export_file).unwrap();
    assert!(content.contains("export-test"));

    // Import into a fresh state (the registry already has it, but import should not fail)
    cmd(sandbox.path())
        .args(["import", "-i", &export_file.to_string_lossy()])
        .assert()
        .success()
        .stdout(predicate::str::contains("imported"));
}

#[test]
fn sandbox_export_import_preserves_artifact_mappings() {
    // The v2 export carries the artifact graph — importing into a FRESH data
    // dir must restore the tool associations, not just bare project paths
    // (the v1 format silently dropped them; audit M2).
    let sandbox = TempDir::new().unwrap();
    let project = create_claude_project(sandbox.path(), "graph-test");
    let export_file = sandbox.path().join("export.json");

    cmd(sandbox.path())
        .args(["watch", &project.to_string_lossy()])
        .assert()
        .success();
    cmd(sandbox.path())
        .args(["export", "-o", &export_file.to_string_lossy()])
        .assert()
        .success()
        .stdout(predicate::str::contains("artifact mapping"));

    // The bundle itself must carry the artifact rows.
    let content = fs::read_to_string(&export_file).unwrap();
    let v: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(v["sessionguard_export_version"], 2);
    let arts = v["projects"][0]["artifacts"].as_array().unwrap();
    assert!(
        !arts.is_empty(),
        "export must include artifact mappings, got: {content}"
    );

    // Import into a brand-new data dir and confirm the project came back.
    let fresh = TempDir::new().unwrap();
    cmd(fresh.path())
        .args(["import", "-i", &export_file.to_string_lossy()])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 project(s)"))
        .stdout(predicate::str::is_match(r"[1-9]\d* artifact mapping").unwrap());
    cmd(fresh.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("graph-test"));
}

#[test]
fn sandbox_scan_multi_tool_project() {
    let sandbox = TempDir::new().unwrap();
    create_multi_tool_project(sandbox.path(), "dual-tool");

    cmd(sandbox.path())
        .args(["scan", &sandbox.path().to_string_lossy()])
        .assert()
        .success()
        .stdout(predicate::str::contains("dual-tool"))
        .stdout(predicate::str::contains("Claude Code"));
}

// ── migrate / undo / migrate-cleanup end-to-end (config discovery) ──────────
//
// These drive the real compiled binary through the v0.4 migrate feature against
// a throwaway config-discovery tool. Everything is isolated under a temp HOME +
// data/config dirs and an explicit `--config`, so the operator's real
// `~/.codex` / `~/.local/share/opencode` are never reachable.

/// A fully-isolated `sessionguard` command for migrate e2e tests.
fn isolated(root: &Path) -> Command {
    let mut c = Command::cargo_bin("sessionguard").unwrap();
    c.env("SESSIONGUARD_DATA_DIR", root.join("sgdata"))
        .env("SESSIONGUARD_CONFIG_DIR", root.join("sgconfig"))
        .env("HOME", root);
    c
}

/// Build a throwaway config-discovery tool under `root`: a source data dir, a
/// JSON config file naming it, and a sessionguard config declaring the tool.
/// Returns (sessionguard_config, source_dir, tool_config).
fn setup_config_tool(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let src = root.join("toolsrc");
    fs::create_dir_all(src.join("nested")).unwrap();
    fs::write(src.join("data.txt"), "payload").unwrap();
    fs::write(src.join("nested/blob.bin"), vec![7u8; 2048]).unwrap();

    let tool_cfg = root.join("tool.json");
    fs::write(&tool_cfg, format!(r#"{{"data_dir": "{}"}}"#, src.display())).unwrap();

    let sg_cfg = root.join("sessionguard.toml");
    fs::write(
        &sg_cfg,
        format!(
            r#"[[tools]]
name = "demo"
display_name = "Demo Tool"
on_move = "notify"
session_patterns = ["AGENTS.md"]

[tools.home_dir_layout]
default_path = "{src}"
discovery = "config"

[[tools.home_dir_layout.config_files]]
file = "{cfg}"
field = "data_dir"
format = "json"
"#,
            src = src.display(),
            cfg = tool_cfg.display(),
        ),
    )
    .unwrap();
    (sg_cfg, src, tool_cfg)
}

/// Find the `.migrated-<unix>` sidecar sibling of `src`, if any.
fn find_sidecar(src: &Path) -> Option<PathBuf> {
    let parent = src.parent()?;
    let prefix = format!("{}.migrated-", src.file_name()?.to_str()?);
    fs::read_dir(parent)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&prefix))
                .unwrap_or(false)
        })
}

#[test]
fn sandbox_migrate_undo_round_trips_via_cli() {
    let home = TempDir::new().unwrap();
    let root = home.path();
    let (sg_cfg, src, tool_cfg) = setup_config_tool(root);
    let dst = root.join("dst");

    // dry-run: walks the state machine, changes nothing.
    isolated(root)
        .arg("--config")
        .arg(&sg_cfg)
        .args(["migrate", "demo", "--to"])
        .arg(&dst)
        .arg("--dry-run")
        .assert()
        .success();
    assert!(!dst.exists(), "dry-run must not create dst");
    assert!(src.join("data.txt").exists(), "dry-run must not touch src");

    // real migrate.
    isolated(root)
        .arg("--config")
        .arg(&sg_cfg)
        .args(["migrate", "demo", "--to"])
        .arg(&dst)
        .assert()
        .success();
    assert!(dst.join("data.txt").exists(), "dst should be populated");
    assert!(dst.join("nested/blob.bin").exists(), "nested file copied");
    assert!(
        find_sidecar(&src).is_some(),
        "original should be preserved as a .migrated-<unix> sidecar"
    );
    let cfg_after = fs::read_to_string(&tool_cfg).unwrap();
    assert!(
        cfg_after.contains(&dst.display().to_string()),
        "config should be rewritten to name dst, got: {cfg_after}"
    );

    // undo migration #1: restore source, remove the copy, restore config.
    isolated(root)
        .arg("--config")
        .arg(&sg_cfg)
        .args(["undo", "--migration", "1"])
        .assert()
        .success();
    assert!(
        src.join("data.txt").exists(),
        "undo should restore the source"
    );
    assert!(!dst.exists(), "undo should remove the migrated copy");
    let cfg_restored = fs::read_to_string(&tool_cfg).unwrap();
    assert!(
        cfg_restored.contains(&src.display().to_string()),
        "undo should restore the config to name src, got: {cfg_restored}"
    );
}

#[test]
fn sandbox_migrate_cleanup_removes_sidecar_keeps_live_data_via_cli() {
    let home = TempDir::new().unwrap();
    let root = home.path();
    let (sg_cfg, src, _tool_cfg) = setup_config_tool(root);
    let dst = root.join("dst");

    isolated(root)
        .arg("--config")
        .arg(&sg_cfg)
        .args(["migrate", "demo", "--to"])
        .arg(&dst)
        .assert()
        .success();
    let sidecar = find_sidecar(&src).expect("sidecar should exist after migrate");
    assert!(sidecar.exists());

    // report-only (no --execute) must not delete anything.
    isolated(root)
        .arg("--config")
        .arg(&sg_cfg)
        .arg("migrate-cleanup")
        .assert()
        .success();
    assert!(
        sidecar.exists(),
        "report-only cleanup must not delete the sidecar"
    );

    // --execute removes the preserved original; the live copy at dst survives.
    isolated(root)
        .arg("--config")
        .arg(&sg_cfg)
        .args(["migrate-cleanup", "--execute"])
        .assert()
        .success();
    assert!(
        !sidecar.exists(),
        "cleanup --execute should remove the sidecar"
    );
    assert!(
        dst.join("data.txt").exists(),
        "live migrated data must survive cleanup"
    );
}

#[test]
fn sandbox_doctor_shows_launcher_health_section() {
    // `doctor` always renders a launcher-health section for every registered
    // tool (present/missing/not-configured). Surfacing was previously unit-only.
    let sandbox = TempDir::new().unwrap();
    cmd(sandbox.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("launcher health"));
}

#[test]
fn sandbox_migrate_symlink_discovery_round_trips_via_cli() {
    // Parity with the config-discovery e2e, for the symlink discovery branch:
    // migrate leaves a symlink at the source pointing at the destination and
    // preserves the original; undo restores the real directory.
    let home = TempDir::new().unwrap();
    let root = home.path();
    let src = root.join("symsrc");
    fs::create_dir_all(src.join("nested")).unwrap();
    fs::write(src.join("data.txt"), "payload").unwrap();
    fs::write(src.join("nested/blob.bin"), vec![9u8; 1024]).unwrap();

    let sg_cfg = root.join("sessionguard.toml");
    fs::write(
        &sg_cfg,
        format!(
            r#"[[tools]]
name = "symdemo"
display_name = "Symlink Demo"
on_move = "notify"
session_patterns = ["AGENTS.md"]

[tools.home_dir_layout]
default_path = "{src}"
discovery = "symlink"
"#,
            src = src.display(),
        ),
    )
    .unwrap();

    let dst = root.join("dst");

    isolated(root)
        .arg("--config")
        .arg(&sg_cfg)
        .args(["migrate", "symdemo", "--to"])
        .arg(&dst)
        .assert()
        .success();
    assert!(dst.join("data.txt").exists(), "destination populated");
    let meta = fs::symlink_metadata(&src).unwrap();
    assert!(
        meta.file_type().is_symlink(),
        "source should be a symlink after symlink-discovery migrate"
    );
    assert!(
        src.join("data.txt").exists(),
        "the symlink should resolve to the migrated data"
    );
    assert!(
        find_sidecar(&src).is_some(),
        "original should be preserved as a .migrated sidecar"
    );

    isolated(root)
        .arg("--config")
        .arg(&sg_cfg)
        .args(["undo", "--migration", "1"])
        .assert()
        .success();
    let meta = fs::symlink_metadata(&src).unwrap();
    assert!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "source should be restored to a real directory"
    );
    assert!(src.join("data.txt").exists(), "source data restored");
    assert!(!dst.exists(), "destination removed by undo");
}
