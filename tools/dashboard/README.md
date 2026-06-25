# SessionGuard Dashboard

A tiny local web UI for inspecting the SessionGuard daemon's state —
tracked projects, reconciliation events (including undone ones), and
registered tool patterns. **Read-only**: the dashboard opens the SQLite
registry with `mode=ro` and performs no mutations. Use the CLI (`sessionguard
watch`, `undo`, `migrate`, etc.) for any action.

Built for quick, zero-dependency dogfooding. No `pip install`, no `npm`,
no Docker — just the Python standard library.

## Quickstart

```bash
# Auto-detect the data dir, bind to 127.0.0.1:8787
python3 tools/dashboard/app.py

# Bind on LAN so you can hit it from another machine
python3 tools/dashboard/app.py --host 0.0.0.0 --port 8787

# Point at a specific data dir (e.g., when running as a different user)
python3 tools/dashboard/app.py --data-dir ~/.local/share/sessionguard
```

Open <http://127.0.0.1:8787/> in a browser. The page auto-refreshes every
3 seconds.

## What it shows

- **Activity** *(default tab)* — per-project view across the three known
  home-directory session stores: Claude Code (`~/.claude/projects`),
  Codex (`~/.codex/sessions/*.jsonl`, joined by `cwd` from each session's
  first JSON line), and OpenCode (`~/.local/share/opencode/opencode.db`,
  joined by `session.directory`). One row per project; cells show
  per-assistant session counts and most-recent activity. Projects
  registered with the SessionGuard daemon are tagged `tracked`.
  🟢 = touched within 5 min, 🟡 = within 1 hour. Cached 30s.
- **Projects** — every directory registered via `sessionguard watch` or
  `sessionguard scan`, with its detected artifact files and whether the
  path still exists on disk.
- **Events** — every reconciliation action from the event log, marked
  **live** or **undone** based on `undone_at`. Rolled back with
  `sessionguard undo` — they stay visible here but show as undone.
- **Sessions** — total session-store sizes per assistant (counts +
  bytes + last-modified). Companion view to **Activity**: where
  Activity flips the axis to "by project," Sessions answers "how big
  is each store?"
- **Tools** — every registered tool pattern (built-in, system, user, and
  project-level), with their session patterns and path fields. Sourced
  from `sessionguard tools list --format json` so the resolution chain
  matches the daemon's view.

## Running as a systemd user service

```ini
# ~/.config/systemd/user/sessionguard-dashboard.service
[Unit]
Description=SessionGuard Dashboard
After=default.target

[Service]
Type=simple
ExecStart=/usr/bin/python3 /path/to/sessionguard/tools/dashboard/app.py --host 127.0.0.1 --port 8787
Restart=on-failure

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now sessionguard-dashboard.service
```

## Limitations

This is a **read-only inspection tool**, not a full control surface. By
design:

- No undo / migrate / watch actions from the UI — it's read-only by design.
  Use the CLI for any change (`migrate`, `undo`, `migrate-cleanup` all shipped
  in v0.4). A write-capable interactive UI remains a later-version idea
  (see `ROADMAP.md`).
- The `Tools` tab reads `sessionguard tools list --format json` (as do the
  other tabs that need the resolution chain).
- `--host 0.0.0.0` exposes project paths and event history over the
  network. If you bind beyond localhost, make sure the network is
  trusted — there's no authentication layer.
