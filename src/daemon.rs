// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Daemon lifecycle management.
//!
//! Handles starting/stopping the SessionGuard daemon, PID file
//! management, and signal handling for graceful shutdown.

use std::path::PathBuf;

use tokio::signal;
use tracing::{info, warn};

use crate::config::Config;
use crate::error::{Error, Result};

/// PID file location.
fn pid_file_path() -> PathBuf {
    Config::data_dir().join("sessionguard.pid")
}

/// Write the current process PID to the PID file.
///
/// Uses `create_new` (O_EXCL) so acquiring the PID file is ATOMIC — two
/// daemons racing to start cannot both succeed (the old check-then-write had a
/// TOCTOU window where both saw "no daemon" and both wrote). If the file
/// already exists it's either a live daemon (refuse) or stale (remove and
/// retry the exclusive create once).
pub fn write_pid_file() -> Result<()> {
    use std::io::Write;
    let pid_path = pid_file_path();
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    for attempt in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&pid_path)
        {
            Ok(mut f) => {
                f.write_all(std::process::id().to_string().as_bytes())?;
                f.sync_all()?;
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt == 0 => {
                // Live daemon → refuse. Stale/foreign file → clear and retry.
                if let Ok(Some(existing)) = read_pid() {
                    if existing != std::process::id() && is_sessionguard_process(existing) {
                        return Err(Error::Daemon(format!(
                            "another sessionguard daemon is already running (PID {existing})"
                        )));
                    }
                }
                let _ = std::fs::remove_file(&pid_path);
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::Daemon(
        "could not acquire the PID file (another daemon raced us to it)".into(),
    ))
}

/// Unconditionally delete the PID file. For deliberate operator cleanup of a
/// STALE file (`stop` after verifying the process is gone) — the daemon's own
/// exit path uses [`remove_pid_file`], which only deletes its own entry.
pub fn clear_pid_file() -> Result<()> {
    let pid_path = pid_file_path();
    if pid_path.exists() {
        std::fs::remove_file(&pid_path)?;
    }
    Ok(())
}

/// Remove the PID file — but only if it still records OUR pid. A losing racer
/// or late guard must never delete the winner's PID file.
pub fn remove_pid_file() -> Result<()> {
    let pid_path = pid_file_path();
    if pid_path.exists() {
        let ours = std::fs::read_to_string(&pid_path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            == Some(std::process::id());
        if ours {
            std::fs::remove_file(&pid_path)?;
        }
    }
    Ok(())
}

/// Read the PID from the PID file, if it exists.
pub fn read_pid() -> Result<Option<u32>> {
    let pid_path = pid_file_path();
    if !pid_path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&pid_path)?;
    let pid = content
        .trim()
        .parse::<u32>()
        .map_err(|e| Error::Daemon(format!("invalid PID file content: {e}")))?;
    Ok(Some(pid))
}

/// Check if a SessionGuard daemon is currently running.
///
/// Verifies both that the stored PID is alive AND that the process is actually
/// sessionguard — not an unrelated process that recycled the PID after a crash.
/// On non-Unix platforms, returns `true` whenever a PID file exists.
pub fn is_running() -> bool {
    read_pid()
        .ok()
        .flatten()
        .is_some_and(is_sessionguard_process)
}

/// Ask a running daemon to reload its watch set (SIGHUP). Best-effort; returns
/// whether a signal was sent. Lets `watch`/`unwatch` take effect without a
/// restart.
pub fn signal_reload() -> bool {
    #[cfg(unix)]
    {
        if let Ok(Some(pid)) = read_pid() {
            if is_sessionguard_process(pid) {
                return unsafe { libc::kill(pid as i32, libc::SIGHUP) == 0 };
            }
        }
        false
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Whether `pid` is a live process that is a sessionguard daemon.
///
/// A bare liveness check (`kill(pid, 0)`) is not enough: after a crash without
/// cleanup + a reboot, the OS can recycle the stored PID to an unrelated
/// process, and `stop`/`status` would then signal or report *that* process.
/// So we also confirm the process command is sessionguard.
fn is_sessionguard_process(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // Must exist (signal 0 sends nothing).
        if unsafe { libc::kill(pid as i32, 0) } != 0 {
            return false;
        }
        // ...and its command must be sessionguard. `ps -p <pid> -o comm=`
        // works on both Linux and macOS (comm = executable basename).
        match std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
        {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
                .to_lowercase()
                .contains("sessionguard"),
            // If `ps` isn't available, fall back to liveness-only rather than
            // refusing to ever stop a genuine daemon.
            _ => true,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Run the daemon event loop until a shutdown signal is received.
pub async fn run(config: &Config) -> Result<()> {
    write_pid_file()?;
    info!("daemon started (PID {})", std::process::id());

    // RAII guard: removes the PID file on ANY exit from this scope (normal
    // shutdown, early error, panic-recovered drop).
    struct PidGuard;
    impl Drop for PidGuard {
        fn drop(&mut self) {
            let _ = remove_pid_file();
        }
    }
    let _pid_guard = PidGuard;

    // Initialize subsystems
    let tool_registry = crate::tools::ToolRegistry::new_with_config(config)?;
    let registry = crate::registry::Registry::open_default()?;
    let event_log = crate::event_log::EventLog::open_default()?;

    // Start filesystem watcher over the configured roots AND every registered
    // project's parent, so a project tracked via `watch` (which may live outside
    // any configured root) is actually monitored.
    let watch_set = build_watch_set(config, &registry);
    let mut watcher = crate::watcher::FsWatcher::new(&watch_set, &config.watch_mode)?;

    info!(watch_roots = ?watch_set, "watching for filesystem events");

    // Main event loop
    loop {
        tokio::select! {
            Some(event) = watcher.events.recv() => {
                tracing::debug!(?event, "received filesystem event");
                handle_session_event(event, &registry, &tool_registry, &event_log);
            }
            _ = reload_signal() => {
                // SIGHUP: pick up newly-registered projects without a restart
                // (the `watch` command sends this to us).
                let set = build_watch_set(config, &registry);
                match crate::watcher::FsWatcher::new(&set, &config.watch_mode) {
                    Ok(w) => {
                        watcher = w;
                        info!(watch_roots = ?set, "reloaded watch set (SIGHUP)");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "watch reload failed; keeping previous set");
                    }
                }
            }
            _ = shutdown_signal() => {
                info!("shutdown signal received");
                break;
            }
        }
    }

    info!("daemon stopped");
    Ok(())
}

/// The set of directories the daemon should watch: the configured `watch_roots`
/// plus the parent directory of every registered project (so a project renamed
/// or moved is seen even if it lives outside a configured root). Deduplicated
/// and filtered to existing directories.
fn build_watch_set(
    config: &Config,
    registry: &crate::registry::Registry,
) -> Vec<std::path::PathBuf> {
    let mut set: Vec<std::path::PathBuf> = config.watch_roots.clone();
    for p in registry.list_projects().unwrap_or_default() {
        if let Some(parent) = p.path.parent() {
            set.push(parent.to_path_buf());
        }
    }
    set.sort();
    set.dedup();
    set.retain(|p| p.is_dir());
    set
}

/// Resolve when a reload (SIGHUP) is requested. On non-Unix it never fires.
#[cfg(unix)]
async fn reload_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::hangup()) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(_) => std::future::pending::<()>().await,
    }
}

#[cfg(not(unix))]
async fn reload_signal() {
    std::future::pending::<()>().await;
}

/// Dispatch a filesystem event through the detector → reconciler pipeline.
///
/// For `Moved` events with both `from` and `to` paths: detects tools at the
/// new location, reconciles each tool's artifacts, updates the registry.
/// Errors are logged in place — this function never fails. Partial move events
/// (missing `from` or `to`) are silently skipped.
fn handle_session_event(
    event: crate::watcher::SessionEvent,
    registry: &crate::registry::Registry,
    tool_registry: &crate::tools::ToolRegistry,
    event_log: &crate::event_log::EventLog,
) {
    use crate::watcher::SessionEvent;

    match event {
        SessionEvent::Moved {
            from: Some(old_path),
            to: Some(new_path),
        } => {
            info!(from = %old_path.display(), to = %new_path.display(), "project moved");

            // Detect which AI tools have artifacts at the new location
            let detected = crate::detector::detect_tools(&new_path, tool_registry);
            if detected.is_empty() {
                tracing::debug!("no AI session artifacts at new path, skipping");
                return;
            }

            // Reconcile each detected tool
            for detection in &detected {
                if let Some(tool) = tool_registry.get(&detection.tool_name) {
                    let result =
                        crate::reconciler::reconcile(tool, &old_path, &new_path, event_log);
                    if result.success {
                        info!(
                            tool = %detection.display_name,
                            rewrites = result.actions_taken.len(),
                            "reconciled session artifacts"
                        );
                    } else {
                        warn!(
                            tool = %detection.display_name,
                            error = ?result.error,
                            "reconciliation failed"
                        );
                    }
                }
            }

            // Update registry: re-register under new path and drop old entry
            match registry.register_project(&new_path) {
                Ok(new_id) => {
                    for detection in &detected {
                        for artifact in &detection.artifact_files {
                            let _ = registry.add_artifact(new_id, &detection.tool_name, artifact);
                        }
                    }
                }
                Err(e) => warn!(error = %e, "failed to register new project path"),
            }
            if let Err(e) = registry.unregister_project(&old_path) {
                tracing::debug!(error = %e, "could not remove old registry entry (may not have been watched)");
            }
        }
        SessionEvent::Moved { .. } => {
            // Partial move event — notify only emits both paths on some platforms
            tracing::debug!("partial move event (missing from/to), skipping");
        }
        SessionEvent::Removed(path) => {
            // On Linux, a rename's "from" half arrives here. Without cookie
            // pairing we can't confidently reconcile — but if the old path is
            // in the registry and no longer exists on disk, that's a strong
            // signal something moved. Logged as info for now; reconciliation
            // via rename pairing is tracked as a v0.3 feature.
            if !path.exists() {
                if let Ok(projects) = registry.list_projects() {
                    if projects.iter().any(|p| p.path == path) {
                        info!(
                            path = %path.display(),
                            "tracked project path vanished — manual reconcile or wait for matching create"
                        );
                        return;
                    }
                }
            }
            tracing::debug!(path = %path.display(), "path removed");
        }
        SessionEvent::Created(path) => {
            tracing::debug!(path = %path.display(), "path created");
        }
    }
}

/// Wait for a shutdown signal (SIGINT or SIGTERM).
///
/// Signal-registration errors are logged and the failing source is replaced
/// with a pending future — we never panic inside the daemon event loop.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = signal::ctrl_c().await {
            warn!(error = %e, "failed to listen for ctrl+c");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                warn!(error = %e, "failed to listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

#[cfg(test)]
mod tests {
    use super::handle_session_event;
    use crate::event_log::EventLog;
    use crate::registry::Registry;
    use crate::tools::ToolRegistry;
    use crate::watcher::SessionEvent;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    #[cfg(unix)]
    #[test]
    fn pid_identity_rejects_non_sessionguard_process() {
        // PID 1 (init/launchd) is always alive but is NOT sessionguard, so a
        // liveness-only check would wrongly treat it as our daemon. The
        // identity check must reject it.
        assert!(
            !super::is_sessionguard_process(1),
            "PID 1 is not sessionguard and must not be treated as a running daemon"
        );
        // A very high, almost-certainly-dead PID is also rejected.
        assert!(!super::is_sessionguard_process(4_000_000_000));
    }

    fn claude_project(root: &Path, name: &str) -> PathBuf {
        let p = root.join(name);
        std::fs::create_dir_all(p.join(".claude")).unwrap();
        std::fs::write(p.join("CLAUDE.md"), "# test").unwrap();
        std::fs::write(
            p.join(".claude/settings.json"),
            format!(r#"{{"project_path": "{}","model": "opus"}}"#, p.display()),
        )
        .unwrap();
        p
    }

    // The core pipeline seam: a paired Moved event detects the tool at the new
    // location, reconciles its artifacts, registers the new path, and logs it.
    #[test]
    fn handle_session_event_moved_reconciles_and_reregisters() {
        let dir = TempDir::new().unwrap();
        let old = claude_project(dir.path(), "alpha");
        let new = dir.path().join("beta");
        std::fs::rename(&old, &new).unwrap(); // settings.json still names `old`

        let registry = Registry::open_in_memory().unwrap();
        let tools = ToolRegistry::new().unwrap();
        let log = EventLog::open_in_memory().unwrap();

        handle_session_event(
            SessionEvent::Moved {
                from: Some(old.clone()),
                to: Some(new.clone()),
            },
            &registry,
            &tools,
            &log,
        );

        let settings = std::fs::read_to_string(new.join(".claude/settings.json")).unwrap();
        assert!(
            settings.contains(&new.display().to_string()),
            "project_path should be rewritten to the new path"
        );
        assert!(
            !settings.contains(&old.display().to_string()),
            "the old path should be gone"
        );

        let projects = registry.list_projects().unwrap();
        assert!(
            projects.iter().any(|p| p.path == new),
            "new path should be registered"
        );
        assert!(
            !projects.iter().any(|p| p.path == old),
            "old path should not be registered"
        );
        assert!(
            log.count().unwrap() >= 1,
            "a reconcile event should be logged"
        );
    }

    // Moved to a location with no AI artifacts: detect finds nothing, so the
    // registry and event log stay untouched.
    #[test]
    fn handle_session_event_no_artifacts_leaves_registry_empty() {
        let dir = TempDir::new().unwrap();
        let new = dir.path().join("plain-new");
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("README.md"), "# plain").unwrap();

        let registry = Registry::open_in_memory().unwrap();
        let tools = ToolRegistry::new().unwrap();
        let log = EventLog::open_in_memory().unwrap();

        handle_session_event(
            SessionEvent::Moved {
                from: Some(dir.path().join("plain-old")),
                to: Some(new),
            },
            &registry,
            &tools,
            &log,
        );

        assert!(registry.list_projects().unwrap().is_empty());
        assert_eq!(log.count().unwrap(), 0);
    }

    // A partial move (one half of the pair missing) is skipped, not acted on.
    #[test]
    fn handle_session_event_partial_move_is_noop() {
        let dir = TempDir::new().unwrap();
        let registry = Registry::open_in_memory().unwrap();
        let tools = ToolRegistry::new().unwrap();
        let log = EventLog::open_in_memory().unwrap();

        handle_session_event(
            SessionEvent::Moved {
                from: Some(dir.path().join("x")),
                to: None,
            },
            &registry,
            &tools,
            &log,
        );

        assert!(registry.list_projects().unwrap().is_empty());
        assert_eq!(log.count().unwrap(), 0);
    }

    // A Removed event for a tracked-but-vanished path is informational only —
    // it never mutates the registry or logs a reconcile.
    #[test]
    fn handle_session_event_removed_tracked_path_does_not_mutate() {
        let dir = TempDir::new().unwrap();
        let registry = Registry::open_in_memory().unwrap();
        let tools = ToolRegistry::new().unwrap();
        let log = EventLog::open_in_memory().unwrap();

        let gone = dir.path().join("vanished");
        std::fs::create_dir_all(&gone).unwrap();
        registry.register_project(&gone).unwrap();
        std::fs::remove_dir_all(&gone).unwrap();

        handle_session_event(SessionEvent::Removed(gone.clone()), &registry, &tools, &log);

        assert!(
            registry
                .list_projects()
                .unwrap()
                .iter()
                .any(|p| p.path == gone),
            "the entry should remain (Removed is informational)"
        );
        assert_eq!(log.count().unwrap(), 0);
    }
}
