#!/bin/bash
# stop-grill-reminder.sh — Stop hook that reminds about /grill before shipping
# Fires when Claude's turn ends. Checks if there are unpushed commits with
# changed .rs files that haven't been /grill'd.
#
# Mechanical enforcement: writes reminder to stderr which Claude sees next turn.
# NOT a hard gate (can't block — Stop hooks are advisory). The pre-push gate
# handles hard enforcement for banned patterns. /grill needs Claude's brain.

set -uo pipefail

CWD="${CLAUDE_PROJECT_DIR:-$(pwd)}"
HOOKS_DIR="$CWD/.claude/hooks"
GRILL_STATE="$HOOKS_DIR/.last-grill-pass"

# Fast exit: if no git repo, nothing to check
if ! git -C "$CWD" rev-parse --git-dir >/dev/null 2>&1; then
  exit 0
fi

# Get current HEAD
HEAD=$(git -C "$CWD" rev-parse HEAD 2>/dev/null || echo "")
if [ -z "$HEAD" ]; then
  exit 0
fi

# Check if /grill already passed for this HEAD
if [ -f "$GRILL_STATE" ]; then
  LAST_GRILLED_HEAD=$(awk '{print $1}' "$GRILL_STATE" 2>/dev/null || echo "")
  if [ "$HEAD" = "$LAST_GRILLED_HEAD" ]; then
    # Already grilled this HEAD — no reminder needed
    exit 0
  fi
fi

# Check if there are unpushed commits with .rs changes
BRANCH=$(git -C "$CWD" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")
if [ -z "$BRANCH" ] || [ "$BRANCH" = "HEAD" ]; then
  exit 0
fi

# Check if remote tracking branch exists
REMOTE_REF=$(git -C "$CWD" rev-parse "origin/$BRANCH" 2>/dev/null || echo "")
if [ -z "$REMOTE_REF" ]; then
  # No remote tracking — there are unpushed commits
  HAS_UNPUSHED=true
else
  UNPUSHED=$(git -C "$CWD" log "origin/$BRANCH..HEAD" --oneline 2>/dev/null | head -1)
  if [ -n "$UNPUSHED" ]; then
    HAS_UNPUSHED=true
  else
    HAS_UNPUSHED=false
  fi
fi

if [ "$HAS_UNPUSHED" = false ]; then
  exit 0
fi

# Check if any .rs files were changed in unpushed commits
RS_CHANGED=""
if [ -n "$REMOTE_REF" ]; then
  RS_CHANGED=$(git -C "$CWD" diff --name-only "origin/$BRANCH..HEAD" 2>/dev/null | grep '\.rs$' | head -1)
else
  # No remote — check all commits on branch
  RS_CHANGED=$(git -C "$CWD" diff --name-only HEAD~5..HEAD 2>/dev/null | grep '\.rs$' | head -1)
fi

# Also check uncommitted .rs changes
RS_UNCOMMITTED=$(git -C "$CWD" diff --name-only 2>/dev/null | grep '\.rs$' | head -1)
RS_STAGED=$(git -C "$CWD" diff --cached --name-only 2>/dev/null | grep '\.rs$' | head -1)

if [ -z "$RS_CHANGED" ] && [ -z "$RS_UNCOMMITTED" ] && [ -z "$RS_STAGED" ]; then
  # No Rust changes — skip reminder (shell/config-only changes)
  exit 0
fi

# Emit reminder — Claude sees this on next turn
echo "" >&2
echo "┌─────────────────────────────────────────────────┐" >&2
echo "│  REMINDER: Run /grill before pushing.            │" >&2
echo "│  Unpushed Rust changes detected without review.  │" >&2
echo "│  /grill catches edge cases, races, and bugs      │" >&2
echo "│  that mechanical gates can't find.                │" >&2
echo "└─────────────────────────────────────────────────┘" >&2

exit 0
