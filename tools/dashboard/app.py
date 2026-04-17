#!/usr/bin/env python3
# Copyright 2026 Devin R O'Loughlin / Droco LLC
# SPDX-License-Identifier: MIT
"""
SessionGuard Dashboard — read-only local web UI for inspecting tracked
projects and reconciliation events.

Design principles (these match the rest of the project):
- No external services. Reads SQLite DBs directly (read-only connection).
- No npm, no build step. Vanilla HTML/CSS/JS served from a single file.
- Stateless. The source of truth is still sessionguard's data dir.
- Safe to run anywhere the sessionguard binary would run.

Usage:
    # Point at the default sessionguard data dir (auto-detected):
    python3 app.py

    # Explicitly:
    SESSIONGUARD_DATA_DIR=~/.local/share/sessionguard python3 app.py

    # Bind to a specific interface / port:
    python3 app.py --host 0.0.0.0 --port 8787

Dependencies: only the Python standard library. No pip install needed.
"""
from __future__ import annotations

import argparse
import json
import os
import sqlite3
import subprocess
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any


# ── data-dir resolution ──────────────────────────────────────────────────────
def default_data_dir() -> Path:
    """Resolve sessionguard's data dir the same way the Rust binary does.

    Order: $SESSIONGUARD_DATA_DIR, then platform-specific ProjectDirs
    equivalents, then a .sessionguard fallback.
    """
    env = os.environ.get("SESSIONGUARD_DATA_DIR")
    if env:
        return Path(env).expanduser()

    home = Path.home()
    import sys

    if sys.platform == "darwin":
        return home / "Library/Application Support/dev.droco.sessionguard"
    if sys.platform.startswith("linux"):
        xdg = os.environ.get("XDG_DATA_HOME")
        base = Path(xdg) if xdg else home / ".local/share"
        return base / "sessionguard"
    return home / ".sessionguard"


# ── db helpers (read-only, fail-soft) ────────────────────────────────────────
def _connect(db_path: Path) -> sqlite3.Connection | None:
    if not db_path.is_file():
        return None
    # `mode=ro` — SQLite refuses writes on this connection.
    uri = f"file:{db_path}?mode=ro"
    conn = sqlite3.connect(uri, uri=True, timeout=2)
    conn.row_factory = sqlite3.Row
    return conn


def list_projects(data_dir: Path) -> list[dict[str, Any]]:
    db = _connect(data_dir / "registry.db")
    if db is None:
        return []
    try:
        cur = db.execute(
            "SELECT id, path, created_at, updated_at FROM projects ORDER BY id DESC"
        )
        projects = []
        for row in cur:
            path = row["path"]
            on_disk = Path(path).is_dir()
            arts = db.execute(
                """
                SELECT tool_name, artifact_path, created_at
                FROM session_artifacts WHERE project_id = ?1
                ORDER BY tool_name
                """,
                (row["id"],),
            ).fetchall()
            projects.append(
                {
                    "id": row["id"],
                    "path": path,
                    "on_disk": on_disk,
                    "created_at": row["created_at"],
                    "updated_at": row["updated_at"],
                    "artifacts": [dict(a) for a in arts],
                }
            )
        return projects
    finally:
        db.close()


def list_events(data_dir: Path, limit: int = 100) -> list[dict[str, Any]]:
    db = _connect(data_dir / "event_log.db")
    if db is None:
        return []
    try:
        cur = db.execute(
            """
            SELECT id, timestamp, tool_name, file_path, field, format,
                   old_value, new_value, undone_at
            FROM events ORDER BY id DESC LIMIT ?1
            """,
            (limit,),
        )
        return [dict(r) for r in cur]
    finally:
        db.close()


def list_tools() -> list[dict[str, Any]]:
    """Shell out to `sessionguard tools list --verbose` and parse.

    Delegating to the binary keeps it authoritative — the same resolution
    chain (built-in + system + user + project) that the daemon uses.
    """
    binary = os.environ.get("SESSIONGUARD_BIN", "sessionguard")
    try:
        out = subprocess.run(
            [binary, "tools", "list", "--verbose"],
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return []
    tools: list[dict[str, Any]] = []
    current: dict[str, Any] | None = None
    for line in out.stdout.splitlines():
        s = line.rstrip()
        if not s:
            continue
        if s.startswith("NAME") or s.endswith(" registered. Use --verbose for patterns."):
            continue
        if not s.startswith(" "):
            parts = s.split()
            if len(parts) >= 3:
                if current:
                    tools.append(current)
                current = {
                    "name": parts[0],
                    "display_name": " ".join(parts[1:-1]),
                    "version": parts[-1],
                    "session_patterns": [],
                    "path_fields": [],
                    "on_move": None,
                }
        elif current is not None:
            stripped = s.strip()
            if stripped.startswith("- "):
                item = stripped[2:]
                ctx = current.get("_ctx") or ""
                if "session_patterns" in ctx:
                    current["session_patterns"].append(item)
                elif "path_fields" in ctx:
                    current["path_fields"].append(item)
            elif stripped.startswith("session_patterns:"):
                current["_ctx"] = "session_patterns"
            elif stripped.startswith("path_fields:"):
                current["_ctx"] = "path_fields"
            elif stripped.startswith("on_move:"):
                current["on_move"] = stripped.split(":", 1)[1].strip()
                current.pop("_ctx", None)
    if current:
        tools.append(current)
    for t in tools:
        t.pop("_ctx", None)
    return tools


# ── home-dir session stores ──────────────────────────────────────────────────
#
# AI tools split over two storage conventions:
#   1. *in-project* state — SessionGuard reconciles this automatically
#      via the tool patterns' `path_fields`.
#   2. *home-dir* state — session histories under ~/.codex, ~/.local/share,
#      etc., keyed on absolute project paths. SessionGuard can't rewrite
#      these yet (v0.4 `migrate` scope), but it CAN surface them so you
#      know what's there.
#
# This section enumerates home-dir stores for visibility. Results are
# cached for 30 seconds because walking a multi-GB tree on every 3s poll
# is wasteful.

_HOME_SESSION_STORES = [
    {
        "tool": "claude_code",
        "display": "Claude Code",
        "path": "~/.claude/projects",
        "kind": "dir_per_project",
    },
    {
        "tool": "codex",
        "display": "Codex",
        "path": "~/.codex/sessions",
        "kind": "jsonl_tree",
    },
    {
        "tool": "opencode",
        "display": "OpenCode",
        "path": "~/.local/share/opencode",
        "kind": "mixed",
    },
    {
        "tool": "cursor",
        "display": "Cursor",
        "path": "~/.cursor",
        "kind": "mixed",
    },
    {
        "tool": "gemini_cli",
        "display": "Gemini CLI",
        "path": "~/.gemini",
        "kind": "mixed",
    },
]

# Max files we'll walk per store before giving up and calling the count
# an estimate. Protects against pathological cases (millions of files).
_HOME_WALK_CAP = 200_000

_home_sessions_cache: dict[str, Any] = {"ts": 0.0, "data": None}


def _store_summary(store: dict[str, Any]) -> dict[str, Any]:
    path = Path(os.path.expanduser(store["path"]))
    out = {
        "tool": store["tool"],
        "display": store["display"],
        "path": str(path),
        "kind": store["kind"],
        "present": path.exists(),
        "count": 0,
        "size_bytes": 0,
        "mtime": None,
        "truncated": False,
    }
    if not path.exists():
        return out

    try:
        out["mtime"] = path.stat().st_mtime
    except OSError:
        pass

    total_files = 0
    size_bytes = 0

    try:
        if store["kind"] == "dir_per_project":
            # Claude Code: one directory per project, usually flat.
            out["count"] = sum(1 for e in path.iterdir() if e.is_dir())
        elif store["kind"] == "jsonl_tree":
            # Codex: ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl
            for root, _, files in os.walk(path):
                for f in files:
                    if f.endswith(".jsonl"):
                        out["count"] += 1
                    total_files += 1
                    if total_files >= _HOME_WALK_CAP:
                        out["truncated"] = True
                        break
                if out["truncated"]:
                    break
        else:
            # mixed: count top-level entries and let size speak for itself
            out["count"] = sum(1 for _ in path.iterdir())

        # Aggregate size (capped).
        for root, _, files in os.walk(path):
            for f in files:
                try:
                    size_bytes += (Path(root) / f).stat().st_size
                except OSError:
                    pass
                total_files += 1
                if total_files >= _HOME_WALK_CAP:
                    out["truncated"] = True
                    break
            if out["truncated"]:
                break
    except (OSError, PermissionError) as e:
        out["error"] = str(e)

    out["size_bytes"] = size_bytes
    return out


def list_home_sessions() -> list[dict[str, Any]]:
    now = time.time()
    cached = _home_sessions_cache.get("data")
    if cached is not None and (now - _home_sessions_cache["ts"]) < 30.0:
        return cached
    data = [_store_summary(s) for s in _HOME_SESSION_STORES]
    _home_sessions_cache["ts"] = now
    _home_sessions_cache["data"] = data
    return data


def daemon_status(data_dir: Path) -> dict[str, Any]:
    pid_file = data_dir / "sessionguard.pid"
    if not pid_file.is_file():
        return {"running": False, "pid": None}
    try:
        pid = int(pid_file.read_text().strip())
    except ValueError:
        return {"running": False, "pid": None, "error": "invalid PID file"}
    try:
        os.kill(pid, 0)
        return {"running": True, "pid": pid}
    except ProcessLookupError:
        return {"running": False, "pid": pid, "error": "stale PID file"}
    except PermissionError:
        return {"running": True, "pid": pid, "note": "owned by another user"}


# ── http handler ─────────────────────────────────────────────────────────────
# Notes on security:
# - The backend only accepts GET requests and performs no mutations.
# - The SQLite connection is opened with mode=ro — writes are refused.
# - The frontend escapes every database-sourced string before injecting it
#   into the DOM, guarding against XSS if a watched project path or session
#   file happens to contain HTML-like characters. See `esc()` below.
# - Default bind is 127.0.0.1; users who bind to 0.0.0.0 should trust their
#   network — the dashboard exposes project paths and event history that may
#   be sensitive.
INDEX_HTML = r"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>SessionGuard Dashboard</title>
<style>
  :root {
    --bg: #0e1116;
    --panel: #161b22;
    --border: #30363d;
    --ink: #e6edf3;
    --dim: #8b949e;
    --accent: #58a6ff;
    --good: #3fb950;
    --warn: #d29922;
    --bad: #f85149;
    --undone: #a371f7;
  }
  * { box-sizing: border-box; }
  body {
    margin: 0; font: 14px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
    background: var(--bg); color: var(--ink);
  }
  header {
    padding: 1rem 1.5rem; border-bottom: 1px solid var(--border);
    display: flex; align-items: center; justify-content: space-between; gap: 1rem;
  }
  header h1 { font-size: 1rem; font-weight: 600; margin: 0; }
  header .muted { color: var(--dim); font-size: 12px; }
  .dot { display: inline-block; width: 8px; height: 8px; border-radius: 50%; margin-right: 4px; }
  .dot.good { background: var(--good); }
  .dot.bad { background: var(--bad); }
  nav {
    display: flex; gap: 0; border-bottom: 1px solid var(--border);
    padding: 0 1.5rem; background: var(--panel);
  }
  nav button {
    background: transparent; border: 0; color: var(--dim);
    padding: 0.75rem 1rem; font: inherit; cursor: pointer;
    border-bottom: 2px solid transparent;
  }
  nav button.active { color: var(--ink); border-color: var(--accent); }
  nav button:hover { color: var(--ink); }
  main { padding: 1.25rem 1.5rem; }
  .panel {
    background: var(--panel); border: 1px solid var(--border);
    border-radius: 6px; padding: 0.75rem 1rem; margin-bottom: 1rem;
  }
  table { width: 100%; border-collapse: collapse; font-size: 13px; }
  th, td { padding: 8px 10px; text-align: left; border-bottom: 1px solid var(--border); vertical-align: top; }
  th { font-weight: 500; color: var(--dim); }
  tr:last-child td { border-bottom: 0; }
  code { font: 12px/1.4 "SF Mono", Menlo, Consolas, monospace; background: #0b0f14; padding: 1px 4px; border-radius: 3px; }
  .tag { display: inline-block; padding: 1px 6px; border-radius: 10px; font-size: 11px; }
  .tag.good { background: #0e1f12; color: var(--good); }
  .tag.warn { background: #2a1f08; color: var(--warn); }
  .tag.bad  { background: #2c0d0d; color: var(--bad); }
  .tag.undone { background: #1f1333; color: var(--undone); }
  .muted { color: var(--dim); }
  .row-stale td { opacity: 0.55; }
  .hidden { display: none; }
  .arrow { color: var(--dim); margin: 0 6px; }
  .empty { padding: 2rem; text-align: center; color: var(--dim); }
</style>
</head>
<body>
<header>
  <div>
    <h1>SessionGuard Dashboard</h1>
    <div class="muted" id="status-line">loading...</div>
  </div>
  <div class="muted" id="refresh-note">auto-refresh every 3s</div>
</header>
<nav>
  <button data-tab="projects" class="active">Projects</button>
  <button data-tab="events">Events</button>
  <button data-tab="sessions">Sessions</button>
  <button data-tab="tools">Tools</button>
</nav>
<main>
  <section id="projects"></section>
  <section id="events" class="hidden"></section>
  <section id="sessions" class="hidden"></section>
  <section id="tools" class="hidden"></section>
</main>
<script>
(() => {
  const state = { tab: "projects", data: null };

  // HTML-escape every database-sourced string before inserting into the DOM.
  // The dashboard reads watched-project paths, tool field values, and file
  // paths that could conceivably contain HTML-significant characters. `esc`
  // guards the innerHTML assignments below.
  const esc = (s) => String(s ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");

  const fmtTime = (t) => t ? String(t).replace("T", " ").replace("Z", "") : "-";

  const renderProjects = (projects) => {
    if (!projects.length) return `<div class="empty">no projects tracked yet<br><code>sessionguard watch &lt;path&gt;</code> or <code>sessionguard scan ~/projects</code> to add some</div>`;
    const rows = projects.map(p => {
      const stale = !p.on_disk;
      const status = stale
        ? `<span class="tag bad">missing</span>`
        : `<span class="tag good">ok</span>`;
      const artifacts = p.artifacts.length
        ? p.artifacts.map(a => `<div><code>${esc(a.tool_name)}</code> ${esc(a.artifact_path)}</div>`).join("")
        : `<span class="muted">-</span>`;
      return `
        <tr class="${stale ? 'row-stale' : ''}">
          <td>${esc(p.id)}</td>
          <td><code>${esc(p.path)}</code></td>
          <td>${status}</td>
          <td>${artifacts}</td>
          <td class="muted">${esc(fmtTime(p.updated_at))}</td>
        </tr>`;
    }).join("");
    return `
      <div class="panel">
        <table>
          <thead><tr>
            <th>#</th><th>Path</th><th>State</th><th>Artifacts</th><th>Updated</th>
          </tr></thead>
          <tbody>${rows}</tbody>
        </table>
      </div>`;
  };

  const renderEvents = (events) => {
    if (!events.length) return `<div class="empty">no reconciliation events yet</div>`;
    const rows = events.map(e => {
      const tag = e.undone_at
        ? `<span class="tag undone">undone ${esc(fmtTime(e.undone_at))}</span>`
        : `<span class="tag good">live</span>`;
      return `
        <tr>
          <td>${esc(e.id)}</td>
          <td class="muted">${esc(fmtTime(e.timestamp))}</td>
          <td>${tag}</td>
          <td><code>${esc(e.tool_name)}</code></td>
          <td><code>${esc(e.format)}</code></td>
          <td>
            <div><code>${esc(e.file_path)}</code></div>
            <div class="muted">${esc(e.field)}: <code>${esc(e.old_value)}</code><span class="arrow">→</span><code>${esc(e.new_value)}</code></div>
          </td>
        </tr>`;
    }).join("");
    return `
      <div class="panel">
        <table>
          <thead><tr>
            <th>#</th><th>When</th><th>State</th><th>Tool</th><th>Fmt</th><th>Change</th>
          </tr></thead>
          <tbody>${rows}</tbody>
        </table>
      </div>`;
  };

  const fmtSize = (b) => {
    if (!b) return "0 B";
    const units = ["B", "KB", "MB", "GB", "TB"];
    let i = 0, n = b;
    while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
    return (n >= 10 ? Math.round(n) : n.toFixed(1)) + " " + units[i];
  };
  const fmtMtime = (t) => {
    if (!t) return "-";
    const d = new Date(t * 1000);
    return d.toISOString().replace("T", " ").replace(/\.\d+Z$/, "");
  };

  const renderSessions = (sessions) => {
    if (!sessions.length) return `<div class="empty">no session stores known</div>`;
    const rows = sessions.map(s => {
      const tag = !s.present
        ? `<span class="tag muted">absent</span>`
        : s.error
          ? `<span class="tag warn">${esc(s.error)}</span>`
          : `<span class="tag good">present</span>`;
      const countDisplay = s.truncated
        ? `${esc(s.count)}+ <span class="muted">(truncated)</span>`
        : esc(s.count);
      return `
        <tr>
          <td><strong>${esc(s.display)}</strong><br><code>${esc(s.tool)}</code></td>
          <td><code>${esc(s.path)}</code></td>
          <td>${tag}</td>
          <td>${countDisplay}</td>
          <td>${esc(fmtSize(s.size_bytes))}</td>
          <td class="muted">${esc(fmtMtime(s.mtime))}</td>
        </tr>`;
    }).join("");
    return `
      <div class="panel">
        <table>
          <thead><tr>
            <th>Tool</th><th>Path</th><th>State</th><th>Items</th><th>Size</th><th>Last Modified</th>
          </tr></thead>
          <tbody>${rows}</tbody>
        </table>
        <div class="muted" style="margin-top:0.75rem;font-size:12px">
          Home-dir session stores. SessionGuard reconciles <em>in-project</em> state today;
          these home-dir stores are visibility-only until v0.4 <code>migrate</code> lands.
          (cached 30s)
        </div>
      </div>`;
  };

  const renderTools = (tools) => {
    if (!tools.length) return `<div class="empty">no tool patterns loaded</div>`;
    return tools.map(t => `
      <div class="panel">
        <div><strong>${esc(t.display_name)}</strong> <code>${esc(t.name)}</code>
             <span class="muted">v${esc(t.version)}${t.on_move ? ' · on_move: ' + esc(t.on_move) : ''}</span></div>
        <div style="margin-top:0.5rem"><span class="muted">session_patterns:</span>
          ${t.session_patterns.map(p => `<code>${esc(p)}</code>`).join(" ") || "<span class='muted'>-</span>"}
        </div>
        ${t.path_fields.length ? `<div><span class="muted">path_fields:</span>
          ${t.path_fields.map(p => `<code>${esc(p)}</code>`).join(" ")}
        </div>` : ""}
      </div>`).join("");
  };

  const render = () => {
    if (!state.data) return;
    const d = state.data;
    const statusDot = d.daemon.running ? "good" : "bad";
    const statusText = d.daemon.running
      ? `daemon up (PID ${esc(d.daemon.pid)})`
      : "daemon stopped";
    document.getElementById("status-line").innerHTML =
      `<span class="dot ${statusDot}"></span>${statusText}
       · data_dir: <code>${esc(d.data_dir)}</code>
       · ${d.projects.length} projects · ${d.events.length} events`;

    document.querySelectorAll("nav button").forEach(b => {
      b.classList.toggle("active", b.dataset.tab === state.tab);
    });
    ["projects", "events", "sessions", "tools"].forEach(t => {
      document.getElementById(t).classList.toggle("hidden", state.tab !== t);
    });

    document.getElementById("projects").innerHTML = renderProjects(d.projects);
    document.getElementById("events").innerHTML = renderEvents(d.events);
    document.getElementById("sessions").innerHTML = renderSessions(d.sessions || []);
    document.getElementById("tools").innerHTML = renderTools(d.tools);
  };

  const fetchData = async () => {
    try {
      const r = await fetch("/api/state");
      state.data = await r.json();
      render();
    } catch (e) {
      document.getElementById("status-line").textContent =
        "error fetching state: " + e;
    }
  };

  document.querySelectorAll("nav button").forEach(b => {
    b.addEventListener("click", () => { state.tab = b.dataset.tab; render(); });
  });

  fetchData();
  setInterval(fetchData, 3000);
})();
</script>
</body>
</html>"""


class Handler(BaseHTTPRequestHandler):
    # Suppress the default access-log spew — the dashboard polls every 3s.
    def log_message(self, *_args: Any) -> None:
        pass

    def _json(self, payload: Any, status: int = 200) -> None:
        body = json.dumps(payload, default=str).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _html(self, body: str) -> None:
        raw = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self) -> None:  # noqa: N802 (std http name)
        data_dir: Path = self.server.data_dir  # type: ignore[attr-defined]

        if self.path == "/":
            self._html(INDEX_HTML)
        elif self.path.startswith("/api/state"):
            self._json(
                {
                    "data_dir": str(data_dir),
                    "daemon": daemon_status(data_dir),
                    "projects": list_projects(data_dir),
                    "events": list_events(data_dir, limit=200),
                    "tools": list_tools(),
                    "sessions": list_home_sessions(),
                }
            )
        elif self.path == "/healthz":
            self._json({"ok": True})
        else:
            self._json({"error": "not found", "path": self.path}, status=404)


# ── entrypoint ───────────────────────────────────────────────────────────────
def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1", help="bind address")
    parser.add_argument("--port", type=int, default=8787, help="bind port")
    parser.add_argument(
        "--data-dir",
        default=None,
        help="override sessionguard data dir (defaults to $SESSIONGUARD_DATA_DIR or platform default)",
    )
    args = parser.parse_args()

    data_dir = Path(args.data_dir).expanduser() if args.data_dir else default_data_dir()

    server = ThreadingHTTPServer((args.host, args.port), Handler)
    server.data_dir = data_dir  # type: ignore[attr-defined]

    print("SessionGuard Dashboard")
    print(f"  data_dir: {data_dir}")
    reg = "registry.db" if (data_dir / "registry.db").exists() else "(no registry yet)"
    log = "event_log.db" if (data_dir / "event_log.db").exists() else "(no event log yet)"
    print(f"  reading:  {reg}, {log}")
    print(f"  http://{args.host}:{args.port}/  (Ctrl-C to stop)")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nshutting down")


if __name__ == "__main__":
    main()
