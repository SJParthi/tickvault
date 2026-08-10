#!/bin/bash
# test-count-guard.sh — Ratcheting test count baseline
# Prevents accidental test deletion. Count can only go UP.
# Called by: pre-commit (Gate 8), pre-push (Gate 5), pre-PR (Gate 5)
#
# Baseline stored in .claude/hooks/.test-count-baseline (gitignored)
# Manual override: echo <count> > .claude/hooks/.test-count-baseline

set -uo pipefail

PROJECT_DIR="${1:-.}"
# 2026-08-10: was `|| exit 0` — a failed cd exited SUCCESS with no output, so
# a bad working directory became a silent vacuous pass. That is the same
# fail-open class both of these files were rewritten to eliminate for their
# zero-count and missing-baseline cases; this was the last one left in a guard
# that now gates merges. `plan-gate.sh` already does it this way.
cd "$PROJECT_DIR" || { echo "FATAL: cannot cd to $PROJECT_DIR — refusing to pass vacuously" >&2; exit 1; }

HOOKS_DIR="$(cd "$(dirname "$0")" && pwd)"
BASELINE_FILE="$HOOKS_DIR/.test-count-baseline"

# Count test functions across all crates.
#
# 2026-08-10 FIX — BLIND SPOT: this counted ONLY `#[test]`. The string
# `#[tokio::test]` does NOT contain `#[test]` (verified: grep -c '#\[test\]'
# against a file holding `#[tokio::test]` returns 0), so every async test was
# invisible to the ratchet. At the time of this fix that was 1,166 test
# functions — roughly an eighth of the suite — which could have been deleted
# wholesale without the "count may only increase" guard noticing.
#
# Both forms are now counted. Because this ADDS a category the total can only
# rise, so the baseline is re-stamped to the true combined count in the same
# commit; the ratchet stays strictly stronger, never weaker.
SYNC_COUNT=$(grep -r '#\[test\]' crates/ --include='*.rs' 2>/dev/null | wc -l | tr -d ' ')
ASYNC_COUNT=$(grep -rE '#\[(tokio|async_std)::test' crates/ --include='*.rs' 2>/dev/null | wc -l | tr -d ' ')
CURRENT_COUNT=$(( SYNC_COUNT + ASYNC_COUNT ))

if [ -z "$CURRENT_COUNT" ] || [ "$CURRENT_COUNT" -eq 0 ]; then
  # 2026-08-10: was `WARNING ... exit 0` — a VACUOUS PASS. A zero count does not
  # mean "no tests to check", it means the count MECHANISM broke (wrong working
  # directory, crates/ renamed, grep unavailable). Passing there is the exact
  # false-OK this repo forbids: the guard reports green having compared nothing.
  echo "  FAIL: test-count guard counted ZERO tests." >&2
  echo "  That is a broken COUNT, not an empty suite — crates/ has thousands." >&2
  echo "  Check the working directory and that crates/ exists." >&2
  exit 1
fi

if [ ! -f "$BASELINE_FILE" ]; then
  # FAIL CLOSED in CI (2026-08-07). Auto-establishing a baseline is correct on
  # a developer machine seeing the repo for the first time, but in CI it is a
  # false-OK: the guard would "pass" without ever having compared anything.
  # The baseline is tracked in git now, so its absence in CI means it was
  # deleted or the checkout is broken — either way, not something to paper over.
  if [ -n "${CI:-}" ]; then
    echo "  FAIL: test-count baseline missing in CI: $BASELINE_FILE" >&2
    echo "  This guard is a RATCHET; with no baseline it cannot compare and would pass vacuously." >&2
    echo "  The baseline is tracked in git — restore it rather than regenerating." >&2
    exit 1
  fi
  # First run (local dev only): establish baseline
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
  # Baseline is TRACKED IN GIT since 2026-08-07 so the ratchet is real in CI.
  # Auto-writing it here would dirty every working tree on every machine whose
  # count differs -- exactly the breakage that forced the 2026-07-13 untracking
  # (see .claude/rules/project/enforcement.md). Ratchet movement is now a
  # deliberate, PR-visible edit, same discipline as
  # quality/crate-coverage-thresholds.toml. Improvement still PASSES.
  echo "  PASS: Test count increased ($BASELINE -> $CURRENT_COUNT)." >&2
  echo "  Ratchet it in THIS PR:  echo $CURRENT_COUNT > $BASELINE_FILE" >&2
else
  echo "  PASS: Test count stable ($CURRENT_COUNT tests)" >&2
fi

exit 0
