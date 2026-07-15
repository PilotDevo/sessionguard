// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Self-update for `sessionguard update` (v0.5+).
//!
//! See `docs/design/update.md` for the full contract. The design rules this
//! module enforces:
//!
//! - **Don't fight the package manager.** Detect how the running binary was
//!   installed and *defer* to brew/cargo rather than overwrite their managed
//!   files. Only a `Standalone` install (the `install.sh` target) is swapped.
//! - **Integrity first.** A downloaded asset is verified against the release's
//!   `SHA256SUMS` before it is ever made executable on `PATH`. `--check` is
//!   read-only and does none of that.
//! - **Reversible.** SU3's swap keeps the previous binary as `<bin>.bak-<ver>`.
//!
//! Network access is abstracted behind [`ReleaseClient`] so tests never hit the
//! network — the default [`CurlReleaseClient`] shells out to `curl` (already a
//! documented dependency of `install.sh`), and tests substitute a fake.

use std::path::{Path, PathBuf};

/// Canonical GitHub repo the updater pulls releases from.
pub const REPO: &str = "PilotDevo/sessionguard";

/// Errors from the update flow.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// A network/`curl` invocation failed.
    #[error("network error: {0}")]
    Network(String),
    /// Release metadata couldn't be parsed.
    #[error("could not parse release metadata: {0}")]
    Parse(String),
    /// The update was refused (wrong install method, dev build, etc.).
    #[error("{0}")]
    Refused(String),
    /// A downloaded asset failed its checksum check.
    #[error("checksum mismatch for {asset}: expected {expected}, got {actual}")]
    Checksum {
        asset: String,
        expected: String,
        actual: String,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

// ── Install-method detection ────────────────────────────────────────────────

/// How the running `sessionguard` binary was installed. Determines whether
/// `update` may self-replace it or must defer to a package manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethod {
    /// A plain binary we own (the `install.sh` target: `/usr/local/bin`,
    /// `~/.local/bin`, …). The only method `update` self-replaces.
    Standalone { path: PathBuf },
    /// Installed via `cargo install`. Defer to `cargo install --force`.
    Cargo,
    /// Installed via Homebrew. Defer to `brew upgrade`.
    Homebrew,
    /// Running a `cargo build` artifact from a source checkout. Refuse —
    /// don't clobber a dev build with a release.
    GitCheckout,
}

/// Classify an executable path into an [`InstallMethod`]. Pure (no FS access)
/// so it's unit-testable with synthetic paths.
///
/// Package-manager markers are anchored to the path shapes those managers
/// actually produce (brew prefixes, `~/.cargo/bin`), not bare substrings —
/// a standalone install that merely has "homebrew" somewhere in its path must
/// not be refused, and misclassifying the other way would overwrite a managed
/// binary (the one thing this module promises never to do).
pub fn classify_install_method(exe: &Path) -> InstallMethod {
    let s = exe.to_string_lossy();
    if s.contains("/target/debug/") || s.contains("/target/release/") {
        InstallMethod::GitCheckout
    } else if s.contains("/.cargo/bin/") {
        InstallMethod::Cargo
    } else if s.starts_with("/opt/homebrew/")
        || s.starts_with("/home/linuxbrew/")
        || s.contains("/Cellar/")
        || s.contains("/linuxbrew/")
        || s.contains("/Homebrew/")
    {
        InstallMethod::Homebrew
    } else {
        // Everything else is a manually-placed binary we can overwrite —
        // the install.sh case (/usr/local/bin, ~/.local/bin, /usr/bin, …).
        InstallMethod::Standalone {
            path: exe.to_path_buf(),
        }
    }
}

/// Detect the current process's install method via `current_exe()`.
pub fn detect_install_method() -> Result<InstallMethod, UpdateError> {
    let exe = std::env::current_exe()?;
    // Resolve symlinks so a symlinked launcher classifies by its real location.
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    Ok(classify_install_method(&exe))
}

// ── Version comparison ──────────────────────────────────────────────────────

/// Parse a `vX.Y.Z` (or `X.Y.Z`) tag into `(major, minor, patch)`, ignoring any
/// `-prerelease`/`+build` suffix. Returns `None` if it isn't three numbers.
pub fn parse_version(tag: &str) -> Option<(u64, u64, u64)> {
    let v = tag.trim().trim_start_matches('v');
    let core = v.split(['-', '+']).next().unwrap_or(v);
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None; // more than three components
    }
    Some((major, minor, patch))
}

/// True if `latest` is a strictly newer version than `current`. Unparseable
/// inputs compare as "not newer" (fail safe — never claim an update we can't
/// reason about).
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// Result of a read-only update check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheck {
    /// The running version (no leading `v`).
    pub current: String,
    /// The latest release tag (as published, may carry a leading `v`).
    pub latest: String,
    /// Whether `latest` is strictly newer than `current`.
    pub update_available: bool,
}

// ── Release client (network, behind a trait for tests) ──────────────────────

/// Abstraction over the network calls the updater needs, so tests don't touch
/// the network or the real GitHub API.
pub trait ReleaseClient {
    /// Return the latest release tag for `repo` (e.g. `v0.5.0`).
    fn latest_tag(&self, repo: &str) -> Result<String, UpdateError>;
    /// Fetch a URL's body as text (used for `SHA256SUMS`).
    fn fetch_text(&self, url: &str) -> Result<String, UpdateError>;
    /// Download a URL to `dest`.
    fn fetch_to(&self, url: &str, dest: &Path) -> Result<(), UpdateError>;
}

/// Default [`ReleaseClient`] that shells out to `curl` — already required by
/// `install.sh`, so no new dependency tree.
pub struct CurlReleaseClient;

impl ReleaseClient for CurlReleaseClient {
    fn latest_tag(&self, repo: &str) -> Result<String, UpdateError> {
        let url = format!("https://api.github.com/repos/{repo}/releases/latest");
        let body = self.fetch_text(&url)?;
        let json: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| UpdateError::Parse(e.to_string()))?;
        json.get("tag_name")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| UpdateError::Parse("no tag_name in releases/latest".into()))
    }

    fn fetch_text(&self, url: &str) -> Result<String, UpdateError> {
        let out = std::process::Command::new("curl")
            .args(["-fsSL", "-H", "User-Agent: sessionguard-update", url])
            .output()?;
        if !out.status.success() {
            return Err(UpdateError::Network(format!(
                "curl {url} exited {}: {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn fetch_to(&self, url: &str, dest: &Path) -> Result<(), UpdateError> {
        let out = std::process::Command::new("curl")
            .args(["-fsSL", "-H", "User-Agent: sessionguard-update", url, "-o"])
            .arg(dest)
            .output()?;
        if !out.status.success() {
            return Err(UpdateError::Network(format!(
                "curl {url} exited {}: {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }
}

/// Read-only update check: compare the running version against the latest
/// release. Does not download or modify anything.
pub fn check_update(
    client: &dyn ReleaseClient,
    repo: &str,
    current: &str,
) -> Result<UpdateCheck, UpdateError> {
    let latest = client.latest_tag(repo)?;
    Ok(UpdateCheck {
        current: current.trim_start_matches('v').to_string(),
        update_available: is_newer(&latest, current),
        latest,
    })
}

// ── Performing the update (SU3) ─────────────────────────────────────────────

/// The platform target triple, matching the release asset names that
/// `release.yml` / `install.sh` produce. Refuses platforms with no prebuilt
/// asset (the operator can `cargo install` there).
pub fn target_triple() -> Result<String, UpdateError> {
    let t = if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "aarch64-apple-darwin"
    } else {
        return Err(UpdateError::Refused(
            "no prebuilt release binary for this platform — use `cargo install sessionguard`"
                .into(),
        ));
    };
    Ok(t.to_string())
}

/// Extract the expected SHA-256 for `asset` from a `SHA256SUMS` body
/// (`<hash>  <filename>` lines). Pure; unit-tested.
pub fn expected_sha(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut p = line.split_whitespace();
        let hash = p.next()?;
        let name = p.next()?;
        (name == asset).then(|| hash.to_string())
    })
}

/// SHA-256 of a file, via `sha256sum` or `shasum` (same tools `install.sh`
/// relies on). Kept as a shell-out to avoid a crypto dependency.
fn sha256_file(path: &Path) -> Result<String, UpdateError> {
    let try_cmd = |prog: &str, args: &[&str]| -> Option<String> {
        let out = std::process::Command::new(prog)
            .args(args)
            .arg(path)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| {
                String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
            .filter(|s| !s.is_empty())
    };
    try_cmd("sha256sum", &[])
        .or_else(|| try_cmd("shasum", &["-a", "256"]))
        .ok_or_else(|| {
            UpdateError::Refused("need `sha256sum` or `shasum` to verify the download".into())
        })
}

/// Report of an `update` run (or dry-run).
#[derive(Debug, Clone)]
pub struct UpdateReport {
    pub dry_run: bool,
    pub from: String,
    pub to: String,
    pub steps: Vec<String>,
    /// The retained previous binary, for rollback.
    pub backup: Option<PathBuf>,
}

/// Restart a running SessionGuard daemon after a swap. Best-effort: if there's
/// no systemd user unit (or no daemon), this is a no-op. Returns a human note.
fn restart_daemon_if_running() -> String {
    // Only attempt if the user unit is active; never start a daemon that wasn't
    // already running.
    let active = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "sessionguard.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !active {
        return "no active systemd --user daemon; nothing to restart".to_string();
    }
    match std::process::Command::new("systemctl")
        .args(["--user", "restart", "sessionguard.service"])
        .status()
    {
        Ok(s) if s.success() => "restarted systemd --user sessionguard.service".to_string(),
        _ => "WARNING: failed to restart sessionguard.service — restart it manually".to_string(),
    }
}

/// Replace `dest` with `new_bin`: keep `dest` as `backup`, move the new binary
/// into place, make it executable. Uses `sudo` only if `dest`'s directory isn't
/// writable (the root-owned `/usr/local/bin` case).
fn swap_binary(dest: &Path, new_bin: &Path, backup: &Path) -> Result<(), UpdateError> {
    let dir = dest.parent().unwrap_or(Path::new("/"));
    let dest_name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("sessionguard");
    // Stage the new binary as a temp in the *same directory* so the final step
    // is an atomic same-filesystem rename.
    let tmp = dir.join(format!(".{dest_name}.sg-new-{}", std::process::id()));

    let writable = dir
        .metadata()
        .map(|m| !m.permissions().readonly())
        .unwrap_or(false)
        && std::fs::File::create(dir.join(".sg-update-write-test"))
            .map(|_| {
                let _ = std::fs::remove_file(dir.join(".sg-update-write-test"));
                true
            })
            .unwrap_or(false);

    if writable {
        // Ordering matters: keep `dest` in place until the very last atomic
        // rename, so a failure at any step leaves the *working* binary intact
        // (the previous brick-risk: it renamed dest away first).
        let staged = (|| -> std::io::Result<()> {
            std::fs::copy(new_bin, &tmp)?; // 1. stage new binary as temp
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
            }
            std::fs::copy(dest, backup)?; // 2. copy current aside for rollback (dest stays)
            std::fs::rename(&tmp, dest) // 3. atomic replace — dest is never absent
        })();
        if staged.is_err() {
            let _ = std::fs::remove_file(&tmp); // dest is untouched; just clean the temp
        }
        staged.map_err(UpdateError::Io)
    } else {
        // Root-owned dir: run the whole stage→backup→atomic-mv sequence in ONE
        // `sudo sh -c` with `set -e`, so a partial failure never leaves `dest`
        // missing — `mv -f` atomically replaces it only as the last step.
        // Paths are passed as positional args ($1..$4), not interpolated.
        let script =
            "set -e; cp \"$1\" \"$4\"; chmod 755 \"$4\"; cp \"$2\" \"$3\"; mv -f \"$4\" \"$2\"";
        let st = std::process::Command::new("sudo")
            .args(["sh", "-c", script, "sh"])
            .arg(new_bin)
            .arg(dest)
            .arg(backup)
            .arg(&tmp)
            .status()?;
        if st.success() {
            Ok(())
        } else {
            Err(UpdateError::Refused(
                "sudo swap failed; the original binary was left intact".into(),
            ))
        }
    }
}

/// Security-relevant opt-ins for [`perform_update`], off by default.
#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateOpts {
    /// Honor the `SESSIONGUARD_UPDATE_BASE_URL` override (a code-execution seam;
    /// used only by the offline dogfood/tests via an explicit `--allow-custom-base`).
    pub allow_custom_base: bool,
    /// Permit installing an older release than the running one.
    pub allow_downgrade: bool,
}

/// Perform a self-update of a Standalone install to `tag`. Downloads the
/// asset + `SHA256SUMS`, verifies, swaps (keeping a `.bak-<ver>`), and restarts
/// a running daemon. `dry_run` walks every step touching nothing.
pub fn perform_update(
    client: &dyn ReleaseClient,
    dest: &Path,
    repo: &str,
    tag: &str,
    current: &str,
    dry_run: bool,
    opts: UpdateOpts,
) -> Result<UpdateReport, UpdateError> {
    // Validate the tag shape before it's ever interpolated into a URL — rejects
    // path-traversal / arbitrary strings (`parse_version` returns None for those).
    let Some(tag_ver) = parse_version(tag) else {
        return Err(UpdateError::Refused(format!(
            "`{tag}` is not a valid version tag (expected vX.Y.Z)"
        )));
    };
    // Refuse a downgrade unless explicitly allowed — stops `--to <old-vulnerable>`.
    if !opts.allow_downgrade {
        if let Some(cur) = parse_version(current) {
            if tag_ver < cur {
                return Err(UpdateError::Refused(format!(
                    "{tag} is older than the current {current}; pass --allow-downgrade to force it"
                )));
            }
        }
    }

    let triple = target_triple()?;
    let asset = format!("sessionguard-{triple}.tar.gz");
    // Base URL of the release's assets. The `SESSIONGUARD_UPDATE_BASE_URL`
    // override is a code-execution seam (it points the self-replacing binary at
    // an arbitrary release), so it is honored ONLY when `--allow-custom-base` is
    // passed — the dogfood/test path opts in explicitly; production never does.
    let base = if opts.allow_custom_base {
        std::env::var("SESSIONGUARD_UPDATE_BASE_URL")
            .unwrap_or_else(|_| format!("https://github.com/{repo}/releases/download/{tag}"))
    } else {
        format!("https://github.com/{repo}/releases/download/{tag}")
    };
    let asset_url = format!("{base}/{asset}");
    let sums_url = format!("{base}/SHA256SUMS");
    let backup = dest.with_file_name(format!(
        "{}.bak-{}",
        dest.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("sessionguard"),
        current.trim_start_matches('v')
    ));

    let mut steps = Vec::new();
    if dry_run {
        steps.push(format!("would download {asset_url}"));
        steps.push(format!("would verify against {sums_url}"));
        steps.push(format!(
            "would back up {} → {}",
            dest.display(),
            backup.display()
        ));
        steps.push(format!("would install {tag} to {}", dest.display()));
        steps.push("would restart the daemon if running".to_string());
        return Ok(UpdateReport {
            dry_run: true,
            from: current.trim_start_matches('v').to_string(),
            to: tag.trim_start_matches('v').to_string(),
            steps,
            backup: None,
        });
    }

    // Unpredictable, owner-only workdir. A guessable name in shared /tmp is a
    // symlink-attack target: a local attacker who pre-creates the path could
    // redirect where the verified tarball/binary lands. `create_dir` (not
    // create_dir_all) fails if the path already exists — never adopt a
    // directory we didn't create — and 0700 keeps other users out.
    let tmp = {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("sg-update-{}-{nanos:08x}", std::process::id()));
        std::fs::create_dir(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        dir
    };
    let tarball = tmp.join(&asset);

    // 1. download
    client.fetch_to(&asset_url, &tarball)?;
    steps.push(format!("downloaded {asset}"));

    // 2. verify
    let sums = client.fetch_text(&sums_url)?;
    let expected = expected_sha(&sums, &asset)
        .ok_or_else(|| UpdateError::Refused(format!("SHA256SUMS has no entry for {asset}")))?;
    let actual = sha256_file(&tarball)?;
    if expected != actual {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(UpdateError::Checksum {
            asset,
            expected,
            actual,
        });
    }
    steps.push(format!("verified checksum ({asset})"));

    // 3. extract
    let st = std::process::Command::new("tar")
        .args([
            "-xzf",
            &tarball.to_string_lossy(),
            "-C",
            &tmp.to_string_lossy(),
        ])
        .status()?;
    if !st.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(UpdateError::Refused(
            "failed to extract the downloaded tarball".into(),
        ));
    }
    let new_bin = tmp.join("sessionguard");
    if !new_bin.exists() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(UpdateError::Refused(
            "tarball did not contain a `sessionguard` binary".into(),
        ));
    }

    // 4. swap (keep .bak)
    swap_binary(dest, &new_bin, &backup)?;
    steps.push(format!(
        "installed {tag}; previous kept at {}",
        backup.display()
    ));

    // 5. restart daemon
    steps.push(restart_daemon_if_running());

    let _ = std::fs::remove_dir_all(&tmp);
    Ok(UpdateReport {
        dry_run: false,
        from: current.trim_start_matches('v').to_string(),
        to: tag.trim_start_matches('v').to_string(),
        steps,
        backup: Some(backup),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_install_methods() {
        assert_eq!(
            classify_install_method(Path::new("/usr/local/bin/sessionguard")),
            InstallMethod::Standalone {
                path: PathBuf::from("/usr/local/bin/sessionguard")
            }
        );
        assert_eq!(
            classify_install_method(Path::new("/home/devo/.local/bin/sessionguard")),
            InstallMethod::Standalone {
                path: PathBuf::from("/home/devo/.local/bin/sessionguard")
            }
        );
        assert_eq!(
            classify_install_method(Path::new("/home/devo/.cargo/bin/sessionguard")),
            InstallMethod::Cargo
        );
        assert_eq!(
            classify_install_method(Path::new(
                "/opt/homebrew/Cellar/sessionguard/0.4.3/bin/sessionguard"
            )),
            InstallMethod::Homebrew
        );
        assert_eq!(
            classify_install_method(Path::new(
                "/Users/devo/Droco/side-projects/ai-session-track/target/release/sessionguard"
            )),
            InstallMethod::GitCheckout
        );
        assert_eq!(
            classify_install_method(Path::new("/work/proj/target/debug/sessionguard")),
            InstallMethod::GitCheckout
        );
    }

    #[test]
    fn parses_and_compares_versions() {
        assert_eq!(parse_version("v0.5.0"), Some((0, 5, 0)));
        assert_eq!(parse_version("0.4.3"), Some((0, 4, 3)));
        assert_eq!(parse_version("v1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_version("v1.2"), None);
        assert_eq!(parse_version("nightly"), None);

        assert!(is_newer("v0.5.0", "0.4.3"));
        assert!(is_newer("v0.4.4", "v0.4.3"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(!is_newer("v0.4.3", "0.4.3"));
        assert!(!is_newer("v0.4.2", "0.4.3"));
        // Unparseable → never claims an update.
        assert!(!is_newer("garbage", "0.4.3"));
        assert!(!is_newer("v0.5.0", "garbage"));
    }

    /// In-memory release client for tests — no network.
    struct FakeReleaseClient {
        tag: String,
    }
    impl ReleaseClient for FakeReleaseClient {
        fn latest_tag(&self, _repo: &str) -> Result<String, UpdateError> {
            Ok(self.tag.clone())
        }
        fn fetch_text(&self, _url: &str) -> Result<String, UpdateError> {
            Ok(String::new())
        }
        fn fetch_to(&self, _url: &str, _dest: &Path) -> Result<(), UpdateError> {
            Ok(())
        }
    }

    #[test]
    fn check_update_reports_available_when_behind() {
        let client = FakeReleaseClient {
            tag: "v0.5.0".into(),
        };
        let c = check_update(&client, "x/y", "0.4.3").unwrap();
        assert_eq!(c.current, "0.4.3");
        assert_eq!(c.latest, "v0.5.0");
        assert!(c.update_available);
    }

    #[test]
    fn check_update_reports_current_when_latest() {
        let client = FakeReleaseClient {
            tag: "v0.4.3".into(),
        };
        let c = check_update(&client, "x/y", "0.4.3").unwrap();
        assert!(!c.update_available);
    }

    #[test]
    fn expected_sha_extracts_by_basename() {
        let sums = "\
deadbeef  sessionguard-x86_64-apple-darwin.tar.gz
c0ffee00  sessionguard-x86_64-unknown-linux-gnu.tar.gz
";
        assert_eq!(
            expected_sha(sums, "sessionguard-x86_64-unknown-linux-gnu.tar.gz").as_deref(),
            Some("c0ffee00")
        );
        assert_eq!(expected_sha(sums, "sessionguard-missing.tar.gz"), None);
    }

    #[test]
    fn perform_update_dry_run_touches_nothing() {
        let client = FakeReleaseClient {
            tag: "v0.5.0".into(),
        };
        let dest = PathBuf::from("/usr/local/bin/sessionguard");
        let r = perform_update(
            &client,
            &dest,
            "x/y",
            "v0.5.0",
            "0.4.3",
            true,
            UpdateOpts::default(),
        )
        .unwrap();
        assert!(r.dry_run);
        assert_eq!(r.from, "0.4.3");
        assert_eq!(r.to, "0.5.0");
        assert!(r.backup.is_none());
        assert!(r.steps.iter().any(|s| s.contains("would install")));
    }

    #[test]
    fn perform_update_refuses_invalid_tag() {
        let client = FakeReleaseClient { tag: "v".into() };
        let dest = PathBuf::from("/usr/local/bin/sessionguard");
        let err = perform_update(
            &client,
            &dest,
            "x/y",
            "../../evil",
            "0.5.0",
            false,
            UpdateOpts::default(),
        )
        .unwrap_err();
        assert!(matches!(err, UpdateError::Refused(_)));
    }

    #[test]
    fn perform_update_refuses_downgrade_without_optin() {
        let client = FakeReleaseClient {
            tag: "v0.4.0".into(),
        };
        let dest = PathBuf::from("/usr/local/bin/sessionguard");
        // Older tag than current, no allow_downgrade → refused (even on dry-run).
        let err = perform_update(
            &client,
            &dest,
            "x/y",
            "v0.4.0",
            "0.5.0",
            true,
            UpdateOpts::default(),
        )
        .unwrap_err();
        assert!(matches!(err, UpdateError::Refused(m) if m.contains("older")));

        // With the opt-in it proceeds (dry-run).
        let r = perform_update(
            &client,
            &dest,
            "x/y",
            "v0.4.0",
            "0.5.0",
            true,
            UpdateOpts {
                allow_downgrade: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(r.dry_run);
    }
}
