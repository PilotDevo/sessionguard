# SessionGuard

**A system-level daemon that keeps AI coding sessions intact when your projects move.**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange.svg?logo=rust)](https://www.rust-lang.org)
[![CI](https://github.com/PilotDevo/sessionguard/actions/workflows/ci.yml/badge.svg)](https://github.com/PilotDevo/sessionguard/actions/workflows/ci.yml)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen.svg)](CONTRIBUTING.md)
[![Built with Tokio](https://img.shields.io/badge/async-Tokio-blue.svg)](https://tokio.rs)
[![SQLite](https://img.shields.io/badge/storage-SQLite-003B57.svg?logo=sqlite)](https://www.sqlite.org)
[![Platform: macOS | Linux](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey.svg)]()
[![Conventional Commits](https://img.shields.io/badge/commits-Conventional-FE5196.svg?logo=conventionalcommits)](https://conventionalcommits.org)

---

## The Problem

Modern developers use AI coding assistants daily — Claude Code, Cursor, Gemini Code Assist, Windsurf, Codex, and more. Each of these tools generates session state: context files, project configs, conversation history, and cached embeddings that live alongside your code.

**But none of them survive a `mv`.**

Rename a project folder? Your Claude sessions are orphaned. Restructure a monorepo? Cursor loses its context. Move a project to a new drive? Every tool forgets everything.

There is no system-level awareness that AI session state is a first-class artifact worth preserving.

**SessionGuard fixes this.**

## What It Does

SessionGuard is a lightweight filesystem daemon that:

- **Watches** for project directory moves, renames, and restructuring events
- **Detects** AI tool session files across all major coding assistants
- **Reconciles** broken paths, symlinks, and internal references when projects move
- **Preserves** your accumulated AI context so you never start from zero
- **Stays out of your way** — zero config for common setups, runs quietly in the background

## Supported Tools

| Tool | Session Artifacts | Status |
|------|------------------|--------|
| **Claude Code** | `.claude/`, `CLAUDE.md`, `.claudeignore`, session SQLite DBs | 🎯 Priority |
| **Cursor** | `.cursor/`, `.cursorignore`, `.cursorindexingignore`, chat history | 🎯 Priority |
| **Windsurf (Codeium)** | `.windsurf/`, `.windsurfrules`, cascade history | 🔜 Planned |
| **GitHub Copilot** | `.github/copilot-instructions.md`, VS Code chat history | 🔜 Planned |
| **Gemini Code Assist** | `.gemini/`, `GEMINI.md`, session context | 🔜 Planned |
| **Codex (OpenAI)** | `codex.md`, `.codex/`, sandbox state | 🔜 Planned |
| **Aider** | `.aider*`, `.aider.conf.yml`, chat history | 🔜 Planned |
| **Continue.dev** | `.continue/`, `config.json`, session logs | 🔜 Planned |
| **Custom / Other** | User-defined patterns via `sessionguard.toml` | ✅ Supported |

> **Tool authors:** We'd love your help defining the canonical session artifact list for your tool. See [Contributing](#contributing).

## How It Works

```
┌─────────────────────────────────────────────────────────┐
│                    SessionGuard Daemon                   │
│                                                         │
│  ┌──────────┐   ┌──────────────┐   ┌────────────────┐  │
│  │ Watcher  │──▶│   Detector   │──▶│  Reconciler    │  │
│  │ (fswatch)│   │ (tool index) │   │ (path rewrite) │  │
│  └──────────┘   └──────────────┘   └────────────────┘  │
│       │                                     │           │
│       ▼                                     ▼           │
│  ┌──────────┐                     ┌────────────────┐    │
│  │ Registry │                     │  Event Log     │    │
│  │ (SQLite) │                     │  (structured)  │    │
│  └──────────┘                     └────────────────┘    │
└─────────────────────────────────────────────────────────┘
```

1. **Watcher** — Listens for filesystem events (inotify on Linux, FSEvents on macOS, ReadDirectoryChanges on Windows) targeting registered project roots.
2. **Detector** — Maintains an index of known AI tool session patterns. When a move/rename event fires, it identifies which session artifacts are affected.
3. **Registry** — A lightweight SQLite database mapping project roots to their associated session artifacts and tool configurations.
4. **Reconciler** — Rewrites internal paths, updates symlinks, migrates session databases, and invalidates stale caches so tools pick up where they left off.
5. **Event Log** — Structured log of all reconciliation actions for auditability and undo capability.

## Quick Start

### Install

```bash
# From source (requires Rust 1.75+)
git clone https://github.com/PilotDevo/sessionguard.git
cd sessionguard
cargo install --path .

# Or via Homebrew (coming soon)
brew install sessionguard

# Or via cargo
cargo install sessionguard
```

### Run

```bash
# Start the daemon (watches common project directories)
sessionguard start

# Register a specific project root
sessionguard watch ~/projects/my-app

# Check status of tracked sessions
sessionguard status

# See what would happen if you moved a project (dry run)
sessionguard simulate mv ~/projects/old-name ~/projects/new-name

# View reconciliation history
sessionguard log --last 20
```

### Configure

SessionGuard works out of the box for common setups. For custom configuration:

```toml
# ~/.config/sessionguard/config.toml

# Directories to watch (defaults to ~/projects, ~/repos, ~/code, ~/dev)
watch_roots = [
    "~/projects",
    "~/work",
    "/mnt/dev"
]

# How aggressively to watch (battery-friendly by default)
watch_mode = "balanced"  # "aggressive" | "balanced" | "passive"

# Custom tool definitions
[[tools]]
name = "my-internal-tool"
session_patterns = [".mytool/", "mytool.config.json"]
path_fields = ["mytool.config.json:project_root", "mytool.config.json:cache_dir"]
on_move = "rewrite_paths"
```

## Architecture Decisions

### Why a daemon?

File moves are atomic OS-level events. If we only ran on-demand, we'd have to diff the entire filesystem to figure out what changed. A lightweight daemon catches events in real-time with negligible overhead.

### Why Rust?

SessionGuard needs to be:
- **Fast** — filesystem event processing can't introduce latency
- **Low memory** — it's a background daemon, not a foreground app
- **Cross-platform** — developers use macOS, Linux, and Windows
- **Reliable** — corrupting session state is worse than losing it

### Why SQLite for the registry?

It's the right tool for a single-writer, multi-reader local database. No network. No setup. Battle-tested. The entire registry for thousands of projects fits in a few MB.

### What about symlink farms?

Some tools resolve symlinks before writing session state, which breaks the "just symlink it" approach. SessionGuard handles this by rewriting internal path references rather than relying on filesystem indirection.

## CLI Reference

```
sessionguard <COMMAND>

Commands:
  start       Start the daemon (foreground or background with -d)
  stop        Stop the running daemon
  status      Show tracked projects and their session health
  watch       Register a directory tree for monitoring
  unwatch     Remove a directory from monitoring
  scan        One-time scan to discover and register existing sessions
  simulate    Dry-run a move/rename and show what would be reconciled
  log         View reconciliation event history
  doctor      Diagnose common issues (stale refs, orphaned sessions)
  export      Export session metadata for backup/migration
  import      Import session metadata from backup
  config      View or edit configuration
  version     Print version info
```

## Project Philosophy

- **Vendor-neutral.** SessionGuard doesn't favor any AI tool. The tool pattern registry is community-maintained and extensible.
- **Non-invasive.** It never modifies your code. It only touches AI tool session artifacts, and logs every change it makes.
- **Undo-friendly.** Every reconciliation action can be reversed. If something goes wrong, `sessionguard undo` restores the previous state.
- **Privacy-first.** SessionGuard never reads your code or session *contents*. It only tracks file paths, timestamps, and structural metadata. Nothing leaves your machine.
- **Unix philosophy.** It does one thing well. It watches for moves and fixes session paths. That's it.

## Roadmap

### Phase 1 — Foundation (v0.1)
- [ ] Core daemon with filesystem watching (Linux + macOS)
- [ ] SQLite registry for project ↔ session mapping
- [ ] Claude Code session reconciliation
- [ ] Cursor session reconciliation
- [ ] Basic CLI (start, stop, status, watch)
- [ ] Structured event logging

### Phase 2 — Ecosystem (v0.2)
- [ ] Windsurf, Copilot, Gemini, Codex, Aider support
- [ ] `sessionguard doctor` diagnostic command
- [ ] `sessionguard simulate` dry-run mode
- [ ] Windows support
- [ ] Homebrew and cargo distribution
- [ ] Shell completions (bash, zsh, fish)

### Phase 3 — Intelligence (v0.3)
- [ ] Auto-discovery of new AI tools via heuristics
- [ ] Session health scoring (staleness, completeness)
- [ ] Integration with `git` hooks for branch-aware sessions
- [ ] Optional session deduplication and cleanup
- [ ] Plugin API for tool-specific reconciliation logic

### Phase 4 — Community (v1.0)
- [ ] Stable API and configuration format
- [ ] Comprehensive test suite with CI
- [ ] Tool vendor partnerships for first-class support
- [ ] Session export/import for machine migration
- [ ] Documentation site

## Contributing

SessionGuard is MIT-licensed and contributions are welcome from day one.

### High-Impact Contributions

- **Tool pattern definitions** — If you use an AI coding tool, help us map its session artifacts. See `tools/` for the pattern format.
- **Platform support** — Especially Windows filesystem watching.
- **Reconciliation logic** — Each tool stores paths differently. PRs that handle edge cases for specific tools are extremely valuable.
- **Testing** — We need test fixtures for every supported tool's session format.

### Getting Started

```bash
git clone https://github.com/PilotDevo/sessionguard.git
cd sessionguard
cargo build
cargo test

# Run with debug logging
RUST_LOG=debug cargo run -- start --foreground
```

### Code of Conduct

Be kind. Be constructive. Ship code. See [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## FAQ

**Q: Does this need root/sudo?**
No. SessionGuard watches directories your user has access to. No elevated privileges required.

**Q: How much battery/CPU does it use?**
Negligible in `balanced` mode. Filesystem event APIs are interrupt-driven, not polling. SessionGuard sleeps until something happens.

**Q: What if a tool changes its session format?**
Tool pattern definitions are versioned. If a tool updates its format, the community can submit an updated pattern, and SessionGuard will handle migration between versions.

**Q: Can I use this with remote dev environments?**
Not yet. v1.0 focuses on local development. Remote/container support is on the long-term roadmap.

**Q: What if two tools conflict on session data?**
SessionGuard treats each tool's session artifacts independently. It never merges data between tools.

## License

MIT — see [LICENSE](LICENSE).

---

**Built by [Droco](https://droco.dev) — sovereign infrastructure for builders who ship.**

*SessionGuard is not affiliated with or endorsed by any of the AI tool vendors listed above.*
