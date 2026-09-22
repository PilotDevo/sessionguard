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

    // The home whose session stores get re-keyed on a move. Resolved ONCE
    // here rather than per event: it cannot change under a running daemon,
    // and re-resolving would stat the filesystem on every rename. An empty
    // path means "unresolvable" and disables re-keying (see `rekey_stores`)
    // rather than silently resolving `~/...` against the working directory.
    let census_root = crate::config::home_dir().unwrap_or_else(|| {
        warn!("cannot resolve home directory; session stores will NOT be re-keyed");
        std::path::PathBuf::new()
    });

    // Bound the activity log at startup. Observability must not become the
    // thing that fills the disk.
    match event_log.prune_activity(config.activity_retention_days, config.activity_max_rows) {
        Ok(n) if n > 0 => info!(pruned = n, "pruned old activity rows"),
        Ok(_) => {}
        Err(e) => warn!(error = %e, "could not prune the activity log"),
    }

    // Which directories are projects. Everything else a rename touches is
    // ignored before any store is read — see `known.rs`.
    let mut known = crate::known::KnownProjects::build(&census_root, &tool_registry, &registry);
    info!(projects = known.len(), "indexed known projects");

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
                handle_session_event(
                    &census_root,
                    event,
                    &registry,
                    &tool_registry,
                    &event_log,
                    &mut known,
                );
            }
            _ = reload_signal() => {
                // SIGHUP: pick up newly-registered projects without a restart
                // (the `watch` command sends this to us). The watch set is
                // updated IN PLACE — replacing the watcher dropped events
                // still queued in its channel, so a project moved right after
                // `watch` lost its move — and only then is the project index
                // rebuilt, so the rebuild can't widen that window either.
                let set = build_watch_set(config, &registry);
                match watcher.update_roots(&set) {
                    Ok(()) => info!(watch_roots = ?watcher.watched(), "reloaded watch set (SIGHUP)"),
                    Err(e) => warn!(error = %e, "some watch roots could not be added"),
                }
                known = crate::known::KnownProjects::build(&census_root, &tool_registry, &registry);
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
    // Canonicalize, then drop any root inside another. The same directory
    // spelled two ways (`/var/…` and `/private/var/…` on macOS), or a folder
    // plus its own parent, would otherwise be watched twice — and every event
    // under it reported twice.
    let mut set: Vec<std::path::PathBuf> = set
        .into_iter()
        .filter(|p| p.is_dir())
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .collect();
    set.sort();
    set.dedup();
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    for p in set {
        if !out.iter().any(|o| p.starts_with(o)) {
            out.push(p);
        }
    }
    out
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
/// Re-key every declared session store from `old_path` to `new_path`.
///
/// This is the reconcile that matters for Claude Code, Codex and OpenCode:
/// none of them keep the project path inside the project, so rewriting
/// in-project files moves nothing for them. Failures are logged, never fatal —
/// a daemon that dies on one unhappy store stops watching everything else.
fn rekey_stores(
    census_root: &std::path::Path,
    tool_registry: &crate::tools::ToolRegistry,
    moved: (&std::path::Path, &std::path::Path),
    pairs: &[(std::path::PathBuf, std::path::PathBuf)],
    event_log: &crate::event_log::EventLog,
) {
    if census_root.as_os_str().is_empty() {
        warn!("no census root; session stores not re-keyed");
        return;
    }
    let env = |var: &str| std::env::var(var).ok();
    let stores = crate::sessions::resolve_stores(tool_registry.all(), Some(&env));
    for report in
        crate::rekey::rekey_pairs(census_root, &stores, moved, pairs, Some(event_log), false)
    {
        // Outcomes are recorded by `rekey_pairs` itself (same for the CLI
        // path). This is the daemon's operator-facing summary line only.
        if let Some(e) = &report.error {
            warn!(tool = %report.tool, "session store not re-keyed: {e}");
        } else if report.applied {
            info!(
                tool = %report.tool,
                project = %report.plan.new_path,
                actions = report.plan.actions.len(),
                undo_id = ?report.log_id,
                "session store re-keyed to the new project path"
            );
        }
    }
}

/// Minimum age before a miss triggers a full index rebuild. A rebuild is a
/// census (~20 ms warm on a real machine), so this caps the cost of a burst of
/// unrelated directory renames — a Cargo build renames incremental dirs — at
/// one census per interval, while still recognising a project that got its
/// first session moments ago.
const INDEX_REBUILD_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(2);

/// Which known projects a directory move carries, or `None` if it carries
/// none. On a miss, first re-reads the registry (cheap — it covers a project
/// `watch`ed a moment before being moved), then, if the index is old enough,
/// rebuilds it from the census.
fn known_pairs(
    census_root: &std::path::Path,
    tool_registry: &crate::tools::ToolRegistry,
    registry: &crate::registry::Registry,
    known: &mut crate::known::KnownProjects,
    from: &std::path::Path,
    to: &std::path::Path,
) -> Option<Vec<(std::path::PathBuf, std::path::PathBuf)>> {
    let pairs = known.pairs_for_move(from, to);
    if !pairs.is_empty() {
        return Some(pairs);
    }
    if let Ok(projects) = registry.list_projects() {
        let home = (!census_root.as_os_str().is_empty()).then_some(census_root);
        let fresh =
            crate::known::KnownProjects::from_keys(projects.into_iter().map(|p| p.path), home);
        let pairs = fresh.pairs_for_move(from, to);
        if !pairs.is_empty() {
            return Some(pairs);
        }
    }
    if known.age() >= INDEX_REBUILD_MIN_AGE {
        *known = crate::known::KnownProjects::build(census_root, tool_registry, registry);
        let pairs = known.pairs_for_move(from, to);
        if !pairs.is_empty() {
            return Some(pairs);
        }
    }
    None
}

/// `census_root` is the home directory whose session stores get re-keyed. It
/// is passed in rather than read from the environment so the caller owns it:
/// `run()` resolves it once at startup (cheaper than per-event), and tests
/// point it at a temp dir instead of the operator's real `$HOME`. Setting
/// `HOME` in tests would be process-global — a data race across parallel test
/// threads, and `set_var` is `unsafe` in the 2024 edition.
fn handle_session_event(
    census_root: &std::path::Path,
    event: crate::watcher::SessionEvent,
    registry: &crate::registry::Registry,
    tool_registry: &crate::tools::ToolRegistry,
    event_log: &crate::event_log::EventLog,
    known: &mut crate::known::KnownProjects,
) {
    use crate::watcher::SessionEvent;

    match event {
        SessionEvent::Moved {
            from: Some(old_path),
            to: Some(new_path),
        } => {
            // 1. Only a directory can be a project. File renames — an
            //    editor's atomic save, git's lock-file dance — are the vast
            //    majority of rename events in a working tree, and can never
            //    move a project. One `stat` rules them out.
            if !new_path.is_dir() {
                tracing::trace!(to = %new_path.display(), "file rename; not a project move");
                return;
            }

            // 2. Only act on a directory that IS, or CONTAINS, a known
            //    project. Through v0.10 every rename planned a re-key across
            //    every session store (~1.2 s and ~3.9 GB per event on a real
            //    machine), so a `cargo build` or `git commit` inside a watched
            //    tree hammered the host. See `known.rs`.
            let pairs = match known_pairs(
                census_root,
                tool_registry,
                registry,
                known,
                &old_path,
                &new_path,
            ) {
                Some(p) => p,
                None => {
                    tracing::debug!(
                        from = %old_path.display(),
                        to = %new_path.display(),
                        "directory rename of no known project; ignoring"
                    );
                    return;
                }
            };
            info!(
                from = %old_path.display(),
                to = %new_path.display(),
                projects = pairs.len(),
                "project moved"
            );

            // 3. Re-key the home-dir session stores FIRST, and before any
            //    in-project detection. `detect_tools` scans the PROJECT
            //    directory, but a `session_store` lives under `$HOME` — a
            //    project can have a year of Claude Code history and not one
            //    `.claude/` file inside it. v0.9.0 ran this after an early
            //    return on "no in-project artifacts" and so never re-keyed the
            //    case it exists for. (Regression test:
            //    `handle_session_event_moved_rekeys_the_store_under_the_given_root`.)
            //    All projects the move carried are re-keyed as ONE undo.
            rekey_stores(
                census_root,
                tool_registry,
                (&old_path, &new_path),
                &pairs,
                event_log,
            );

            // 4. Per moved project: in-project reconcile, then the registry.
            for (old, new) in &pairs {
                let detected = crate::detector::detect_tools(new, tool_registry);
                for detection in &detected {
                    if let Some(tool) = tool_registry.get(&detection.tool_name) {
                        let result = crate::reconciler::reconcile(tool, old, new, event_log);
                        // Recorded whatever it was — including a no-op. A store
                        // of actions taken cannot answer "why did nothing happen?".
                        crate::activity::ActivityRecord::new(
                            crate::activity::ActivityKind::Reconcile,
                            result.outcome,
                        )
                        .tool(&result.tool_name)
                        .project(new.display().to_string())
                        .emit(Some(event_log));
                    }
                }

                let was_registered = registry
                    .list_projects()
                    .map(|ps| ps.iter().any(|p| &p.path == old))
                    .unwrap_or(false);
                if !was_registered && detected.is_empty() {
                    continue;
                }
                match registry.register_project(new) {
                    Ok(new_id) => {
                        for detection in &detected {
                            for artifact in &detection.artifact_files {
                                let _ =
                                    registry.add_artifact(new_id, &detection.tool_name, artifact);
                            }
                        }
                    }
                    Err(e) => warn!(error = %e, "failed to register new project path"),
                }
                if let Err(e) = registry.unregister_project(old) {
                    tracing::debug!(error = %e, "could not remove old registry entry");
                }
            }
            known.apply_move(&pairs);
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
    use crate::known::KnownProjects;
    use crate::registry::Registry;
    use crate::tools::{PathFieldSpec, ReconcileStrategy, ToolDefinition, ToolRegistry};
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

    // `claude_code` declares no `path_fields` (see the honesty patch — it
    // names no in-project file that actually holds its path). This pipeline
    // test needs a tool with a real rewritable field, so it registers its
    // own synthetic one rather than leaning on a builtin's fictional one.
    fn synthetic_json_tool() -> ToolDefinition {
        ToolDefinition {
            name: "test_json_tool".to_string(),
            display_name: "Test JSON Tool".to_string(),
            session_patterns: vec![".testtool/".to_string()],
            path_fields: vec![PathFieldSpec {
                file: ".testtool/settings.json".to_string(),
                field: "project_path".to_string(),
                format: "json".to_string(),
            }],
            on_move: ReconcileStrategy::RewritePaths,
            version: None,
            binary: None,
            home_dir_layout: None,
            session_store: None,
        }
    }

    fn synthetic_project(root: &Path, name: &str) -> PathBuf {
        let p = root.join(name);
        std::fs::create_dir_all(p.join(".testtool")).unwrap();
        std::fs::write(
            p.join(".testtool/settings.json"),
            format!(r#"{{"project_path": "{}","model": "opus"}}"#, p.display()),
        )
        .unwrap();
        p
    }

    // The daemon's whole reason to exist for Claude Code / Codex / OpenCode:
    // none of them keep the project path inside the project, so a move must
    // re-key their HOME-DIR store. v0.9.0 wired this up with no test at all.
    //
    // This also pins the isolation contract: the store that moves is the one
    // under the `census_root` PASSED IN. Before that argument existed, this
    // path resolved the operator's real `$HOME` and walked their actual
    // multi-GB session stores on every test run.
    #[test]
    fn handle_session_event_moved_rekeys_the_store_under_the_given_root() {
        let home = TempDir::new().unwrap();
        let old = home.path().join("work/app");
        std::fs::create_dir_all(&old).unwrap();

        // A Claude Code store dir for `old`, named the way the real tool
        // names it, with the true path recorded inside the transcript.
        let enc: String = old
            .display()
            .to_string()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let store = home.path().join(".claude/projects").join(&enc);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(
            store.join("s.jsonl"),
            format!("{{\"cwd\":\"{}\"}}\n", old.display()),
        )
        .unwrap();

        let new = home.path().join("work/moved");
        let registry = Registry::open_in_memory().unwrap();
        let tools = ToolRegistry::new().unwrap();
        // Indexed BEFORE the move, as the daemon does at startup: the store is
        // keyed to `old`, which is what makes this directory a known project.
        let mut known = KnownProjects::build(home.path(), &tools, &registry);
        assert_eq!(known.len(), 1, "the census indexes the project's store key");

        std::fs::rename(&old, &new).unwrap();
        let log = EventLog::open_in_memory().unwrap();
        handle_session_event(
            home.path(),
            SessionEvent::Moved {
                from: Some(old.clone()),
                to: Some(new.clone()),
            },
            &registry,
            &tools,
            &log,
            &mut known,
        );

        // The census — the operator-visible truth — must now report the new
        // path, with the session intact and not orphaned.
        let stores = crate::sessions::resolve_stores(tools.all(), None);
        let groups = crate::sessions::census(home.path(), &stores, false);
        let g = groups
            .iter()
            .find(|g| g.project_path == new.display().to_string())
            .unwrap_or_else(|| {
                panic!(
                    "store did not follow the move; census says {:?}",
                    groups.iter().map(|g| &g.project_path).collect::<Vec<_>>()
                )
            });
        assert!(!g.orphaned, "a re-keyed project is live, not orphaned");
        assert_eq!(g.tools["claude_code"].count, 1, "the session survived");
        assert!(
            !groups
                .iter()
                .any(|g| g.project_path == old.display().to_string()),
            "nothing may still be keyed to the old path"
        );

        // ...and it is undoable, like every other mutation this daemon makes.
        assert!(
            log.latest_pending_rekey().unwrap().is_some(),
            "the re-key must be recorded so `undo` can reverse it"
        );
    }

    /// Claude Code store dir for `project`, keyed both ways as the real tool does.
    fn claude_store(home: &Path, project: &Path) {
        let enc: String = project
            .display()
            .to_string()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let store = home.join(".claude/projects").join(&enc);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(
            store.join("s.jsonl"),
            format!("{{\"cwd\":\"{}\"}}\n", project.display()),
        )
        .unwrap();
    }

    fn move_event(from: &Path, to: &Path) -> SessionEvent {
        SessionEvent::Moved {
            from: Some(from.to_path_buf()),
            to: Some(to.to_path_buf()),
        }
    }

    /// The noise a working tree makes — measured on a real daemon: one
    /// `git init && git commit` produced 9 rename events, a hello-world
    /// `cargo build` 3. Through v0.10 each planned a re-key across every
    /// store (~1.2 s / ~3.9 GB on a real machine). None of them is a project
    /// move, so none may do anything at all: no store walk, no activity row.
    #[test]
    fn file_renames_and_unknown_directory_renames_do_nothing() {
        let home = TempDir::new().unwrap();
        let project = home.path().join("work/app");
        std::fs::create_dir_all(project.join(".git/refs/heads")).unwrap();
        std::fs::create_dir_all(project.join("target/debug/incremental")).unwrap();
        claude_store(home.path(), &project);

        let registry = Registry::open_in_memory().unwrap();
        let tools = ToolRegistry::new().unwrap();
        let log = EventLog::open_in_memory().unwrap();
        let mut known = KnownProjects::build(home.path(), &tools, &registry);

        // git's lock-file dance: a FILE rename.
        std::fs::write(project.join(".git/index"), b"x").unwrap();
        let lock = project.join(".git/index.lock");
        // Cargo's incremental dir finalisation: a DIRECTORY rename, but not a project.
        let inc_from = project.join("target/debug/incremental/s-abc-working");
        let inc_to = project.join("target/debug/incremental/s-abc");
        std::fs::create_dir_all(&inc_to).unwrap();

        for ev in [
            move_event(&lock, &project.join(".git/index")),
            move_event(&inc_from, &inc_to),
        ] {
            handle_session_event(home.path(), ev, &registry, &tools, &log, &mut known);
        }

        assert!(
            log.recent_activity(10).unwrap().is_empty(),
            "noise must not even be planned, let alone recorded"
        );
        assert!(log.latest_pending_rekey().unwrap().is_none());
        let stores = crate::sessions::resolve_stores(tools.all(), None);
        let groups = crate::sessions::census(home.path(), &stores, false);
        assert_eq!(
            groups[0].project_path,
            project.display().to_string(),
            "the real project's store is untouched"
        );
    }

    /// Moving a FOLDER moves every project in it. Before the index, the
    /// daemon only ever re-keyed the renamed path itself, so a reorganisation
    /// like `junk-drawer/rndm → junk-drawer/devins-stuff/legal` stranded every
    /// project inside — and `undo` must bring them all back together.
    #[test]
    fn moving_a_folder_rekeys_every_project_beneath_it_as_one_undo() {
        let home = TempDir::new().unwrap();
        let folder = home.path().join("junk/rndm");
        let a = folder.join("peoples");
        let b = folder.join("deep/nested");
        let bystander = home.path().join("junk/keep");
        for p in [&a, &b, &bystander] {
            std::fs::create_dir_all(p).unwrap();
            claude_store(home.path(), p);
        }

        let registry = Registry::open_in_memory().unwrap();
        let tools = ToolRegistry::new().unwrap();
        let log = EventLog::open_in_memory().unwrap();
        let mut known = KnownProjects::build(home.path(), &tools, &registry);

        let moved = home.path().join("junk/legal");
        std::fs::rename(&folder, &moved).unwrap();
        handle_session_event(
            home.path(),
            move_event(&folder, &moved),
            &registry,
            &tools,
            &log,
            &mut known,
        );

        let stores = crate::sessions::resolve_stores(tools.all(), None);
        let paths: Vec<String> = crate::sessions::census(home.path(), &stores, false)
            .into_iter()
            .map(|g| g.project_path)
            .collect();
        for want in [
            moved.join("peoples"),
            moved.join("deep/nested"),
            bystander.clone(),
        ] {
            assert!(
                paths.contains(&want.display().to_string()),
                "{} missing from census {paths:?}",
                want.display()
            );
        }
        assert_eq!(
            paths.len(),
            3,
            "nothing left keyed to the old folder: {paths:?}"
        );

        let undo_rows = log.recent_rekeys(10).unwrap();
        assert_eq!(undo_rows.len(), 1, "one folder move is ONE undo");
        assert_eq!(undo_rows[0].old_path, folder.display().to_string());

        // ...and that one undo brings both projects back.
        let plans: crate::rekey::RecordedUndo =
            serde_json::from_str(&undo_rows[0].undo_plan).unwrap();
        std::fs::rename(&moved, &folder).unwrap();
        for plan in plans.plans() {
            crate::rekey::apply(&plan).expect("undo applies");
        }
        let back: Vec<String> = crate::sessions::census(home.path(), &stores, false)
            .into_iter()
            .map(|g| g.project_path)
            .collect();
        assert!(back.contains(&a.display().to_string()));
        assert!(back.contains(&b.display().to_string()));
    }

    // The core pipeline seam: a paired Moved event detects the tool at the new
    // location, reconciles its artifacts, registers the new path, and logs it.
    #[test]
    fn handle_session_event_moved_reconciles_and_reregisters() {
        let dir = TempDir::new().unwrap();
        let old = synthetic_project(dir.path(), "alpha");
        let new = dir.path().join("beta");
        std::fs::rename(&old, &new).unwrap(); // settings.json still names `old`

        let registry = Registry::open_in_memory().unwrap();
        // As `sessionguard watch` would: the daemon only acts on directories
        // it knows are projects.
        registry.register_project(&old).unwrap();
        let mut tools = ToolRegistry::new().unwrap();
        tools.register(synthetic_json_tool());
        let log = EventLog::open_in_memory().unwrap();

        handle_session_event(
            dir.path(),
            SessionEvent::Moved {
                from: Some(old.clone()),
                to: Some(new.clone()),
            },
            &registry,
            &tools,
            &log,
            &mut KnownProjects::from_keys(Vec::<PathBuf>::new(), None),
        );

        let settings = std::fs::read_to_string(new.join(".testtool/settings.json")).unwrap();
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
            dir.path(),
            SessionEvent::Moved {
                from: Some(dir.path().join("plain-old")),
                to: Some(new),
            },
            &registry,
            &tools,
            &log,
            &mut KnownProjects::from_keys(Vec::<PathBuf>::new(), None),
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
            dir.path(),
            SessionEvent::Moved {
                from: Some(dir.path().join("x")),
                to: None,
            },
            &registry,
            &tools,
            &log,
            &mut KnownProjects::from_keys(Vec::<PathBuf>::new(), None),
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

        handle_session_event(
            dir.path(),
            SessionEvent::Removed(gone.clone()),
            &registry,
            &tools,
            &log,
            &mut KnownProjects::from_keys(Vec::<PathBuf>::new(), None),
        );

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
