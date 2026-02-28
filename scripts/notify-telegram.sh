#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Send Telegram notification
# =============================================================================
# Reads bot token + chat ID from real AWS SSM Parameter Store.
# Telegram credentials ALWAYS come from real AWS SSM — never LocalStack.
#
# Usage:
#   ./scripts/notify-telegram.sh "Task completed: environment setup done"
#   ./scripts/notify-telegram.sh "Error: QuestDB write failed"
#
# Prerequisites: Run bootstrap.sh once — it handles all secret setup automatically.
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
SSM_BOT_TOKEN="/dlt/${ENV}/telegram/bot-token"
SSM_CHAT_ID="/dlt/${ENV}/telegram/chat-id"
TELEGRAM_API="https://api.telegram.org"

# Telegram ALWAYS uses real AWS SSM — never LocalStack.
# Unset AWS_ENDPOINT_URL to ensure we hit real AWS.
unset AWS_ENDPOINT_URL 2>/dev/null || true

# ---------------------------------------------------------------------------
# Validate input
# ---------------------------------------------------------------------------
if [ $# -eq 0 ]; then
    echo "Usage: $0 <message>"
    echo "Example: $0 'Task completed: environment setup done'"
    exit 1
fi

MESSAGE="$1"

# ---------------------------------------------------------------------------
# Ensure AWS CLI is available (auto-install if missing)
# ---------------------------------------------------------------------------
if ! command -v aws > /dev/null 2>&1; then
    pip3 install awscli --quiet 2>/dev/null || pip install awscli --quiet 2>/dev/null || true
    if ! command -v aws > /dev/null 2>&1; then
        echo "SKIP: aws CLI not available — Telegram notification not sent"
        exit 0
    fi
fi

# ---------------------------------------------------------------------------
# Fetch secrets from real AWS SSM
# ---------------------------------------------------------------------------
SSM_ARGS="--region ${REGION} --with-decryption --output text --query Parameter.Value --no-cli-pager"

BOT_TOKEN=$(aws ssm get-parameter --name "${SSM_BOT_TOKEN}" ${SSM_ARGS} 2>/dev/null) || true
if [ -z "${BOT_TOKEN}" ]; then
    echo "SKIP: Bot token not in SSM — run bootstrap.sh to set up"
    exit 0
fi

CHAT_ID=$(aws ssm get-parameter --name "${SSM_CHAT_ID}" ${SSM_ARGS} 2>/dev/null) || true
if [ -z "${CHAT_ID}" ]; then
    echo "SKIP: Chat ID not in SSM — run bootstrap.sh to set up"
    exit 0
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
