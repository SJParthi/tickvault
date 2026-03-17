#!/bin/bash
# post-push-sync.sh — PostToolUse hook for Bash
# After a successful git push, sync session branch to claude/integration.
# Runs sync-to-integration.sh in background — never blocks Claude.
#
# IMPORTANT: This hook MUST be in its own matcher group in settings.json
# (separate from post-commit-state.sh) so each gets independent stdin.

# Require jq — without it, JSON parsing fails silently and sync never fires
command -v jq >/dev/null 2>&1 || exit 0

# Read stdin (hook framework pipes JSON)
INPUT=$(cat)

# Fast string check — skip jq for non-push commands (99% of invocations)
case "$INPUT" in
  *"git push"*) ;;
  *) exit 0 ;;
esac

COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null)

# Only act on git push commands (must START with "git push")
case "$COMMAND" in
  git\ push*) ;;
  *) exit 0 ;;
esac

# Skip if pushing to integration branch (avoid infinite loop)
case "$COMMAND" in
  *claude/integration*) exit 0 ;;
esac

# Skip if push clearly failed — check for git-specific error patterns only.
# Do NOT check for generic "error"/"fatal" strings — they match branch names
# like "claude/fix-error-handling" or "claude/fix-fatal-crash".
STDERR=$(echo "$INPUT" | jq -r '.tool_response.stderr // empty' 2>/dev/null)
case "$STDERR" in
  *"! [rejected]"*|*"failed to push"*|*"non-fast-forward"*) exit 0 ;;
  *"fatal: "*)
    # Only match "fatal: " with trailing space — git error format.
    # Branch names don't have "fatal: " in git push output.
    exit 0 ;;
esac

CWD=$(echo "$INPUT" | jq -r '.cwd // empty' 2>/dev/null)
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

# Run sync in background with a hard 90s timeout.
# - nohup/disown: fully detached from Claude's process
# - timeout 90: hard kill if git hangs (network, lock wait, etc.)
# - >/dev/null: sync script handles its own logging via log() function
nohup timeout 90 "$SYNC_SCRIPT" >/dev/null 2>&1 &
disown

exit 0
