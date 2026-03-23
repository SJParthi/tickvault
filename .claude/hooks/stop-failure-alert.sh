#!/usr/bin/env bash
# stop-failure-alert.sh — Alert when Claude's turn ends due to API error
# Fires on StopFailure event. Matcher covers all failure types.
#
# Input JSON contains: stop_hook_active, error type info
# Output to stderr → Claude sees it as feedback.

set -uo pipefail

INPUT=$(cat)

# Extract error details from the hook input
ERROR_TYPE=$(echo "$INPUT" | jq -r '.error.type // .stop_reason // "unknown"' 2>/dev/null)
ERROR_MSG=$(echo "$INPUT" | jq -r '.error.message // ""' 2>/dev/null)

echo "" >&2
echo "┌─────────────────────────────────────────────────────┐" >&2
echo "│  STOP FAILURE DETECTED                               │" >&2
echo "│                                                      │" >&2

case "$ERROR_TYPE" in
  *rate_limit*)
    echo "│  Type: RATE LIMIT                                    │" >&2
    echo "│  Action: Wait before retrying. Check token usage.    │" >&2
    ;;
  *authentication*)
    echo "│  Type: AUTH FAILURE                                   │" >&2
    echo "│  Action: Check API key / session validity.           │" >&2
    ;;
  *max_output_tokens*)
    echo "│  Type: MAX OUTPUT TOKENS                              │" >&2
    echo "│  Action: Response was truncated. Continue from here. │" >&2
    ;;
  *server_error*)
    echo "│  Type: SERVER ERROR                                   │" >&2
    echo "│  Action: Transient — retry the last request.         │" >&2
    ;;
  *billing*)
    echo "│  Type: BILLING ERROR                                  │" >&2
    echo "│  Action: Check plan/billing at claude.ai/settings.   │" >&2
    ;;
  *)
    echo "│  Type: $ERROR_TYPE" >&2
    ;;
esac

if [ -n "$ERROR_MSG" ] && [ "$ERROR_MSG" != "" ] && [ "$ERROR_MSG" != "null" ]; then
  # Truncate long messages
  SHORT_MSG=$(echo "$ERROR_MSG" | head -c 50)
  echo "│  Detail: $SHORT_MSG" >&2
fi

echo "│                                                      │" >&2
echo "│  Session state preserved. Resume when ready.         │" >&2
echo "└─────────────────────────────────────────────────────┘" >&2

exit 0
