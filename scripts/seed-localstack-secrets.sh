#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Seed LocalStack SSM Parameter Store
# =============================================================================
# Populates SSM Parameter Store in LocalStack with ALL credentials for
# offline development: Dhan API credentials AND Telegram tokens.
#
# Secret path convention: /dlt/<environment>/<service>/<key>
#
# Usage (with env vars):
#   export DHAN_CLIENT_ID="your-real-client-id"
#   export DHAN_CLIENT_SECRET="your-real-jwt-token"
#   export DHAN_TOTP_SECRET="your-real-totp-secret"
#   export TELEGRAM_BOT_TOKEN="your-bot-token-from-botfather"
#   export TELEGRAM_CHAT_ID="your-chat-id-from-userinfobot"
#   ./scripts/seed-localstack-secrets.sh
#
# Without env vars: seeds placeholder values. App boots in degraded mode.
# =============================================================================

set -euo pipefail

LOCALSTACK_URL="http://localhost:4566"
REGION="ap-south-1"
ENVIRONMENT="dev"
PLACEHOLDER="PLACEHOLDER_REPLACE_ME"

# LocalStack accepts any credentials — these are required by the AWS CLI
export AWS_ACCESS_KEY_ID="test"
export AWS_SECRET_ACCESS_KEY="test"

echo "Seeding SSM Parameter Store in LocalStack..."
echo "Endpoint: ${LOCALSTACK_URL}"
echo "Region:   ${REGION}"
echo ""

# Helper function to put a SecureString parameter
put_secret() {
    local path="$1"
    local value="$2"

    aws ssm put-parameter \
        --endpoint-url "${LOCALSTACK_URL}" \
        --region "${REGION}" \
        --name "${path}" \
        --value "${value}" \
        --type "SecureString" \
        --overwrite \
        --no-cli-pager \
        > /dev/null 2>&1

    echo "  [OK] ${path}"
}

# ---------------------------------------------------------------------------
# Dhan API Credentials
# ---------------------------------------------------------------------------
echo "--- Dhan Credentials ---"

DHAN_CID="${DHAN_CLIENT_ID:-${PLACEHOLDER}}"
DHAN_CS="${DHAN_CLIENT_SECRET:-${PLACEHOLDER}}"
DHAN_TS="${DHAN_TOTP_SECRET:-${PLACEHOLDER}}"

if [ "${DHAN_CID}" = "${PLACEHOLDER}" ]; then
    echo "  No DHAN_CLIENT_ID env var — seeding placeholders"
    echo "  Set env vars and re-run to replace with real credentials"
fi

put_secret "/dlt/${ENVIRONMENT}/dhan/client-id" "${DHAN_CID}"
put_secret "/dlt/${ENVIRONMENT}/dhan/client-secret" "${DHAN_CS}"
put_secret "/dlt/${ENVIRONMENT}/dhan/totp-secret" "${DHAN_TS}"

# ---------------------------------------------------------------------------
# Telegram Tokens
# ---------------------------------------------------------------------------
echo ""
echo "--- Telegram Tokens ---"

TG_TOKEN="${TELEGRAM_BOT_TOKEN:-${PLACEHOLDER}}"
TG_CHAT="${TELEGRAM_CHAT_ID:-${PLACEHOLDER}}"

if [ "${TG_TOKEN}" = "${PLACEHOLDER}" ]; then
    echo "  No TELEGRAM_BOT_TOKEN env var — seeding placeholders"
    echo "  Set env vars and re-run to replace with real tokens"
fi

put_secret "/dlt/${ENVIRONMENT}/telegram/bot-token" "${TG_TOKEN}"
put_secret "/dlt/${ENVIRONMENT}/telegram/chat-id" "${TG_CHAT}"

# ---------------------------------------------------------------------------
# Verification
# ---------------------------------------------------------------------------
echo ""
echo "--- Verification ---"
PARAM_COUNT=$(aws ssm describe-parameters \
    --endpoint-url "${LOCALSTACK_URL}" \
    --region "${REGION}" \
    --no-cli-pager \
    --query "length(Parameters)" \
    --output text 2>/dev/null)

echo "Total parameters stored: ${PARAM_COUNT}"
echo ""
echo "Seed complete."
