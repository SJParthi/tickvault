#!/bin/bash
# session-sanity.sh — Lightweight sanity check on every session start
# MUST be fast (<5s). Heavy checks belong in pre-commit/pre-push gates.
# No blocking network ops. No cargo check (commit gate covers it).

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

# Check 3: Test count baseline (fast file read, no compilation)
HOOKS_DIR="$CWD/.claude/hooks"
if [ -f "$HOOKS_DIR/.test-count-baseline" ]; then
  BASELINE=$(cat "$HOOKS_DIR/.test-count-baseline" | tr -d ' \n')
  echo "Test baseline: $BASELINE tests" >&2
fi

# Check 4: Network ops in background (non-blocking)
# Push branch to remote + collision detection — runs async, does NOT block session
# CRITICAL: redirect stdout+stderr to log file and disown, otherwise the hook
# framework waits for stderr to close and the entire SessionStart hangs.
NETWORK_LOG="$CWD/.claude/hooks/.session-network.log"
(
  if [ "$BRANCH" != "detached" ]; then
    REMOTE_REF=$(git -C "$CWD" ls-remote --heads origin "$BRANCH" 2>/dev/null | head -1)
    if [ -z "$REMOTE_REF" ]; then
      timeout 15 git -C "$CWD" push -u origin "$BRANCH" 2>/dev/null || true
    fi
  fi

  # Lightweight collision warning (ls-remote only, no fetch)
  REMOTE_REFS=$(timeout 10 git -C "$CWD" ls-remote origin 'refs/auto-save/*' 2>/dev/null)
  if [ -n "$REMOTE_REFS" ]; then
    echo "[$(date '+%H:%M:%S')] Found auto-save snapshots from previous sessions."
  fi
) >> "$NETWORK_LOG" 2>&1 &
disown

# Remind about principles and phase
echo "" >&2
echo "Three principles: (1) Zero alloc hot path (2) O(1) or compile fail (3) All versions pinned" >&2
echo "Current phase: docs/phases/PHASE_1_LIVE_TRADING.md" >&2
