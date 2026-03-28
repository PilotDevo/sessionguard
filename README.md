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

> **Status: Early Development (v0.1-dev)** — Core scaffold is in place, CLI is functional, session detection works. Reconciliation pipeline is stubbed. Not yet ready for production use.

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

Tool support is defined via runtime-loaded TOML patterns — add new tools without recompiling.

| Tool | Session Artifacts | Status |
|------|------------------|--------|
| **Claude Code** | `.claude/`, `CLAUDE.md`, `.claudeignore` | 🔨 In Progress |
| **Cursor** | `.cursor/`, `.cursorignore`, `.cursorindexingignore` | 🔨 In Progress |
| **Windsurf (Codeium)** | `.windsurf/`, `.windsurfrules`, cascade history | 🔜 Planned |
| **GitHub Copilot** | `.github/copilot-instructions.md`, VS Code chat history | 🔜 Planned |
| **Gemini Code Assist** | `.gemini/`, `GEMINI.md`, session context | 🔜 Planned |
| **Codex (OpenAI)** | `codex.md`, `.codex/`, sandbox state | 🔜 Planned |
| **Aider** | `.aider*`, `.aider.conf.yml`, chat history | 🔜 Planned |
| **Continue.dev** | `.continue/`, `config.json`, session logs | 🔜 Planned |
| **Custom / Other** | User-defined patterns via config TOML | ✅ Supported |

> **Tool authors:** We'd love your help defining the canonical session artifact list for your tool. See [Contributing](#contributing).

## How It Works

```
┌─────────────────────────────────────────────────────────┐
│                    SessionGuard Daemon                   │
│                                                         │
│  ┌──────────┐   ┌──────────────┐   ┌────────────────┐  │
│  │ Watcher  │──▶│   Detector   │──▶│  Reconciler    │  │
│  │ (notify) │   │ (tool TOML)  │   │ (path rewrite) │  │
│  └──────────┘   └──────────────┘   └────────────────┘  │
│       │                                     │           │
│       ▼                                     ▼           │
│  ┌──────────┐                     ┌────────────────┐    │
│  │ Registry │                     │  Event Log     │    │
│  │ (SQLite) │                     │  (structured)  │    │
│  └──────────┘                     └────────────────┘    │
└─────────────────────────────────────────────────────────┘
```

1. **Watcher** — Listens for filesystem events (FSEvents on macOS, inotify on Linux) targeting registered project roots via the [notify](https://crates.io/crates/notify) crate.
2. **Detector** — Maintains a registry of AI tool session patterns loaded from TOML at runtime. When a move/rename event fires, it identifies which session artifacts are affected.
3. **Registry** — A lightweight SQLite database mapping project roots to their associated session artifacts and tool configurations.
4. **Reconciler** — Rewrites internal path references in session artifacts so tools pick up where they left off.
5. **Event Log** — Structured SQLite log of all reconciliation actions for auditability and undo capability.

## Quick Start

> **Note:** SessionGuard is in early development. Building from source is currently the only installation method.

### Install from source

```bash
# Requires Rust 1.75+
git clone https://github.com/PilotDevo/sessionguard.git
cd sessionguard
cargo install --path .
```

### Basic usage

```bash
# Start the daemon (foreground, watches configured directories)
sessionguard start --foreground

# Register a specific project root
sessionguard watch ~/projects/my-app

# Check status of tracked sessions
sessionguard status

# Scan directories to discover existing AI sessions
sessionguard scan

# See what would happen if you moved a project (dry run)
sessionguard simulate mv ~/projects/old-name ~/projects/new-name

# View reconciliation history
sessionguard log --last 20

# Diagnose common issues
sessionguard doctor

# Generate shell completions
sessionguard completions zsh > ~/.zfunc/_sessionguard
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

# Custom tool definitions (loaded at runtime, no recompile needed)
[[tools]]
name = "my-internal-tool"
display_name = "My Tool"
session_patterns = [".mytool/", "mytool.config.json"]
on_move = "rewrite_paths"

[[tools.path_fields]]
file = "mytool.config.json"
field = "project_root"
format = "json"
```

Tool patterns can also be placed as individual TOML files in `~/.config/sessionguard/tools/`.

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

### Why runtime TOML patterns?

AI tools evolve fast. New tools appear constantly. By defining tool patterns as data (TOML files) rather than code, anyone can add support for a new tool by dropping a file in their config directory — no recompilation, no PRs required.

## Roadmap

### Phase 1 — Foundation (v0.1) `← current`
- [x] Core daemon with filesystem watching (macOS + Linux)
- [x] SQLite registry for project-to-session mapping
- [x] Runtime-loaded tool pattern system (TOML)
- [x] Built-in patterns for Claude Code and Cursor
- [x] Full CLI (start, stop, status, watch, scan, simulate, doctor, log, export/import, config, completions)
- [x] Structured event logging with SQLite audit trail
- [x] CI/CD pipeline (GitHub Actions, cargo-deny, dependabot)
- [ ] End-to-end reconciliation pipeline (watcher → detector → reconciler)
- [ ] Path rewriting for Claude Code session databases
- [ ] Path rewriting for Cursor session state
- [ ] Integration test suite with sandbox environments

### Phase 2 — Ecosystem (v0.2)
- [ ] Windsurf, Copilot, Gemini, Codex, Aider tool patterns
- [ ] `sessionguard undo` — reverse reconciliation actions
- [ ] Background daemonization (`-d` flag)
- [ ] Windows support
- [ ] Homebrew formula
- [ ] Publish to crates.io

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

## Project Philosophy

- **Vendor-neutral.** SessionGuard doesn't favor any AI tool. The tool pattern registry is community-maintained and extensible.
- **Non-invasive.** It never modifies your code. It only touches AI tool session artifacts, and logs every change it makes.
- **Undo-friendly.** Every reconciliation action can be reversed. If something goes wrong, `sessionguard undo` restores the previous state.
- **Privacy-first.** SessionGuard never reads your code or session *contents*. It only tracks file paths, timestamps, and structural metadata. Nothing leaves your machine.
- **Unix philosophy.** It does one thing well. It watches for moves and fixes session paths. That's it.

## Contributing

SessionGuard is MIT-licensed and contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for the full guide.

### High-Impact Contributions

- **Tool pattern definitions** — Add TOML files to `src/tools/builtin/` for new AI tools
- **Platform support** — Especially Windows filesystem watching
- **Reconciliation logic** — Each tool stores paths differently. PRs that handle edge cases are extremely valuable
- **Testing** — Test fixtures for every supported tool's session format

### Development Setup

```bash
git clone https://github.com/PilotDevo/sessionguard.git
cd sessionguard
cargo build
cargo test

# Run with debug logging
RUST_LOG=debug cargo run -- start --foreground

# If you have `just` installed:
just check   # fmt + clippy + test
just run     # daemon foreground with debug logging
```

### Code of Conduct

Be kind. Be constructive. Ship code. See [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## Supporting the Project

SessionGuard is built and maintained by [Droco](https://droco.dev). If you find it useful, consider supporting development:

- **Star the repo** — It helps with visibility
- **Contribute** — Code, tool patterns, bug reports, and docs are all welcome
- **Sponsor** — [GitHub Sponsors](https://github.com/sponsors/PilotDevo)
- **Share** — Tell other developers about SessionGuard

## FAQ

**Q: Does this need root/sudo?**
No. SessionGuard watches directories your user has access to. No elevated privileges required.

**Q: How much battery/CPU does it use?**
Negligible in `balanced` mode. Filesystem event APIs are interrupt-driven, not polling. SessionGuard sleeps until something happens.

**Q: What if a tool changes its session format?**
Tool pattern definitions are versioned TOML files. If a tool updates its format, submit an updated pattern — no code changes needed.

**Q: Can I use this with remote dev environments?**
Not yet. v1.0 focuses on local development. Remote/container support is on the long-term roadmap.

**Q: What if two tools conflict on session data?**
SessionGuard treats each tool's session artifacts independently. It never merges data between tools.

**Q: How do I add support for a new AI tool?**
Create a TOML file in `~/.config/sessionguard/tools/` following the pattern in `src/tools/builtin/claude_code.toml`. No recompilation needed.

## License

MIT — see [LICENSE](LICENSE).

---

**Built by [Droco](https://droco.io) — sovereign infrastructure for builders who ship.**

*SessionGuard is not affiliated with or endorsed by any of the AI tool vendors listed above.*
