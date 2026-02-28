#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Automated Secret Setup
# =============================================================================
# ONE-TIME fully automated secret provisioning. Run by bootstrap.sh.
# Handles BOTH LocalStack (Dhan dev creds) AND real AWS SSM (Telegram).
#
# What it does:
#   1. Checks if AWS CLI is installed (installs via pip if missing)
#   2. Checks if AWS credentials are configured (runs 'aws configure' if not)
#   3. Seeds Dhan credentials into LocalStack SSM (prompts if not set)
#   4. Seeds Telegram tokens into real AWS SSM (prompts if not already stored)
#   5. Verifies everything works
#
# After first run: all secrets are in SSM. Every subsequent bootstrap/session
# just reads from SSM — zero manual configuration ever again.
#
# Secret paths:
#   /dlt/dev/dhan/client-id          → LocalStack SSM
#   /dlt/dev/dhan/client-secret      → LocalStack SSM
#   /dlt/dev/dhan/totp-secret        → LocalStack SSM
#   /dlt/dev/telegram/bot-token      → Real AWS SSM
#   /dlt/dev/telegram/chat-id        → Real AWS SSM
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

# ---------------------------------------------------------------------------
# Helper: prompt for a secret value (hides input)
# ---------------------------------------------------------------------------
prompt_secret() {
    local prompt_text="$1"
    local result=""
    echo -ne "  ${CYAN}${prompt_text}:${NC} "
    read -r result
    echo "${result}"
}

echo ""
echo -e "${CYAN}--- Secret Setup ---${NC}"
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

# ---- Step 2: AWS Credentials (for real AWS SSM) ----
echo -n "  Checking AWS credentials... "
if aws sts get-caller-identity --region "${REGION}" > /dev/null 2>&1; then
    ACCOUNT=$(aws sts get-caller-identity --region "${REGION}" --query Account --output text 2>/dev/null)
    echo -e "${GREEN}configured${NC} (account: ${ACCOUNT})"
else
    echo -e "${YELLOW}not configured${NC}"
    echo ""
    echo -e "  ${CYAN}AWS credentials are needed for Telegram notifications (real AWS SSM).${NC}"
    echo -e "  ${CYAN}Running 'aws configure' — enter your AWS Access Key, Secret Key, region (ap-south-1):${NC}"
    echo ""
    aws configure --region "${REGION}"
    echo ""
    if aws sts get-caller-identity --region "${REGION}" > /dev/null 2>&1; then
        echo -e "  ${GREEN}AWS credentials configured successfully${NC}"
    else
        echo -e "  ${YELLOW}AWS credentials not configured — Telegram will be in no-op mode${NC}"
        echo -e "  ${YELLOW}(You can run 'aws configure' later and re-run bootstrap)${NC}"
    fi
fi
echo ""

# ---- Step 3: Dhan Credentials → LocalStack SSM ----
echo -e "${CYAN}--- Dhan Credentials (LocalStack SSM) ---${NC}"

DHAN_NEEDS_SEED=false
if ssm_secret_exists "/dlt/${ENVIRONMENT}/dhan/client-id" "localstack" 2>/dev/null; then
    echo -e "  ${GREEN}Dhan credentials already in LocalStack${NC}"
else
    DHAN_NEEDS_SEED=true
fi

if [ "${DHAN_NEEDS_SEED}" = true ]; then
    echo -e "  Dhan credentials not found in LocalStack. Enter them now:"
    echo ""

    if [ -n "${DHAN_CLIENT_ID:-}" ]; then
        echo -e "  ${GREEN}Using DHAN_CLIENT_ID from environment${NC}"
    else
        DHAN_CLIENT_ID=$(prompt_secret "Dhan Client ID")
    fi

    if [ -n "${DHAN_CLIENT_SECRET:-}" ]; then
        echo -e "  ${GREEN}Using DHAN_CLIENT_SECRET from environment${NC}"
    else
        DHAN_CLIENT_SECRET=$(prompt_secret "Dhan Access Token (JWT)")
    fi

    if [ -n "${DHAN_TOTP_SECRET:-}" ]; then
        echo -e "  ${GREEN}Using DHAN_TOTP_SECRET from environment${NC}"
    else
        DHAN_TOTP_SECRET=$(prompt_secret "Dhan TOTP Secret")
    fi

    echo ""
    put_ssm_secret "/dlt/${ENVIRONMENT}/dhan/client-id" "${DHAN_CLIENT_ID}" "localstack"
    put_ssm_secret "/dlt/${ENVIRONMENT}/dhan/client-secret" "${DHAN_CLIENT_SECRET}" "localstack"
    put_ssm_secret "/dlt/${ENVIRONMENT}/dhan/totp-secret" "${DHAN_TOTP_SECRET}" "localstack"
fi
echo ""

# ---- Step 4: Telegram Tokens → Real AWS SSM ----
echo -e "${CYAN}--- Telegram Notifications (Real AWS SSM) ---${NC}"

# Check if AWS credentials work before attempting real SSM
if ! aws sts get-caller-identity --region "${REGION}" > /dev/null 2>&1; then
    echo -e "  ${YELLOW}AWS credentials not configured — skipping Telegram setup${NC}"
    echo -e "  ${YELLOW}Run 'aws configure' and re-run bootstrap to enable Telegram${NC}"
    echo ""
    echo -e "${GREEN}Secret setup complete (Telegram deferred).${NC}"
    exit 0
fi

TELEGRAM_NEEDS_SEED=false
if ssm_secret_exists "/dlt/${ENVIRONMENT}/telegram/bot-token" "aws" 2>/dev/null; then
    echo -e "  ${GREEN}Telegram tokens already in AWS SSM — no action needed${NC}"
else
    TELEGRAM_NEEDS_SEED=true
fi

if [ "${TELEGRAM_NEEDS_SEED}" = true ]; then
    echo -e "  Telegram tokens not found in AWS SSM. Enter them now:"
    echo -e "  (Get bot token from @BotFather on Telegram, chat ID from @userinfobot)"
    echo ""

    if [ -n "${TELEGRAM_BOT_TOKEN:-}" ]; then
        echo -e "  ${GREEN}Using TELEGRAM_BOT_TOKEN from environment${NC}"
    else
        TELEGRAM_BOT_TOKEN=$(prompt_secret "Telegram Bot Token")
    fi

    if [ -n "${TELEGRAM_CHAT_ID:-}" ]; then
        echo -e "  ${GREEN}Using TELEGRAM_CHAT_ID from environment${NC}"
    else
        TELEGRAM_CHAT_ID=$(prompt_secret "Telegram Chat ID")
    fi

    echo ""
    put_ssm_secret "/dlt/${ENVIRONMENT}/telegram/bot-token" "${TELEGRAM_BOT_TOKEN}" "aws"
    put_ssm_secret "/dlt/${ENVIRONMENT}/telegram/chat-id" "${TELEGRAM_CHAT_ID}" "aws"

    # Verify Telegram works by sending a test message
    echo ""
    echo -n "  Sending test notification... "
    RESPONSE=$(curl -s -X POST "https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/sendMessage" \
        -H "Content-Type: application/json" \
        -d "{\"chat_id\": \"${TELEGRAM_CHAT_ID}\", \"text\": \"dhan-live-trader: Bootstrap complete. Telegram notifications active.\", \"parse_mode\": \"HTML\"}" 2>/dev/null)

    OK=$(echo "${RESPONSE}" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))" 2>/dev/null || echo "False")

    if [ "${OK}" = "True" ]; then
        echo -e "${GREEN}sent! Check your Telegram.${NC}"
    else
        echo -e "${YELLOW}failed (check token/chat ID)${NC}"
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
if ssm_secret_exists "/dlt/${ENVIRONMENT}/telegram/bot-token" "aws" 2>/dev/null; then
    AWS_TG_STATUS="configured"
fi
echo "  AWS SSM Telegram: ${AWS_TG_STATUS}"

echo ""
echo -e "${GREEN}Secret setup complete.${NC}"
