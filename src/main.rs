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

        Command::Status { format } => {
            let registry = Registry::open_default()?;
            let projects = registry.list_projects()?;
            let running = sessionguard::daemon::is_running();
            match format {
                sessionguard::cli::Format::Json => {
                    let payload = serde_json::json!({
                        "daemon_running": running,
                        "projects": projects,
                    });
                    println!("{}", serde_json::to_string_pretty(&payload)?);
                }
                sessionguard::cli::Format::Text => {
                    if projects.is_empty() {
                        println!("no tracked projects");
                    } else {
                        println!("{:<6} PATH", "ID");
                        for p in &projects {
                            println!("{:<6} {}", p.id, p.path.display());
                        }
                    }
                    println!("\ndaemon running: {running}");
                }
            }
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

        Command::Log { last, format } => {
            let event_log = EventLog::open_default()?;
            let entries = event_log.recent(last)?;
            match format {
                sessionguard::cli::Format::Json => {
                    println!("{}", serde_json::to_string_pretty(&entries)?);
                }
                sessionguard::cli::Format::Text => {
                    if entries.is_empty() {
                        println!("no reconciliation events");
                    } else {
                        for e in &entries {
                            let undone = if e.undone_at.is_some() {
                                " (undone)"
                            } else {
                                ""
                            };
                            println!(
                                "[{}] {} {} :: {} -> {}{}",
                                e.timestamp,
                                e.tool_name,
                                e.file_path.display(),
                                e.old_value,
                                e.new_value,
                                undone
                            );
                        }
                    }

                    let migrations = event_log.recent_migrations(last)?;
                    if !migrations.is_empty() {
                        println!("\nmigrations:");
                        for m in &migrations {
                            let undone = if m.undone_at.is_some() {
                                " (undone)"
                            } else if m.cleaned_at.is_some() {
                                " (cleaned — not undoable)"
                            } else {
                                ""
                            };
                            println!(
                                "[{}] #{} migrate {} :: {} -> {}{}",
                                m.timestamp,
                                m.id,
                                m.tool_name,
                                m.src.display(),
                                m.dst.display(),
                                undone
                            );
                        }
                        println!(
                            "\n(reverse a migration with `sessionguard undo --migration <id>`)"
                        );
                    }
                }
            }
        }

        Command::Doctor { clean, dry_run } => {
            use sessionguard::health::{check_binary, BinaryStatus};

            println!("running diagnostics...");
            let mut issues: u32 = 0;

            // --- tracked-project paths ---
            //
            // Pure report by default. With `--clean`, also unregister
            // projects whose path no longer exists on disk; with both
            // `--clean --dry-run`, just print what *would* be removed.
            let registry = Registry::open_default()?;
            let projects = registry.list_projects()?;
            let mut stale: Vec<&sessionguard::registry::ProjectEntry> = Vec::new();
            println!("\ntracked projects:");
            if projects.is_empty() {
                println!(
                    "  (none — `sessionguard watch <path>` or `sessionguard scan` to register)"
                );
            } else {
                for p in &projects {
                    if p.path.exists() {
                        println!("  [OK]   {}", p.path.display());
                    } else {
                        println!(
                            "  [WARN] project path no longer exists: {}",
                            p.path.display()
                        );
                        stale.push(p);
                        issues += 1;
                    }
                }
            }

            // If asked to clean, do it (or report the planned cleanup
            // under --dry-run). ON DELETE CASCADE on session_artifacts
            // means we don't need to remove artifacts manually.
            if clean && !stale.is_empty() {
                println!();
                if dry_run {
                    println!(
                        "--clean --dry-run: would unregister {} stale project(s):",
                        stale.len()
                    );
                    for p in &stale {
                        println!("  [DRY]  {}", p.path.display());
                    }
                } else {
                    println!("--clean: unregistering {} stale project(s)...", stale.len());
                    let mut removed = 0;
                    for p in &stale {
                        match registry.unregister_project(&p.path) {
                            Ok(_) => {
                                println!("  [DEL]  {}", p.path.display());
                                removed += 1;
                                issues = issues.saturating_sub(1);
                            }
                            Err(e) => {
                                println!("  [ERR]  {} ({e})", p.path.display());
                            }
                        }
                    }
                    println!("removed {removed} stale registry entries");
                }
            } else if !clean && !stale.is_empty() {
                println!(
                    "\n(run `sessionguard doctor --clean` to unregister {} stale entries;\n \
                     add `--dry-run` to preview without writing)",
                    stale.len()
                );
            }

            // --- launcher health for every registered tool ---
            //
            // The recurring "I upgraded Node and my sessions are gone" pain
            // really means "the launcher binary that wrote those sessions is
            // no longer on PATH" — the data is fine. Report explicitly so
            // users don't think their history is lost.
            let tool_registry = ToolRegistry::new_with_config(&config)?;
            let mut tools: Vec<_> = tool_registry.all().collect();
            tools.sort_by(|a, b| a.name.cmp(&b.name));

            println!("\nlauncher health:");
            if tools.is_empty() {
                println!("  (no tools registered)");
            } else {
                for t in &tools {
                    match check_binary(t) {
                        BinaryStatus::Present { path } => {
                            println!("  [OK]   {} -> {}", t.display_name, path.display());
                        }
                        BinaryStatus::Missing { binary } => {
                            println!(
                                "  [WARN] {} - launcher `{}` not on PATH \
                                 (session data intact; check installer / runtime version)",
                                t.display_name, binary
                            );
                            issues += 1;
                        }
                        BinaryStatus::NotConfigured => {
                            println!(
                                "  [--]   {} - no launcher binary configured",
                                t.display_name
                            );
                        }
                    }
                }
            }

            println!();
            if issues == 0 {
                println!("no issues found");
            } else {
                println!("{issues} issue(s) found");
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
            use sessionguard::health::{check_binary, BinaryStatus};

            let tool_registry = ToolRegistry::new_with_config(&config)?;
            let mut tools: Vec<_> = tool_registry.all().collect();
            tools.sort_by(|a, b| a.name.cmp(&b.name));

            let (verbose, format) = match action {
                Some(sessionguard::cli::ToolsAction::List { verbose, format }) => (verbose, format),
                None => (false, sessionguard::cli::Format::Text),
            };

            // Each tool gets its current launcher status alongside its
            // static definition. The dashboard reads this directly via
            // `tools list --format json`.
            #[derive(serde::Serialize)]
            struct ToolWithHealth<'a> {
                #[serde(flatten)]
                tool: &'a sessionguard::tools::ToolDefinition,
                binary_status: BinaryStatus,
            }

            let enriched: Vec<ToolWithHealth<'_>> = tools
                .iter()
                .map(|t| ToolWithHealth {
                    tool: t,
                    binary_status: check_binary(t),
                })
                .collect();

            if matches!(format, sessionguard::cli::Format::Json) {
                println!("{}", serde_json::to_string_pretty(&enriched)?);
            } else if tools.is_empty() {
                println!("no tools registered");
            } else {
                println!("{:<16} {:<24} {:<8} LAUNCHER", "NAME", "DISPLAY", "VERSION");
                for (t, e) in tools.iter().zip(enriched.iter()) {
                    let launcher = match &e.binary_status {
                        BinaryStatus::Present { path } => path.display().to_string(),
                        BinaryStatus::Missing { binary } => format!("⚠ `{binary}` not on PATH"),
                        BinaryStatus::NotConfigured => "—".to_string(),
                    };
                    println!(
                        "{:<16} {:<24} {:<8} {}",
                        t.name,
                        t.display_name,
                        t.version.as_deref().unwrap_or("-"),
                        launcher
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

        Command::Migrate {
            tool,
            to,
            dry_run,
            format,
        } => {
            use sessionguard::migrate;

            let tool_registry = ToolRegistry::new_with_config(&config)?;
            let tool_def = tool_registry
                .get(&tool)
                .ok_or_else(|| anyhow::anyhow!("no tool named `{tool}` registered"))?;

            // The tool's home_dir_layout (if any) tells us the source
            // path; for now we derive it directly from the layout's
            // default_path. Future polish: --from override.
            let layout = tool_def.home_dir_layout.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "tool `{tool}` has no home_dir_layout — nothing to migrate. \
                     See docs/history/migrate.md for what's needed."
                )
            })?;
            let src = sessionguard::inventory::expand_home(&layout.default_path);

            match migrate::migrate(tool_def, &src, &to, dry_run) {
                Ok(result) => {
                    // Persist a reversible record for a successful real
                    // migration so `sessionguard undo` can reverse it.
                    if let Some(plan) = result.undo_plan() {
                        let event_log = EventLog::open_default()?;
                        let undo_json = serde_json::to_string(&plan)?;
                        event_log.record_migration(
                            &plan.tool_name,
                            &plan.src,
                            &plan.dst,
                            &undo_json,
                        )?;
                    }
                    match format {
                        sessionguard::cli::Format::Json => {
                            println!("{}", serde_json::to_string_pretty(&result)?);
                        }
                        sessionguard::cli::Format::Text => {
                            let mode = if dry_run { "DRY-RUN" } else { "LIVE" };
                            println!("{} migrate: {} -> {}", mode, src.display(), to.display());
                            for e in &result.events {
                                println!("  [{:?}] {}", e.stage, e.detail);
                            }
                            println!(
                                "\nfinal stage: {:?}  success: {}",
                                result.final_stage, result.success
                            );
                            if result.dry_run {
                                println!("\n(dry-run only — no changes were made.)");
                            } else if result.success {
                                println!(
                                    "\nMigration complete. Original preserved; run \
                                     `sessionguard undo` to reverse it."
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("migrate failed: {e}"));
                }
            }
        }

        Command::MigrateCleanup { migration, execute } => {
            let event_log = EventLog::open_default()?;

            let candidates = match migration {
                Some(mid) => {
                    let entry = event_log
                        .get_migration(mid)?
                        .ok_or_else(|| anyhow::anyhow!("no migration with id {mid}"))?;
                    if entry.undone_at.is_some() {
                        return Err(anyhow::anyhow!(
                            "migration {mid} was undone; nothing to clean up"
                        ));
                    }
                    if entry.cleaned_at.is_some() {
                        println!("migration {mid} was already cleaned up");
                        return Ok(());
                    }
                    vec![entry]
                }
                None => event_log.cleanable_migrations(100)?,
            };

            if candidates.is_empty() {
                println!("no migrations with preserved originals to clean up");
                return Ok(());
            }

            let mut total: u64 = 0;
            let mut cleaned = 0usize;
            for entry in &candidates {
                let plan: sessionguard::migrate::MigrationUndo =
                    serde_json::from_str(&entry.undo_plan).map_err(|e| {
                        anyhow::anyhow!("migration {} has a corrupt undo plan: {e}", entry.id)
                    })?;
                let report = sessionguard::migrate::cleanup_migration(&plan, !execute)
                    .map_err(|e| anyhow::anyhow!("cleanup failed: {e}"))?;
                total += report.total_bytes();

                let verb = if execute { "removing" } else { "would remove" };
                println!(
                    "migration #{} ({}: {} -> {})",
                    entry.id,
                    plan.tool_name,
                    plan.src.display(),
                    plan.dst.display()
                );
                for item in &report.items {
                    if item.existed {
                        println!(
                            "  {} {} ({})",
                            verb,
                            item.path.display(),
                            fmt_size(item.bytes)
                        );
                    } else {
                        println!("  (already gone) {}", item.path.display());
                    }
                }
                if execute {
                    event_log.mark_migration_cleaned(entry.id)?;
                    cleaned += 1;
                }
            }

            if execute {
                println!(
                    "\ncleaned {cleaned} migration(s), reclaimed {}",
                    fmt_size(total)
                );
            } else {
                println!(
                    "\n{} reclaimable across {} migration(s). \
                     Re-run with --execute to delete.",
                    fmt_size(total),
                    candidates.len()
                );
            }
        }

        Command::Inventory { format } => {
            use sessionguard::inventory::inventory_tools_impl;

            let tool_registry = ToolRegistry::new_with_config(&config)?;
            let mut tools: Vec<_> = tool_registry.all().collect();
            tools.sort_by(|a, b| a.name.cmp(&b.name));
            let entries = inventory_tools_impl(tools.iter().copied());

            match format {
                sessionguard::cli::Format::Json => {
                    println!("{}", serde_json::to_string_pretty(&entries)?);
                }
                sessionguard::cli::Format::Text => {
                    if entries.is_empty() {
                        println!(
                            "no tools declare a home_dir_layout — nothing to migrate.\n\
                             (only tools with a [tool.home_dir_layout] block in their TOML\n\
                             can be migrated by `sessionguard migrate`; see\n\
                             docs/history/migrate.md.)"
                        );
                    } else {
                        println!(
                            "{:<14} {:<48} {:>12} {:>10} {:>14}",
                            "TOOL", "LOCATION", "SIZE", "FILES", "LAST MODIFIED"
                        );
                        for e in &entries {
                            let size = fmt_size(e.size_bytes);
                            let when = e.last_modified.map(fmt_ago).unwrap_or_else(|| "-".into());
                            let loc = if e.exists {
                                e.path.display().to_string()
                            } else {
                                format!("(absent) {}", e.path.display())
                            };
                            let trunc = if e.truncated { "+" } else { "" };
                            println!(
                                "{:<14} {:<48} {:>12} {:>9}{} {:>14}",
                                e.tool_name, loc, size, e.file_count, trunc, when
                            );
                            for note in &e.notes {
                                println!("  ! {note}");
                            }
                        }
                        if entries.iter().any(|e| e.truncated) {
                            println!(
                                "\n(+) walk truncated at the file cap; actual size/count is a floor"
                            );
                        }
                    }
                }
            }
        }

        Command::Undo {
            last,
            id,
            migration,
            dry_run,
        } => {
            let event_log = EventLog::open_default()?;

            // Resolve which migration (if any) this invocation targets:
            //  --migration <id>  → that specific migration
            //  bare `undo`       → the latest pending migration, if one exists
            //  --id / --last     → reconciliation events only (never a migration)
            let target_migration = if let Some(mid) = migration {
                Some(
                    event_log
                        .get_migration(mid)?
                        .ok_or_else(|| anyhow::anyhow!("no migration with id {mid}"))?,
                )
            } else if id.is_none() {
                event_log.latest_pending_migration()?
            } else {
                None
            };

            if let Some(entry) = target_migration {
                if entry.undone_at.is_some() {
                    println!("migration {} was already undone", entry.id);
                } else if entry.cleaned_at.is_some() {
                    return Err(anyhow::anyhow!(
                        "migration {} was cleaned up ({}); its preserved original \
                         is gone, so undo is no longer available. The migrated data \
                         at the destination is unaffected.",
                        entry.id,
                        entry.cleaned_at.as_deref().unwrap_or("")
                    ));
                } else {
                    let plan: sessionguard::migrate::MigrationUndo =
                        serde_json::from_str(&entry.undo_plan).map_err(|e| {
                            anyhow::anyhow!("migration {} has a corrupt undo plan: {e}", entry.id)
                        })?;
                    let tool_registry = ToolRegistry::new_with_config(&config)?;
                    let layout = tool_registry
                        .get(&plan.tool_name)
                        .and_then(|t| t.home_dir_layout.as_ref())
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "tool `{}` is no longer registered or lost its \
                                 home_dir_layout; cannot undo migration {}",
                                plan.tool_name,
                                entry.id
                            )
                        })?;

                    println!(
                        "{} migration {} ({}: {} -> {}):",
                        if dry_run { "would undo" } else { "undoing" },
                        entry.id,
                        plan.tool_name,
                        plan.src.display(),
                        plan.dst.display()
                    );
                    let report = sessionguard::migrate::undo_migration(
                        &plan,
                        layout,
                        &sessionguard::migrate::SystemdQuiescer,
                        &sessionguard::migrate::SystemdEnvWriter,
                        dry_run,
                    )
                    .map_err(|e| anyhow::anyhow!("undo failed: {e}"))?;
                    for step in &report.steps {
                        println!("  - {step}");
                    }
                    if !dry_run {
                        event_log.mark_migration_undone(entry.id)?;
                        println!("\nmigration {} undone", entry.id);
                    }
                }
                return Ok(());
            }

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

        Command::Update { check, dry_run, to } => {
            use sessionguard::update::{
                check_update, detect_install_method, perform_update, CurlReleaseClient,
                InstallMethod, REPO,
            };
            let current = env!("CARGO_PKG_VERSION");
            let client = CurlReleaseClient;

            // `--check`: read-only; non-zero exit when behind (scriptable drift probe).
            if check {
                let c = check_update(&client, REPO, current)
                    .map_err(|e| anyhow::anyhow!("update check failed: {e}"))?;
                if c.update_available {
                    println!(
                        "sessionguard {} is available (you have {}).",
                        c.latest, c.current
                    );
                    println!("run `sessionguard update` to upgrade.");
                    std::process::exit(1);
                }
                println!("sessionguard {} is current.", c.current);
                return Ok(());
            }

            // Don't fight the package manager.
            let dest = match detect_install_method()
                .map_err(|e| anyhow::anyhow!("could not determine install method: {e}"))?
            {
                InstallMethod::Standalone { path } => path,
                InstallMethod::Cargo => {
                    println!(
                        "Installed via cargo — run `cargo install sessionguard --force` to update."
                    );
                    return Ok(());
                }
                InstallMethod::Homebrew => {
                    println!("Installed via Homebrew — run `brew upgrade sessionguard` to update.");
                    return Ok(());
                }
                InstallMethod::GitCheckout => {
                    return Err(anyhow::anyhow!(
                        "running a dev build from a source checkout — rebuild from source \
                         rather than self-updating."
                    ));
                }
            };

            // Resolve the target tag.
            let tag = match to {
                Some(t) if t.starts_with('v') => t,
                Some(t) => format!("v{t}"),
                None => {
                    let c = check_update(&client, REPO, current)
                        .map_err(|e| anyhow::anyhow!("update check failed: {e}"))?;
                    if !c.update_available {
                        println!("sessionguard {} is already current.", c.current);
                        return Ok(());
                    }
                    c.latest
                }
            };

            println!(
                "{} sessionguard {} → {} ({})",
                if dry_run { "would update" } else { "updating" },
                current,
                tag.trim_start_matches('v'),
                dest.display()
            );
            let report = perform_update(&client, &dest, REPO, &tag, current, dry_run)
                .map_err(|e| anyhow::anyhow!("update failed: {e}"))?;
            for step in &report.steps {
                println!("  - {step}");
            }
            if dry_run {
                println!("\n(dry-run only — nothing was changed.)");
            } else {
                println!(
                    "\nupdated to {}. previous binary kept at {}.",
                    report.to,
                    report
                        .backup
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                );
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

/// Render a byte count in a compact human-friendly form for the
/// `inventory` text table. Stops at TB; everything beyond would be
/// surprising on a single tool's data dir.
fn fmt_size(bytes: u64) -> String {
    const UNITS: &[(&str, u64)] = &[
        ("TB", 1u64 << 40),
        ("GB", 1u64 << 30),
        ("MB", 1u64 << 20),
        ("KB", 1u64 << 10),
    ];
    for (suffix, threshold) in UNITS {
        if bytes >= *threshold {
            let value = bytes as f64 / *threshold as f64;
            return format!("{value:.1} {suffix}");
        }
    }
    format!("{bytes} B")
}

/// Render a unix-epoch-seconds timestamp as "X ago". Approximate; the
/// inventory's purpose is "is this stale or recent?", not precision.
fn fmt_ago(unix_seconds: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(unix_seconds);
    let age = now.saturating_sub(unix_seconds);
    if age < 60 {
        format!("{age}s ago")
    } else if age < 3600 {
        format!("{}m ago", age / 60)
    } else if age < 86_400 {
        format!("{}h ago", age / 3600)
    } else {
        format!("{}d ago", age / 86_400)
    }
}

#[cfg(test)]
mod main_tests {
    use super::*;

    #[test]
    fn fmt_size_picks_largest_appropriate_unit() {
        assert_eq!(fmt_size(0), "0 B");
        assert_eq!(fmt_size(1023), "1023 B");
        assert_eq!(fmt_size(1024), "1.0 KB");
        assert_eq!(fmt_size(1024 * 1024), "1.0 MB");
        assert_eq!(fmt_size(1024 * 1024 * 1024), "1.0 GB");
        // 20 GB the user's OpenCode store should render reasonably.
        assert_eq!(fmt_size(20_u64 * (1u64 << 30)), "20.0 GB");
    }

    #[test]
    fn fmt_ago_buckets_correctly() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(fmt_ago(now.saturating_sub(30)).contains("s ago"));
        assert!(fmt_ago(now.saturating_sub(300)).contains("m ago"));
        assert!(fmt_ago(now.saturating_sub(7200)).contains("h ago"));
        assert!(fmt_ago(now.saturating_sub(2 * 86_400)).contains("d ago"));
    }
}
