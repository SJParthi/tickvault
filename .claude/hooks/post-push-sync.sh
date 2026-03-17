#!/usr/bin/env bash
# post-push-sync.sh — PostToolUse hook for Bash
# After a successful git push, sync session branch to claude/integration.
# Runs sync-to-integration.sh in background — never blocks Claude.
#
# IMPORTANT: This hook MUST be in its own matcher group in settings.json
# (separate from post-commit-state.sh) so each gets independent stdin.

# Require jq — without it, JSON parsing fails silently and sync never fires
if ! command -v jq >/dev/null 2>&1; then
  echo "post-push-sync: jq not found, skipping integration sync" >&2
  exit 0
fi

# Read stdin (hook framework pipes JSON)
INPUT=$(cat)

# Fast string check — skip jq for non-push commands (99% of invocations)
case "$INPUT" in
  *"git push"*) ;;
  *) exit 0 ;;
esac

COMMAND=$(printf '%s' "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null)

# Only act on git push commands (must START with "git push")
case "$COMMAND" in
  git\ push*) ;;
  *) exit 0 ;;
esac

# Skip if pushing to integration branch (avoid infinite loop)
# Exact match only — don't skip branches like claude/integration-v2
case "$COMMAND" in
  *" claude/integration "*|*" claude/integration") exit 0 ;;
  *"claude/integration "*|*"claude/integration") exit 0 ;;
esac

# Skip if push clearly failed — check for git-specific error patterns only.
# Do NOT check for generic "error"/"fatal" strings — they match branch names
# like "claude/fix-error-handling" or "claude/fix-fatal-crash".
STDERR=$(printf '%s' "$INPUT" | jq -r '.tool_response.stderr // empty' 2>/dev/null)
case "$STDERR" in
  *"! [rejected]"*|*"failed to push"*|*"non-fast-forward"*) exit 0 ;;
  *"fatal: "*)
    # Only match "fatal: " with trailing space — git error format.
    # Branch names don't have "fatal: " in git push output.
    exit 0 ;;
esac

CWD=$(printf '%s' "$INPUT" | jq -r '.cwd // empty' 2>/dev/null)
PROJECT_DIR="${CWD:-${CLAUDE_PROJECT_DIR:-.}}"

# Resolve to absolute path
if [ "$PROJECT_DIR" = "." ]; then
  PROJECT_DIR=$(pwd)
fi

SYNC_SCRIPT="$PROJECT_DIR/scripts/sync-to-integration.sh"

# Verify script exists and is executable
if [ ! -x "$SYNC_SCRIPT" ]; then
  exit 0
fi

# Extract branch name from push command to pass explicitly (avoids race condition
# where Claude switches branches before background sync reads HEAD)
PUSH_BRANCH=$(printf '%s' "$COMMAND" | grep -oE 'claude/[a-zA-Z0-9_/-]+' | head -1)
if [ -z "$PUSH_BRANCH" ]; then
  # Fallback: read current branch (slightly racy but better than nothing)
  PUSH_BRANCH=$(git -C "$PROJECT_DIR" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")
fi

# Run sync in background with a hard 90s timeout.
# - nohup/disown: fully detached from Claude's process
# - timeout 90: hard kill if git hangs (network, lock wait, etc.)
# - >/dev/null: sync script handles its own logging via log() function
# - $PUSH_BRANCH passed as arg to avoid HEAD race condition
nohup timeout 90 "$SYNC_SCRIPT" "$PUSH_BRANCH" >/dev/null 2>&1 &
disown

exit 0
