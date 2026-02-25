#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Seed LocalStack SSM Parameter Store
# =============================================================================
# Populates SSM Parameter Store in LocalStack with placeholder dev values.
# Uses awslocal CLI (installed inside LocalStack container).
# Secret path convention: /dlt/<environment>/<service>/<key>
# =============================================================================

set -euo pipefail

LOCALSTACK_URL="http://localhost:4566"
REGION="ap-south-1"
ENVIRONMENT="dev"

# LocalStack accepts any credentials — these are dummy values for local dev only
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
# Dhan API Credentials (placeholders for dev)
# ---------------------------------------------------------------------------
echo "--- Dhan Credentials ---"
put_secret "/dlt/${ENVIRONMENT}/dhan/client-id" "dev-client-id-placeholder"
put_secret "/dlt/${ENVIRONMENT}/dhan/client-secret" "dev-client-secret-placeholder"
put_secret "/dlt/${ENVIRONMENT}/dhan/totp-secret" "dev-totp-secret-placeholder"

# ---------------------------------------------------------------------------
# Telegram Alert Bot (placeholders for dev)
# ---------------------------------------------------------------------------
echo ""
echo "--- Telegram Alerts ---"
put_secret "/dlt/${ENVIRONMENT}/telegram/bot-token" "dev-telegram-bot-token-placeholder"
put_secret "/dlt/${ENVIRONMENT}/telegram/chat-id" "dev-telegram-chat-id-placeholder"

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
