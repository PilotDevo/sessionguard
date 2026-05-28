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

    /// One-time scan to discover and register existing sessions.
    Scan {
        /// Directory to scan (defaults to configured watch roots).
        path: Option<PathBuf>,
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

    /// **EXPERIMENTAL (v0.4 in flight).** Migrate a tool's home-dir data
    /// to a new location. Only `--dry-run` works today — real
    /// migrations are gated until the rewrite / resume / validate
    /// stages land. See `docs/design/migrate.md` for the design.
    Migrate {
        /// Tool name to migrate (e.g. `codex`, `opencode`).
        tool: String,
        /// Destination directory. Must not exist; sessionguard refuses
        /// to overwrite an existing path.
        #[arg(long)]
        to: PathBuf,
        /// Walk every implemented stage of the state machine without
        /// touching the filesystem. Today this is the only supported
        /// mode; a real migration without `--dry-run` errors out.
        #[arg(long)]
        dry_run: bool,
        /// Output format for the per-stage event log.
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
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
