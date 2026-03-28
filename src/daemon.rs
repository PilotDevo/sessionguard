//! Daemon lifecycle management.
//!
//! Handles starting/stopping the SessionGuard daemon, PID file
//! management, and signal handling for graceful shutdown.

use std::path::PathBuf;

use tokio::signal;
use tracing::info;

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
    let _tool_registry = crate::tools::ToolRegistry::new()?;
    let _registry = crate::registry::Registry::open_default()?;
    let _event_log = crate::event_log::EventLog::open_default()?;

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
                // TODO: dispatch to detector → reconciler pipeline
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
