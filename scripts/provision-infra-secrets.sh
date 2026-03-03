#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Infrastructure Secret Provisioner
# =============================================================================
# Auto-creates QuestDB and Grafana credentials in AWS SSM if they don't exist.
# Generates cryptographically random passwords. Idempotent — safe to re-run.
#
# Dhan/Telegram secrets are NOT touched here — those are user-provided and
# managed manually via AWS Console by Parthiban.
#
# Usage:
#   ./scripts/provision-infra-secrets.sh          # defaults to dev
#   ENVIRONMENT=prod ./scripts/provision-infra-secrets.sh
# =============================================================================

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

REGION="ap-south-1"
ENVIRONMENT="${ENVIRONMENT:-dev}"
CREATED=0
EXISTED=0

echo "" >&2
echo -e "${CYAN}--- Infrastructure Secret Provisioner (${ENVIRONMENT}) ---${NC}" >&2
echo "" >&2

# ---- Prerequisite: AWS CLI + credentials ----
if ! command -v aws > /dev/null 2>&1; then
    echo -e "  ${YELLOW}AWS CLI not found — auto-installing...${NC}" >&2
    if command -v pip3 > /dev/null 2>&1; then
        pip3 install awscli --quiet 2>/dev/null
    elif command -v pip > /dev/null 2>&1; then
        pip install awscli --quiet 2>/dev/null
    fi
    if ! command -v aws > /dev/null 2>&1; then
        echo -e "  ${RED}Could not auto-install AWS CLI. Install: pip3 install awscli${NC}" >&2
        exit 1
    fi
    echo -e "  ${GREEN}AWS CLI installed${NC}" >&2
fi

if ! aws sts get-caller-identity --region "${REGION}" > /dev/null 2>&1; then
    echo -e "  ${RED}AWS credentials not configured. Run: aws configure --region ${REGION}${NC}" >&2
    exit 1
fi

# ---- Helper: generate random password ----
# 32 chars, alphanumeric + symbols safe for shell/YAML/JDBC
generate_password() {
    # Use /dev/urandom → base64 → strip unsafe chars → take 32 chars
    head -c 48 /dev/urandom | base64 | tr -d '/+=' | head -c 32
}

# ---- Helper: provision a secret if it doesn't exist ----
provision_secret() {
    local ssm_path="$1"
    local description="$2"
    local value="$3"

    echo -n "  ${description}... " >&2

    # Check if it already exists
    if aws ssm get-parameter \
        --region "${REGION}" \
        --name "${ssm_path}" \
        --no-cli-pager > /dev/null 2>&1; then
        echo -e "${GREEN}EXISTS${NC}" >&2
        EXISTED=$((EXISTED + 1))
        return 0
    fi

    # Create it
    if aws ssm put-parameter \
        --region "${REGION}" \
        --name "${ssm_path}" \
        --type "SecureString" \
        --value "${value}" \
        --description "${description} (auto-provisioned by dhan-live-trader)" \
        --no-cli-pager > /dev/null 2>&1; then
        echo -e "${GREEN}CREATED${NC}" >&2
        CREATED=$((CREATED + 1))
    else
        echo -e "${RED}FAILED${NC}" >&2
        return 1
    fi
}

# ---- QuestDB PG wire credentials ----
QUESTDB_PASSWORD=$(generate_password)
provision_secret "/dlt/${ENVIRONMENT}/questdb/pg-user" "QuestDB PG User" "admin"
provision_secret "/dlt/${ENVIRONMENT}/questdb/pg-password" "QuestDB PG Password" "${QUESTDB_PASSWORD}"

# ---- Grafana admin credentials ----
GRAFANA_PASSWORD=$(generate_password)
provision_secret "/dlt/${ENVIRONMENT}/grafana/admin-user" "Grafana Admin User" "admin"
provision_secret "/dlt/${ENVIRONMENT}/grafana/admin-password" "Grafana Admin Password" "${GRAFANA_PASSWORD}"

# ---- Summary ----
echo "" >&2
TOTAL=$((CREATED + EXISTED))
echo -e "  ${GREEN}${TOTAL}/4 infra secrets ready${NC} (${CREATED} created, ${EXISTED} already existed)" >&2
echo "" >&2
