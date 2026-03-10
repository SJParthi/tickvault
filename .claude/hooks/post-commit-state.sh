#!/bin/bash
# post-commit-state.sh — PostToolUse hook for Bash matching "git commit"
# Updates the quality state file with the NEW HEAD hash after a successful commit.
#
# Why this exists:
#   pre-commit-gate.sh runs BEFORE the commit (PreToolUse), so it can only save
#   the OLD HEAD hash. After the commit succeeds, HEAD changes. Without this hook,
#   pre-push-gate's dedup check would never match (different hashes = full 8-gate
#   suite = 300s+ = push appears to hang).
#
# This hook reads the state file (written by pre-commit-gate with old HEAD + timestamp)
# and updates just the hash to the actual new HEAD. Timestamp and test count are preserved.

# Read stdin (required — hook framework pipes JSON) and fast-check for 'git commit'
# before doing any expensive jq parsing.
INPUT=$(cat)

# Fast string check — skip jq entirely for non-commit commands (99% of invocations)
case "$INPUT" in
  *"git commit"*) ;;
  *) exit 0 ;;
esac

COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty')

# Only act on git commit commands
if ! echo "$COMMAND" | grep -q 'git commit'; then
  exit 0
fi

CWD=$(echo "$INPUT" | jq -r '.cwd // empty')
if [ -z "$CWD" ]; then
  CWD="."
fi

HOOKS_DIR="$CWD/.claude/hooks"
STATE_FILE="$HOOKS_DIR/.last-quality-pass"

# Only update if pre-commit gate wrote a state file recently (within 120s)
if [ -f "$STATE_FILE" ]; then
  SAVED_TIME=$(awk '{print $2}' "$STATE_FILE" 2>/dev/null)
  NOW=$(date +%s)
  AGE=$(( NOW - SAVED_TIME ))

  if [ "$AGE" -lt 120 ]; then
    # State file is fresh from pre-commit gate — update hash to new HEAD
    NEW_HEAD=$(git -C "$CWD" rev-parse HEAD 2>/dev/null || echo "")
    if [ -n "$NEW_HEAD" ]; then
      SAVED_TESTS=$(awk '{print $3}' "$STATE_FILE" 2>/dev/null)
      echo "$NEW_HEAD $NOW ${SAVED_TESTS:-0}" > "$STATE_FILE"
    fi
  fi
fi

exit 0
