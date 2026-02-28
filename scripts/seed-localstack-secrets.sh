#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Seed LocalStack SSM Parameter Store
# =============================================================================
# Copies ALL secrets from real AWS SSM (ap-south-1) into LocalStack SSM.
# Real AWS SSM is the source of truth — Parthiban manages secrets via
# the AWS Console. This script just mirrors them locally.
#
# Usage:
#   ./scripts/seed-localstack-secrets.sh
#
# Prerequisites:
#   - AWS CLI configured (aws configure — one-time)
#   - Secrets already in real AWS SSM
#   - LocalStack running (docker compose up dlt-localstack)
# =============================================================================

set -euo pipefail

LOCALSTACK_URL="http://localhost:4566"
REGION="ap-south-1"
ENVIRONMENT="dev"

echo "Seeding LocalStack SSM from real AWS SSM..."
echo "Source:   Real AWS SSM (${REGION})"
echo "Target:   LocalStack (${LOCALSTACK_URL})"
echo ""

# Verify real AWS credentials
if ! aws sts get-caller-identity --region "${REGION}" > /dev/null 2>&1; then
    echo "ERROR: AWS credentials not configured. Run 'aws configure' first."
    exit 1
fi

# Helper: copy a secret from real AWS → LocalStack
copy_secret() {
    local path="$1"

    # Read from real AWS SSM
    VALUE=$(aws ssm get-parameter \
        --region "${REGION}" \
        --name "${path}" \
        --with-decryption \
        --output text \
        --query "Parameter.Value" \
        --no-cli-pager \
        2>/dev/null)

    if [ -z "${VALUE}" ]; then
        echo "  [SKIP] ${path} — not found in real AWS SSM"
        return 1
    fi

    # Write to LocalStack SSM
    AWS_ACCESS_KEY_ID="test" AWS_SECRET_ACCESS_KEY="test" \
    aws ssm put-parameter \
        --endpoint-url "${LOCALSTACK_URL}" \
        --region "${REGION}" \
        --name "${path}" \
        --value "${VALUE}" \
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
copy_secret "/dlt/${ENVIRONMENT}/dhan/client-id"
copy_secret "/dlt/${ENVIRONMENT}/dhan/client-secret"
copy_secret "/dlt/${ENVIRONMENT}/dhan/totp-secret"

# ---------------------------------------------------------------------------
# Telegram Tokens
# ---------------------------------------------------------------------------
echo ""
echo "--- Telegram Tokens ---"
copy_secret "/dlt/${ENVIRONMENT}/telegram/bot-token"
copy_secret "/dlt/${ENVIRONMENT}/telegram/chat-id"

# ---------------------------------------------------------------------------
# Verification
# ---------------------------------------------------------------------------
echo ""
echo "--- Verification ---"
PARAM_COUNT=$(AWS_ACCESS_KEY_ID="test" AWS_SECRET_ACCESS_KEY="test" \
    aws ssm describe-parameters \
    --endpoint-url "${LOCALSTACK_URL}" \
    --region "${REGION}" \
    --no-cli-pager \
    --query "length(Parameters)" \
    --output text 2>/dev/null)

echo "Total parameters in LocalStack: ${PARAM_COUNT} (expected: 5)"
echo ""
echo "Seed complete."
