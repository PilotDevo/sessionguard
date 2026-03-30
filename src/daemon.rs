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
pub fn write_pid_file() -> Result<()> {
    let pid_path = pid_file_path();
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&pid_path, std::process::id().to_string())?;
    Ok(())
}

/// Remove the PID file.
pub fn remove_pid_file() -> Result<()> {
    let pid_path = pid_file_path();
    if pid_path.exists() {
        std::fs::remove_file(&pid_path)?;
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

/// Check if a daemon is currently running.
///
/// On non-Unix platforms, returns `true` whenever a PID file exists
/// (process liveness cannot be verified without `kill(pid, 0)`).
pub fn is_running() -> bool {
    read_pid().ok().flatten().is_some_and(process_exists)
}

/// Check if a process with the given PID exists.
fn process_exists(pid: u32) -> bool {
    // On Unix, signal 0 checks existence without sending a signal.
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        // Fallback: assume running if PID file exists.
        true
    }
}

/// Run the daemon event loop until a shutdown signal is received.
pub async fn run(config: &Config) -> Result<()> {
    write_pid_file()?;
    info!("daemon started (PID {})", std::process::id());

    // Initialize subsystems
    let tool_registry = crate::tools::ToolRegistry::new_with_config(config)?;
    let registry = crate::registry::Registry::open_default()?;
    let event_log = crate::event_log::EventLog::open_default()?;

    // Start filesystem watcher
    let mut watcher = crate::watcher::FsWatcher::new(&config.watch_roots, &config.watch_mode)?;

    info!(
        watch_roots = ?config.watch_roots,
        "watching for filesystem events"
    );

    // Main event loop
    loop {
        tokio::select! {
            Some(event) = watcher.events.recv() => {
                tracing::debug!(?event, "received filesystem event");
                handle_session_event(event, &registry, &tool_registry, &event_log);
            }
            _ = shutdown_signal() => {
                info!("shutdown signal received");
                break;
            }
        }
    }

    remove_pid_file()?;
    info!("daemon stopped");
    Ok(())
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
            tracing::debug!(path = %path.display(), "path removed");
        }
        SessionEvent::Created(path) => {
            tracing::debug!(path = %path.display(), "path created");
        }
    }
}

/// Wait for a shutdown signal (SIGINT or SIGTERM).
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to listen for ctrl+c");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to listen for SIGTERM")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
