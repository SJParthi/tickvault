#!/bin/bash
# session-sanity.sh — Quick sanity check on every session start
# Lightweight: cargo check only (no full test suite, ~10s)
# Fires on ALL session starts, not just compaction.

CWD="${CLAUDE_PROJECT_DIR:-$(pwd)}"
cd "$CWD" || exit 0

echo "SESSION START SANITY CHECK" >&2
echo "=========================" >&2

# Check 1: Current branch
BRANCH=$(git branch --show-current 2>/dev/null || echo "detached")
echo "Branch: $BRANCH" >&2

# Check 2: Uncommitted changes?
DIRTY=$(git status --porcelain 2>/dev/null | head -5)
if [ -n "$DIRTY" ]; then
  echo "WARNING: Uncommitted changes:" >&2
  echo "$DIRTY" >&2
  echo "" >&2
else
  echo "Working tree: clean" >&2
fi

# Check 3: Quick compile check (much faster than full test)
echo "Running cargo check..." >&2
CHECK_OUT=$(cargo check --workspace 2>&1)
CHECK_EXIT=$?
if [ "$CHECK_EXIT" -ne 0 ]; then
  echo "WARNING: cargo check failed:" >&2
  echo "$CHECK_OUT" | tail -10 >&2
else
  echo "Compile: OK" >&2
fi

# Check 4: Test count baseline
HOOKS_DIR="$CWD/.claude/hooks"
if [ -f "$HOOKS_DIR/.test-count-baseline" ]; then
  BASELINE=$(cat "$HOOKS_DIR/.test-count-baseline" | tr -d ' \n')
  echo "Test baseline: $BASELINE tests" >&2
fi

# Check 5: Launch auto-save watchdog in background
WATCHDOG="$CWD/.claude/hooks/auto-save-watchdog.sh"
if [ -x "$WATCHDOG" ]; then
  CLAUDE_PROJECT_DIR="$CWD" nohup "$WATCHDOG" >/dev/null 2>&1 &
  echo "Auto-save watchdog: started (PID $!)" >&2
else
  echo "Auto-save watchdog: not found, skipping" >&2
fi

# Remind about principles and phase
echo "" >&2
echo "Three principles: (1) Zero alloc hot path (2) O(1) or compile fail (3) All versions pinned" >&2
echo "Current phase: docs/phases/PHASE_1_LIVE_TRADING.md" >&2
