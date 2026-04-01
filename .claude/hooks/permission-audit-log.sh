#!/bin/bash
# permission-audit-log.sh — Notification hook for permission-related events
# Logs all tool permission events for audit trail.
# Non-blocking, best-effort logging only.
#
# Exit 0 = always allow

INPUT=$(cat)
CWD="${CLAUDE_PROJECT_DIR:-$(pwd)}"

LOG_DIR="$CWD/data/logs"
mkdir -p "$LOG_DIR" 2>/dev/null

TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
TOOL=$(echo "$INPUT" | jq -r '.tool_name // .tool // "unknown"' 2>/dev/null)
EVENT=$(echo "$INPUT" | jq -r '.event // "unknown"' 2>/dev/null)

echo "$TIMESTAMP | event=$EVENT | tool=$TOOL" >> "$LOG_DIR/permission-audit.log" 2>/dev/null

exit 0
