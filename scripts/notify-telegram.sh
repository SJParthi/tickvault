#!/usr/bin/env bash
# =============================================================================
# tickvault — Send Telegram notification
# =============================================================================
# Reads bot token + chat ID from AWS SSM Parameter Store — the SAME
# /tickvault/<env>/telegram/{bot-token,chat-id} parameters the app itself
# uses (crates/core/src/auth/secret_manager.rs build_ssm_path). FIX 5.2
# (operator live run 2026-07-04 20:00 IST): the previous /dlt/<env>/...
# path belonged to another project — every launcher Telegram failed with
# "bot-token missing from SSM (/dlt/dev/telegram/bot-token)".
#
# In-memory cache: when the caller has already exported
# TV_TELEGRAM_BOT_TOKEN + TV_TELEGRAM_CHAT_ID (the exact names
# bootstrap.sh and the launcher's tg_init export after THEIR OWN SSM
# fetch), the per-message SSM round-trip is skipped. SSM remains the
# single source of truth — the env values are the SSM fetch, cached.
#
# Usage:
#   ./scripts/notify-telegram.sh "Task completed: environment setup done"
#   ./scripts/notify-telegram.sh "Error: QuestDB write failed"
#
# Environment:
#   ENVIRONMENT — "dev" (default) or "prod"
# =============================================================================

set -euo pipefail

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------
REGION="ap-south-1"
# FIX 8.1: resolve the SSM environment the way the APP does
# (TV_ENVIRONMENT → ENVIRONMENT → "prod", per secret_manager.rs
# resolve_environment). The previous dev-default read a nonexistent
# /tickvault/dev/telegram/* parameter while the app's prod default worked.
ENV="${TV_ENVIRONMENT:-${ENVIRONMENT:-prod}}"
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
# Credentials: cached env (the caller's earlier SSM fetch) → SSM fetch.
# ---------------------------------------------------------------------------
BOT_TOKEN="${TV_TELEGRAM_BOT_TOKEN:-}"
CHAT_ID="${TV_TELEGRAM_CHAT_ID:-}"

if [ -z "${BOT_TOKEN}" ] || [ -z "${CHAT_ID}" ]; then
    if ! command -v aws > /dev/null 2>&1; then
        echo "ERROR: aws CLI not available — Telegram notification NOT sent" >&2
        echo "  Message was: ${MESSAGE}" >&2
        echo "  Install aws CLI and configure credentials with SSM read access." >&2
        exit 2
    fi
    SSM_ARGS="--region ${REGION} --with-decryption --output text --query Parameter.Value --no-cli-pager"
    BOT_TOKEN=$(aws ssm get-parameter --name "${SSM_BOT_TOKEN_PATH}" ${SSM_ARGS} 2>/dev/null) || true
    CHAT_ID=$(aws ssm get-parameter --name "${SSM_CHAT_ID_PATH}" ${SSM_ARGS} 2>/dev/null) || true
fi

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
# FIX 10 B4: build the JSON payload with a REAL JSON encoder (jq or
# python3), never string interpolation — a message containing a double
# quote, backslash, or newline (log tails do, constantly) previously
# produced invalid JSON and the send silently failed. parse_mode is
# dropped entirely: messages are plain text (HTML mode also broke on `<`
# characters in log tails).
if command -v jq > /dev/null 2>&1; then
    PAYLOAD=$(jq -cn --arg chat_id "${CHAT_ID}" --arg text "${MESSAGE}" \
        '{chat_id: $chat_id, text: $text}')
elif command -v python3 > /dev/null 2>&1; then
    PAYLOAD=$(python3 -c 'import json, sys; print(json.dumps({"chat_id": sys.argv[1], "text": sys.argv[2]}))' \
        "${CHAT_ID}" "${MESSAGE}")
else
    echo "ERROR: neither jq nor python3 available to JSON-encode the message — Telegram NOT sent" >&2
    echo "  Message was: ${MESSAGE}" >&2
    exit 2
fi

# FIX 15.7: hard time bounds — a black-holed network must never hang this
# script (it runs inside the autopilot monitor loop; an unbounded curl
# would delay relaunches and the 15:35 EOD stop).
RESPONSE=$(curl -s --max-time 10 --connect-timeout 5 \
    -X POST "${TELEGRAM_API}/bot${BOT_TOKEN}/sendMessage" \
    -H "Content-Type: application/json" \
    -d "${PAYLOAD}")

# Check response
OK=$(echo "${RESPONSE}" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))" 2>/dev/null)

if [ "${OK}" = "True" ]; then
    echo "Telegram notification sent successfully"
else
    echo "ERROR: Telegram send failed"
    echo "${RESPONSE}" | python3 -m json.tool 2>/dev/null || echo "${RESPONSE}"
    exit 1
fi
