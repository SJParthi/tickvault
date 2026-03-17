#!/bin/bash
# post-push-sync.sh — PostToolUse hook for Bash
# After a successful git push, sync session branch to claude/integration.
# Runs sync-to-integration.sh in background — never blocks Claude.

# Read stdin (hook framework pipes JSON)
INPUT=$(cat)

# Fast string check — skip jq for non-push commands (99% of invocations)
case "$INPUT" in
  *"git push"*) ;;
  *) exit 0 ;;
esac

COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null)

# Only act on git push commands
case "$COMMAND" in
  git\ push*) ;;
  *) exit 0 ;;
esac

# Skip if pushing to integration branch (avoid infinite loop)
case "$COMMAND" in
  *claude/integration*) exit 0 ;;
esac

# Skip if push failed
STDOUT=$(echo "$INPUT" | jq -r '.tool_response.stdout // empty' 2>/dev/null)
STDERR=$(echo "$INPUT" | jq -r '.tool_response.stderr // empty' 2>/dev/null)
case "$STDOUT$STDERR" in
  *"rejected"*|*"error"*|*"fatal"*) exit 0 ;;
esac

CWD=$(echo "$INPUT" | jq -r '.cwd // empty' 2>/dev/null)
PROJECT_DIR="${CWD:-${CLAUDE_PROJECT_DIR:-.}}"
SYNC_SCRIPT="$PROJECT_DIR/scripts/sync-to-integration.sh"

# Verify script exists
if [ ! -x "$SYNC_SCRIPT" ]; then
  exit 0
fi

# Run sync in background — completely detached from Claude's process
# stdout/stderr to log file, not to hook output (would confuse Claude)
nohup "$SYNC_SCRIPT" >> "$PROJECT_DIR/.claude/hooks/.integration-sync.log" 2>&1 &
disown

exit 0
