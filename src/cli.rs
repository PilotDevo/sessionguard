// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! CLI definition using clap derive.
//!
//! All subcommands from the SessionGuard README are defined here.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use clap_complete::Shell;

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
    Status,

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
    },

    /// Diagnose common issues (stale refs, orphaned sessions).
    Doctor,

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

    /// Undo previous reconciliation actions from the event log.
    Undo {
        /// Undo the last N actions. Default: 1.
        #[arg(long, default_value = "1")]
        last: usize,
        /// Undo a specific event by id (mutually exclusive with --last).
        #[arg(long)]
        id: Option<i64>,
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
        #[arg(short, long)]
        verbose: bool,
    },
}
