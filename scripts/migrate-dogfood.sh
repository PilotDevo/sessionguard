#!/usr/bin/env bash
# Copyright 2026 Devin R O'Loughlin / Droco LLC
# SPDX-License-Identifier: MIT
#
# scripts/migrate-dogfood.sh — End-to-end migrate → undo smoke test.
#
# Builds a throwaway config-discovery tool (a source data dir + a JSON config
# file naming it) under an isolated SessionGuard config/data/HOME, then drives
# the real `migrate` state machine: dry-run (no-op), real migrate (copies data,
# rewrites the config, preserves the original as a `.migrated-<unix>` sidecar),
# and `undo` (restores source, removes the copy). Never touches the operator's
# real `~/.codex` / `~/.local/share/opencode`.
#
# Usage:
#   scripts/migrate-dogfood.sh                      # uses `sessionguard` from PATH
#   SESSIONGUARD_BIN=./target/release/sessionguard scripts/migrate-dogfood.sh
#
# Exit codes:
#   0  migrate + undo round-trip succeeded
#   1  a stage produced the wrong result
#   2  environment error (missing binary, etc.)

set -euo pipefail

# ── setup ────────────────────────────────────────────────────────────────
WORKDIR=$(mktemp -d -t sg-migrate-dogfood-XXXXXX)
cleanup() { rm -rf "$WORKDIR"; }
trap cleanup EXIT INT TERM

# ── locate binary ────────────────────────────────────────────────────────
SG=${SESSIONGUARD_BIN:-$(command -v sessionguard 2>/dev/null || true)}
if [[ -z "$SG" ]]; then
    echo "error: sessionguard not found." >&2
    echo "  install with 'cargo install sessionguard'" >&2
    echo "  or set SESSIONGUARD_BIN=/path/to/sessionguard" >&2
    exit 2
fi

# ── isolated environment ──────────────────────────────────────────────────
export SESSIONGUARD_DATA_DIR="$WORKDIR/sgdata"
export SESSIONGUARD_CONFIG_DIR="$WORKDIR/sgconfig"
export HOME="$WORKDIR"          # so nothing resolves to the real home
mkdir -p "$SESSIONGUARD_DATA_DIR" "$SESSIONGUARD_CONFIG_DIR"

# ── fixture: a config-discovery tool ──────────────────────────────────────
SRC="$WORKDIR/toolsrc"
DST="$WORKDIR/dst"
TOOL_CFG="$WORKDIR/tool.json"
SG_CFG="$WORKDIR/sessionguard.toml"

mkdir -p "$SRC/nested"
printf 'payload\n' > "$SRC/data.txt"
head -c 2048 /dev/zero > "$SRC/nested/blob.bin"
printf '{"data_dir": "%s"}' "$SRC" > "$TOOL_CFG"
cat > "$SG_CFG" <<EOF
[[tools]]
name = "demo"
display_name = "Demo Tool"
on_move = "notify"
session_patterns = ["AGENTS.md"]

[tools.home_dir_layout]
default_path = "$SRC"
discovery = "config"

[[tools.home_dir_layout.config_files]]
file = "$TOOL_CFG"
field = "data_dir"
format = "json"
EOF

sg() { "$SG" --config "$SG_CFG" "$@"; }

# ── banner ───────────────────────────────────────────────────────────────
echo "╭─ sessionguard migrate dogfood ───────────────────────────────────"
echo "│ binary : $SG"
echo "│ version: $(sg version 2>&1)"
echo "│ host   : $(uname -s) $(uname -r)"
echo "│ workdir: $WORKDIR"
echo "╰──────────────────────────────────────────────────────────────────"
echo

fail() { echo "❌ FAIL — $1"; exit 1; }

# ── 1. dry-run changes nothing ────────────────────────────────────────────
echo "▶ dry-run..."
sg migrate demo --to "$DST" --dry-run > "$WORKDIR/dryrun.log" 2>&1 || fail "dry-run exited non-zero"
[[ -e "$DST" ]] && fail "dry-run created the destination"
[[ -f "$SRC/data.txt" ]] || fail "dry-run disturbed the source"
echo "  ✓ dry-run touched nothing"

# ── 2. real migrate ───────────────────────────────────────────────────────
echo "▶ migrate..."
sg migrate demo --to "$DST" > "$WORKDIR/migrate.log" 2>&1 || fail "migrate exited non-zero"
[[ -f "$DST/data.txt" ]] || fail "destination not populated"
[[ -f "$DST/nested/blob.bin" ]] || fail "nested file not copied"

SIDECAR=$(find "$WORKDIR" -maxdepth 1 -name 'toolsrc.migrated-*' -print -quit)
[[ -n "$SIDECAR" ]] || fail "original not preserved as a .migrated-<unix> sidecar"
grep -q "$DST" "$TOOL_CFG" || fail "config not rewritten to name the destination"
echo "  ✓ data copied, original preserved at $(basename "$SIDECAR"), config rewritten"

# ── 3. undo restores ──────────────────────────────────────────────────────
echo "▶ undo --migration 1..."
sg undo --migration 1 > "$WORKDIR/undo.log" 2>&1 || fail "undo exited non-zero"
[[ -f "$SRC/data.txt" ]] || fail "undo did not restore the source"
[[ -e "$DST" ]] && fail "undo did not remove the migrated copy"
grep -q "$SRC" "$TOOL_CFG" || fail "undo did not restore the config"
echo "  ✓ source restored, copy removed, config restored"

echo
echo "✅ PASS — migrate → undo round-trip intact, original never lost"
exit 0
