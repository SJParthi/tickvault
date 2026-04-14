#!/bin/bash
# dep-freshness-scanner.sh — S7 / Phase 7
#
# Real, honest answer to "are we always at the latest version?"
#
# Scans every workspace dependency in Cargo.toml and uses `cargo search`
# to compare the pinned version against the latest available on
# crates.io. Reports drift in three categories:
#
#   PATCH:  pinned 1.2.3, latest 1.2.4 — safe auto-bump
#   MINOR:  pinned 1.2.3, latest 1.3.0 — needs cargo check + tests
#   MAJOR:  pinned 1.2.3, latest 2.0.0 — needs code review (API breaking)
#   FRESH:  pinned == latest                — nothing to do
#   UNKNOWN: cargo search returned no version (network issue, yanked, etc.)
#
# Exit codes:
#   0 — all fresh OR only PATCH drift (informational)
#   1 — MINOR or MAJOR drift detected (actionable, not blocking)
#   2 — UNKNOWN (network failure)
#
# This is NOT a commit/push blocker by default — semver lets minor
# bumps slip in legitimate breaking changes, so a human (or a focused
# session) must review before bumping. Wire it into a nightly cron
# / GitHub Action / make target instead. The OPERATOR is the one who
# decides "this minor bump is safe; this one needs work".
#
# Usage:
#   bash .claude/hooks/dep-freshness-scanner.sh           # human-readable report
#   bash .claude/hooks/dep-freshness-scanner.sh --json    # machine-readable
#   bash .claude/hooks/dep-freshness-scanner.sh --fail-on-minor  # exit 1 on drift
#
# Built deliberately with no external deps beyond cargo + grep + sed
# so it runs on Mac dev, Linux CI, and Docker images uniformly.

set -uo pipefail

REPO_ROOT="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$REPO_ROOT" || exit 0

# Mode flags.
JSON_OUTPUT=0
FAIL_ON_MINOR=0
for arg in "$@"; do
  case "$arg" in
    --json) JSON_OUTPUT=1 ;;
    --fail-on-minor) FAIL_ON_MINOR=1 ;;
  esac
done

# Extract the [workspace.dependencies] block from Cargo.toml.
# We accept lines of the form `name = "1.2.3"` and
# `name = { version = "1.2.3", ... }`.
DEP_LINES=$(awk '
  /^\[workspace\.dependencies\]/ { in_block = 1; next }
  /^\[/ { in_block = 0 }
  in_block && /^[a-z][a-zA-Z0-9_-]* = / { print }
' Cargo.toml)

if [ -z "$DEP_LINES" ]; then
  echo "ERROR: no [workspace.dependencies] block found in Cargo.toml" >&2
  exit 2
fi

PATCH_DRIFT=()
MINOR_DRIFT=()
MAJOR_DRIFT=()
UNKNOWN=()
FRESH_COUNT=0

while IFS= read -r LINE; do
  # Extract dep name (everything before " = ").
  NAME=$(echo "$LINE" | sed -nE 's/^([a-z][a-zA-Z0-9_-]*) = .*/\1/p')
  # Extract pinned version (first quoted x.y.z anywhere on the line).
  PINNED=$(echo "$LINE" | grep -oE '"[0-9]+\.[0-9]+\.[0-9]+[^"]*"' | head -1 | tr -d '"')
  if [ -z "$NAME" ] || [ -z "$PINNED" ]; then
    continue
  fi

  # Query cargo search for the latest version.
  # cargo search returns lines like: `name = "x.y.z" # description`
  LATEST=$(cargo search "$NAME" --limit 5 2>/dev/null \
    | grep -E "^${NAME} = " \
    | head -1 \
    | grep -oE '"[0-9]+\.[0-9]+\.[0-9]+"' \
    | tr -d '"')
  if [ -z "$LATEST" ]; then
    UNKNOWN+=("${NAME}: pinned=${PINNED}")
    continue
  fi

  # Strip pre-release suffix from pinned for comparison.
  PINNED_BASE=$(echo "$PINNED" | grep -oE '^[0-9]+\.[0-9]+\.[0-9]+')

  if [ "$PINNED_BASE" = "$LATEST" ]; then
    FRESH_COUNT=$((FRESH_COUNT + 1))
    continue
  fi

  # Parse semver for category detection.
  P_MAJ=$(echo "$PINNED_BASE" | cut -d. -f1)
  P_MIN=$(echo "$PINNED_BASE" | cut -d. -f2)
  L_MAJ=$(echo "$LATEST" | cut -d. -f1)
  L_MIN=$(echo "$LATEST" | cut -d. -f2)

  if [ "$P_MAJ" != "$L_MAJ" ]; then
    MAJOR_DRIFT+=("${NAME}: pinned=${PINNED} latest=${LATEST}")
  elif [ "$P_MIN" != "$L_MIN" ]; then
    MINOR_DRIFT+=("${NAME}: pinned=${PINNED} latest=${LATEST}")
  else
    PATCH_DRIFT+=("${NAME}: pinned=${PINNED} latest=${LATEST}")
  fi
done <<< "$DEP_LINES"

if [ "$JSON_OUTPUT" -eq 1 ]; then
  printf '{\n'
  printf '  "fresh_count": %d,\n' "$FRESH_COUNT"
  for CATEGORY in PATCH_DRIFT MINOR_DRIFT MAJOR_DRIFT UNKNOWN; do
    KEY=$(echo "$CATEGORY" | tr '[:upper:]' '[:lower:]')
    printf '  "%s": [' "$KEY"
    eval "ITEMS=( \"\${${CATEGORY}[@]}\" )"
    FIRST=1
    if [ "${#ITEMS[@]}" -gt 0 ]; then
      for ITEM in "${ITEMS[@]}"; do
        if [ "$FIRST" -eq 0 ]; then printf ', '; fi
        printf '"%s"' "$ITEM"
        FIRST=0
      done
    fi
    if [ "$CATEGORY" = "UNKNOWN" ]; then
      printf ']\n'
    else
      printf '],\n'
    fi
  done
  printf '}\n'
else
  echo "============================================="
  echo "  S7 Dep Freshness Scanner"
  echo "============================================="
  echo "Fresh (pinned == latest):  ${FRESH_COUNT} deps"
  echo ""
  if [ "${#PATCH_DRIFT[@]}" -gt 0 ]; then
    echo "PATCH drift (${#PATCH_DRIFT[@]} — safe to auto-bump):"
    for D in "${PATCH_DRIFT[@]}"; do echo "  $D"; done
    echo ""
  fi
  if [ "${#MINOR_DRIFT[@]}" -gt 0 ]; then
    echo "MINOR drift (${#MINOR_DRIFT[@]} — needs cargo check + tests):"
    for D in "${MINOR_DRIFT[@]}"; do echo "  $D"; done
    echo ""
  fi
  if [ "${#MAJOR_DRIFT[@]}" -gt 0 ]; then
    echo "MAJOR drift (${#MAJOR_DRIFT[@]} — needs code review, API breaking):"
    for D in "${MAJOR_DRIFT[@]}"; do echo "  $D"; done
    echo ""
  fi
  if [ "${#UNKNOWN[@]}" -gt 0 ]; then
    echo "UNKNOWN (${#UNKNOWN[@]} — cargo search returned no version):"
    for D in "${UNKNOWN[@]}"; do echo "  $D"; done
    echo ""
  fi
  TOTAL_DRIFT=$((${#PATCH_DRIFT[@]} + ${#MINOR_DRIFT[@]} + ${#MAJOR_DRIFT[@]}))
  if [ "$TOTAL_DRIFT" -eq 0 ] && [ "${#UNKNOWN[@]}" -eq 0 ]; then
    echo "ALL CLEAN: every workspace dep is at the latest version."
  fi
fi

# Exit semantics:
#   - Always 0 if FAIL_ON_MINOR is unset (informational mode)
#   - 1 if FAIL_ON_MINOR and any minor/major drift found
#   - 2 if any UNKNOWN entries (network issue or yanked crate)
if [ "${#UNKNOWN[@]}" -gt 0 ]; then
  exit 2
fi
if [ "$FAIL_ON_MINOR" -eq 1 ] && [ $((${#MINOR_DRIFT[@]} + ${#MAJOR_DRIFT[@]})) -gt 0 ]; then
  exit 1
fi
exit 0
