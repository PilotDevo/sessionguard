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
            session_store: None,
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

/// Whether the daemon is doing its job — derived, never stored.
///
/// A stored health record is a lie the moment the process dies, so every
/// field here is computed at query time from the PID file, the config, the
/// filesystem and the activity log.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DaemonHealth {
    pub running: bool,
    pub pid: Option<u32>,
    pub version: String,
    /// Configured watch roots that do NOT exist on disk. A root renamed out
    /// from under the config is a silent way for the daemon to become inert:
    /// it is up, watching nothing.
    pub missing_watch_roots: Vec<String>,
    pub watch_root_count: usize,
    pub tracked_projects: usize,
    /// Newest decision of any kind.
    pub last_activity: Option<String>,
    /// Newest decision that actually CHANGED something.
    pub last_acted: Option<String>,
    /// Counts per outcome over the retained window.
    pub outcomes: Vec<(String, usize)>,
    /// Whether a login service is installed (`None` on an unsupported
    /// platform). Without one the daemon dies at logout — which is how it came
    /// to have run for 49 seconds, ever, on the operator's own Mac.
    pub service_installed: Option<bool>,
    /// Problems worth an operator's attention, in plain language.
    pub warnings: Vec<String>,
}

impl DaemonHealth {
    /// True when the daemon is up but has never actually changed anything.
    ///
    /// This is the direct antidote to the failure that motivated this module:
    /// for months the daemon ran, logged, reported success, and moved zero
    /// sessions. "Up" was never the same question as "working".
    pub fn is_inert(&self) -> bool {
        self.running && self.last_acted.is_none()
    }

    /// Gather everything. `registry` and `log` are passed in so this stays
    /// testable and never opens the operator's real databases implicitly.
    pub fn gather(
        config: &crate::config::Config,
        registry: &crate::registry::Registry,
        log: &crate::event_log::EventLog,
    ) -> Self {
        let running = crate::daemon::is_running();
        let pid = crate::daemon::read_pid().ok().flatten();
        let missing_watch_roots: Vec<String> = config
            .watch_roots
            .iter()
            .filter(|p| !p.is_dir())
            .map(|p| p.display().to_string())
            .collect();
        let tracked_projects = registry.list_projects().map(|p| p.len()).unwrap_or(0);
        let last_activity = log.last_activity_at().ok().flatten();
        let last_acted = log.last_acted_at().ok().flatten();
        let outcomes = log.activity_counts().unwrap_or_default();

        let mut health = Self {
            running,
            pid,
            version: env!("CARGO_PKG_VERSION").to_string(),
            missing_watch_roots,
            watch_root_count: config.watch_roots.len(),
            tracked_projects,
            last_activity,
            last_acted,
            outcomes,
            service_installed: crate::config::home_dir()
                .and_then(|h| crate::service::is_installed(&h)),
            warnings: Vec::new(),
        };

        if !health.running {
            health.warnings.push(
                "the daemon is not running — no moves are being reconciled. Start it with \
                 `sessionguard start`."
                    .into(),
            );
        }
        if health.service_installed == Some(false) {
            health.warnings.push(
                "not installed as a login service, so the daemon stops at logout or reboot and \
                 moves after that go unnoticed. Run `sessionguard service install`."
                    .into(),
            );
        }
        if health.watch_root_count == 0 {
            health.warnings.push(
                "no watch roots are configured, so nothing is being watched. Add some with \
                 `sessionguard watch <path>` or in config.toml."
                    .into(),
            );
        }
        for root in &health.missing_watch_roots {
            health.warnings.push(format!(
                "watch root {root} does not exist — it was moved or deleted, so nothing under \
                 it is being watched"
            ));
        }
        if health.is_inert() {
            health.warnings.push(
                "the daemon is running but has NEVER changed anything. If projects have moved \
                 since it started, it is not doing its job — check `sessionguard log --activity`."
                    .into(),
            );
        }
        let refused = health
            .outcomes
            .iter()
            .find(|(k, _)| k == "refused")
            .map(|(_, n)| *n)
            .unwrap_or(0);
        if refused > 0 {
            health.warnings.push(format!(
                "{refused} operation(s) were REFUSED to protect your data and need your \
                 attention — see `sessionguard log --activity`"
            ));
        }
        let failed = health
            .outcomes
            .iter()
            .find(|(k, _)| k == "failed")
            .map(|(_, n)| *n)
            .unwrap_or(0);
        if failed > 0 {
            health.warnings.push(format!(
                "{failed} operation(s) FAILED — see `sessionguard log --activity`"
            ));
        }
        health
    }
}

#[cfg(test)]
mod daemon_health_tests {
    use super::DaemonHealth;

    /// A daemon that is up but has never changed anything is INERT, and must
    /// be reported that way. This is the v0.9.0 scenario: running, logging,
    /// reporting success, moving zero sessions, for months.
    #[test]
    fn running_but_never_acted_is_inert_and_warns() {
        let h = DaemonHealth {
            running: true,
            pid: Some(42),
            version: "test".into(),
            missing_watch_roots: vec![],
            watch_root_count: 2,
            tracked_projects: 3,
            last_activity: Some("2026-09-18 00:00:00".into()),
            last_acted: None,
            outcomes: vec![("noop".into(), 12)],
            service_installed: Some(true),
            warnings: vec![],
        };
        assert!(h.is_inert(), "busy but achieving nothing is inert");

        let working = DaemonHealth {
            last_acted: Some("2026-09-18 00:00:01".into()),
            ..h.clone()
        };
        assert!(!working.is_inert());

        // A stopped daemon is "not running", which is a different problem —
        // don't also cry "inert" at it.
        let stopped = DaemonHealth {
            running: false,
            ..h.clone()
        };
        assert!(!stopped.is_inert());
    }
}
