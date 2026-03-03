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

# Check 5: Launch auto-save watchdog in background (local stashes)
WATCHDOG="$CWD/.claude/hooks/auto-save-watchdog.sh"
if [ -x "$WATCHDOG" ]; then
  CLAUDE_PROJECT_DIR="$CWD" nohup "$WATCHDOG" >/dev/null 2>&1 &
  echo "Auto-save watchdog: started (PID $!)" >&2
else
  echo "Auto-save watchdog: not found, skipping" >&2
fi

# Check 6: Verify current branch is pushed to remote
if [ "$BRANCH" != "detached" ]; then
  REMOTE_REF=$(git -C "$CWD" ls-remote --heads origin "$BRANCH" 2>/dev/null | head -1)
  if [ -z "$REMOTE_REF" ]; then
    echo "Pushing branch to remote..." >&2
    git -C "$CWD" push -u origin "$BRANCH" 2>/dev/null && \
      echo "Branch pushed: $BRANCH -> origin" >&2 || \
      echo "WARNING: Could not push branch to remote" >&2
  fi
fi

# Check 7: Detect orphan WIP snapshots from previous crashed sessions
ORPHAN_REFS=$(git -C "$CWD" ls-remote origin 'refs/auto-save/*' 2>/dev/null | head -5)
if [ -n "$ORPHAN_REFS" ]; then
  echo "" >&2
  echo "WARNING: RECOVERY AVAILABLE — Found auto-save snapshots from a previous session:" >&2
  echo "$ORPHAN_REFS" | while read -r sha ref; do
    echo "  $ref (${sha:0:7})" >&2
  done
  echo "Run: .claude/hooks/recover-wip.sh to list/restore" >&2
  echo "" >&2
fi

# Check 8: Launch auto-save REMOTE watchdog in background (pushes to GitHub)
REMOTE_WATCHDOG="$CWD/.claude/hooks/auto-save-remote.sh"
if [ -x "$REMOTE_WATCHDOG" ]; then
  CLAUDE_PROJECT_DIR="$CWD" CLAUDE_SESSION_ID="${CLAUDE_SESSION_ID:-$(date +%s)}" \
    nohup "$REMOTE_WATCHDOG" >/dev/null 2>&1 &
  echo "Auto-save remote watchdog: started (PID $!)" >&2
else
  echo "Auto-save remote watchdog: not found, skipping" >&2
fi

# Remind about principles and phase
echo "" >&2
echo "Three principles: (1) Zero alloc hot path (2) O(1) or compile fail (3) All versions pinned" >&2
echo "Current phase: docs/phases/PHASE_1_LIVE_TRADING.md" >&2
