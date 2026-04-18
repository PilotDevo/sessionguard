// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! CLI smoke tests — basic invocation, flag parsing, and graceful output
//! when no daemon is running.

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn cli_help() {
    Command::cargo_bin("sessionguard")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("AI coding sessions"));
}

#[test]
fn cli_version() {
    Command::cargo_bin("sessionguard")
        .unwrap()
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn cli_status_no_daemon() {
    Command::cargo_bin("sessionguard")
        .unwrap()
        .arg("status")
        .assert()
        .success();
}

#[test]
fn cli_config_show() {
    Command::cargo_bin("sessionguard")
        .unwrap()
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("watch_mode"));
}

#[test]
fn cli_log_empty() {
    Command::cargo_bin("sessionguard")
        .unwrap()
        .arg("log")
        .assert()
        .success();
}

#[test]
fn cli_tools_list_shows_builtins() {
    Command::cargo_bin("sessionguard")
        .unwrap()
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
    Command::cargo_bin("sessionguard")
        .unwrap()
        .args(["tools", "list", "--verbose"])
        .assert()
        .success()
        .stdout(predicate::str::contains("session_patterns:"))
        .stdout(predicate::str::contains("path_fields:"));
}

#[test]
fn cli_tools_list_format_json_is_valid_array() {
    let out = Command::cargo_bin("sessionguard")
        .unwrap()
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
    let tmp = tempfile::TempDir::new().unwrap();
    let out = Command::cargo_bin("sessionguard")
        .unwrap()
        .env("SESSIONGUARD_DATA_DIR", tmp.path())
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
    let tmp = tempfile::TempDir::new().unwrap();
    let out = Command::cargo_bin("sessionguard")
        .unwrap()
        .env("SESSIONGUARD_DATA_DIR", tmp.path())
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
fn cli_undo_no_events_prints_message() {
    // Fresh in-process data dir so we know the log is empty
    let tmp = tempfile::TempDir::new().unwrap();
    Command::cargo_bin("sessionguard")
        .unwrap()
        .env("SESSIONGUARD_DATA_DIR", tmp.path())
        .arg("undo")
        .assert()
        .success()
        .stdout(predicate::str::contains("no actions to undo"));
}
