#!/usr/bin/env bash
# Copyright 2026 Devin R O'Loughlin / Droco LLC
# SPDX-License-Identifier: MIT
#
# scripts/check-consistency.sh — release-metadata consistency gate.
#
# The recurring failure mode of this repo is "code shipped, docs didn't":
# the README Status line, ROADMAP "current" marker, and SECURITY supported
# table have each silently sat out multiple releases — three separate times.
# This script makes that class of drift a CI FAILURE instead of an audit
# finding. Run from the repo root; CI runs it in the check job.
#
# Checks:
#   1. README "Status: vX.Y.Z" == Cargo.toml version
#   2. ROADMAP "(current)" marker mentions the current MAJOR.MINOR
#   3. SECURITY supported table covers the current MAJOR.MINOR line
#   4. CHANGELOG has an entry for the current version, not future-dated
#   5. install.sh's offered target triples ⊆ release.yml's built targets
#
# Exit codes: 0 all consistent; 1 drift found (each drift printed).

set -uo pipefail

fail=0
err() { echo "✗ $1"; fail=1; }
ok()  { echo "✓ $1"; }

VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
MINOR=$(echo "$VERSION" | cut -d. -f1-2)
[ -n "$VERSION" ] || { echo "cannot read version from Cargo.toml"; exit 1; }
echo "Cargo.toml version: $VERSION (line: $MINOR.x)"

# 1. README status line
README_V=$(grep -m1 -oE 'Status: v[0-9]+\.[0-9]+\.[0-9]+' README.md | sed 's/Status: v//')
if [ "$README_V" = "$VERSION" ]; then
    ok "README Status line matches ($README_V)"
else
    err "README Status says '${README_V:-<none>}' but Cargo.toml is $VERSION"
fi

# 2. ROADMAP current marker
if grep -qE "\*\*v${MINOR//./\\.}[0-9.]* \(current\)" ROADMAP.md; then
    ok "ROADMAP '(current)' is on the $MINOR.x line"
else
    err "ROADMAP '(current)' marker is not on the $MINOR.x line"
fi

# 3. SECURITY supported table
if grep -qE "^\| *${MINOR//./\\.}\.x" SECURITY.md; then
    ok "SECURITY supports $MINOR.x"
else
    err "SECURITY.md supported-versions table does not list $MINOR.x"
fi

# 4. CHANGELOG entry exists and isn't future-dated
CL_LINE=$(grep -m1 -E "^## \[$VERSION\]" CHANGELOG.md || true)
if [ -z "$CL_LINE" ]; then
    err "CHANGELOG.md has no entry for $VERSION"
else
    CL_DATE=$(echo "$CL_LINE" | grep -oE '[0-9]{4}-[0-9]{2}-[0-9]{2}' || true)
    TODAY=$(date +%Y-%m-%d)
    if [ -n "$CL_DATE" ] && [[ "$CL_DATE" > "$TODAY" ]]; then
        err "CHANGELOG entry for $VERSION is future-dated ($CL_DATE > $TODAY)"
    else
        ok "CHANGELOG entry for $VERSION present (${CL_DATE:-undated})"
    fi
fi

# 5. install.sh triples ⊆ release.yml targets (a triple offered but never
#    built is a guaranteed download 404).
RELEASE_TARGETS=$(grep -oE 'target: [a-z0-9_-]+' .github/workflows/release.yml | awk '{print $2}' | sort -u)
INSTALL_TRIPLES=$(grep -oE 'echo "[a-z0-9_]+-[a-z0-9-]+"' install.sh | sed 's/echo "//; s/"//' | sort -u)
for t in $INSTALL_TRIPLES; do
    if echo "$RELEASE_TARGETS" | grep -qx "$t"; then
        ok "install.sh triple $t is built by release.yml"
    else
        err "install.sh offers '$t' but release.yml never builds it (404 at download)"
    fi
done

exit $fail
