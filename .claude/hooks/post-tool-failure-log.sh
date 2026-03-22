#!/usr/bin/env bash
# post-tool-failure-log.sh — Log failed tool calls for debugging
# Hook: PostToolUseFailure
set -euo pipefail

# Read tool input from stdin
INPUT=$(cat)
TOOL_NAME=$(echo "$INPUT" | jq -r '.tool_name // "unknown"' 2>/dev/null)
ERROR=$(echo "$INPUT" | jq -r '.error // "no error details"' 2>/dev/null)

echo "TOOL FAILURE: ${TOOL_NAME} — ${ERROR}" >&2
