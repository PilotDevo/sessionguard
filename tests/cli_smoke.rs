// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! CLI smoke tests — basic invocation, flag parsing, and graceful output
//! when no daemon is running.

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// A `sessionguard` command fully isolated from the operator's real
/// environment. Points the data dir, config dir, and `HOME` at a throwaway
/// temp dir so no test reads `~/.config/sessionguard`, the real registry/event
/// log, or the real `~/.codex` / `~/.local/share/opencode`. Hold the returned
/// `TempDir` for the command's lifetime.
fn sg(home: &TempDir) -> Command {
    let mut c = Command::cargo_bin("sessionguard").unwrap();
    c.env("SESSIONGUARD_DATA_DIR", home.path().join("data"))
        .env("SESSIONGUARD_CONFIG_DIR", home.path().join("config"))
        .env("HOME", home.path());
    c
}

#[test]
fn cli_help() {
    let home = TempDir::new().unwrap();
    sg(&home)
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("AI coding sessions"));
}

#[test]
fn cli_version() {
    let home = TempDir::new().unwrap();
    sg(&home)
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn cli_status_no_daemon() {
    let home = TempDir::new().unwrap();
    sg(&home).arg("status").assert().success();
}

#[test]
fn cli_config_show() {
    let home = TempDir::new().unwrap();
    sg(&home)
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("watch_mode"));
}

#[test]
fn cli_log_empty() {
    let home = TempDir::new().unwrap();
    sg(&home).arg("log").assert().success();
}

#[test]
fn cli_tools_list_shows_builtins() {
    let home = TempDir::new().unwrap();
    sg(&home)
        .arg("tools")
        .assert()
        .success()
        .stdout(predicate::str::contains("claude_code"))
        .stdout(predicate::str::contains("cursor"))
        .stdout(predicate::str::contains("windsurf"))
        .stdout(predicate::str::contains("aider"))
        .stdout(predicate::str::contains("gemini_cli"))
        .stdout(predicate::str::contains("codex"))
        .stdout(predicate::str::contains("opencode"));
}

#[test]
fn cli_tools_list_verbose_shows_patterns() {
    let home = TempDir::new().unwrap();
    sg(&home)
        .args(["tools", "list", "--verbose"])
        .assert()
        .success()
        .stdout(predicate::str::contains("session_patterns:"))
        .stdout(predicate::str::contains("path_fields:"));
}

#[test]
fn cli_tools_list_format_json_is_valid_array() {
    let home = TempDir::new().unwrap();
    let out = sg(&home)
        .args(["tools", "list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value =
        serde_json::from_slice(&out).expect("tools --format json should be valid JSON");
    let arr = parsed.as_array().expect("tools JSON should be an array");
    assert!(
        arr.len() >= 7,
        "should have at least 7 builtin tools, got {}",
        arr.len()
    );
    // Each entry must have the fields the dashboard consumes
    for t in arr {
        assert!(t.get("name").is_some(), "tool entry missing name");
        assert!(t.get("session_patterns").is_some());
    }
}

#[test]
fn cli_log_format_json_is_valid_array() {
    let home = TempDir::new().unwrap();
    let out = sg(&home)
        .args(["log", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value =
        serde_json::from_slice(&out).expect("log --format json should be valid JSON");
    assert!(parsed.is_array(), "log JSON should be an array");
}

#[test]
fn cli_status_format_json_has_expected_keys() {
    let home = TempDir::new().unwrap();
    let out = sg(&home)
        .args(["status", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value =
        serde_json::from_slice(&out).expect("status --format json should be valid JSON");
    assert!(parsed.get("daemon_running").is_some());
    assert!(parsed.get("projects").is_some());
}

#[test]
fn cli_inventory_text_lists_codex_and_opencode() {
    // Both codex and opencode declare home_dir_layout, so they're listed by
    // inventory regardless of whether their data dir exists on this host
    // (confirmed by inventory's reports_missing_path_with_exists_false unit
    // test). Other built-ins without a layout don't appear.
    let home = TempDir::new().unwrap();
    sg(&home)
        .arg("inventory")
        .assert()
        .success()
        .stdout(predicate::str::contains("codex"))
        .stdout(predicate::str::contains("opencode"));
}

#[test]
fn cli_inventory_format_json_is_valid_array() {
    let home = TempDir::new().unwrap();
    let out = sg(&home)
        .args(["inventory", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value =
        serde_json::from_slice(&out).expect("inventory --format json should be valid JSON");
    let arr = parsed
        .as_array()
        .expect("inventory JSON should be an array");
    // codex + opencode at minimum
    assert!(
        arr.len() >= 2,
        "expected >=2 inventory entries, got {}",
        arr.len()
    );
    for entry in arr {
        assert!(entry.get("tool_name").is_some());
        assert!(entry.get("path").is_some());
        assert!(entry.get("size_bytes").is_some());
    }
}

#[test]
fn cli_undo_no_events_prints_message() {
    let home = TempDir::new().unwrap();
    sg(&home)
        .arg("undo")
        .assert()
        .success()
        .stdout(predicate::str::contains("no actions to undo"));
}

#[test]
fn cli_sessions_census_groups_and_flags_orphans() {
    // One live Claude project (store dir decodes against the real fs) and one
    // Codex session whose cwd no longer exists (orphan). Orphan detection is
    // exact for Codex/OpenCode (they store literal paths); an undecodable
    // Claude store shows [ENCODED NAME] instead, because its sanitization is
    // lossy and "gone" can't be proven from the name alone.
    let home = TempDir::new().unwrap();
    let live = home.path().join("work/app");
    std::fs::create_dir_all(&live).unwrap();
    let enc_live = live.display().to_string().replace('/', "-");
    let store = home.path().join(".claude/projects").join(&enc_live);
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("session.jsonl"), "{}").unwrap();

    let gone = home.path().join("work/deleted-proj");
    let codex = home.path().join(".codex/sessions/2026/07");
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::write(
        codex.join("rollout-1.jsonl"),
        format!("{{\"cwd\": \"{}\"}}\n", gone.display()),
    )
    .unwrap();

    let out = sg(&home)
        .args(["sessions", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let groups: serde_json::Value = serde_json::from_slice(&out).expect("sessions JSON parses");
    let arr = groups.as_array().expect("array of groups");
    assert_eq!(arr.len(), 2, "two projects with sessions");
    let live_g = arr
        .iter()
        .find(|g| g["project_path"] == live.display().to_string())
        .expect("live group");
    assert_eq!(live_g["orphaned"], false);
    assert_eq!(live_g["tools"]["claude_code"]["count"], 1);
    assert!(
        arr.iter().any(|g| g["orphaned"] == true),
        "deleted project must be flagged orphaned"
    );

    // --orphans filters to just the orphan.
    sg(&home)
        .args(["sessions", "--orphans"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[ORPHANED]"))
        .stdout(predicate::str::contains("1 project(s) with sessions"));
}

#[test]
fn cli_tools_list_json_carries_binary_status() {
    // The launcher-health column the dashboard consumes.
    let home = TempDir::new().unwrap();
    let out = sg(&home)
        .args(["tools", "list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value = serde_json::from_slice(&out).expect("tools JSON should parse");
    for t in parsed.as_array().expect("array") {
        assert!(
            t.get("binary_status").is_some(),
            "each tool entry should carry binary_status"
        );
    }
}
