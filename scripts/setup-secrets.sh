#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Secret Verification
# =============================================================================
# Verifies all secrets exist in real AWS SSM (ap-south-1).
# Secrets are managed by Parthiban via AWS Console — this script only READS.
#
# What it does:
#   1. Ensures AWS CLI is installed (auto-installs via pip if missing)
#   2. Verifies AWS credentials are configured
#   3. Verifies all 9 secrets exist in real AWS SSM
#   4. Sends test Telegram notification
#
# All secrets read from real AWS SSM directly.
# =============================================================================

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

REGION="ap-south-1"
ENVIRONMENT="dev"

echo ""
echo -e "${CYAN}--- Secret Verification ---${NC}"
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

# ---- Step 2: AWS Credentials ----
echo -n "  Checking AWS credentials... "
if ! aws sts get-caller-identity --region "${REGION}" > /dev/null 2>&1; then
    echo -e "${RED}not configured${NC}"
    echo -e "  ${YELLOW}Run 'aws configure --region ${REGION}' once on your Mac.${NC}"
    echo -e "  ${YELLOW}App will start in degraded mode until configured.${NC}"
    exit 0
fi

ACCOUNT=$(aws sts get-caller-identity --region "${REGION}" --query Account --output text 2>/dev/null)
echo -e "${GREEN}configured${NC} (account: ${ACCOUNT})"
echo ""

# ---- Step 3: Verify all secrets exist in real AWS SSM ----
echo -e "${CYAN}--- Verifying secrets in AWS SSM (${REGION}) ---${NC}"

MISSING=0

verify_secret() {
    local name="$1"
    local description="$2"
    echo -n "  ${description}... "
    if aws ssm get-parameter --region "${REGION}" --name "${name}" --with-decryption --no-cli-pager > /dev/null 2>&1; then
        echo -e "${GREEN}OK${NC}"
    else
        echo -e "${RED}MISSING${NC}"
        MISSING=$((MISSING + 1))
    fi
}

verify_secret "/dlt/${ENVIRONMENT}/dhan/client-id" "Dhan Client ID"
verify_secret "/dlt/${ENVIRONMENT}/dhan/client-secret" "Dhan Access Token"
verify_secret "/dlt/${ENVIRONMENT}/dhan/totp-secret" "Dhan TOTP Secret"
verify_secret "/dlt/${ENVIRONMENT}/telegram/bot-token" "Telegram Bot Token"
verify_secret "/dlt/${ENVIRONMENT}/telegram/chat-id" "Telegram Chat ID"
verify_secret "/dlt/${ENVIRONMENT}/questdb/pg-user" "QuestDB PG User"
verify_secret "/dlt/${ENVIRONMENT}/questdb/pg-password" "QuestDB PG Password"
verify_secret "/dlt/${ENVIRONMENT}/grafana/admin-user" "Grafana Admin User"
verify_secret "/dlt/${ENVIRONMENT}/grafana/admin-password" "Grafana Admin Password"

echo ""

if [ "${MISSING}" -gt 0 ]; then
    echo -e "  ${RED}${MISSING} secret(s) missing in AWS SSM.${NC}"
    echo -e "  ${YELLOW}Add them via AWS Console: SSM Parameter Store → ${REGION}${NC}"
    exit 0
fi

echo -e "  ${GREEN}All 9 secrets verified in AWS SSM${NC}"

# ---- Step 4: Test Telegram notification ----
echo ""
echo -n "  Sending test Telegram notification... "
TG_TOKEN=$(aws ssm get-parameter --region "${REGION}" --name "/dlt/${ENVIRONMENT}/telegram/bot-token" --with-decryption --output text --query "Parameter.Value" --no-cli-pager 2>/dev/null)
TG_CHAT=$(aws ssm get-parameter --region "${REGION}" --name "/dlt/${ENVIRONMENT}/telegram/chat-id" --with-decryption --output text --query "Parameter.Value" --no-cli-pager 2>/dev/null)

RESPONSE=$(curl -s -X POST "https://api.telegram.org/bot${TG_TOKEN}/sendMessage" \
    -H "Content-Type: application/json" \
    -d "{\"chat_id\": \"${TG_CHAT}\", \"text\": \"dhan-live-trader: Bootstrap complete. Telegram active.\", \"parse_mode\": \"HTML\"}" 2>/dev/null)

OK=$(echo "${RESPONSE}" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))" 2>/dev/null || echo "False")

if [ "${OK}" = "True" ]; then
    echo -e "${GREEN}sent! Check your Telegram.${NC}"
else
    echo -e "${YELLOW}failed (check bot token/chat ID in AWS SSM Console)${NC}"
fi

echo ""
echo -e "${GREEN}Secret verification complete.${NC}"
