#!/usr/bin/env bash
# Copyright 2026 Devin R O'Loughlin / Droco LLC
# SPDX-License-Identifier: MIT
#
# scripts/rekey-dogfood.sh — End-to-end store re-key → undo smoke test.
#
# Store re-keying (v0.9) is the reconcile for home-dir stores: Claude Code,
# Codex and OpenCode keep NO project path inside the project, so a move must
# re-point their `$HOME` stores instead. This drives the real thing against a
# throwaway HOME seeded with all three store layouts:
#
#   encoded_dir   (Claude Code) — dir named by the encoded path, AND the true
#                 path recorded inside the transcript. Both keys must move;
#                 renaming alone would let the census read the old path back
#                 out of the renamed directory.
#   jsonl_field   (Codex)       — `cwd` on the first line of a JSONL session.
#   sqlite_column (OpenCode)    — a row column, updated in place.
#
# Checks the properties that make this safe to run automatically: --dry-run
# writes nothing, a re-key is undoable to a BYTE-IDENTICAL store, an unrelated
# project is never touched, and re-keying onto an existing store is REFUSED
# rather than silently merging two projects' histories.
#
# Never touches the operator's real ~/.claude, ~/.codex or ~/.local/share.
#
# Usage:
#   scripts/rekey-dogfood.sh                      # uses `sessionguard` from PATH
#   SESSIONGUARD_BIN=./target/release/sessionguard scripts/rekey-dogfood.sh
#
# Exit codes:
#   0  re-key + undo round-trip intact, refusal held
#   1  a stage produced the wrong result
#   2  environment error (missing binary, etc.)

set -euo pipefail

# ── setup ────────────────────────────────────────────────────────────────
WORKDIR=$(mktemp -d -t sg-rekey-dogfood-XXXXXX)
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

sg() { "$SG" "$@"; }
fail() { echo "❌ FAIL — $1"; exit 1; }

# Claude Code's encoding: every non-alphanumeric character becomes `-`.
encode() { printf '%s' "$1" | sed 's/[^A-Za-z0-9]/-/g'; }

# ── fixture: one project with all three stores, plus a bystander ──────────
PROJ="$WORKDIR/work/my_app"          # `_` on purpose: lossy under the encoding
MOVED="$WORKDIR/elsewhere/my_app"
OTHER="$WORKDIR/work/untouched"
mkdir -p "$PROJ" "$OTHER" "$WORKDIR/elsewhere"

CLAUDE_DIR="$HOME/.claude/projects/$(encode "$PROJ")"
mkdir -p "$CLAUDE_DIR"
printf '{"type":"summary","summary":"x"}\n{"type":"user","cwd":"%s"}\n' "$PROJ" \
    > "$CLAUDE_DIR/s.jsonl"

OTHER_DIR="$HOME/.claude/projects/$(encode "$OTHER")"
mkdir -p "$OTHER_DIR"
printf '{"cwd":"%s"}\n' "$OTHER" > "$OTHER_DIR/s.jsonl"

CODEX_DIR="$HOME/.codex/sessions/2026/07"
mkdir -p "$CODEX_DIR"
printf '{"cwd": "%s"}\n' "$PROJ" > "$CODEX_DIR/rollout.jsonl"

OC_DIR="$HOME/.local/share/opencode"
mkdir -p "$OC_DIR"
if command -v sqlite3 >/dev/null 2>&1; then
    sqlite3 "$OC_DIR/opencode.db" \
      "CREATE TABLE session (directory TEXT, time_updated INTEGER, time_archived INTEGER);
       INSERT INTO session VALUES ('$PROJ', 1752610000000, NULL);
       INSERT INTO session VALUES ('$OTHER', 1752610000000, NULL);"
    HAVE_SQLITE=1
else
    echo "note: sqlite3 not on PATH — skipping the OpenCode (sqlite_column) leg"
    HAVE_SQLITE=0
fi

# Freeze a reference copy to prove the undo is byte-exact.
cp -R "$HOME/.claude" "$WORKDIR/snapshot-claude"
cp -R "$HOME/.codex"  "$WORKDIR/snapshot-codex"

# ── banner ───────────────────────────────────────────────────────────────
echo "╭─ sessionguard rekey dogfood ─────────────────────────────────────"
echo "│ binary : $SG"
echo "│ version: $(sg version 2>&1 | head -1)"
echo "│ host   : $(uname -s) $(uname -r)"
echo "│ workdir: $WORKDIR"
echo "╰──────────────────────────────────────────────────────────────────"
echo

# ── 0. baseline: the census sees the project at its original path ─────────
sg sessions --format json > "$WORKDIR/before.json" 2>/dev/null \
    || fail "sessions census failed"
grep -q "$PROJ" "$WORKDIR/before.json" || fail "census did not see the fixture project"
echo "▶ baseline: census sees $PROJ"

# ── 1. the project moves; it is now an orphan ─────────────────────────────
mv "$PROJ" "$MOVED"
sg sessions --orphans --format json > "$WORKDIR/orphans.json" 2>/dev/null || true
grep -q "$PROJ" "$WORKDIR/orphans.json" \
    || fail "a moved project's sessions should be reported orphaned"
echo "  ✓ after the move, sessions are stranded at the old path"

# ── 2. dry-run writes nothing ─────────────────────────────────────────────
echo "▶ dry-run..."
sg rekey "$PROJ" "$MOVED" --dry-run > "$WORKDIR/dryrun.log" 2>&1 \
    || fail "dry-run exited non-zero"
grep -q "nothing changed" "$WORKDIR/dryrun.log" \
    || fail "dry-run did not announce that it changed nothing"
[[ -d "$CLAUDE_DIR" ]] || fail "dry-run renamed the store directory"
diff -r "$WORKDIR/snapshot-claude" "$HOME/.claude" >/dev/null \
    || fail "dry-run modified the Claude store"
diff -r "$WORKDIR/snapshot-codex" "$HOME/.codex" >/dev/null \
    || fail "dry-run modified the Codex store"
echo "  ✓ dry-run touched nothing"

# ── 3. real re-key ────────────────────────────────────────────────────────
echo "▶ rekey..."
sg rekey "$PROJ" "$MOVED" > "$WORKDIR/rekey.log" 2>&1 || fail "rekey exited non-zero"

sg sessions --format json > "$WORKDIR/after.json" 2>/dev/null || fail "census failed after rekey"
grep -q "$MOVED" "$WORKDIR/after.json" || fail "census does not report the new path"
python3 - "$WORKDIR/after.json" "$PROJ" "$MOVED" <<'PY' || fail "re-key left the store inconsistent"
import json, sys
groups = json.load(open(sys.argv[1]))
old, new = sys.argv[2], sys.argv[3]
if any(g["project_path"] == old for g in groups):
    sys.exit("something is still keyed to the old path")
g = next((g for g in groups if g["project_path"] == new), None)
if g is None:
    sys.exit("the new path is absent from the census")
if g["orphaned"]:
    sys.exit("the re-keyed project must not be orphaned — it exists")
if "claude_code" not in g["tools"] or "codex" not in g["tools"]:
    sys.exit(f"expected both claude_code and codex under the new path, got {list(g['tools'])}")
PY
# The bystander project must be untouched.
grep -q "$OTHER" "$WORKDIR/after.json" || fail "an unrelated project vanished from the census"
echo "  ✓ claude_code + codex now keyed to the new path; bystander untouched"

if [[ "$HAVE_SQLITE" == "1" ]]; then
    ROWS=$(sqlite3 "$OC_DIR/opencode.db" "SELECT COUNT(*) FROM session WHERE directory='$MOVED'")
    [[ "$ROWS" == "1" ]] || fail "opencode row not re-keyed (found $ROWS)"
    KEPT=$(sqlite3 "$OC_DIR/opencode.db" "SELECT COUNT(*) FROM session WHERE directory='$OTHER'")
    [[ "$KEPT" == "1" ]] || fail "opencode re-key disturbed an unrelated row"
    echo "  ✓ opencode row re-keyed, unrelated row intact"
fi

# ── 4. undo restores byte-for-byte ────────────────────────────────────────
echo "▶ undo..."
sg undo > "$WORKDIR/undo.log" 2>&1 || fail "undo exited non-zero"
diff -r "$WORKDIR/snapshot-claude" "$HOME/.claude" >/dev/null \
    || fail "undo did not restore the Claude store byte-for-byte"
diff -r "$WORKDIR/snapshot-codex" "$HOME/.codex" >/dev/null \
    || fail "undo did not restore the Codex store byte-for-byte"
echo "  ✓ stores restored byte-for-byte"

# ── 5. refusal: never merge two projects' histories ───────────────────────
echo "▶ refusal (destination store already exists)..."
if sg rekey "$PROJ" "$OTHER" > "$WORKDIR/refuse.log" 2>&1; then
    fail "re-keying onto an existing store must exit non-zero"
fi
grep -qi "refused" "$WORKDIR/refuse.log" || fail "refusal not reported to the operator"
grep -qi "merge"   "$WORKDIR/refuse.log" || fail "refusal did not explain why"
diff -r "$WORKDIR/snapshot-claude" "$HOME/.claude" >/dev/null \
    || fail "a REFUSED re-key still modified the store"
echo "  ✓ refused, explained, and changed nothing"

# ── 6. the DAEMON path: noise does nothing, real moves are re-keyed ──────
# The unit tests call the move handler with fabricated events; this runs the
# real daemon against real filesystem events. v0.9.0's automatic re-key never
# fired, and through v0.10 every rename (git, cargo, editor saves) planned a
# re-key across every store — both invisible without exactly this.
echo "▶ daemon: real watcher, real git commit, real project + folder moves..."
D="$WORKDIR/daemon"
mkdir -p "$D/code/proj" "$D/code/folder/inner"
for p in "$D/code/proj" "$D/code/folder/inner"; do
    SD="$HOME/.claude/projects/$(encode "$p")"
    mkdir -p "$SD"
    printf '{"cwd":"%s"}\n' "$p" > "$SD/s.jsonl"
done
printf 'watch_roots = ["%s"]\n' "$D/code" > "$SESSIONGUARD_CONFIG_DIR/config.toml"

activity_count() { sg log --activity --last 10000 --format json 2>/dev/null \
    | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))'; }
BEFORE=$(activity_count)

sg start --foreground > "$WORKDIR/daemon.log" 2>&1 &
DPID=$!
for _ in $(seq 1 50); do
    grep -q "watching for filesystem events" "$WORKDIR/daemon.log" 2>/dev/null && break
    sleep 0.1
done
kill -0 "$DPID" 2>/dev/null || { cat "$WORKDIR/daemon.log"; fail "daemon exited during startup"; }
sleep 1

(cd "$D/code/proj" && git init -q && echo x > f && git add f \
    && git -c user.email=a@b -c user.name=t commit -qm x)
sleep 2
AFTER_GIT=$(activity_count)
[[ "$AFTER_GIT" == "$BEFORE" ]] \
    || fail "a git commit is not a project move, but the daemon recorded $((AFTER_GIT - BEFORE)) decision(s)"
echo "  ✓ git commit inside a watched project: no re-key planned"

mv "$D/code/proj" "$D/code/proj-moved"
mv "$D/code/folder" "$D/code/folder2"
sleep 3
kill -TERM "$DPID" 2>/dev/null; wait "$DPID" 2>/dev/null || true

sg sessions --format json > "$WORKDIR/daemon-after.json" 2>/dev/null
python3 - "$WORKDIR/daemon-after.json" "$D/code" <<'PY2' || { cat "$WORKDIR/daemon.log"; fail "the daemon did not re-key the moves"; }
import json, os, sys
groups = {os.path.realpath(g["project_path"]): g for g in json.load(open(sys.argv[1]))}
code = os.path.realpath(sys.argv[2])
for want in (f"{code}/proj-moved", f"{code}/folder2/inner"):
    g = groups.get(want)
    if g is None:
        sys.exit(f"{want} missing — sessions did not follow the move; census has {sorted(groups)}")
    if g["orphaned"]:
        sys.exit(f"{want} is orphaned")
for gone in (f"{code}/proj", f"{code}/folder/inner"):
    if gone in groups:
        sys.exit(f"{gone} still has sessions keyed to it")
PY2
echo "  ✓ project move and folder move both re-keyed by the daemon"

echo
echo "✅ PASS — rekey → undo byte-exact, bystanders untouched, merge refused, daemon re-keys real moves and ignores noise"
exit 0
