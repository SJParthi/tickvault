#!/usr/bin/env bash
# =============================================================================
# tickvault — Send Telegram notification
# =============================================================================
# Reads bot token + chat ID from AWS SSM Parameter Store only. No env var
# fallback: AWS SSM is the single source of truth for all secrets.
#
# Usage:
#   ./scripts/notify-telegram.sh "Task completed: environment setup done"
#   ./scripts/notify-telegram.sh "Error: QuestDB write failed"
#
# Prerequisites: AWS CLI configured with credentials that can read
# /dlt/<env>/telegram/{bot-token,chat-id} from SSM Parameter Store.
#
# Environment:
#   ENVIRONMENT — "dev" (default) or "prod"
# =============================================================================

set -euo pipefail

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------
REGION="ap-south-1"
ENV="${ENVIRONMENT:-dev}"
SSM_BOT_TOKEN_PATH="/dlt/${ENV}/telegram/bot-token"
SSM_CHAT_ID_PATH="/dlt/${ENV}/telegram/chat-id"
TELEGRAM_API="https://api.telegram.org"

# ---------------------------------------------------------------------------
# Validate input
# ---------------------------------------------------------------------------
if [ $# -eq 0 ]; then
    echo "Usage: $0 <message>" >&2
    echo "Example: $0 'Task completed: environment setup done'" >&2
    exit 1
fi

MESSAGE="$1"

# ---------------------------------------------------------------------------
# Require AWS CLI — SSM is the single source of truth
# ---------------------------------------------------------------------------
if ! command -v aws > /dev/null 2>&1; then
    echo "ERROR: aws CLI not available — Telegram notification NOT sent" >&2
    echo "  Message was: ${MESSAGE}" >&2
    echo "  Install aws CLI and configure credentials with SSM read access." >&2
    exit 2
fi

SSM_ARGS="--region ${REGION} --with-decryption --output text --query Parameter.Value --no-cli-pager"

BOT_TOKEN=$(aws ssm get-parameter --name "${SSM_BOT_TOKEN_PATH}" ${SSM_ARGS} 2>/dev/null) || true
CHAT_ID=$(aws ssm get-parameter --name "${SSM_CHAT_ID_PATH}" ${SSM_ARGS} 2>/dev/null) || true

if [ -z "${BOT_TOKEN}" ]; then
    echo "ERROR: Telegram bot-token missing from SSM (${SSM_BOT_TOKEN_PATH})" >&2
    echo "  Message was: ${MESSAGE}" >&2
    exit 2
fi

if [ -z "${CHAT_ID}" ]; then
    echo "ERROR: Telegram chat-id missing from SSM (${SSM_CHAT_ID_PATH})" >&2
    echo "  Message was: ${MESSAGE}" >&2
    exit 2
fi

# ---------------------------------------------------------------------------
# Send Telegram message
# ---------------------------------------------------------------------------
RESPONSE=$(curl -s -X POST "${TELEGRAM_API}/bot${BOT_TOKEN}/sendMessage" \
    -H "Content-Type: application/json" \
    -d "{\"chat_id\": \"${CHAT_ID}\", \"text\": \"${MESSAGE}\", \"parse_mode\": \"HTML\"}")

# Check response
OK=$(echo "${RESPONSE}" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))" 2>/dev/null)

if [ "${OK}" = "True" ]; then
    echo "Telegram notification sent successfully"
else
    echo "ERROR: Telegram send failed"
    echo "${RESPONSE}" | python3 -m json.tool 2>/dev/null || echo "${RESPONSE}"
    exit 1
fi
