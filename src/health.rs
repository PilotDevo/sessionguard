// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Tool launcher health checks.
//!
//! For each registered tool, verify the `binary` named in its definition
//! can be found on the user's `PATH`. The *visibility* layer of the
//! "runtime upgrade lost my launcher" problem: SessionGuard doesn't
//! restore launchers, it just notices when they're gone and surfaces
//! that the underlying session data is still intact.
//!
//! Motivating scenario: a developer upgrades Node v23 → v24, npm globals
//! evaporate, the `claude` / `codex` / `gemini` binaries are no longer on
//! PATH — but `~/.claude/projects/`, `~/.codex/sessions/`,
//! `~/.local/share/opencode/` are untouched. From the user's POV
//! "sessions are gone" — they aren't. This module gives the dashboard and
//! `sessionguard doctor` the data to say so explicitly.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::tools::ToolDefinition;

/// Status of a tool's launcher binary on the user's PATH.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum BinaryStatus {
    /// Tool definition declares a binary and it was found on PATH.
    Present { path: PathBuf },
    /// Tool definition declares a binary, but it isn't on PATH.
    Missing { binary: String },
    /// Tool definition does not declare a binary (some "tools" are IDEs
    /// or library-only patterns with no CLI launcher).
    NotConfigured,
}

/// Look up the launcher binary for `tool`. Returns a [`BinaryStatus`]
/// reflecting current PATH state.
pub fn check_binary(tool: &ToolDefinition) -> BinaryStatus {
    match tool.binary.as_deref() {
        None => BinaryStatus::NotConfigured,
        Some(name) => match which(name) {
            Some(path) => BinaryStatus::Present { path },
            None => BinaryStatus::Missing {
                binary: name.to_string(),
            },
        },
    }
}

/// Resolve `name` against the user's `PATH` using the same algorithm as
/// the venerable `which(1)`. Returns the absolute path to the first
/// executable file found, or `None`.
///
/// We don't shell out to `which(1)` itself because:
/// - It's not guaranteed to exist on minimal Linux images
/// - Avoiding a subprocess per check keeps the doctor + dashboard fast
fn which(name: &str) -> Option<PathBuf> {
    // Absolute or relative path: use directly if it's an executable file.
    if name.contains(std::path::MAIN_SEPARATOR) {
        let p = PathBuf::from(name);
        return is_executable(&p).then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    // On Windows we'd want to check PATHEXT and look for .exe/.bat/.cmd
    // counterparts, but SessionGuard doesn't support Windows yet (see
    // ROADMAP.md). For now: any regular file with the exact name.
    p.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ReconcileStrategy, ToolDefinition};

    fn tool_with_binary(binary: Option<&str>) -> ToolDefinition {
        ToolDefinition {
            name: "test".to_string(),
            display_name: "Test".to_string(),
            session_patterns: vec![],
            path_fields: vec![],
            on_move: ReconcileStrategy::Notify,
            version: None,
            binary: binary.map(|s| s.to_string()),
            home_dir_layout: None,
        }
    }

    #[test]
    fn not_configured_when_binary_unset() {
        let t = tool_with_binary(None);
        assert_eq!(check_binary(&t), BinaryStatus::NotConfigured);
    }

    #[test]
    fn present_for_universally_available_binary() {
        // `sh` is required by POSIX; should be on PATH on any Unix CI.
        let t = tool_with_binary(Some("sh"));
        match check_binary(&t) {
            BinaryStatus::Present { path } => {
                assert!(path.is_absolute(), "expected absolute path, got {path:?}");
                assert!(
                    path.ends_with("sh"),
                    "expected path to end with `sh`, got {path:?}"
                );
            }
            other => panic!("expected Present, got {other:?}"),
        }
    }

    #[test]
    fn missing_for_nonsense_name() {
        // Generated junk name; vanishingly unlikely to exist on PATH.
        let t = tool_with_binary(Some("sessionguard-no-such-binary-zzz9"));
        match check_binary(&t) {
            BinaryStatus::Missing { binary } => {
                assert_eq!(binary, "sessionguard-no-such-binary-zzz9");
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn absolute_path_resolves_when_executable() {
        // `/bin/sh` is the canonical POSIX shell location.
        let t = tool_with_binary(Some("/bin/sh"));
        if Path::new("/bin/sh").exists() {
            match check_binary(&t) {
                BinaryStatus::Present { path } => assert_eq!(path, PathBuf::from("/bin/sh")),
                other => panic!("expected Present, got {other:?}"),
            }
        }
    }

    #[test]
    fn binary_status_serialises_with_tagged_repr() {
        // The dashboard consumes this as JSON — verify the shape.
        let p = BinaryStatus::Present {
            path: PathBuf::from("/usr/bin/example"),
        };
        let j = serde_json::to_value(&p).unwrap();
        assert_eq!(j["status"], "present");
        assert_eq!(j["path"], "/usr/bin/example");

        let m = BinaryStatus::Missing {
            binary: "ghost".into(),
        };
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["status"], "missing");
        assert_eq!(j["binary"], "ghost");

        let n = BinaryStatus::NotConfigured;
        let j = serde_json::to_value(&n).unwrap();
        assert_eq!(j["status"], "not_configured");
    }
}
