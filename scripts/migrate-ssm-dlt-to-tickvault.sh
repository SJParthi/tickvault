#!/usr/bin/env bash
# =============================================================================
# tickvault — SSM namespace migration: /dlt/* → /tickvault/*
# =============================================================================
# One-shot migration. Copies every /dlt/<env>/* parameter to the same path
# under /tickvault/<env>/* (preserving the SecureString type) and, once you
# verify the app boots cleanly, deletes the /dlt/* originals.
#
# The migration is idempotent — safe to re-run. Uses --overwrite so existing
# /tickvault/* duplicates get the correct values from /dlt/*.
#
# Usage:
#   ./scripts/migrate-ssm-dlt-to-tickvault.sh            # copy only (safe)
#   ./scripts/migrate-ssm-dlt-to-tickvault.sh --delete   # copy + delete /dlt/*
#   ./scripts/migrate-ssm-dlt-to-tickvault.sh --env prod # migrate prod env
#
# Prerequisites:
#   - AWS CLI configured with ap-south-1 region
#   - IAM permissions: ssm:GetParametersByPath, ssm:PutParameter, ssm:DeleteParameter
# =============================================================================

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

REGION="ap-south-1"
ENV="dev"
DELETE_OLD=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --env)    ENV="$2"; shift 2 ;;
        --delete) DELETE_OLD=true; shift ;;
        --region) REGION="$2"; shift 2 ;;
        *)        echo "Unknown arg: $1" >&2; exit 1 ;;
    esac
done

OLD_PREFIX="/dlt/${ENV}"
NEW_PREFIX="/tickvault/${ENV}"

echo ""
echo -e "${CYAN}╔══════════════════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║  tickvault SSM migration: ${OLD_PREFIX} → ${NEW_PREFIX}${NC}"
echo -e "${CYAN}╚══════════════════════════════════════════════════════════╝${NC}"
echo ""

if ! command -v aws > /dev/null 2>&1; then
    echo -e "${RED}ERROR: aws CLI not found${NC}" >&2
    exit 1
fi

# List all /dlt/<env>/* parameters
echo -e "${CYAN}[1/3] Fetching all ${OLD_PREFIX}/* parameters...${NC}"
PARAMS_RAW=$(aws ssm get-parameters-by-path \
    --path "${OLD_PREFIX}" \
    --recursive \
    --with-decryption \
    --region "${REGION}" \
    --query 'Parameters[*].[Name,Value,Type]' \
    --output json \
    --no-cli-pager 2>&1)

if [ -z "${PARAMS_RAW}" ] || [ "${PARAMS_RAW}" = "[]" ]; then
    echo -e "${YELLOW}  No parameters found under ${OLD_PREFIX} — nothing to migrate${NC}"
    exit 0
fi

PARAM_COUNT=$(echo "${PARAMS_RAW}" | python3 -c "import sys,json; print(len(json.load(sys.stdin)))")
echo -e "  ${GREEN}Found ${PARAM_COUNT} parameters to migrate${NC}"

# Copy each /dlt/* → /tickvault/*
echo ""
echo -e "${CYAN}[2/3] Copying parameters (${OLD_PREFIX}/* → ${NEW_PREFIX}/*)...${NC}"
echo "${PARAMS_RAW}" | python3 -c "
import sys, json
for p in json.load(sys.stdin):
    name, value, ptype = p[0], p[1], p[2]
    new_name = name.replace('${OLD_PREFIX}/', '${NEW_PREFIX}/', 1)
    print(f'{name}\t{value}\t{ptype}\t{new_name}')
" | while IFS=$'\t' read -r OLD_NAME VALUE PTYPE NEW_NAME; do
    printf "  %s → %s ... " "${OLD_NAME}" "${NEW_NAME}"
    if aws ssm put-parameter \
        --name "${NEW_NAME}" \
        --value "${VALUE}" \
        --type "${PTYPE}" \
        --overwrite \
        --region "${REGION}" \
        --no-cli-pager > /dev/null 2>&1; then
        echo -e "${GREEN}OK${NC}"
    else
        echo -e "${RED}FAILED${NC}"
    fi
done

# Verify all /tickvault/* params are in place
echo ""
echo -e "${CYAN}[3/3] Verifying ${NEW_PREFIX}/* ...${NC}"
NEW_COUNT=$(aws ssm get-parameters-by-path \
    --path "${NEW_PREFIX}" \
    --recursive \
    --region "${REGION}" \
    --query 'length(Parameters)' \
    --output text \
    --no-cli-pager 2>/dev/null || echo "0")
echo -e "  ${GREEN}${NEW_COUNT} parameters now under ${NEW_PREFIX}${NC}"

if [ "${NEW_COUNT}" -lt "${PARAM_COUNT}" ]; then
    echo -e "${RED}WARNING: Only ${NEW_COUNT}/${PARAM_COUNT} parameters migrated${NC}"
    echo -e "${RED}Review errors above. Do NOT run with --delete until all green.${NC}"
    exit 1
fi

echo ""
echo -e "${GREEN}═══ Migration PHASE 1 complete: all parameters copied ═══${NC}"
echo ""

# Delete old /dlt/* parameters if --delete flag was passed
if [ "${DELETE_OLD}" = true ]; then
    echo -e "${YELLOW}[DELETE] Removing old ${OLD_PREFIX}/* parameters...${NC}"
    echo "${PARAMS_RAW}" | python3 -c "
import sys, json
for p in json.load(sys.stdin):
    print(p[0])
" | while read -r OLD_NAME; do
        printf "  rm %s ... " "${OLD_NAME}"
        if aws ssm delete-parameter \
            --name "${OLD_NAME}" \
            --region "${REGION}" \
            --no-cli-pager > /dev/null 2>&1; then
            echo -e "${GREEN}DELETED${NC}"
        else
            echo -e "${RED}FAILED${NC}"
        fi
    done
    echo ""
    echo -e "${GREEN}═══ Migration PHASE 2 complete: old params removed ═══${NC}"
else
    echo -e "${YELLOW}Old ${OLD_PREFIX}/* parameters are still in place.${NC}"
    echo ""
    echo "Next steps:"
    echo -e "  1. ${CYAN}git pull${NC}"
    echo -e "  2. ${CYAN}cargo run --bin tickvault -p tickvault-app${NC}"
    echo -e "  3. Verify the app boots and ticks flow."
    echo -e "  4. When confident, delete the old namespace:"
    echo -e "     ${CYAN}./scripts/migrate-ssm-dlt-to-tickvault.sh --delete${NC}"
fi

echo ""
