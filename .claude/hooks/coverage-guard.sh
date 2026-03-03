#!/bin/bash
# coverage-guard.sh — Ratcheting coverage baseline
# Prevents accidental coverage regression. Coverage can only go UP.
# Called by: pre-push (Gate 8)
#
# Requires: cargo-llvm-cov installed
# Baseline stored in .claude/hooks/.coverage-baseline (gitignored)
# Manual override: echo <percentage> > .claude/hooks/.coverage-baseline

set -uo pipefail

PROJECT_DIR="${1:-.}"
cd "$PROJECT_DIR" || { echo "FAIL: cannot cd to $PROJECT_DIR" >&2; exit 2; }

HOOKS_DIR="$(cd "$(dirname "$0")" && pwd)"
BASELINE_FILE="$HOOKS_DIR/.coverage-baseline"

# Check if cargo-llvm-cov is installed
if ! command -v cargo-llvm-cov > /dev/null 2>&1; then
  echo "  SKIP: cargo-llvm-cov not installed (install: cargo install cargo-llvm-cov)" >&2
  exit 0
fi

# Get current workspace coverage percentage
COV_JSON=$(cargo llvm-cov --workspace --json 2>/dev/null)
if [ $? -ne 0 ] || [ -z "$COV_JSON" ]; then
  echo "  FAIL: cargo llvm-cov failed — blocking for safety" >&2
  exit 2
fi

CURRENT_PCT=$(echo "$COV_JSON" | python3 -c "
import json, sys
data = json.load(sys.stdin)
totals = data.get('data', [{}])[0].get('totals', {}).get('lines', {})
count = totals.get('count', 0)
covered = totals.get('covered', 0)
if count == 0:
    print('100.0')
else:
    print(f'{(covered / count) * 100.0:.1f}')
" 2>/dev/null)

if [ -z "$CURRENT_PCT" ] || ! echo "$CURRENT_PCT" | grep -qE '^[0-9]+(\.[0-9]+)?$'; then
  echo "  FAIL: Cannot parse coverage output — blocking for safety" >&2
  exit 2
fi

if [ ! -f "$BASELINE_FILE" ]; then
  # First run: establish baseline
  echo "$CURRENT_PCT" > "$BASELINE_FILE"
  echo "  PASS: Coverage baseline established: ${CURRENT_PCT}%" >&2
  exit 0
fi

BASELINE=$(cat "$BASELINE_FILE" | tr -d ' \n')

# Compare using python3 for float comparison
RESULT=$(python3 -c "
baseline = float('$BASELINE')
current = float('$CURRENT_PCT')
if current < baseline:
    drop = baseline - current
    print(f'FAIL:{drop:.1f}')
elif current > baseline:
    print(f'UP:{current:.1f}')
else:
    print('SAME')
" 2>/dev/null)

case "$RESULT" in
  FAIL:*)
    DROP="${RESULT#FAIL:}"
    echo "  FAIL: Coverage dropped by ${DROP}% (was ${BASELINE}%, now ${CURRENT_PCT}%)" >&2
    echo "  If coverage was intentionally lowered, update baseline:" >&2
    echo "    echo $CURRENT_PCT > $BASELINE_FILE" >&2
    exit 2
    ;;
  UP:*)
    echo "$CURRENT_PCT" > "$BASELINE_FILE"
    echo "  PASS: Coverage increased (${BASELINE}% -> ${CURRENT_PCT}%) — baseline updated" >&2
    ;;
  *)
    echo "  PASS: Coverage stable (${CURRENT_PCT}%)" >&2
    ;;
esac

exit 0
