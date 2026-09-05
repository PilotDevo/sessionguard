// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Fleet-wide session census.
//!
//! Runs the *remote* `sessionguard` over ssh and merges its JSON. The remote
//! binary owns its own filesystem truth — in particular whether a project
//! directory still exists — so orphan status is never re-derived locally.
//! Read-only by construction: the only remote commands are `--version` and
//! `sessions --format json`.

use crate::config::HostSpec;
use crate::sessions::SessionGroup;

/// Oldest remote release that has `sessionguard sessions`.
const MIN_REMOTE_VERSION: (u64, u64, u64) = (0, 7, 0);

#[derive(Debug, thiserror::Error)]
pub enum FleetError {
    /// ssh itself could not reach or authenticate to the host (spawn failure
    /// or ssh's own exit status 255).
    #[error("host `{host}`: unreachable ({detail})")]
    Unreachable { host: String, detail: String },
    /// The host was reached but the remote command exited non-zero — most
    /// often 127, `sessionguard` not on the remote non-interactive PATH.
    #[error("host `{host}`: remote command exited {status} ({detail})")]
    RemoteFailed {
        host: String,
        status: i32,
        detail: String,
    },
    #[error("host `{host}`: sessionguard {found} is too old for `sessions` (needs 0.7.0+); run `sessionguard update` there")]
    TooOld { host: String, found: String },
    #[error("host `{host}`: could not parse remote output ({detail})")]
    BadOutput { host: String, detail: String },
    #[error(
        "host `{host}`: ssh destination `{ssh}` starts with `-`, which ssh would parse as an \
         option rather than a hostname (refusing to avoid local command execution via argv \
         injection)"
    )]
    InvalidDestination { host: String, ssh: String },
    #[error(
        "host `{host}`: `binary` {binary:?} is not a plain path (allowed: letters, digits, \
         `_ - . / ~`, not starting with `-`); it is run through the remote shell, so nothing \
         else is accepted"
    )]
    InvalidBinary { host: String, binary: String },
}

/// Verify a remote `sessionguard --version` output meets the floor.
///
/// Only a line of the form `sessionguard X.Y.Z` counts. The remote command
/// runs through a login shell, so its stdout can carry an MOTD or shell-rc
/// chatter ahead of the real line, and scanning every token for anything
/// version-shaped would happily accept `Fedora 42.0.1` as the version.
pub fn check_remote_version(host: &str, version_output: &str) -> Result<(), FleetError> {
    let found = version_output.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("sessionguard")?;
        let token = rest.split_whitespace().next()?;
        crate::update::parse_version(token).map(|v| (token.to_string(), v))
    });
    match found {
        Some((_, v)) if v >= MIN_REMOTE_VERSION => Ok(()),
        Some((raw, _)) => Err(FleetError::TooOld {
            host: host.to_string(),
            found: raw,
        }),
        None => Err(FleetError::BadOutput {
            host: host.to_string(),
            detail: format!(
                "no `sessionguard X.Y.Z` line in --version output; received: {}",
                snippet(version_output)
            ),
        }),
    }
}

/// Stamp provenance on groups returned by a remote host. Deliberately does NOT
/// touch `orphaned` — that verdict belongs to the host the sessions live on.
pub fn adopt_host(groups: &mut [SessionGroup], host: &str) {
    for g in groups.iter_mut() {
        g.host = host.to_string();
    }
}

/// Parse a remote `sessions --format json` payload into groups.
///
/// A host running 0.7.0 (the version floor this feature targets) emits the
/// OLD shape: `{project_path, decoded, orphaned, tools}` — no `confidence`
/// and no `host` field. Deserializing that straight into today's
/// `SessionGroup` would fail at runtime against exactly the host this
/// feature targets, so the payload is upgraded first:
///
/// - a missing `confidence` is synthesized from `decoded` (0.7.0's only
///   signal): `true` -> `"exact"`, `false` -> `"unresolved"` — this is
///   exactly 0.7.0's semantics, which had no `Inferred` state.
/// - a missing `host` gets a placeholder; [`adopt_host`] overwrites it
///   regardless, so the placeholder value itself is never observed.
fn parse_remote_groups(json: &str, host: &str) -> Result<Vec<SessionGroup>, FleetError> {
    let mut value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| FleetError::BadOutput {
            host: host.to_string(),
            detail: format!("{e}; received: {}", snippet(json)),
        })?;
    if let Some(groups) = value.as_array_mut() {
        for group in groups.iter_mut() {
            let Some(obj) = group.as_object_mut() else {
                continue;
            };
            if !obj.contains_key("confidence") {
                let decoded = obj
                    .get("decoded")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let confidence = if decoded { "exact" } else { "unresolved" };
                obj.insert(
                    "confidence".to_string(),
                    serde_json::Value::String(confidence.to_string()),
                );
            }
            obj.entry("host")
                .or_insert_with(|| serde_json::Value::String("unknown".to_string()));
        }
    }
    serde_json::from_value(value).map_err(|e| FleetError::BadOutput {
        host: host.to_string(),
        detail: format!("{e}; received: {}", snippet(json)),
    })
}

/// A short, quoted prefix of what the remote actually sent. Without it a
/// `BadOutput` reads as a bare serde message ("expected value at line 1
/// column 1") with no hint that the real cause was, say, a log line or an
/// MOTD ahead of the JSON. Truncated on a char boundary so a UTF-8 payload
/// can't panic the error path.
fn snippet(raw: &str) -> String {
    const MAX: usize = 200;
    let trimmed = raw.trim();
    if trimmed.len() <= MAX {
        return format!("{trimmed:?}");
    }
    let mut end = MAX;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{:?}...", &trimmed[..end])
}

/// Reject an `ssh` destination that begins with `-`. Such a value (e.g.
/// `-oProxyCommand=...`) would be consumed by ssh as an OPTION rather than a
/// hostname — the one shape of a configured `HostSpec` that can cause command
/// execution on THIS machine rather than the remote one. Checked before any
/// process is spawned. [`ssh`] additionally places a `--` terminator ahead of
/// the destination (verified to end option parsing on OpenSSH); the explicit
/// check stays because `ssh(1)` does not document `--`, so it is not relied
/// on alone.
fn check_destination(host: &HostSpec) -> Result<(), FleetError> {
    if host.ssh.starts_with('-') {
        return Err(FleetError::InvalidDestination {
            host: host.name.clone(),
            ssh: host.ssh.clone(),
        });
    }
    Ok(())
}

/// The remote `binary` is joined into a command line that the REMOTE shell
/// parses, so it is restricted to a plain path: letters, digits, `_ - . / ~`
/// (`~` so `~/.cargo/bin/sessionguard` expands there), never starting with
/// `-`. Anything else — whitespace, `;`, `$`, quotes — could smuggle a second
/// command onto the host, which would break the read-only guarantee of this
/// module.
fn check_binary(host: &HostSpec) -> Result<(), FleetError> {
    let b = host.binary();
    let plain = !b.is_empty()
        && !b.starts_with('-')
        && b.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '~'));
    if plain {
        Ok(())
    } else {
        Err(FleetError::InvalidBinary {
            host: host.name.clone(),
            binary: b.to_string(),
        })
    }
}

/// Turn a non-zero exit into the error that says what actually happened.
/// ssh reserves exit status 255 for its OWN failures (no route, refused key,
/// timeout); any other status is the remote command's, and 127 in particular
/// is the remote shell saying "command not found" — the host was reached,
/// `sessionguard` just isn't on its non-interactive PATH.
fn classify_failure(host: &str, code: Option<i32>, stderr: String) -> FleetError {
    match code {
        Some(255) | None => FleetError::Unreachable {
            host: host.to_string(),
            detail: stderr,
        },
        Some(127) => FleetError::RemoteFailed {
            host: host.to_string(),
            status: 127,
            detail: format!(
                "{stderr}; the remote login shell cannot find the binary — set \
                 `binary = \"/absolute/path/to/sessionguard\"` for this host in config.toml"
            ),
        },
        Some(c) => FleetError::RemoteFailed {
            host: host.to_string(),
            status: c,
            detail: stderr,
        },
    }
}

/// Run one read-only command on a host. `ConnectTimeout` bounds only the
/// *handshake*; `ServerAliveInterval`/`ServerAliveCountMax` bound the session
/// after it, so a host that accepts the connection and then wedges (hung
/// filesystem, suspended laptop, dropped route) tears down in ~30s instead of
/// blocking `--all-hosts` indefinitely on one member of the fleet. `--` ends
/// ssh's option parsing before the destination.
fn ssh(host: &HostSpec, remote_args: &[&str]) -> Result<String, FleetError> {
    let out = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=10",
            "-o",
            "ServerAliveCountMax=3",
            "--",
            &host.ssh,
        ])
        .args(remote_args)
        .output()
        .map_err(|e| FleetError::Unreachable {
            host: host.name.clone(),
            detail: e.to_string(),
        })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(classify_failure(&host.name, out.status.code(), stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Census one remote host. Read-only: the only commands run there are
/// `<binary> --version` and `<binary> sessions --format json`, where
/// `<binary>` is the host's configured `sessionguard` path.
pub fn remote_census(host: &HostSpec) -> Result<Vec<SessionGroup>, FleetError> {
    check_destination(host)?;
    check_binary(host)?;
    let bin = host.binary();
    let version = ssh(host, &[bin, "--version"])?;
    check_remote_version(&host.name, &version)?;
    let json = ssh(host, &[bin, "sessions", "--format", "json"])?;
    let mut groups = parse_remote_groups(&json, &host.name)?;
    adopt_host(&mut groups, &host.name);
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::DecodeConfidence;

    #[test]
    fn adopt_host_stamps_provenance_without_re_evaluating_orphans() {
        // A remote path that does not exist locally must KEEP the origin host's
        // orphan verdict. Re-deriving it locally would flag every remote group as
        // orphaned (observed: all 8 fedora paths report exists=false on the Mac).
        let mut groups = vec![SessionGroup {
            project_path: "/home/devo/personal/v2".into(),
            confidence: DecodeConfidence::Exact,
            decoded: true,
            orphaned: false, // origin host says it exists there
            host: "local".into(),
            tools: Default::default(),
        }];
        adopt_host(&mut groups, "fedora");
        assert_eq!(groups[0].host, "fedora");
        assert!(
            !groups[0].orphaned,
            "orphan status must come from the origin host, never be re-derived"
        );
    }

    #[test]
    fn remote_version_below_minimum_is_rejected_with_a_named_error() {
        let e = check_remote_version("fedora", "sessionguard 0.6.3").unwrap_err();
        assert!(matches!(e, FleetError::TooOld { .. }));
        assert!(e.to_string().contains("fedora"));
        assert!(e.to_string().contains("0.6.3"));
    }

    #[test]
    fn remote_version_at_minimum_is_accepted() {
        assert!(check_remote_version("fedora", "sessionguard 0.7.0").is_ok());
    }

    #[test]
    fn remote_version_is_read_only_from_the_sessionguard_line() {
        // A login shell's MOTD ahead of the real line must not be mistaken
        // for the version — `Fedora 42.0.1` parses as a version triple.
        assert!(check_remote_version(
            "fedora",
            "Welcome to Fedora 42.0.1\nLast login: today\nsessionguard 0.8.0\n"
        )
        .is_ok());
        let e = check_remote_version("fedora", "Welcome to Fedora 42.0.1\n").unwrap_err();
        assert!(matches!(e, FleetError::BadOutput { .. }), "{e}");
        assert!(
            e.to_string().contains("Fedora"),
            "quotes what was received: {e}"
        );
        // ...and an old release on that same line is still TooOld, not lost.
        let e = check_remote_version("fedora", "Fedora 42.0.1\nsessionguard 0.6.3").unwrap_err();
        assert!(matches!(e, FleetError::TooOld { .. }), "{e}");
    }

    #[test]
    fn remote_census_refuses_a_binary_that_is_not_a_plain_path() {
        // The binary is parsed by the REMOTE shell; a value carrying a second
        // command would break the module's read-only guarantee. Refused before
        // ssh is ever spawned (marker file proves no local execution either).
        let dir = tempfile::TempDir::new().unwrap();
        let marker = dir.path().join("pwned");
        for bad in [
            format!("sessionguard; touch {}", marker.display()),
            "$(id)".to_string(),
            "-oProxyCommand=id".to_string(),
            "".to_string(),
        ] {
            let host = HostSpec {
                name: "h".into(),
                ssh: "devo@192.0.2.10".into(),
                binary: Some(bad.clone()),
            };
            let e = remote_census(&host).unwrap_err();
            assert!(
                matches!(e, FleetError::InvalidBinary { .. }),
                "{bad:?}: {e}"
            );
        }
        assert!(!marker.exists());
        // Plain paths — including `~` — are accepted by the check itself.
        for good in [
            "sessionguard",
            "/opt/homebrew/bin/sessionguard",
            "~/.cargo/bin/sg-0.8",
        ] {
            let host = HostSpec {
                name: "h".into(),
                ssh: "devo@192.0.2.10".into(),
                binary: Some(good.into()),
            };
            assert!(check_binary(&host).is_ok(), "{good}");
        }
    }

    #[test]
    fn exit_status_classification_separates_ssh_failures_from_remote_ones() {
        let unreachable = classify_failure("h", Some(255), "Connection refused".into());
        assert!(matches!(unreachable, FleetError::Unreachable { .. }));

        let not_found = classify_failure("h", Some(127), "zsh:1: command not found".into());
        match &not_found {
            FleetError::RemoteFailed { status, detail, .. } => {
                assert_eq!(*status, 127);
                assert!(
                    detail.contains("binary ="),
                    "must point at the fix: {detail}"
                );
            }
            other => panic!("expected RemoteFailed, got {other}"),
        }
        assert!(
            !not_found.to_string().contains("unreachable"),
            "a reached host must not be reported unreachable: {not_found}"
        );

        let other = classify_failure("h", Some(2), "usage error".into());
        assert!(matches!(other, FleetError::RemoteFailed { status: 2, .. }));
        let killed = classify_failure("h", None, "".into());
        assert!(matches!(killed, FleetError::Unreachable { .. }));
    }

    #[test]
    fn remote_census_refuses_ssh_destination_that_looks_like_an_option() {
        // A destination beginning with `-` (e.g. `-oProxyCommand=...`) is
        // consumed by ssh as an OPTION, not a hostname — argv injection that
        // executes arbitrary commands on THIS machine, never the remote one.
        // remote_census must refuse it before ever invoking `ssh`, so no
        // process is spawned at all: prove that with a marker file a real
        // ProxyCommand invocation would have created.
        let dir = tempfile::TempDir::new().unwrap();
        let marker = dir.path().join("pwned");
        let host = HostSpec {
            name: "evil".into(),
            ssh: format!("-oProxyCommand=touch {}", marker.display()),
            binary: None,
        };

        let e = remote_census(&host).unwrap_err();

        assert!(matches!(e, FleetError::InvalidDestination { .. }));
        assert!(e.to_string().contains("evil"));
        assert!(e.to_string().contains(&host.ssh));
        assert!(
            !marker.exists(),
            "no process must be spawned for a rejected destination"
        );
    }

    #[test]
    fn remote_census_json_upgrades_0_7_0_payload_with_no_confidence_or_host() {
        // Exactly the shape a 0.7.0 host emits: no `confidence`, no `host`.
        // This compatibility path is the difference between the feature
        // working and erroring against the real fleet.
        let json = r#"[
            {
                "project_path": "/home/devo/personal/v2",
                "decoded": false,
                "orphaned": true,
                "tools": {}
            }
        ]"#;
        let groups = parse_remote_groups(json, "fedora").expect("0.7.0-shaped payload must parse");
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].confidence,
            DecodeConfidence::Unresolved,
            "decoded: false must synthesize confidence: unresolved (0.7.0 had no Inferred state)"
        );
        assert!(groups[0].orphaned, "orphaned must pass through unchanged");
    }

    #[test]
    fn remote_census_json_leaves_a_new_shape_payload_confidence_alone() {
        // The other direction of the shim: a payload that ALREADY carries
        // `confidence` must keep it verbatim — synthesizing from `decoded`
        // here would downgrade every `inferred` group to `unresolved`.
        let json = r#"[
            {
                "project_path": "/home/devo/work/app",
                "decoded": false,
                "confidence": "inferred",
                "orphaned": true,
                "host": "local",
                "tools": {}
            }
        ]"#;
        let groups = parse_remote_groups(json, "fedora").expect("new-shape payload must parse");
        assert_eq!(
            groups[0].confidence,
            DecodeConfidence::Inferred,
            "an existing confidence must survive the compat shim unmodified"
        );
    }

    #[test]
    fn remote_census_json_parses_a_future_payload_that_dropped_decoded() {
        // `decoded` is scheduled for removal. `#[serde(default)]` is what
        // keeps an older local binary able to read a newer remote host once
        // that lands — the shim only upgrades in the older-remote direction.
        let json = r#"[
            {
                "project_path": "/home/devo/work/app",
                "confidence": "exact",
                "orphaned": false,
                "host": "local",
                "tools": {}
            }
        ]"#;
        let groups =
            parse_remote_groups(json, "fedora").expect("payload without `decoded` must parse");
        assert_eq!(groups[0].confidence, DecodeConfidence::Exact);
        assert!(!groups[0].decoded, "missing `decoded` defaults to false");
    }

    #[test]
    fn bad_output_error_quotes_what_the_remote_actually_sent() {
        // The realistic failure: a `WARN` line (or an MOTD) ahead of the JSON.
        // A bare serde message can't be acted on; the received bytes can.
        let e = parse_remote_groups("WARN session_store declares…\n[]", "fedora").unwrap_err();
        let msg = e.to_string();
        assert!(matches!(e, FleetError::BadOutput { .. }));
        assert!(
            msg.contains("WARN"),
            "error must quote the received bytes, got: {msg}"
        );
    }
}
