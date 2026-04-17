#!/usr/bin/env bash
# Copyright 2026 Devin R O'Loughlin / Droco LLC
# SPDX-License-Identifier: MIT
#
# scripts/dogfood.sh — End-to-end reconciliation smoke test.
#
# Creates a synthetic Claude Code project with an isolated SessionGuard
# config and data directory, starts the daemon, moves the project, and
# verifies that `.claude/settings.json` was rewritten to the new path.
#
# The script is self-contained and cleans up after itself — safe to run
# anywhere. It never touches the operator's real registry or config.
#
# Usage:
#   scripts/dogfood.sh                      # uses `sessionguard` from PATH
#   SESSIONGUARD_BIN=./target/release/sessionguard scripts/dogfood.sh
#
# Exit codes:
#   0  reconciliation succeeded (path was rewritten)
#   1  reconciliation did NOT happen (path unchanged)
#   2  environment error (missing binary, etc.)

set -euo pipefail

# ── setup ────────────────────────────────────────────────────────────────
WORKDIR=$(mktemp -d -t sg-dogfood-XXXXXX)
OLD_NAME="dogfood-alpha"
NEW_NAME="dogfood-beta"
OLD_PATH="$WORKDIR/$OLD_NAME"
NEW_PATH="$WORKDIR/$NEW_NAME"
SG_PID=""

cleanup() {
    if [[ -n "$SG_PID" ]] && kill -0 "$SG_PID" 2>/dev/null; then
        kill "$SG_PID" 2>/dev/null || true
        wait "$SG_PID" 2>/dev/null || true
    fi
    rm -rf "$WORKDIR"
}
trap cleanup EXIT INT TERM

# ── locate binary ────────────────────────────────────────────────────────
SG=${SESSIONGUARD_BIN:-$(command -v sessionguard 2>/dev/null || true)}
if [[ -z "$SG" ]]; then
    echo "error: sessionguard not found." >&2
    echo "  install with 'cargo install sessionguard'" >&2
    echo "  or set SESSIONGUARD_BIN=/path/to/sessionguard" >&2
    exit 2
fi

# ── fixture ──────────────────────────────────────────────────────────────
mkdir -p "$OLD_PATH/.claude"
printf '{"project_path": "%s", "model": "opus", "notes": "cloned from %s"}' \
    "$OLD_PATH" "$OLD_PATH" > "$OLD_PATH/.claude/settings.json"
cat > "$OLD_PATH/CLAUDE.md" <<'EOF'
# Dogfood test project
EOF

# Isolated config + data dirs — never touch the operator's real state
DATA_DIR="$WORKDIR/sg-data"
CONFIG_FILE="$WORKDIR/config.toml"
mkdir -p "$DATA_DIR"
export SESSIONGUARD_DATA_DIR="$DATA_DIR"
cat > "$CONFIG_FILE" <<EOF
watch_roots = ["$WORKDIR"]
watch_mode = "balanced"
EOF

# ── banner ───────────────────────────────────────────────────────────────
echo "╭─ sessionguard dogfood ───────────────────────────────────────────"
echo "│ binary : $SG"
echo "│ version: $("$SG" version 2>&1)"
echo "│ host   : $(uname -s) $(uname -r)"
echo "│ workdir: $WORKDIR"
echo "│ move   : $OLD_NAME → $NEW_NAME"
echo "╰──────────────────────────────────────────────────────────────────"
echo

# ── start daemon ─────────────────────────────────────────────────────────
LOG="$WORKDIR/daemon.log"
echo "▶ starting daemon (RUST_LOG=debug)..."
RUST_LOG=sessionguard=debug "$SG" --config "$CONFIG_FILE" start --foreground \
    > "$LOG" 2>&1 &
SG_PID=$!

# Wait up to 5s for the watcher to report it's watching our dir
for _ in $(seq 1 10); do
    if grep -q "watching directory" "$LOG" 2>/dev/null; then break; fi
    sleep 0.5
done
if ! kill -0 "$SG_PID" 2>/dev/null; then
    echo "✗ daemon exited during startup"
    echo "── log ──"; cat "$LOG"
    exit 1
fi
echo "  ✓ daemon up (PID $SG_PID)"

# ── register + move ──────────────────────────────────────────────────────
echo "▶ registering project..."
"$SG" --config "$CONFIG_FILE" watch "$OLD_PATH" 2>&1 | sed 's/^/  /'

echo "▶ moving directory..."
mv "$OLD_PATH" "$NEW_PATH"

# Give notify a moment to emit + daemon to process
sleep 3

# ── verify ───────────────────────────────────────────────────────────────
SETTINGS_FILE="$NEW_PATH/.claude/settings.json"
if [[ ! -f "$SETTINGS_FILE" ]]; then
    echo "✗ settings.json missing at new location"
    exit 1
fi

FINAL=$(python3 -c "
import json, sys
with open('$SETTINGS_FILE') as f:
    print(json.load(f)['project_path'])
" 2>/dev/null || echo "<parse-error>")

echo
echo "▶ result:"
echo "  expected project_path: $NEW_PATH"
echo "  actual project_path:   $FINAL"
echo

# ── daemon log ───────────────────────────────────────────────────────────
echo "── daemon log (last 40 lines) ────────────────────────────────────"
tail -40 "$LOG"
echo "──────────────────────────────────────────────────────────────────"
echo

# ── verdict ──────────────────────────────────────────────────────────────
if [[ "$FINAL" == "$NEW_PATH" ]]; then
    # Also verify the "notes" field was NOT touched (prefix-safety proof)
    NOTES=$(python3 -c "
import json
with open('$SETTINGS_FILE') as f:
    print(json.load(f).get('notes', ''))
" 2>/dev/null)
    if [[ "$NOTES" == *"$OLD_PATH"* ]]; then
        echo "✅ PASS — project_path rewritten, sibling 'notes' field left intact"
    else
        echo "⚠  PARTIAL — project_path rewritten but sibling 'notes' was also modified"
        echo "             notes = $NOTES"
    fi
    exit 0
else
    echo "❌ FAIL — reconciliation did not fire."
    echo "           On Linux this is the known RenameMode::From/To half-event gap —"
    echo "           the daemon sees the rename but doesn't pair the halves yet."
    exit 1
fi
