// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! CLI definition using clap derive.
//!
//! All subcommands from the SessionGuard README are defined here.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

/// Output format for commands that emit structured data (`status`, `log`,
/// `tools`). `Text` is the default human-readable view; `Json` emits
/// machine-parseable output — used by the dashboard and scripting.
#[derive(Debug, Clone, Copy, ValueEnum, Default, PartialEq, Eq)]
#[value(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Text,
    Json,
}

/// SessionGuard — keeps AI coding sessions intact when your projects move.
#[derive(Debug, Parser)]
#[command(
    name = "sessionguard",
    version,
    about = "A system-level daemon that keeps AI coding sessions intact when your projects move",
    long_about = None,
    propagate_version = true,
)]
pub struct Cli {
    /// Enable verbose/debug logging.
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Path to config file (default: ~/.config/sessionguard/config.toml).
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the SessionGuard daemon.
    Start {
        /// Run in the foreground (don't daemonize).
        #[arg(long)]
        foreground: bool,

        /// Daemonize (run in the background).
        #[arg(short, long)]
        daemon: bool,
    },

    /// Stop the running daemon.
    Stop,

    /// Show tracked projects and their session health.
    Status {
        /// Output format.
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },

    /// Register a directory tree for monitoring.
    Watch {
        /// Path to the directory to watch.
        path: PathBuf,
    },

    /// Remove a directory from monitoring.
    Unwatch {
        /// Path to the directory to stop watching.
        path: PathBuf,
    },

    /// First-run setup: scan your home directory for AI-tool projects and
    /// write the directories that contain them to `watch_roots` in the config,
    /// so the daemon monitors where your projects actually live.
    Init {
        /// Show what would be discovered and written without touching config.
        #[arg(long)]
        dry_run: bool,
        /// How deep to search under home for projects.
        #[arg(long, default_value = "4")]
        depth: usize,
    },

    /// One-time scan to discover and register existing sessions.
    ///
    /// Recurses to `--depth` levels (default 4), registering every directory
    /// that contains AI-tool artifacts. Pruned at each detected project.
    Scan {
        /// Directory to scan (defaults to configured watch roots).
        path: Option<PathBuf>,
        /// Maximum directory depth to recurse.
        #[arg(long, default_value = "4")]
        depth: usize,
    },

    /// Tail the background daemon's log file.
    Logs {
        /// Number of trailing lines to print.
        #[arg(long, default_value = "50")]
        lines: usize,
        /// Keep streaming new lines as they're written (Ctrl-C to stop).
        #[arg(short, long)]
        follow: bool,
    },

    /// Dry-run a move/rename and show what would be reconciled.
    Simulate {
        #[command(subcommand)]
        action: SimulateAction,
    },

    /// View reconciliation event history.
    Log {
        /// Number of recent entries to show.
        #[arg(long, default_value = "20")]
        last: usize,
        /// Output format.
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },

    /// Diagnose common issues (stale registry entries, missing launchers).
    Doctor {
        /// Unregister tracked projects whose path no longer exists on
        /// disk. Without this flag, doctor only reports — never mutates.
        #[arg(long)]
        clean: bool,
        /// With `--clean`, print what would be unregistered without
        /// touching the registry.
        #[arg(long)]
        dry_run: bool,
    },

    /// Export session metadata for backup/migration.
    Export {
        /// Output file path.
        #[arg(short, long)]
        output: PathBuf,
    },

    /// Import session metadata from backup.
    Import {
        /// Input file path.
        #[arg(short, long)]
        input: PathBuf,
    },

    /// View or edit configuration.
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },

    /// Inspect registered tool patterns (built-in + user + project).
    Tools {
        #[command(subcommand)]
        action: Option<ToolsAction>,
    },

    /// Enumerate every tool with a declared home-dir layout: where its
    /// data lives, how big it is, when it was last touched. Read-only;
    /// the lead-in to `sessionguard migrate`.
    Inventory {
        /// Output format.
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },

    /// Migrate a tool's home-dir data to a new location, preserving the
    /// original. Runs the full state machine and records a reversible
    /// migration (`sessionguard undo`). See `docs/history/migrate.md`.
    Migrate {
        /// Tool name to migrate (e.g. `codex`, `opencode`).
        tool: String,
        /// Destination directory. Must not exist; sessionguard refuses
        /// to overwrite an existing path.
        #[arg(long)]
        to: PathBuf,
        /// Walk every stage of the state machine without touching the
        /// filesystem — a preview of exactly what a real run would do.
        #[arg(long)]
        dry_run: bool,
        /// Output format for the per-stage event log.
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },

    /// Reclaim space from completed migrations by deleting the preserved
    /// originals (the `.migrated-<unix>` sidecars and config backups).
    ///
    /// Reports what's reclaimable by default. Pass `--execute` to delete.
    /// Cleaning a migration makes it un-undoable, but never touches the
    /// live data at the destination.
    MigrateCleanup {
        /// Clean only this migration id (from `sessionguard log`).
        /// Without it, all cleanable migrations are considered.
        #[arg(long)]
        migration: Option<i64>,
        /// Actually delete. Without this flag, cleanup only reports.
        #[arg(long)]
        execute: bool,
    },

    /// Undo previous reconciliation actions or a completed migration.
    ///
    /// With no flags, undoes the most recent pending migration if one
    /// exists, otherwise the last reconciliation action.
    Undo {
        /// Undo the last N reconciliation actions. Default: 1.
        #[arg(long, default_value = "1")]
        last: usize,
        /// Undo a specific reconciliation event by id.
        #[arg(long)]
        id: Option<i64>,
        /// Undo a specific migration by id (from the migration log).
        #[arg(long)]
        migration: Option<i64>,
        /// Show what would be undone without modifying anything.
        #[arg(long)]
        dry_run: bool,
    },

    /// Update SessionGuard to the latest release.
    ///
    /// Self-replaces a standalone install (the `install.sh` target); defers to
    /// the package manager for Homebrew/cargo installs; refuses a dev build.
    /// The download is verified against the release `SHA256SUMS`, the previous
    /// binary is kept for rollback, and a running daemon is restarted.
    Update {
        /// Only report whether a newer release exists; change nothing.
        #[arg(long)]
        check: bool,
        /// Show what would happen without downloading or replacing anything.
        #[arg(long)]
        dry_run: bool,
        /// Install a specific version instead of the latest (e.g. `v0.4.3`).
        #[arg(long)]
        to: Option<String>,
        /// Permit installing an older release than the one running.
        #[arg(long)]
        allow_downgrade: bool,
        /// Honor `SESSIONGUARD_UPDATE_BASE_URL` (a code-execution seam; used by
        /// the offline dogfood/tests only). Hidden — never needed in normal use.
        #[arg(long, hide = true)]
        allow_custom_base: bool,
    },

    /// Print version info.
    Version,

    /// Generate shell completions.
    Completions {
        /// Shell to generate completions for.
        shell: Shell,
    },
}

#[derive(Debug, Subcommand)]
pub enum SimulateAction {
    /// Simulate moving/renaming a project directory.
    Mv {
        /// Source path.
        from: PathBuf,
        /// Destination path.
        to: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Show the current configuration.
    Show,
    /// Show the config file path.
    Path,
    /// Edit the config file in $EDITOR.
    Edit,
}

#[derive(Debug, Subcommand)]
pub enum ToolsAction {
    /// List all registered tools (default).
    List {
        /// Show each tool's patterns and path_fields, not just the names.
        /// Ignored when `--format json` is used (JSON always includes them).
        #[arg(short, long)]
        verbose: bool,
        /// Output format.
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },
}
