// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! SessionGuard CLI entry point.

use anyhow::{Context, Result};
use clap::Parser;
use clap_complete::generate;
use sessionguard::cli::{Cli, Command, ConfigAction, SimulateAction};
use sessionguard::config::Config;
use sessionguard::detector;
use sessionguard::event_log::EventLog;
use sessionguard::registry::Registry;
use sessionguard::tools::ToolRegistry;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize tracing
    let filter = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .init();

    let config = match &cli.config {
        Some(path) => Config::load_from(path)
            .with_context(|| format!("failed to load config from {}", path.display()))?,
        None => Config::load().context("failed to load config")?,
    };

    match cli.command {
        Command::Start { foreground, .. } => {
            if !foreground {
                eprintln!(
                    "hint: background daemonization not yet implemented, running in foreground"
                );
            }
            sessionguard::daemon::run(&config).await?;
        }

        Command::Stop => {
            match sessionguard::daemon::read_pid()? {
                Some(pid) if sessionguard::daemon::is_running() => {
                    #[cfg(unix)]
                    // SAFETY: `kill(pid, SIGTERM)` with a valid u32-as-i32 PID. We
                    // verified the process exists above; worst case, the PID races
                    // and gets recycled between is_running() and kill(), which is
                    // inherent to any PID-based IPC.
                    unsafe {
                        libc::kill(pid as i32, libc::SIGTERM);
                    }
                    println!("sent stop signal to daemon (PID {pid})");
                }
                Some(pid) => {
                    // PID file exists but the process is gone — clean it up.
                    let _ = sessionguard::daemon::remove_pid_file();
                    println!("stale PID file removed (PID {pid} no longer running)");
                }
                None => {
                    println!("no running daemon found");
                }
            }
        }

        Command::Status => {
            let registry = Registry::open_default()?;
            let projects = registry.list_projects()?;
            if projects.is_empty() {
                println!("no tracked projects");
            } else {
                println!("{:<6} PATH", "ID");
                for p in &projects {
                    println!("{:<6} {}", p.id, p.path.display());
                }
            }
            println!("\ndaemon running: {}", sessionguard::daemon::is_running());
        }

        Command::Watch { path } => {
            let path = std::fs::canonicalize(&path)
                .with_context(|| format!("path does not exist: {}", path.display()))?;
            let registry = Registry::open_default()?;
            let id = registry.register_project(&path)?;

            // Detect tools
            let tool_registry = ToolRegistry::new_with_config(&config)?;
            let detected = detector::detect_tools(&path, &tool_registry);
            for d in &detected {
                for artifact in &d.artifact_files {
                    registry.add_artifact(id, &d.tool_name, artifact)?;
                }
                println!(
                    "  detected: {} ({} patterns matched, {} artifact files)",
                    d.display_name,
                    d.matched_patterns.len(),
                    d.artifact_files.len()
                );
            }

            println!("watching: {}", path.display());
        }

        Command::Unwatch { path } => {
            let registry = Registry::open_default()?;
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            registry.unregister_project(&path)?;
            println!("unwatched: {}", path.display());
        }

        Command::Scan { path } => {
            let roots = match path {
                Some(p) => vec![p],
                None => config.watch_roots.clone(),
            };
            let tool_registry = ToolRegistry::new_with_config(&config)?;
            let registry = Registry::open_default()?;

            for root in &roots {
                if !root.is_dir() {
                    continue;
                }
                println!("scanning: {}", root.display());
                if let Ok(entries) = std::fs::read_dir(root) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_dir() {
                            // Canonicalize so registered paths match what the
                            // daemon sees from filesystem events (e.g. on macOS,
                            // `/var/foo` → `/private/var/foo`). `Watch` already
                            // does this — `Scan` must match for consistency.
                            let canonical = std::fs::canonicalize(&path).unwrap_or(path);
                            let detected = detector::detect_tools(&canonical, &tool_registry);
                            if !detected.is_empty() {
                                let id = registry.register_project(&canonical)?;
                                for d in &detected {
                                    for artifact in &d.artifact_files {
                                        registry.add_artifact(id, &d.tool_name, artifact)?;
                                    }
                                }
                                let tools: Vec<_> =
                                    detected.iter().map(|d| d.display_name.as_str()).collect();
                                println!("  found: {} [{}]", canonical.display(), tools.join(", "));
                            }
                        }
                    }
                }
            }
        }

        Command::Simulate { action } => match action {
            SimulateAction::Mv { from, to } => {
                let tool_registry = ToolRegistry::new_with_config(&config)?;
                let detected = detector::detect_tools(&from, &tool_registry);
                if detected.is_empty() {
                    println!("no AI session artifacts found in {}", from.display());
                } else {
                    println!("simulating: mv {} {}\n", from.display(), to.display());
                    for d in &detected {
                        println!("  {} ({}):", d.display_name, d.tool_name);
                        for pattern in &d.matched_patterns {
                            println!("    artifact: {pattern}");
                        }
                        if let Some(tool) = tool_registry.get(&d.tool_name) {
                            for field in &tool.path_fields {
                                println!("    would rewrite: {}:{}", field.file, field.field);
                            }
                        }
                    }
                }
            }
        },

        Command::Log { last } => {
            let event_log = EventLog::open_default()?;
            let entries = event_log.recent(last)?;
            if entries.is_empty() {
                println!("no reconciliation events");
            } else {
                for e in &entries {
                    println!(
                        "[{}] {} {} :: {} -> {}",
                        e.timestamp,
                        e.tool_name,
                        e.file_path.display(),
                        e.old_value,
                        e.new_value
                    );
                }
            }
        }

        Command::Doctor => {
            println!("running diagnostics...");
            let registry = Registry::open_default()?;
            let projects = registry.list_projects()?;
            let mut issues = 0;
            for p in &projects {
                if !p.path.exists() {
                    println!(
                        "  [WARN] project path no longer exists: {}",
                        p.path.display()
                    );
                    issues += 1;
                }
            }
            if issues == 0 {
                println!("no issues found ({} projects checked)", projects.len());
            } else {
                println!("\n{issues} issue(s) found");
            }
        }

        Command::Export { output } => {
            let registry = Registry::open_default()?;
            let projects = registry.list_projects()?;
            let json =
                serde_json::to_string_pretty(&projects).context("failed to serialize projects")?;
            std::fs::write(&output, json)?;
            println!(
                "exported {} projects to {}",
                projects.len(),
                output.display()
            );
        }

        Command::Import { input } => {
            let content = std::fs::read_to_string(&input)?;
            let projects: Vec<sessionguard::registry::ProjectEntry> =
                serde_json::from_str(&content).context("failed to parse import file")?;
            let registry = Registry::open_default()?;
            for p in &projects {
                registry.register_project(&p.path)?;
            }
            println!(
                "imported {} projects from {}",
                projects.len(),
                input.display()
            );
        }

        Command::Config { action } => match action {
            Some(ConfigAction::Show) | None => {
                let toml_str = toml::to_string_pretty(&config)?;
                println!("{toml_str}");
            }
            Some(ConfigAction::Path) => {
                println!("{}", Config::default_path().display());
            }
            Some(ConfigAction::Edit) => {
                let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
                let path = Config::default_path();
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::process::Command::new(&editor)
                    .arg(&path)
                    .status()
                    .with_context(|| format!("failed to launch {editor}"))?;
            }
        },

        Command::Tools { action } => {
            let tool_registry = ToolRegistry::new_with_config(&config)?;
            let mut tools: Vec<_> = tool_registry.all().collect();
            tools.sort_by(|a, b| a.name.cmp(&b.name));

            let verbose = matches!(
                action,
                Some(sessionguard::cli::ToolsAction::List { verbose: true })
            );

            if tools.is_empty() {
                println!("no tools registered");
            } else {
                println!("{:<16} {:<24} VERSION", "NAME", "DISPLAY");
                for t in &tools {
                    println!(
                        "{:<16} {:<24} {}",
                        t.name,
                        t.display_name,
                        t.version.as_deref().unwrap_or("-")
                    );
                    if verbose {
                        if !t.session_patterns.is_empty() {
                            println!("  session_patterns:");
                            for p in &t.session_patterns {
                                println!("    - {p}");
                            }
                        }
                        if !t.path_fields.is_empty() {
                            println!("  path_fields:");
                            for pf in &t.path_fields {
                                println!("    - {} :: {} ({})", pf.file, pf.field, pf.format);
                            }
                        }
                        println!("  on_move: {:?}", t.on_move);
                        println!();
                    }
                }
                if !verbose {
                    println!(
                        "\n{} tools registered. Use --verbose for patterns.",
                        tools.len()
                    );
                }
            }
        }

        Command::Undo { last, id, dry_run } => {
            let event_log = EventLog::open_default()?;

            let entries = match id {
                Some(event_id) => vec![event_log
                    .get(event_id)?
                    .ok_or_else(|| anyhow::anyhow!("no event with id {event_id}"))?],
                None => event_log.recent_pending_undo(last.max(1))?,
            };

            if entries.is_empty() {
                println!("no actions to undo");
            } else {
                println!(
                    "{} {} action(s):",
                    if dry_run { "would undo" } else { "undoing" },
                    entries.len()
                );
                let mut undone = 0usize;
                let mut failed = 0usize;
                for entry in &entries {
                    match sessionguard::reconciler::undo_event(entry, dry_run) {
                        Ok(changed) => {
                            println!(
                                "  [{}] {} {} :: {} → {}{}",
                                entry.id,
                                entry.tool_name,
                                entry.file_path.display(),
                                entry.new_value,
                                entry.old_value,
                                if !changed {
                                    " (no match — file may have been modified)"
                                } else if dry_run {
                                    " (dry run)"
                                } else {
                                    ""
                                }
                            );
                            if changed && !dry_run {
                                event_log.mark_undone(entry.id)?;
                                undone += 1;
                            }
                        }
                        Err(e) => {
                            println!(
                                "  [{}] FAILED :: {} :: {e}",
                                entry.id,
                                entry.file_path.display()
                            );
                            failed += 1;
                        }
                    }
                }
                if !dry_run {
                    println!("\n{undone} undone, {failed} failed");
                }
            }
        }

        Command::Version => {
            println!(
                "sessionguard {} ({})",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS
            );
        }

        Command::Completions { shell } => {
            let mut cmd = <Cli as clap::CommandFactory>::command();
            generate(shell, &mut cmd, "sessionguard", &mut std::io::stdout());
        }
    }

    Ok(())
}
