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
    // Three shapes: one live Claude project (store dir decodes against the
    // real fs); one Codex session whose cwd no longer exists (Exact-confidence
    // orphan — Codex/OpenCode store literal paths, so "gone" is a direct
    // filesystem check); and one Claude Code project whose directory was
    // deleted but whose parent survives (an Inferred-confidence orphan via
    // the DFS-plus-fold decode — this branch's decode CAN prove "gone" from
    // an encoded name when a real ancestor anchors the guess, unlike a name
    // with no living ancestor at all, which instead shows [ENCODED NAME]).
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

    // "work" already exists (created above for the live project), but
    // "work/deleted-app" itself is never created — DFS validates down to
    // "work" and folds the unvalidated leaf, at Inferred confidence.
    let inferred_gone = home.path().join("work/deleted-app");
    let enc_inferred = inferred_gone.display().to_string().replace('/', "-");
    let inferred_store = home.path().join(".claude/projects").join(&enc_inferred);
    std::fs::create_dir_all(&inferred_store).unwrap();
    std::fs::write(inferred_store.join("s.jsonl"), "{}").unwrap();

    let out = sg(&home)
        .args(["sessions", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let groups: serde_json::Value = serde_json::from_slice(&out).expect("sessions JSON parses");
    let arr = groups.as_array().expect("array of groups");
    assert_eq!(arr.len(), 3, "three projects with sessions");

    // Consumer contract (design doc Testing section): the dashboard's
    // Activity adapter (tools/dashboard/app.py reads `confidence`) and any
    // downstream script depend on every group carrying `confidence`,
    // `decoded`, and `host` — a rename or a stray skip_serializing_if must
    // fail loudly here, not silently in the dashboard.
    for g in arr {
        assert!(
            g.get("confidence").is_some(),
            "group missing confidence: {g}"
        );
        assert!(g.get("decoded").is_some(), "group missing decoded: {g}");
        assert!(g.get("host").is_some(), "group missing host: {g}");
    }

    let live_g = arr
        .iter()
        .find(|g| g["project_path"] == live.display().to_string())
        .expect("live group");
    assert_eq!(live_g["orphaned"], false);
    assert_eq!(live_g["confidence"], "exact");
    assert_eq!(live_g["host"], "local");
    assert_eq!(live_g["tools"]["claude_code"]["count"], 1);

    let codex_orphan = arr
        .iter()
        .find(|g| g["project_path"] == gone.display().to_string())
        .expect("codex orphan group");
    assert_eq!(codex_orphan["orphaned"], true);
    assert_eq!(codex_orphan["confidence"], "exact");

    let inferred_g = arr
        .iter()
        .find(|g| g["project_path"] == inferred_gone.display().to_string())
        .expect("inferred orphan group");
    assert_eq!(inferred_g["orphaned"], true);
    assert_eq!(inferred_g["confidence"], "inferred");

    // --orphans filters to just the two orphans; the text renderer marks the
    // Exact orphan [ORPHANED] and the Inferred orphan [ORPHANED?] — the
    // distinction the dashboard and operators use to tell a confirmed
    // deletion from a best-guess one.
    sg(&home)
        .args(["sessions", "--orphans"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[ORPHANED]"))
        .stdout(predicate::str::contains("[ORPHANED?]"))
        .stdout(predicate::str::contains("2 project(s) with sessions"));
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

#[test]
fn cli_tools_json_declares_real_session_stores_and_no_fictional_fields() {
    let home = TempDir::new().unwrap();
    let out = sg(&home)
        .args(["tools", "list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let tools: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let by = |n: &str| -> serde_json::Value {
        tools
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == n)
            .expect("tool present")
            .clone()
    };

    // Verified stores are declared.
    assert_eq!(by("claude_code")["session_store"]["layout"], "encoded_dir");
    assert_eq!(by("codex")["session_store"]["layout"], "jsonl_field");
    assert_eq!(by("opencode")["session_store"]["layout"], "sqlite_column");

    // Verified-fictional path_fields are gone (the field named does not exist
    // in any real install; see docs/design/session-store-model.md).
    for t in ["claude_code", "gemini_cli"] {
        let pf = by(t)["path_fields"].as_array().cloned().unwrap_or_default();
        assert!(pf.is_empty(), "{t} must not declare unverified path_fields");
    }
}

#[test]
fn cli_sessions_honors_explicit_home_root() {
    // A census root other than $HOME — the mounted/rsync'd-home case.
    let real = TempDir::new().unwrap(); // process HOME: empty
    let other = TempDir::new().unwrap(); // the root we actually census
    let live = other.path().join("p/app");
    std::fs::create_dir_all(&live).unwrap();
    let enc = live.display().to_string().replace('/', "-");
    let store = other.path().join(".claude/projects").join(&enc);
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("s.jsonl"), "{}").unwrap();

    sg(&real)
        .args(["sessions", "--home", &other.path().display().to_string()])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 project(s) with sessions"));
}
