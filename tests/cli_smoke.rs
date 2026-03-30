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
