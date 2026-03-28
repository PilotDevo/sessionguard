//! SessionGuard — a system-level daemon that keeps AI coding sessions intact
//! when your projects move.
//!
//! This library crate contains all core logic. The binary (`main.rs`) is a thin
//! wrapper that parses CLI args and dispatches to library functions.

pub mod cli;
pub mod config;
pub mod daemon;
pub mod detector;
pub mod error;
pub mod event_log;
pub mod reconciler;
pub mod registry;
pub mod tools;
pub mod watcher;
