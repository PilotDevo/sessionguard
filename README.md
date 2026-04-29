# SessionGuard

**A system-level daemon that keeps AI coding sessions intact when your projects move.**

[![Crates.io](https://img.shields.io/crates/v/sessionguard.svg)](https://crates.io/crates/sessionguard)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange.svg?logo=rust)](https://www.rust-lang.org)
[![CI](https://github.com/PilotDevo/sessionguard/actions/workflows/ci.yml/badge.svg)](https://github.com/PilotDevo/sessionguard/actions/workflows/ci.yml)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen.svg)](CONTRIBUTING.md)
[![Built with Tokio](https://img.shields.io/badge/async-Tokio-blue.svg)](https://tokio.rs)
[![SQLite](https://img.shields.io/badge/storage-SQLite-003B57.svg?logo=sqlite)](https://www.sqlite.org)
[![Platform: macOS | Linux](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey.svg)]()
[![Conventional Commits](https://img.shields.io/badge/commits-Conventional-FE5196.svg?logo=conventionalcommits)](https://conventionalcommits.org)

> **Status: v0.3.2** — Verified end-to-end on macOS (FSEvents) and Linux (inotify) with real-data dogfooding. Seven built-in tool patterns. `sessionguard undo` wired to the event log. A read-only local dashboard (`tools/dashboard/`) surfaces what the daemon sees. Still alpha — use it, report issues. See [ROADMAP.md](ROADMAP.md) for the v0.4 `migrate` arc.

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

Two support levels today:
- **Reconcile** — when a project moves, SessionGuard rewrites the tool's in-project session files (e.g. `.claude/settings.json`) to point at the new path, atomically and surgically.
- **Detect** — SessionGuard recognises the project as using the tool but doesn't rewrite yet, because the tool stores its session data in the user's home directory keyed on absolute project paths. Home-dir reconciliation is coming in v0.4 `migrate`.

| Tool | Session Artifacts | Support |
|------|------------------|---------|
| **Claude Code** | `.claude/`, `CLAUDE.md`, `.claudeignore` | ✅ Reconcile |
| **Cursor** | `.cursor/`, `.cursorignore`, `.cursorindexingignore` | ✅ Reconcile |
| **Windsurf** | `.windsurf/`, `.windsurfrules`, `.windsurfignore` | ✅ Reconcile |
| **Gemini CLI** | `.gemini/`, `GEMINI.md`, `.geminiignore` | ✅ Reconcile |
| **Aider** | `.aider.chat.history.md`, `.aider.conf.yml` | ✅ Reconcile *(text adapter)* |
| **Codex (OpenAI)** | `AGENTS.md`, `.codex/` | 🟡 Detect only |
| **OpenCode** | `AGENTS.md`, `opencode.json(c)`, `.opencodeignore` | 🟡 Detect only |
| **GitHub Copilot** | `.github/copilot-instructions.md` | 🔜 Planned |
| **Continue.dev** | `.continue/`, `config.json` | 🔜 Planned |
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
4. **Reconciler** — Uses format-aware adapters (JSON, TOML) to surgically rewrite only the targeted path field in session artifacts, leaving other content untouched.
5. **Event Log** — Structured SQLite log of all reconciliation actions for auditability and undo capability.

## Quick Start

### Homebrew (macOS)

```bash
brew tap PilotDevo/tap
brew install sessionguard
```

### Shell installer (Linux & macOS)

```bash
curl -fsSL https://raw.githubusercontent.com/PilotDevo/sessionguard/main/install.sh | sh
```

Auto-detects your OS and architecture, downloads the right pre-built binary, and installs to `/usr/local/bin` (or `~/.local/bin` as fallback).

### Cargo (any platform with Rust)

```bash
cargo install sessionguard
```

### Build from source

```bash
# Requires Rust 1.75+
git clone https://github.com/PilotDevo/sessionguard.git
cd sessionguard
cargo install --path .
```

### Linux autostart (systemd)

```bash
mkdir -p ~/.config/systemd/user
curl -fsSL https://raw.githubusercontent.com/PilotDevo/sessionguard/main/contrib/sessionguard.service \
    -o ~/.config/systemd/user/sessionguard.service
systemctl --user enable --now sessionguard
```

### Basic usage

```bash
# Start the daemon (foreground, watches configured directories)
sessionguard start --foreground

# Register a specific project root
sessionguard watch ~/projects/my-app

# Discover existing AI sessions under a directory
sessionguard scan ~/projects

# Check status of tracked projects + daemon state
sessionguard status
sessionguard status --format json        # machine-readable

# Inspect registered tool patterns (built-in + user + project)
sessionguard tools list
sessionguard tools list --verbose        # include session_patterns + path_fields
sessionguard tools list --format json    # full structured output

# See what would happen if you moved a project (dry run)
sessionguard simulate mv ~/projects/old-name ~/projects/new-name

# View reconciliation history
sessionguard log --last 20
sessionguard log --format json

# Reverse reconciliation actions (single-step undo)
sessionguard undo                        # undo the last reconciliation
sessionguard undo --last 5               # undo the five most recent
sessionguard undo --id 42                # undo a specific event
sessionguard undo --dry-run              # preview without writing

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

## Dashboard

A lightweight **read-only** local web UI lives in `tools/dashboard/app.py`.
It shows tracked projects, reconciliation events (live + undone), session
stores in `~/.codex`, `~/.claude/projects`, `~/.local/share/opencode`,
and every registered tool pattern. Zero dependencies beyond the Python
standard library — no `pip install`, no `npm`, no build step.

```bash
python3 tools/dashboard/app.py               # localhost:8787
python3 tools/dashboard/app.py --host 0.0.0.0 --port 8787   # LAN access
```

The dashboard opens the SQLite registry with `mode=ro` and talks to
the daemon only via `sessionguard tools list --format json` — no write
paths. Action controls (click-to-undo, click-to-migrate) are intentionally
deferred until the underlying CLI commands they'd drive are fully
designed; see [ROADMAP.md](ROADMAP.md) for v0.5+ plans.

See [`tools/dashboard/README.md`](tools/dashboard/README.md) for a
systemd `--user` unit if you want it persistent.

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

See [ROADMAP.md](ROADMAP.md) for the full living document. Short form:

**Shipped** *(v0.1 – v0.3.2)*
- Full watcher → detector → reconciler pipeline, verified end-to-end on
  macOS + Linux with real-data dogfooding (not just unit tests)
- Seven built-in tool patterns (five reconciling, two detect-only)
- `sessionguard undo` backed by an append-only event log
- Adapter-based rewriting (JSON, TOML, text fallback) with prefix-safety
  guarantees — `/home/me/code` never rewrites inside `/home/me/code-backup`
- `--format json` on `status`, `log`, and `tools` for tooling integration
- Read-only local dashboard (`tools/dashboard/`)
- CI dogfood matrix on Ubuntu + macOS (the regression gate for the
  classes of bugs unit tests historically missed)
- Homebrew tap + crates.io publishing, both automated on release

**Next — v0.4 "Migrate"**
The thesis shift: from *"watches for moves"* to *"the tool that moves
AI dev environments between disks without breaking them"*. New verbs:
`sessionguard migrate <tool> --to <path>`, `relocate <src> <dst>`,
`inventory`. btrfs snapshot integration for atomic rollback. First
dogfood target: a user's multi-GB Codex/OpenCode history → a fast
pool, safely.

**Later**
Tool pattern library (community contributions), Tauri-based local UI,
Windows support, broader launch. See [ROADMAP.md](ROADMAP.md).

## Project Philosophy

- **Vendor-neutral.** SessionGuard doesn't favor any AI tool. The tool pattern registry is community-maintained and extensible.
- **Non-invasive.** It never modifies your code. It only touches AI tool session artifacts, and logs every change it makes.
- **Undo-friendly.** Every reconciliation is append-only in the event log. `sessionguard undo` restores the previous state for the last action (`--last N` for several; `--id <n>` for a specific one; `--dry-run` to preview). Idempotent — already-undone events won't re-undo.
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

SessionGuard is built and maintained by [Droco](https://droco.io). If you find it useful, consider supporting development:

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
Create a TOML file in `~/.config/sessionguard/tools/` following the pattern in `src/tools/builtin/claude_code.toml`. No recompilation needed. Or copy one of the built-ins into `sessionguard.toml` under a `[[tools]]` section in your project.

**Q: What happens to session data my AI tool stores in `~/`, not inside the project?**
Today: SessionGuard detects the project is using the tool but doesn't rewrite the home-dir store on move — that's why Codex and OpenCode are marked *Detect only* in the table above. v0.4's `migrate` command (in active design) will close this gap by safely relocating home-dir stores alongside their project references.

**Q: Is there a GUI?**
Yes — a minimal read-only one. `python3 tools/dashboard/app.py` serves a local web UI on port 8787 with five tabs:
- **Activity** — per-project view across Claude Code / Codex / OpenCode session stores, showing which assistants have touched each project and when (with live indicators for the last 5 min)
- **Projects** — every directory the SessionGuard daemon is tracking
- **Events** — reconciliation history with undone-state badges
- **Sessions** — total store sizes per assistant
- **Tools** — registered tool patterns

No dependencies beyond the Python stdlib. An interactive UI that also drives actions (undo, migrate) is planned for v0.5+.

## License

MIT — see [LICENSE](LICENSE).

---

**Built by [Droco](https://droco.io) — sovereign infrastructure for builders who ship.**

*SessionGuard is not affiliated with or endorsed by any of the AI tool vendors listed above.*
