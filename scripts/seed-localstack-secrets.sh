#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Seed LocalStack SSM Parameter Store
# =============================================================================
# Populates SSM Parameter Store in LocalStack with Dhan API credentials for
# offline development. Telegram tokens are NOT seeded here — they always come
# from real AWS SSM (no placeholders, no fake data).
#
# Secret path convention: /dlt/<environment>/<service>/<key>
#
# IMPORTANT: Replace the Dhan credential values below with your real
# credentials before running. These are SSM paths only — the actual secrets
# are entered by the developer.
# =============================================================================

set -euo pipefail

LOCALSTACK_URL="http://localhost:4566"
REGION="ap-south-1"
ENVIRONMENT="dev"

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
# Set these environment variables before running, or the script will prompt.
# Example:
#   export DHAN_CLIENT_ID="your-real-client-id"
#   export DHAN_CLIENT_SECRET="your-real-jwt-token"
#   export DHAN_TOTP_SECRET="your-real-totp-secret"
#   ./scripts/seed-localstack-secrets.sh
# ---------------------------------------------------------------------------
echo "--- Dhan Credentials ---"

if [ -z "${DHAN_CLIENT_ID:-}" ]; then
    echo -e "\n  DHAN_CLIENT_ID not set. Set it before running:"
    echo "    export DHAN_CLIENT_ID=\"your-real-client-id\""
    echo "    export DHAN_CLIENT_SECRET=\"your-real-jwt-token\""
    echo "    export DHAN_TOTP_SECRET=\"your-real-totp-secret\""
    echo ""
    exit 1
fi

put_secret "/dlt/${ENVIRONMENT}/dhan/client-id" "${DHAN_CLIENT_ID}"
put_secret "/dlt/${ENVIRONMENT}/dhan/client-secret" "${DHAN_CLIENT_SECRET}"
put_secret "/dlt/${ENVIRONMENT}/dhan/totp-secret" "${DHAN_TOTP_SECRET}"

# ---------------------------------------------------------------------------
# Telegram — NOT seeded in LocalStack
# ---------------------------------------------------------------------------
# Telegram bot token and chat ID always come from real AWS SSM.
# To set them up (one-time):
#   aws ssm put-parameter --region ap-south-1 \
#     --name "/dlt/dev/telegram/bot-token" \
#     --value "YOUR_REAL_BOT_TOKEN" \
#     --type SecureString --overwrite
#
#   aws ssm put-parameter --region ap-south-1 \
#     --name "/dlt/dev/telegram/chat-id" \
#     --value "YOUR_REAL_CHAT_ID" \
#     --type SecureString --overwrite
# ---------------------------------------------------------------------------
echo ""
echo "--- Telegram ---"
echo "  [SKIP] Telegram tokens use real AWS SSM (not LocalStack)"
echo "  Run 'aws ssm put-parameter' to set /dlt/dev/telegram/bot-token and chat-id"

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
