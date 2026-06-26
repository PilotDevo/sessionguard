#!/usr/bin/env bash
# Copyright 2026 Devin R O'Loughlin / Droco LLC
# SPDX-License-Identifier: MIT
#
# scripts/update-dogfood.sh — End-to-end `sessionguard update` smoke test.
#
# Exercises the real self-update path WITHOUT network or a published release:
# it builds a fake "newer" release on disk, serves it to the binary via the
# `SESSIONGUARD_UPDATE_BASE_URL=file://...` test seam, and asserts the full
# download → SHA256 verify → atomic swap → .bak retention flow. Also proves the
# checksum gate refuses a tampered asset and leaves the binary untouched.
#
# Usage:
#   SESSIONGUARD_BIN=./target/release/sessionguard scripts/update-dogfood.sh
#
# Exit codes:
#   0  update swap + rollback-retention + tamper-refusal all correct
#   1  a stage produced the wrong result
#   2  environment error (missing binary, unsupported platform, etc.)

set -euo pipefail

WORKDIR=$(mktemp -d -t sg-update-dogfood-XXXXXX)
cleanup() { rm -rf "$WORKDIR"; }
trap cleanup EXIT INT TERM

SG=${SESSIONGUARD_BIN:-$(command -v sessionguard 2>/dev/null || true)}
[ -n "$SG" ] || { echo "error: sessionguard not found; set SESSIONGUARD_BIN" >&2; exit 2; }
SG=$(cd "$(dirname "$SG")" && pwd)/$(basename "$SG")   # absolutise

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)  TRIPLE=x86_64-unknown-linux-gnu ;;
    Darwin-x86_64) TRIPLE=x86_64-apple-darwin ;;
    Darwin-arm64)  TRIPLE=aarch64-apple-darwin ;;
    *) echo "error: unsupported platform for this dogfood" >&2; exit 2 ;;
esac

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
    else shasum -a 256 "$1" | awk '{print $1}'; fi
}
fail() { echo "❌ FAIL — $1"; exit 1; }

# ── stage a Standalone install (NOT under target/, .cargo, or Cellar) ──────
BINDIR="$WORKDIR/opt/bin"
mkdir -p "$BINDIR"
DEST="$BINDIR/sessionguard"
cp "$SG" "$DEST"
chmod +x "$DEST"

# ── build a fake "newer" release: a marker binary, tarball, SHA256SUMS ──────
REL="$WORKDIR/release"
mkdir -p "$REL/stage"
cat > "$REL/stage/sessionguard" <<'EOF'
#!/bin/sh
echo "sessionguard 9.9.9 (fake-update)"
EOF
chmod +x "$REL/stage/sessionguard"
ASSET="sessionguard-${TRIPLE}.tar.gz"
tar czf "$REL/$ASSET" -C "$REL/stage" sessionguard
( cd "$REL" && { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$ASSET"; else shasum -a 256 "$ASSET"; fi; } > SHA256SUMS )

echo "╭─ sessionguard update dogfood ────────────────────────────────────"
echo "│ binary : $SG"
echo "│ version: $("$SG" version 2>&1)"
echo "│ triple : $TRIPLE"
echo "│ workdir: $WORKDIR"
echo "╰──────────────────────────────────────────────────────────────────"
echo

# ── 1. dry-run changes nothing ─────────────────────────────────────────────
echo "▶ dry-run..."
SESSIONGUARD_UPDATE_BASE_URL="file://$REL" "$DEST" update --to v9.9.9 --dry-run >"$WORKDIR/dry.log" 2>&1 \
    || fail "dry-run exited non-zero"
"$DEST" version | grep -q "fake-update" && fail "dry-run swapped the binary"
ls "$BINDIR"/sessionguard.bak-* >/dev/null 2>&1 && fail "dry-run created a backup"
echo "  ✓ dry-run touched nothing"

# ── 2. real update: download(file://) → verify → swap → .bak ────────────────
echo "▶ update --to v9.9.9..."
SESSIONGUARD_UPDATE_BASE_URL="file://$REL" "$DEST" update --to v9.9.9 >"$WORKDIR/upd.log" 2>&1 \
    || { cat "$WORKDIR/upd.log"; fail "update exited non-zero"; }
"$DEST" version | grep -q "9.9.9 (fake-update)" || fail "binary was not swapped to the new release"
BAK=$(ls "$BINDIR"/sessionguard.bak-* 2>/dev/null | head -n1)
[ -n "$BAK" ] || fail "no .bak-<ver> backup retained"
# the backup must be the ORIGINAL (real) binary, usable for rollback
"$BAK" version 2>/dev/null | grep -q "fake-update" && fail "backup is the new binary, not the original"
echo "  ✓ swapped to new release; original retained at $(basename "$BAK")"

# ── 3. rollback from the retained backup works ─────────────────────────────
echo "▶ rollback from .bak..."
cp "$BAK" "$DEST"
"$DEST" version | grep -q "fake-update" && fail "rollback did not restore the original"
echo "  ✓ rollback restored a working original binary"

# ── 4. tampered asset is REFUSED and leaves the binary intact ──────────────
echo "▶ checksum-tamper refusal..."
cp "$SG" "$DEST"                       # fresh original
printf 'tampered' >> "$REL/$ASSET"     # corrupt the tarball; SHA256SUMS now stale
if SESSIONGUARD_UPDATE_BASE_URL="file://$REL" "$DEST" update --to v9.9.9 >"$WORKDIR/tamper.log" 2>&1; then
    fail "update accepted a tampered asset (checksum gate failed)"
fi
grep -qi "checksum mismatch" "$WORKDIR/tamper.log" || fail "tamper rejection lacked a checksum-mismatch error"
"$DEST" version | grep -q "fake-update" && fail "tampered update still swapped the binary"
echo "  ✓ tampered asset refused; binary untouched"

echo
echo "✅ PASS — update swap + .bak rollback + checksum-gate all correct"
exit 0
