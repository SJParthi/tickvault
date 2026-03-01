#!/bin/bash
# test-count-guard.sh — Ratcheting test count baseline
# Prevents accidental test deletion. Count can only go UP.
# Called by: pre-commit (Gate 8), pre-push (Gate 5), pre-PR (Gate 5)
#
# Baseline stored in .claude/hooks/.test-count-baseline (gitignored)
# Manual override: echo <count> > .claude/hooks/.test-count-baseline

set -uo pipefail

PROJECT_DIR="${1:-.}"
cd "$PROJECT_DIR" || exit 0

HOOKS_DIR="$(cd "$(dirname "$0")" && pwd)"
BASELINE_FILE="$HOOKS_DIR/.test-count-baseline"

# Count #[test] functions across all crates
CURRENT_COUNT=$(grep -r '#\[test\]' crates/ --include='*.rs' 2>/dev/null | wc -l | tr -d ' ')

if [ -z "$CURRENT_COUNT" ] || [ "$CURRENT_COUNT" -eq 0 ]; then
  echo "  WARNING: Zero tests detected. Skipping guard." >&2
  exit 0
fi

if [ ! -f "$BASELINE_FILE" ]; then
  # First run: establish baseline
  echo "$CURRENT_COUNT" > "$BASELINE_FILE"
  echo "  PASS: Test count baseline established: $CURRENT_COUNT tests" >&2
  exit 0
fi

BASELINE=$(cat "$BASELINE_FILE" | tr -d ' \n')

if [ "$CURRENT_COUNT" -lt "$BASELINE" ]; then
  DROPPED=$((BASELINE - CURRENT_COUNT))
  echo "  FAIL: Test count dropped by $DROPPED (was $BASELINE, now $CURRENT_COUNT)" >&2
  echo "  If tests were intentionally removed, update baseline:" >&2
  echo "    echo $CURRENT_COUNT > $BASELINE_FILE" >&2
  exit 2
fi

if [ "$CURRENT_COUNT" -gt "$BASELINE" ]; then
  # Tests added — update baseline upward (ratchet)
  echo "$CURRENT_COUNT" > "$BASELINE_FILE"
  echo "  PASS: Test count increased ($BASELINE -> $CURRENT_COUNT) — baseline updated" >&2
else
  echo "  PASS: Test count stable ($CURRENT_COUNT tests)" >&2
fi

exit 0
