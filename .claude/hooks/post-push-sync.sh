#!/bin/bash
# post-push-sync.sh — PostToolUse hook for Bash matching "git push"
# Triggers sync of current session branch to claude/integration.

# Read stdin (hook framework pipes JSON)
INPUT=$(cat)

# Fast string check — skip jq for non-push commands
case "$INPUT" in
  *"git push"*) ;;
  *) exit 0 ;;
esac

COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty')

# Only act on git push commands
if ! echo "$COMMAND" | grep -q 'git push'; then
  exit 0
fi

# Skip if push was to integration branch itself (avoid infinite loop)
if echo "$COMMAND" | grep -q 'claude/integration'; then
  exit 0
fi

# Check if push succeeded (tool_response should indicate success)
EXIT_CODE=$(echo "$INPUT" | jq -r '.tool_response.exitCode // .tool_response.exit_code // "0"')
if [ "$EXIT_CODE" != "0" ]; then
  exit 0
fi

CWD=$(echo "$INPUT" | jq -r '.cwd // empty')
if [ -z "$CWD" ]; then
  CWD="${CLAUDE_PROJECT_DIR:-.}"
fi

# Run sync in background so it doesn't block the session
cd "$CWD" && "$CWD/scripts/sync-to-integration.sh" &

exit 0
