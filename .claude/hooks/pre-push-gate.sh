#!/bin/bash
# pre-push-gate.sh — Blocks git push if tests fail
# Final safety net before code leaves the machine

INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty')

# Only gate git push commands
if ! echo "$COMMAND" | grep -q 'git push'; then
  exit 0
fi

CWD=$(echo "$INPUT" | jq -r '.cwd // empty')
if [ -z "$CWD" ]; then
  CWD="."
fi

cd "$CWD" || exit 0

# Check if any .rs files exist in the project
RS_EXISTS=$(find crates -name '*.rs' 2>/dev/null | head -1)
if [ -z "$RS_EXISTS" ]; then
  exit 0
fi

# Final gate: cargo test must pass before push
if ! cargo test --workspace > /dev/null 2>&1; then
  echo "BLOCKED: cargo test failed. Cannot push broken code." >&2
  exit 2
fi

exit 0
