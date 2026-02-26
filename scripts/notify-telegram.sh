#!/usr/bin/env bash
# dhan-live-trader — Send Telegram notification
#
# Reads bot token + chat ID from AWS SSM Parameter Store.
# Uses LocalStack in dev (AWS_ENDPOINT_URL set), real AWS in prod.
#
# Usage:
#   ./scripts/notify-telegram.sh "✅ Task completed: Block 03 done"
#   ./scripts/notify-telegram.sh "⏳ Approval needed: deploy to staging?"
#   ./scripts/notify-telegram.sh "🚨 Error: QuestDB write failed"
#
# Environment:
#   AWS_ENDPOINT_URL  — set for LocalStack, unset for real AWS
#   ENVIRONMENT       — "dev" (default) or "prod"

set -euo pipefail

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------
REGION="ap-south-1"
ENV="${ENVIRONMENT:-dev}"
SSM_BOT_TOKEN="/dlt/${ENV}/telegram/bot-token"
SSM_CHAT_ID="/dlt/${ENV}/telegram/chat-id"
TELEGRAM_API="https://api.telegram.org"

# LocalStack dev credentials (real AWS uses IAM role — no creds needed)
if [ -n "${AWS_ENDPOINT_URL:-}" ]; then
    export AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:-test}"
    export AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:-test}"
fi

# ---------------------------------------------------------------------------
# Validate input
# ---------------------------------------------------------------------------
if [ $# -eq 0 ]; then
    echo "Usage: $0 <message>"
    echo "Example: $0 '✅ Task completed: Block 03 done'"
    exit 1
fi

MESSAGE="$1"

# ---------------------------------------------------------------------------
# Fetch secrets from SSM (LocalStack or real AWS — same CLI)
# ---------------------------------------------------------------------------
SSM_ARGS="--region ${REGION} --with-decryption --output text --query Parameter.Value"

BOT_TOKEN=$(aws ssm get-parameter --name "${SSM_BOT_TOKEN}" ${SSM_ARGS} 2>/dev/null)
if [ -z "${BOT_TOKEN}" ]; then
    echo "ERROR: Failed to fetch bot token from SSM (${SSM_BOT_TOKEN})"
    exit 1
fi

CHAT_ID=$(aws ssm get-parameter --name "${SSM_CHAT_ID}" ${SSM_ARGS} 2>/dev/null)
if [ -z "${CHAT_ID}" ]; then
    echo "ERROR: Failed to fetch chat ID from SSM (${SSM_CHAT_ID})"
    exit 1
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
