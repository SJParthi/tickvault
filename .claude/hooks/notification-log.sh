#!/usr/bin/env bash
# notification-log.sh — Logs Claude Code notifications to stderr for debugging.
# Hook: Notification
set -euo pipefail

INPUT="$(cat)"
TITLE=$(echo "$INPUT" | jq -r '.title // "unknown"' 2>/dev/null)
MESSAGE=$(echo "$INPUT" | jq -r '.message // "no message"' 2>/dev/null)

echo "NOTIFICATION [$TITLE]: $MESSAGE" >&2
