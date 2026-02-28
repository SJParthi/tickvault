#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Automated Secret Setup (Zero Interactive Input)
# =============================================================================
# Fully automated secret provisioning. Run by bootstrap.sh.
# ZERO prompts — uses environment variables or seeds placeholders.
#
# What it does:
#   1. Ensures AWS CLI is installed (auto-installs via pip if missing)
#   2. Seeds Dhan credentials into LocalStack SSM
#   3. Seeds Telegram tokens into LocalStack SSM (so Rust app can read them)
#   4. Optionally stores Telegram tokens in real AWS SSM (for notify-telegram.sh)
#   5. Sends test notification if real Telegram tokens are available
#
# To provide real credentials, set env vars BEFORE running bootstrap:
#   export DHAN_CLIENT_ID="your-real-client-id"
#   export DHAN_CLIENT_SECRET="your-real-jwt-token"
#   export DHAN_TOTP_SECRET="your-real-totp-secret"
#   export TELEGRAM_BOT_TOKEN="your-bot-token-from-botfather"
#   export TELEGRAM_CHAT_ID="your-chat-id-from-userinfobot"
#   ./scripts/bootstrap.sh
#
# Without env vars: placeholders are seeded. App boots in degraded mode.
# Run again with env vars set to replace placeholders with real values.
#
# Secret paths:
#   /dlt/dev/dhan/client-id          → LocalStack SSM
#   /dlt/dev/dhan/client-secret      → LocalStack SSM
#   /dlt/dev/dhan/totp-secret        → LocalStack SSM
#   /dlt/dev/telegram/bot-token      → LocalStack SSM + Real AWS SSM
#   /dlt/dev/telegram/chat-id        → LocalStack SSM + Real AWS SSM
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
PLACEHOLDER="PLACEHOLDER_REPLACE_ME"

# ---------------------------------------------------------------------------
# Helper: put a secret into SSM (LocalStack or real AWS)
# ---------------------------------------------------------------------------
put_ssm_secret() {
    local endpoint_arg=""
    local name="$1"
    local value="$2"
    local target="$3"  # "localstack" or "aws"

    if [ "${target}" = "localstack" ]; then
        endpoint_arg="--endpoint-url ${LOCALSTACK_URL}"
        export AWS_ACCESS_KEY_ID="test"
        export AWS_SECRET_ACCESS_KEY="test"
    fi

    aws ssm put-parameter \
        ${endpoint_arg} \
        --region "${REGION}" \
        --name "${name}" \
        --value "${value}" \
        --type "SecureString" \
        --overwrite \
        --no-cli-pager \
        > /dev/null 2>&1

    echo -e "  ${GREEN}[OK]${NC} ${name} → ${target}"
}

# ---------------------------------------------------------------------------
# Helper: check if a secret exists in SSM
# ---------------------------------------------------------------------------
ssm_secret_exists() {
    local endpoint_arg=""
    local name="$1"
    local target="$2"

    if [ "${target}" = "localstack" ]; then
        endpoint_arg="--endpoint-url ${LOCALSTACK_URL}"
        export AWS_ACCESS_KEY_ID="test"
        export AWS_SECRET_ACCESS_KEY="test"
    fi

    aws ssm get-parameter \
        ${endpoint_arg} \
        --region "${REGION}" \
        --name "${name}" \
        --with-decryption \
        --no-cli-pager \
        > /dev/null 2>&1
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
echo ""

# ---- Step 2: Dhan Credentials → LocalStack SSM ----
echo -e "${CYAN}--- Dhan Credentials (LocalStack SSM) ---${NC}"

if ssm_secret_exists "/dlt/${ENVIRONMENT}/dhan/client-id" "localstack" 2>/dev/null; then
    echo -e "  ${GREEN}Dhan credentials already in LocalStack — no action needed${NC}"
else
    DHAN_CID="${DHAN_CLIENT_ID:-${PLACEHOLDER}}"
    DHAN_CS="${DHAN_CLIENT_SECRET:-${PLACEHOLDER}}"
    DHAN_TS="${DHAN_TOTP_SECRET:-${PLACEHOLDER}}"

    if [ "${DHAN_CID}" = "${PLACEHOLDER}" ]; then
        echo -e "  ${YELLOW}No DHAN_CLIENT_ID env var — seeding placeholders${NC}"
        echo -e "  ${YELLOW}To set real credentials later:${NC}"
        echo -e "  ${YELLOW}  export DHAN_CLIENT_ID=\"your-id\" DHAN_CLIENT_SECRET=\"your-jwt\" DHAN_TOTP_SECRET=\"your-totp\"${NC}"
        echo -e "  ${YELLOW}  ./scripts/setup-secrets.sh${NC}"
    else
        echo -e "  ${GREEN}Using Dhan credentials from environment variables${NC}"
    fi

    put_ssm_secret "/dlt/${ENVIRONMENT}/dhan/client-id" "${DHAN_CID}" "localstack"
    put_ssm_secret "/dlt/${ENVIRONMENT}/dhan/client-secret" "${DHAN_CS}" "localstack"
    put_ssm_secret "/dlt/${ENVIRONMENT}/dhan/totp-secret" "${DHAN_TS}" "localstack"
fi
echo ""

# ---- Step 3: Telegram Tokens → LocalStack SSM (so Rust app can read them) ----
echo -e "${CYAN}--- Telegram Tokens (LocalStack SSM) ---${NC}"

if ssm_secret_exists "/dlt/${ENVIRONMENT}/telegram/bot-token" "localstack" 2>/dev/null; then
    echo -e "  ${GREEN}Telegram tokens already in LocalStack — no action needed${NC}"
else
    TG_TOKEN="${TELEGRAM_BOT_TOKEN:-${PLACEHOLDER}}"
    TG_CHAT="${TELEGRAM_CHAT_ID:-${PLACEHOLDER}}"

    if [ "${TG_TOKEN}" = "${PLACEHOLDER}" ]; then
        echo -e "  ${YELLOW}No TELEGRAM_BOT_TOKEN env var — seeding placeholders${NC}"
        echo -e "  ${YELLOW}To set real Telegram tokens later:${NC}"
        echo -e "  ${YELLOW}  export TELEGRAM_BOT_TOKEN=\"your-token\" TELEGRAM_CHAT_ID=\"your-chat-id\"${NC}"
        echo -e "  ${YELLOW}  ./scripts/setup-secrets.sh${NC}"
    else
        echo -e "  ${GREEN}Using Telegram tokens from environment variables${NC}"
    fi

    put_ssm_secret "/dlt/${ENVIRONMENT}/telegram/bot-token" "${TG_TOKEN}" "localstack"
    put_ssm_secret "/dlt/${ENVIRONMENT}/telegram/chat-id" "${TG_CHAT}" "localstack"
fi
echo ""

# ---- Step 4: Telegram Tokens → Real AWS SSM (optional, for notify-telegram.sh) ----
echo -e "${CYAN}--- Telegram Tokens (Real AWS SSM — optional) ---${NC}"

TG_TOKEN_VALUE="${TELEGRAM_BOT_TOKEN:-${PLACEHOLDER}}"
TG_CHAT_VALUE="${TELEGRAM_CHAT_ID:-${PLACEHOLDER}}"

if [ "${TG_TOKEN_VALUE}" = "${PLACEHOLDER}" ]; then
    echo -e "  ${YELLOW}Skipping real AWS — no real Telegram tokens available${NC}"
elif ! aws sts get-caller-identity --region "${REGION}" > /dev/null 2>&1; then
    echo -e "  ${YELLOW}AWS credentials not configured — skipping real AWS SSM${NC}"
    echo -e "  ${YELLOW}Run 'aws configure' to enable notify-telegram.sh from CLI${NC}"
else
    ACCOUNT=$(aws sts get-caller-identity --region "${REGION}" --query Account --output text 2>/dev/null)
    echo -e "  ${GREEN}AWS credentials configured${NC} (account: ${ACCOUNT})"

    if ssm_secret_exists "/dlt/${ENVIRONMENT}/telegram/bot-token" "aws" 2>/dev/null; then
        echo -e "  ${GREEN}Telegram tokens already in real AWS SSM — no action needed${NC}"
    else
        put_ssm_secret "/dlt/${ENVIRONMENT}/telegram/bot-token" "${TG_TOKEN_VALUE}" "aws"
        put_ssm_secret "/dlt/${ENVIRONMENT}/telegram/chat-id" "${TG_CHAT_VALUE}" "aws"
    fi

    # Send test notification
    echo ""
    echo -n "  Sending test notification... "
    RESPONSE=$(curl -s -X POST "https://api.telegram.org/bot${TG_TOKEN_VALUE}/sendMessage" \
        -H "Content-Type: application/json" \
        -d "{\"chat_id\": \"${TG_CHAT_VALUE}\", \"text\": \"dhan-live-trader: Bootstrap complete. Telegram notifications active.\", \"parse_mode\": \"HTML\"}" 2>/dev/null)

    OK=$(echo "${RESPONSE}" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))" 2>/dev/null || echo "False")

    if [ "${OK}" = "True" ]; then
        echo -e "${GREEN}sent! Check your Telegram.${NC}"
    else
        echo -e "${YELLOW}failed (check token/chat ID — notifications will retry at app startup)${NC}"
    fi
fi
echo ""

# ---- Verification ----
echo -e "${CYAN}--- Verification ---${NC}"

LOCALSTACK_COUNT=$(aws ssm describe-parameters \
    --endpoint-url "${LOCALSTACK_URL}" \
    --region "${REGION}" \
    --no-cli-pager \
    --query "length(Parameters)" \
    --output text 2>/dev/null || echo "0")
echo "  LocalStack SSM: ${LOCALSTACK_COUNT} parameters"

AWS_TG_STATUS="not configured"
if aws sts get-caller-identity --region "${REGION}" > /dev/null 2>&1; then
    if ssm_secret_exists "/dlt/${ENVIRONMENT}/telegram/bot-token" "aws" 2>/dev/null; then
        AWS_TG_STATUS="configured"
    fi
fi
echo "  Real AWS SSM Telegram: ${AWS_TG_STATUS}"

echo ""
echo -e "${GREEN}Secret setup complete.${NC}"
