#!/usr/bin/env bash
# =============================================================================
# tickvault — Send Telegram notification
# =============================================================================
# Reads bot token + chat ID from one of (in priority order):
#   1. Environment variables TV_TELEGRAM_BOT_TOKEN + TV_TELEGRAM_CHAT_ID
#      (local dev — set these in ~/.zshrc or wherever; no AWS required)
#   2. AWS SSM Parameter Store /tickvault/<env>/telegram/{bot-token,chat-id}
#      (production path — AWS EC2 uses this via instance IAM role)
#
# Usage:
#   ./scripts/notify-telegram.sh "Task completed: environment setup done"
#   ./scripts/notify-telegram.sh "Error: QuestDB write failed"
#
# Local dev setup (one-time, no AWS needed):
#   export TV_TELEGRAM_BOT_TOKEN="<your bot token from @BotFather>"
#   export TV_TELEGRAM_CHAT_ID="<your chat ID from @userinfobot>"
#   # Add both lines to ~/.zshrc so every shell picks them up.
#
# Environment:
#   ENVIRONMENT — "dev" (default) or "prod"  (used for SSM path only)
# =============================================================================

set -euo pipefail

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------
REGION="ap-south-1"
ENV="${ENVIRONMENT:-dev}"
SSM_BOT_TOKEN_PATH="/tickvault/${ENV}/telegram/bot-token"
SSM_CHAT_ID_PATH="/tickvault/${ENV}/telegram/chat-id"
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
# Resolve credentials — try env vars first (local dev), then SSM (AWS)
# ---------------------------------------------------------------------------
BOT_TOKEN="${TV_TELEGRAM_BOT_TOKEN:-}"
CHAT_ID="${TV_TELEGRAM_CHAT_ID:-}"

if [ -z "${BOT_TOKEN}" ] || [ -z "${CHAT_ID}" ]; then
    # Fallback to AWS SSM
    if ! command -v aws > /dev/null 2>&1; then
        cat >&2 <<WARN_EOF
WARN: Telegram notification NOT sent
  Message was: ${MESSAGE}
  Reason: Neither local env vars nor AWS CLI are configured.
  Fix (one-time):
    export TV_TELEGRAM_BOT_TOKEN="<token from @BotFather>"
    export TV_TELEGRAM_CHAT_ID="<chat ID from @userinfobot>"
  Then add both to ~/.zshrc and restart your terminal.
WARN_EOF
        exit 2
    fi

    SSM_ARGS="--region ${REGION} --with-decryption --output text --query Parameter.Value --no-cli-pager"

    if [ -z "${BOT_TOKEN}" ]; then
        BOT_TOKEN=$(aws ssm get-parameter --name "${SSM_BOT_TOKEN_PATH}" ${SSM_ARGS} 2>/dev/null) || true
    fi
    if [ -z "${CHAT_ID}" ]; then
        CHAT_ID=$(aws ssm get-parameter --name "${SSM_CHAT_ID_PATH}" ${SSM_ARGS} 2>/dev/null) || true
    fi

    if [ -z "${BOT_TOKEN}" ] || [ -z "${CHAT_ID}" ]; then
        cat >&2 <<WARN_EOF
WARN: Telegram notification NOT sent
  Message was: ${MESSAGE}
  Reason: Neither local env vars nor SSM parameters are configured.
  Local dev fix (recommended, fastest):
    export TV_TELEGRAM_BOT_TOKEN="<token from @BotFather>"
    export TV_TELEGRAM_CHAT_ID="<chat ID from @userinfobot>"
  AWS fix: run bootstrap.sh once to provision /tickvault/${ENV}/telegram/*
WARN_EOF
        exit 2
    fi
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
