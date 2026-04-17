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
        .stdout(predicate::str::contains("gemini_cli"));
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
