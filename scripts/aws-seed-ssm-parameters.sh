#!/usr/bin/env bash
# =============================================================================
# tickvault — AWS SSM Parameter Seeding
# =============================================================================
# Seeds the 9 required + 3 optional SSM parameters tickvault expects at boot.
# Run ONCE before the first `terraform apply` on a fresh AWS account, AND
# whenever a secret rotates.
#
# Per aws-budget.md rule 8 ("NO manual configuration on AWS deployment")
# every parameter the app reads at boot is created via this script —
# never by hand in the AWS console — so prod and dev stay byte-identical.
#
# Required parameters (boot HALTs if any missing):
#   /tickvault/<env>/dhan/client-id        (SecureString)
#   /tickvault/<env>/dhan/client-secret    (SecureString)
#   /tickvault/<env>/dhan/totp-secret      (SecureString)
#   /tickvault/<env>/questdb/pg-user       (SecureString)
#   /tickvault/<env>/questdb/pg-password   (SecureString)
#   /tickvault/<env>/telegram/bot-token    (SecureString)
#   /tickvault/<env>/telegram/chat-id      (SecureString)
#   /tickvault/<env>/api/bearer-token      (SecureString)
#   /tickvault/<env>/network/static-ip     (String — the EIP from terraform output)
#
# Optional parameters (sandbox mode + SNS SMS alerts + Groww feed #2):
#   /tickvault/<env>/dhan/sandbox-client-id  (SecureString — sandbox mode only)
#   /tickvault/<env>/dhan/sandbox-token      (SecureString — sandbox mode only)
#   /tickvault/<env>/sns/phone-number        (String — for CloudWatch SMS alarms)
#   /tickvault/<env>/groww/api-key           (SecureString — REQUIRED when groww_enabled=true; else the Groww sidecar's SSM fetch backs off forever and Groww silently produces ZERO ticks while Dhan runs fine)
#   /tickvault/<env>/groww/totp-secret       (SecureString — REQUIRED when groww_enabled=true)
#
# Groww env-var names for --from-env mode: TV_GROWW_API_KEY, TV_GROWW_TOTP_SECRET
#
# Usage:
#   scripts/aws-seed-ssm-parameters.sh                        # interactive, env=dev
#   scripts/aws-seed-ssm-parameters.sh --env prod             # interactive, env=prod
#   scripts/aws-seed-ssm-parameters.sh --env prod --dry-run   # validate only, no writes
#   TV_DHAN_CLIENT_ID=... TV_DHAN_CLIENT_SECRET=... \
#     scripts/aws-seed-ssm-parameters.sh --env prod --from-env  # non-interactive (CI/automation)
#
# Idempotent: every put-parameter call uses --overwrite, so re-running with
# the same values is a no-op.
#
# Honest envelope:
#   - The script writes to REAL AWS SSM (no mocks). Wrong creds = wrong account.
#   - In --from-env mode, the script does NOT prompt; missing env vars HALT.
#   - In interactive mode, secret prompts use `read -s` (no echo to terminal).
#   - The script does NOT log secret VALUES anywhere (only PATHS).
# =============================================================================

set -euo pipefail

# ---------------------------------------------------------------------------
# Colors
# ---------------------------------------------------------------------------
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

ok()    { echo -e "  ${GREEN}✓${NC} $1"; }
fail()  { echo -e "  ${RED}✗${NC} $1"; }
info()  { echo -e "  ${CYAN}ℹ${NC} $1"; }
warn()  { echo -e "  ${YELLOW}⚠${NC} $1"; }

# ---------------------------------------------------------------------------
# Defaults + arg parsing
# ---------------------------------------------------------------------------
ENVIRONMENT="dev"
REGION="ap-south-1"  # ap-south-1 Mumbai per aws-budget.md (low-latency Dhan)
DRY_RUN=0
FROM_ENV=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --env)       ENVIRONMENT="$2"; shift 2 ;;
        --region)    REGION="$2"; shift 2 ;;
        --dry-run)   DRY_RUN=1; shift ;;
        --from-env)  FROM_ENV=1; shift ;;
        -h|--help)
            sed -n '2,/^# ===/{/^# ===/!p;}' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *) fail "Unknown arg: $1"; exit 2 ;;
    esac
done

case "$ENVIRONMENT" in
    dev|sandbox|staging|prod) ;;
    *) fail "ENVIRONMENT must be one of: dev sandbox staging prod (got $ENVIRONMENT)"; exit 2 ;;
esac

echo ""
echo -e "${CYAN}╔══════════════════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║  tickvault — AWS SSM Parameter Seeding                   ║${NC}"
echo -e "${CYAN}╚══════════════════════════════════════════════════════════╝${NC}"
echo ""
info "Environment: ${ENVIRONMENT}"
info "Region:      ${REGION}"
info "Dry-run:     $([ "$DRY_RUN" -eq 1 ] && echo YES || echo no)"
info "Mode:        $([ "$FROM_ENV" -eq 1 ] && echo non-interactive\ \(env vars\) || echo interactive)"
echo ""

# ---------------------------------------------------------------------------
# Pre-flight: AWS CLI + auth + region
# ---------------------------------------------------------------------------
echo -e "${CYAN}[1/3] Pre-flight checks${NC}"

if ! command -v aws &>/dev/null; then
    fail "AWS CLI not found. Install: https://docs.aws.amazon.com/cli/latest/userguide/install-cliv2.html"
    exit 2
fi
ok "aws CLI present ($(aws --version 2>&1 | head -1))"

ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text 2>/dev/null || echo "")
if [ -z "$ACCOUNT_ID" ]; then
    fail "AWS auth failed. Run 'aws configure' or 'aws sso login' first."
    exit 2
fi
CALLER_ARN=$(aws sts get-caller-identity --query Arn --output text 2>/dev/null || echo "?")
ok "AWS account: ${ACCOUNT_ID}"
ok "Caller:      ${CALLER_ARN}"

# Confirm region is set / matches
CURRENT_REGION=$(aws configure get region 2>/dev/null || echo "")
if [ "$CURRENT_REGION" != "$REGION" ]; then
    warn "aws CLI default region is '${CURRENT_REGION}' but script targets '${REGION}' — explicit --region will be passed on every call"
fi

echo ""

# ---------------------------------------------------------------------------
# Helper: put_param <ssm_path> <var_name> <type> <description>
# ---------------------------------------------------------------------------
# Reads the value from the env var named <var_name> if --from-env, else
# prompts interactively (silent for SecureString). Writes to SSM unless
# --dry-run.
put_param() {
    local path="$1"
    local var_name="$2"
    local ptype="$3"   # SecureString | String
    local desc="$4"
    local value=""

    if [ "$FROM_ENV" -eq 1 ]; then
        # Non-interactive — value MUST be in env var
        value="${!var_name:-}"
        if [ -z "$value" ]; then
            fail "${var_name} env var empty (required for ${path})"
            return 1
        fi
    else
        # Interactive prompt
        if [ "$ptype" = "SecureString" ]; then
            printf "  → %s (%s): " "$path" "$desc"
            read -rs value
            echo ""  # newline after silent read
        else
            printf "  → %s (%s): " "$path" "$desc"
            read -r value
        fi
        if [ -z "$value" ]; then
            warn "${path} skipped (empty input)"
            return 0
        fi
    fi

    if [ "$DRY_RUN" -eq 1 ]; then
        ok "[DRY-RUN] would put ${path} (type=${ptype}, len=${#value})"
        return 0
    fi

    if aws ssm put-parameter \
        --name "$path" \
        --value "$value" \
        --type "$ptype" \
        --overwrite \
        --region "$REGION" \
        --output text > /dev/null 2>&1; then
        ok "wrote ${path} (type=${ptype}, len=${#value})"
    else
        fail "FAILED to write ${path} — check IAM permissions for ssm:PutParameter"
        return 1
    fi
}

# ---------------------------------------------------------------------------
# Required parameters
# ---------------------------------------------------------------------------
echo -e "${CYAN}[2/3] Required parameters (9)${NC}"

FAILURES=0
put_param "/tickvault/${ENVIRONMENT}/dhan/client-id"        TV_DHAN_CLIENT_ID        SecureString "Dhan API client ID"             || FAILURES=$((FAILURES + 1))
put_param "/tickvault/${ENVIRONMENT}/dhan/client-secret"    TV_DHAN_CLIENT_SECRET    SecureString "Dhan API client secret"         || FAILURES=$((FAILURES + 1))
put_param "/tickvault/${ENVIRONMENT}/dhan/totp-secret"      TV_DHAN_TOTP_SECRET      SecureString "Dhan TOTP secret (32-char base32)" || FAILURES=$((FAILURES + 1))
put_param "/tickvault/${ENVIRONMENT}/questdb/pg-user"       TV_QUESTDB_PG_USER       SecureString "QuestDB PostgreSQL user (default: admin)" || FAILURES=$((FAILURES + 1))
put_param "/tickvault/${ENVIRONMENT}/questdb/pg-password"   TV_QUESTDB_PG_PASSWORD   SecureString "QuestDB PostgreSQL password"    || FAILURES=$((FAILURES + 1))
put_param "/tickvault/${ENVIRONMENT}/telegram/bot-token"    TV_TELEGRAM_BOT_TOKEN    SecureString "Telegram bot token (from @BotFather)" || FAILURES=$((FAILURES + 1))
put_param "/tickvault/${ENVIRONMENT}/telegram/chat-id"      TV_TELEGRAM_CHAT_ID      SecureString "Telegram chat ID (numeric)"     || FAILURES=$((FAILURES + 1))
put_param "/tickvault/${ENVIRONMENT}/api/bearer-token"      TV_API_BEARER_TOKEN      SecureString "API bearer token (UUID v4 or random 32-char)" || FAILURES=$((FAILURES + 1))
put_param "/tickvault/${ENVIRONMENT}/network/static-ip"     TV_NETWORK_STATIC_IP     String       "EIP from 'terraform output elastic_ip'" || FAILURES=$((FAILURES + 1))

echo ""

# ---------------------------------------------------------------------------
# Optional parameters
# ---------------------------------------------------------------------------
echo -e "${CYAN}[3/3] Optional parameters (up to 5 — skip with empty input)${NC}"

if [ "$ENVIRONMENT" = "sandbox" ]; then
    info "sandbox mode: the 2 sandbox-* parameters are needed"
else
    info "non-sandbox env: skipping sandbox-* prompts (they're sandbox-only)"
fi

if [ "$ENVIRONMENT" = "sandbox" ]; then
    put_param "/tickvault/${ENVIRONMENT}/dhan/sandbox-client-id" TV_DHAN_SANDBOX_CLIENT_ID SecureString "Dhan sandbox client ID" || true
    put_param "/tickvault/${ENVIRONMENT}/dhan/sandbox-token"     TV_DHAN_SANDBOX_TOKEN     SecureString "Dhan sandbox JWT (manual paste from web.dhan.co)" || true
fi

put_param "/tickvault/${ENVIRONMENT}/sns/phone-number" TV_SNS_PHONE_NUMBER String "+CountryCode-PhoneNumber for SMS alarms (e.g. +91...)" || true

# Groww second live feed (feed #2). REQUIRED when groww_enabled=true — which is
# the prod default (config/production.toml). Without these two the Groww
# sidecar's SSM fetch backs off FOREVER and Groww silently produces ZERO ticks
# while Dhan runs fine. Optional here (skip with empty input) because Groww is
# default-OFF in dev; a prod run MUST provide them or Groww stays dark.
if [ "$ENVIRONMENT" = "prod" ]; then
    warn "prod runs Groww (groww_enabled=true) — the 2 groww/* params below are REQUIRED; empty input leaves the Groww feed silent (zero ticks)"
fi
put_param "/tickvault/${ENVIRONMENT}/groww/api-key"     TV_GROWW_API_KEY     SecureString "Groww API key (feed #2; required when groww_enabled)" || true
put_param "/tickvault/${ENVIRONMENT}/groww/totp-secret" TV_GROWW_TOTP_SECRET SecureString "Groww TOTP secret (feed #2; required when groww_enabled)" || true

echo ""

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
if [ "$DRY_RUN" -eq 1 ]; then
    info "Dry-run complete — no SSM writes performed."
elif [ "$FAILURES" -gt 0 ]; then
    fail "${FAILURES} required parameter(s) failed to write. Boot will HALT until fixed."
    exit 1
else
    ok "All required SSM parameters seeded for env=${ENVIRONMENT}"
    info "Next step: run 'terraform apply' from deploy/aws/terraform/ — the EC2 instance"
    info "           will read these parameters at boot via its IAM role."
fi
