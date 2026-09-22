// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Run the daemon at login: a launchd agent on macOS, a systemd user unit on
//! Linux.
//!
//! Through v0.10 there was no way to do this on macOS at all — no LaunchAgent,
//! no `service` block in the Homebrew formula — so the daemon ran only while a
//! `sessionguard start` survived, and died at the next logout. On the
//! operator's own Mac it had run for 49 seconds in its lifetime. A watcher that
//! isn't running reconciles nothing, however correct its code.
//!
//! This module only *renders* files and *describes* the commands that load
//! them; `main.rs` performs the writes and runs the commands, so everything
//! here is testable without touching the real launchd or systemd.

use std::path::{Path, PathBuf};

/// launchd label, and the plist's basename.
pub const LAUNCHD_LABEL: &str = "dev.droco.sessionguard";
/// systemd unit name.
pub const SYSTEMD_UNIT: &str = "sessionguard.service";

/// Which service manager this platform uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Launchd,
    Systemd,
}

impl Manager {
    pub fn current() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Manager::Launchd)
        } else if cfg!(target_os = "linux") {
            Some(Manager::Systemd)
        } else {
            None
        }
    }

    /// Where the service definition lives for this user.
    pub fn unit_path(self, home: &Path) -> PathBuf {
        match self {
            Manager::Launchd => home
                .join("Library/LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist")),
            Manager::Systemd => home.join(".config/systemd/user").join(SYSTEMD_UNIT),
        }
    }

    /// Render the service definition that runs `exe start --foreground`.
    pub fn render(self, exe: &Path, log: &Path, config: Option<&Path>) -> String {
        match self {
            Manager::Launchd => render_launchd_plist(exe, log, config),
            Manager::Systemd => render_systemd_unit(exe, config),
        }
    }

    /// Commands that (re)load the service and start it now. The first
    /// command of a launchd install unloads any previous copy and is allowed
    /// to fail (nothing was loaded).
    pub fn install_commands(self, unit: &Path, uid: u32) -> Vec<Vec<String>> {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        match self {
            Manager::Launchd => vec![
                s(&[
                    "launchctl",
                    "bootout",
                    &format!("gui/{uid}/{LAUNCHD_LABEL}"),
                ]),
                s(&[
                    "launchctl",
                    "bootstrap",
                    &format!("gui/{uid}"),
                    &unit.display().to_string(),
                ]),
            ],
            Manager::Systemd => vec![
                s(&["systemctl", "--user", "daemon-reload"]),
                s(&["systemctl", "--user", "enable", "--now", SYSTEMD_UNIT]),
            ],
        }
    }

    /// Commands that stop the service and stop it starting at login.
    pub fn uninstall_commands(self, uid: u32) -> Vec<Vec<String>> {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        match self {
            Manager::Launchd => vec![s(&[
                "launchctl",
                "bootout",
                &format!("gui/{uid}/{LAUNCHD_LABEL}"),
            ])],
            Manager::Systemd => vec![
                s(&["systemctl", "--user", "disable", "--now", SYSTEMD_UNIT]),
                s(&["systemctl", "--user", "daemon-reload"]),
            ],
        }
    }

    /// A command whose success means "the service manager has it loaded".
    pub fn loaded_check(self, uid: u32) -> Vec<String> {
        match self {
            Manager::Launchd => vec![
                "launchctl".into(),
                "print".into(),
                format!("gui/{uid}/{LAUNCHD_LABEL}"),
            ],
            Manager::Systemd => vec![
                "systemctl".into(),
                "--user".into(),
                "is-enabled".into(),
                "--quiet".into(),
                SYSTEMD_UNIT.into(),
            ],
        }
    }
}

/// Whether a service definition is installed for this user (file present).
/// Cheap — no process is spawned — so `status --deep` can always ask.
pub fn is_installed(home: &Path) -> Option<bool> {
    Manager::current().map(|m| m.unit_path(home).is_file())
}

/// The path a login service should run.
///
/// Resolving the running binary through symlinks is not enough: a Homebrew
/// install resolves into a versioned `Cellar/sessionguard/<version>/bin/`
/// directory that `brew upgrade` deletes, which would leave the service
/// pointing at a vanished binary after the first upgrade. The stable name is
/// the entry on `PATH` (`/opt/homebrew/bin/sessionguard`), so prefer a `PATH`
/// entry that resolves to the running binary, and fall back to the resolved
/// binary itself.
pub fn stable_exe_path(exe_canonical: &Path, path_var: Option<&std::ffi::OsStr>) -> PathBuf {
    if let Some(pv) = path_var {
        for dir in std::env::split_paths(pv) {
            let candidate = dir.join("sessionguard");
            if candidate.is_file()
                && std::fs::canonicalize(&candidate).ok().as_deref() == Some(exe_canonical)
            {
                return candidate;
            }
        }
    }
    exe_canonical.to_path_buf()
}

/// A binary inside a Cargo `target/` directory is a development build: a
/// service pointing at it breaks on the next `cargo clean` or rebuild.
pub fn looks_like_dev_build(exe: &Path) -> bool {
    exe.components().any(|c| c.as_os_str() == "target")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn render_launchd_plist(exe: &Path, log: &Path, config: Option<&Path>) -> String {
    let mut args = vec![exe.display().to_string()];
    if let Some(c) = config {
        args.push("--config".into());
        args.push(c.display().to_string());
    }
    args.push("start".into());
    args.push("--foreground".into());
    let args_xml: String = args
        .iter()
        .map(|a| format!("        <string>{}</string>\n", xml_escape(a)))
        .collect();
    let log = xml_escape(&log.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by `sessionguard service install`; remove with `sessionguard service uninstall`. -->
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{args_xml}    </array>
    <!-- Start at login. -->
    <key>RunAtLoad</key>
    <true/>
    <!-- Restart only if it CRASHES: a clean exit (`sessionguard stop`) stays stopped. -->
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ProcessType</key>
    <string>Background</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>info</string>
    </dict>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#
    )
}

fn render_systemd_unit(exe: &Path, config: Option<&Path>) -> String {
    let config_arg = config
        .map(|c| format!(" --config {}", c.display()))
        .unwrap_or_default();
    format!(
        "# Written by `sessionguard service install`; remove with `sessionguard service uninstall`.\n\
         [Unit]\n\
         Description=SessionGuard — AI session artifact reconciliation daemon\n\
         Documentation=https://github.com/PilotDevo/sessionguard\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe}{config_arg} start --foreground\n\
         # Restart only on a crash; `sessionguard stop` (a clean exit) stays stopped.\n\
         Restart=on-failure\n\
         RestartSec=5s\n\
         Environment=RUST_LOG=info\n\
         # Not ProtectHome: the daemon rewrites session files under your home.\n\
         PrivateTmp=true\n\
         NoNewPrivileges=true\n\
         ProtectSystem=full\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launchd_plist_runs_the_daemon_at_login_and_restarts_only_on_crash() {
        let p = render_launchd_plist(
            Path::new("/Users/me/.cargo/bin/sessionguard"),
            Path::new("/Users/me/Library/Application Support/dev.droco.sessionguard/daemon.log"),
            None,
        );
        assert!(p.contains("<string>/Users/me/.cargo/bin/sessionguard</string>"));
        assert!(p.contains("<string>start</string>"));
        assert!(p.contains("<string>--foreground</string>"));
        assert!(p.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(
            p.contains("<key>SuccessfulExit</key>\n        <false/>"),
            "a clean `stop` must stay stopped; only a crash restarts"
        );
        assert!(p.contains(LAUNCHD_LABEL));
        assert!(p.contains("daemon.log"));
    }

    #[test]
    fn launchd_plist_escapes_paths_and_carries_an_explicit_config() {
        let p = render_launchd_plist(
            Path::new("/Users/a&b/bin/sessionguard"),
            Path::new("/tmp/log"),
            Some(Path::new("/Users/a&b/sg <x>.toml")),
        );
        assert!(p.contains("/Users/a&amp;b/bin/sessionguard"));
        assert!(p.contains("<string>--config</string>"));
        assert!(p.contains("/Users/a&amp;b/sg &lt;x&gt;.toml"));
        assert!(!p.contains("a&b"), "raw & would make the plist invalid XML");
    }

    #[test]
    fn systemd_unit_uses_the_real_binary_path() {
        // contrib/sessionguard.service hardcodes /usr/local/bin, which is wrong
        // for cargo and ~/.local/bin installs. The rendered unit uses the
        // binary that installed it.
        let u = render_systemd_unit(Path::new("/home/me/.cargo/bin/sessionguard"), None);
        assert!(u.contains("ExecStart=/home/me/.cargo/bin/sessionguard start --foreground"));
        assert!(u.contains("Restart=on-failure"));
        assert!(u.contains("WantedBy=default.target"));
    }

    #[test]
    fn paths_and_commands_per_manager() {
        let home = Path::new("/Users/me");
        assert_eq!(
            Manager::Launchd.unit_path(home),
            PathBuf::from("/Users/me/Library/LaunchAgents/dev.droco.sessionguard.plist")
        );
        assert_eq!(
            Manager::Systemd.unit_path(home),
            PathBuf::from("/Users/me/.config/systemd/user/sessionguard.service")
        );
        let cmds = Manager::Launchd.install_commands(Path::new("/p.plist"), 501);
        assert_eq!(cmds[1], ["launchctl", "bootstrap", "gui/501", "/p.plist"]);
        assert_eq!(
            Manager::Systemd.install_commands(Path::new("/u"), 1000)[1],
            [
                "systemctl",
                "--user",
                "enable",
                "--now",
                "sessionguard.service"
            ]
        );
    }

    #[test]
    fn a_binary_under_target_is_a_dev_build() {
        assert!(looks_like_dev_build(Path::new(
            "/Users/me/src/sg/target/release/sessionguard"
        )));
        assert!(!looks_like_dev_build(Path::new(
            "/Users/me/.cargo/bin/sessionguard"
        )));
    }

    #[cfg(unix)]
    #[test]
    fn a_homebrew_style_install_uses_the_stable_path_not_the_versioned_cellar() {
        let t = tempfile::TempDir::new().unwrap();
        let cellar = t.path().join("Cellar/sessionguard/0.11.0/bin");
        let bin = t.path().join("bin");
        std::fs::create_dir_all(&cellar).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let real = cellar.join("sessionguard");
        std::fs::write(&real, b"#!/bin/sh\n").unwrap();
        std::os::unix::fs::symlink(&real, bin.join("sessionguard")).unwrap();
        let canonical = std::fs::canonicalize(&real).unwrap();

        let path_var = std::env::join_paths([t.path().join("elsewhere"), bin.clone()]).unwrap();
        assert_eq!(
            stable_exe_path(&canonical, Some(&path_var)),
            bin.join("sessionguard"),
            "`brew upgrade` deletes the Cellar dir; the PATH symlink survives"
        );
        // Not on PATH at all → the resolved binary is all there is.
        assert_eq!(stable_exe_path(&canonical, None), canonical);
    }
}
