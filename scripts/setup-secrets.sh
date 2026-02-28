#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Automated Secret Setup (Zero Interactive Input)
# =============================================================================
# Fully automated secret provisioning. Run by bootstrap.sh.
# ZERO prompts. ZERO env vars needed. Reads from real AWS SSM and copies
# to LocalStack automatically.
#
# How it works:
#   1. Ensures AWS CLI is installed (auto-installs via pip if missing)
#   2. Checks if AWS credentials are configured (aws sts get-caller-identity)
#   3. Reads ALL secrets from real AWS SSM (source of truth)
#   4. Copies them into LocalStack SSM (so Rust app works in dev)
#   5. Sends test Telegram notification to verify
#
# Prerequisites:
#   - AWS CLI configured (aws configure — one-time on Mac)
#   - Secrets already in real AWS SSM (Parthiban set them via AWS Console)
#
# Secret paths (all in real AWS SSM ap-south-1):
#   /dlt/dev/dhan/client-id          → copied to LocalStack
#   /dlt/dev/dhan/client-secret      → copied to LocalStack
#   /dlt/dev/dhan/totp-secret        → copied to LocalStack
#   /dlt/dev/telegram/bot-token      → copied to LocalStack
#   /dlt/dev/telegram/chat-id        → copied to LocalStack
# =============================================================================

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

REGION="ap-south-1"
ENVIRONMENT="dev"
LOCALSTACK_URL="http://localhost:4566"

# ---------------------------------------------------------------------------
# Helper: read a secret from real AWS SSM
# ---------------------------------------------------------------------------
read_aws_secret() {
    local name="$1"
    aws ssm get-parameter \
        --region "${REGION}" \
        --name "${name}" \
        --with-decryption \
        --output text \
        --query "Parameter.Value" \
        --no-cli-pager \
        2>/dev/null
}

# ---------------------------------------------------------------------------
# Helper: put a secret into LocalStack SSM
# ---------------------------------------------------------------------------
put_localstack_secret() {
    local name="$1"
    local value="$2"

    export AWS_ACCESS_KEY_ID="test"
    export AWS_SECRET_ACCESS_KEY="test"

    aws ssm put-parameter \
        --endpoint-url "${LOCALSTACK_URL}" \
        --region "${REGION}" \
        --name "${name}" \
        --value "${value}" \
        --type "SecureString" \
        --overwrite \
        --no-cli-pager \
        > /dev/null 2>&1

    echo -e "  ${GREEN}[OK]${NC} ${name}"
}

# ---------------------------------------------------------------------------
# Helper: check if a secret exists in LocalStack SSM
# ---------------------------------------------------------------------------
localstack_secret_exists() {
    local name="$1"

    export AWS_ACCESS_KEY_ID="test"
    export AWS_SECRET_ACCESS_KEY="test"

    aws ssm get-parameter \
        --endpoint-url "${LOCALSTACK_URL}" \
        --region "${REGION}" \
        --name "${name}" \
        --with-decryption \
        --no-cli-pager \
        > /dev/null 2>&1
}

# ---------------------------------------------------------------------------
# Helper: copy a secret from real AWS SSM → LocalStack SSM
# ---------------------------------------------------------------------------
copy_secret_to_localstack() {
    local name="$1"
    local description="$2"

    echo -n "  Copying ${description}... "
    VALUE=$(read_aws_secret "${name}")
    if [ -n "${VALUE}" ]; then
        put_localstack_secret "${name}" "${VALUE}"
    else
        echo -e "${RED}FAILED${NC} (not found in real AWS SSM: ${name})"
        return 1
    fi
}

echo ""
echo -e "${CYAN}--- Secret Setup (fully automated) ---${NC}"
echo ""

# ---- Step 1: AWS CLI ----
echo -n "  Checking AWS CLI... "
if command -v aws > /dev/null 2>&1; then
    echo -e "${GREEN}installed${NC}"
else
    echo -e "${YELLOW}not found — installing via pip...${NC}"
    pip3 install awscli --quiet 2>/dev/null || pip install awscli --quiet 2>/dev/null
    if command -v aws > /dev/null 2>&1; then
        echo -e "  ${GREEN}AWS CLI installed${NC}"
    else
        echo -e "  ${RED}Failed to install AWS CLI. Install manually: pip3 install awscli${NC}"
        exit 1
    fi
fi

# ---- Step 2: Verify AWS credentials ----
echo -n "  Checking AWS credentials... "
if ! aws sts get-caller-identity --region "${REGION}" > /dev/null 2>&1; then
    echo -e "${RED}not configured${NC}"
    echo ""
    echo -e "  ${RED}AWS credentials are required. Run 'aws configure' first:${NC}"
    echo -e "  ${YELLOW}  aws configure --region ${REGION}${NC}"
    echo -e "  ${YELLOW}  Then re-run: ./scripts/bootstrap.sh${NC}"
    echo ""
    echo -e "  ${YELLOW}Skipping secret setup — app will start in degraded mode.${NC}"
    exit 0
fi

ACCOUNT=$(aws sts get-caller-identity --region "${REGION}" --query Account --output text 2>/dev/null)
echo -e "${GREEN}configured${NC} (account: ${ACCOUNT})"
echo ""

# ---- Step 3: Check if LocalStack already has all secrets ----
ALL_IN_LOCALSTACK=true
for PARAM in "/dlt/${ENVIRONMENT}/dhan/client-id" "/dlt/${ENVIRONMENT}/telegram/bot-token"; do
    if ! localstack_secret_exists "${PARAM}" 2>/dev/null; then
        ALL_IN_LOCALSTACK=false
        break
    fi
done

if [ "${ALL_IN_LOCALSTACK}" = true ]; then
    echo -e "  ${GREEN}All secrets already in LocalStack — no action needed${NC}"
    echo ""
    echo -e "${GREEN}Secret setup complete.${NC}"
    exit 0
fi

# ---- Step 4: Copy ALL secrets from real AWS SSM → LocalStack ----
echo -e "${CYAN}--- Copying secrets: Real AWS SSM → LocalStack ---${NC}"
echo ""

COPY_FAILURES=0

copy_secret_to_localstack "/dlt/${ENVIRONMENT}/dhan/client-id" "Dhan Client ID" || COPY_FAILURES=$((COPY_FAILURES + 1))
copy_secret_to_localstack "/dlt/${ENVIRONMENT}/dhan/client-secret" "Dhan Access Token" || COPY_FAILURES=$((COPY_FAILURES + 1))
copy_secret_to_localstack "/dlt/${ENVIRONMENT}/dhan/totp-secret" "Dhan TOTP Secret" || COPY_FAILURES=$((COPY_FAILURES + 1))
copy_secret_to_localstack "/dlt/${ENVIRONMENT}/telegram/bot-token" "Telegram Bot Token" || COPY_FAILURES=$((COPY_FAILURES + 1))
copy_secret_to_localstack "/dlt/${ENVIRONMENT}/telegram/chat-id" "Telegram Chat ID" || COPY_FAILURES=$((COPY_FAILURES + 1))

echo ""

if [ "${COPY_FAILURES}" -gt 0 ]; then
    echo -e "  ${YELLOW}${COPY_FAILURES} secret(s) could not be copied — check AWS SSM console${NC}"
fi

# ---- Step 5: Send test Telegram notification ----
TG_TOKEN=$(read_aws_secret "/dlt/${ENVIRONMENT}/telegram/bot-token")
TG_CHAT=$(read_aws_secret "/dlt/${ENVIRONMENT}/telegram/chat-id")

if [ -n "${TG_TOKEN}" ] && [ -n "${TG_CHAT}" ]; then
    echo -n "  Sending test Telegram notification... "
    RESPONSE=$(curl -s -X POST "https://api.telegram.org/bot${TG_TOKEN}/sendMessage" \
        -H "Content-Type: application/json" \
        -d "{\"chat_id\": \"${TG_CHAT}\", \"text\": \"dhan-live-trader: Bootstrap complete. Telegram notifications active.\", \"parse_mode\": \"HTML\"}" 2>/dev/null)

    OK=$(echo "${RESPONSE}" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))" 2>/dev/null || echo "False")

    if [ "${OK}" = "True" ]; then
        echo -e "${GREEN}sent! Check your Telegram.${NC}"
    else
        echo -e "${YELLOW}failed (check bot token/chat ID in AWS SSM)${NC}"
    fi
else
    echo -e "  ${YELLOW}Telegram tokens not in AWS SSM — skipping test notification${NC}"
fi

# ---- Verification ----
echo ""
echo -e "${CYAN}--- Verification ---${NC}"

export AWS_ACCESS_KEY_ID="test"
export AWS_SECRET_ACCESS_KEY="test"

LOCALSTACK_COUNT=$(aws ssm describe-parameters \
    --endpoint-url "${LOCALSTACK_URL}" \
    --region "${REGION}" \
    --no-cli-pager \
    --query "length(Parameters)" \
    --output text 2>/dev/null || echo "0")
echo "  LocalStack SSM: ${LOCALSTACK_COUNT} parameters (expected: 5)"

echo ""
echo -e "${GREEN}Secret setup complete.${NC}"
