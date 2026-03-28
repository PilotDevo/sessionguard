//! Sandbox integration tests.
//!
//! Creates realistic project structures with AI tool session artifacts
//! and exercises the full CLI against them.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
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
