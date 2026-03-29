// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Unified error types for SessionGuard.

use std::path::PathBuf;

/// Errors that can occur during SessionGuard operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("failed to parse config at {path}: {source}")]
    ConfigParse {
        path: PathBuf,
        source: toml::de::Error,
    },

    #[error("registry error: {0}")]
    Registry(#[from] rusqlite::Error),

    #[error("filesystem watch error: {0}")]
    Watch(#[from] notify::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("tool definition error: {0}")]
    ToolDefinition(String),

    #[error("reconciliation failed for {path}: {detail}")]
    Reconcile { path: PathBuf, detail: String },

    #[error("daemon error: {0}")]
    Daemon(String),

    #[error("project not found: {0}")]
    ProjectNotFound(PathBuf),

    #[error("path does not exist: {0}")]
    PathNotFound(PathBuf),
}

pub type Result<T> = std::result::Result<T, Error>;
