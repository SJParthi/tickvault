#!/usr/bin/env bash
# session-end-save.sh — Warn about unsaved work when session ends
# Fires on SessionEnd event. Checks for uncommitted changes and unpushed commits.
#
# Cannot block session end (advisory only). Output to stderr.

set -uo pipefail

CWD="${CLAUDE_PROJECT_DIR:-$(pwd)}"

# Fast exit if not a git repo
if ! git -C "$CWD" rev-parse --git-dir >/dev/null 2>&1; then
  exit 0
fi

WARNINGS=0

# Check for uncommitted changes
if ! git -C "$CWD" diff --quiet 2>/dev/null || ! git -C "$CWD" diff --cached --quiet 2>/dev/null; then
  DIRTY_COUNT=$(git -C "$CWD" status --porcelain 2>/dev/null | wc -l | tr -d ' ')
  echo "WARNING: $DIRTY_COUNT uncommitted change(s) in working tree." >&2
  WARNINGS=$((WARNINGS + 1))
fi

# Check for untracked files
UNTRACKED=$(git -C "$CWD" ls-files --others --exclude-standard 2>/dev/null | wc -l | tr -d ' ')
if [ "$UNTRACKED" -gt 0 ]; then
  echo "WARNING: $UNTRACKED untracked file(s)." >&2
  WARNINGS=$((WARNINGS + 1))
fi

# Check for unpushed commits
BRANCH=$(git -C "$CWD" branch --show-current 2>/dev/null || echo "")
if [ -n "$BRANCH" ] && [ "$BRANCH" != "HEAD" ]; then
  if git -C "$CWD" rev-parse "origin/$BRANCH" >/dev/null 2>&1; then
    UNPUSHED=$(git -C "$CWD" rev-list "origin/$BRANCH..HEAD" --count 2>/dev/null || echo "0")
    if [ "$UNPUSHED" -gt 0 ]; then
      echo "WARNING: $UNPUSHED unpushed commit(s) on branch '$BRANCH'." >&2
      WARNINGS=$((WARNINGS + 1))
    fi
  else
    # Branch has no remote tracking
    LOCAL_COMMITS=$(git -C "$CWD" rev-list HEAD --count 2>/dev/null || echo "0")
    if [ "$LOCAL_COMMITS" -gt 0 ]; then
      echo "WARNING: Branch '$BRANCH' has no remote tracking. Work may be lost." >&2
      WARNINGS=$((WARNINGS + 1))
    fi
  fi
fi

if [ "$WARNINGS" -gt 0 ]; then
  echo "" >&2
  echo "SESSION ENDING with $WARNINGS warning(s). Consider committing and pushing." >&2
fi

exit 0
