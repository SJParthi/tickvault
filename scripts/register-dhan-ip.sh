#!/usr/bin/env bash
# register-dhan-ip.sh — register / verify the EC2 elastic IP with Dhan's
# static-IP whitelist for Order APIs.
#
# Charter authority: operator-charter-forever.md §I (static-IP enforced
# 2026-04-01 — orders from unregistered IPs are REJECTED with no grace
# period) + .claude/rules/dhan/authentication.md rule 7 (7-day cooldown
# after `PUT /v2/ip/modifyIP`).
#
# Endpoints (Dhan API v2):
#   GET  /v2/ip/getIP    — read currently-registered IP + ordersAllowed
#   POST /v2/ip/setIP    — first-time registration (NO cooldown)
#   PUT  /v2/ip/modifyIP — change registered IP (7-DAY COOLDOWN — destructive)
#
# Z+ defense:
#   L1 DETECT    — pre-flight: GET current IP, compare to target
#   L2 VERIFY    — IPv4 validation, terraform-output cross-check
#   L4 PREVENT   — dry-run is the default; modify requires explicit --modify
#   L5 AUDIT     — append every action to data/logs/dhan-ip-changes.log
#   L6 RECOVER   — post-action GET to confirm ordersAllowed=true
#   L7 COOLDOWN  — 7-day cooldown warning before --modify
#
# Usage:
#   register-dhan-ip.sh                 # dry-run: show what would happen
#   register-dhan-ip.sh --apply         # first-time setIP (fails if already registered)
#   register-dhan-ip.sh --apply --modify  # PUT modifyIP (7-day cooldown !!)
#   register-dhan-ip.sh --ip 1.2.3.4 --apply
#
# Exit codes:
#   0  success (or dry-run preview)
#   1  pre-flight validation failure
#   2  Dhan API error
#   3  cooldown breach refused
#   4  ordersAllowed verification failed post-action

set -euo pipefail

# ---------------------------------------------------------------------------
# Defaults — overridable via env vars + flags
# ---------------------------------------------------------------------------
: "${AWS_REGION:=ap-south-1}"
: "${ENVIRONMENT:=prod}"
: "${DHAN_API_BASE:=https://api.dhan.co/v2}"
: "${SSM_CLIENT_ID_PATH:=/tickvault/${ENVIRONMENT}/dhan/client-id}"
# The Dhan JWT is NOT stored in SSM — it's generated at runtime by the
# app using client-id + client-secret + totp-secret, and cached on
# disk at this path. The script reads the JWT from the local cache
# instead of asking the operator to paste it. Path matches
# crates/common/src/constants.rs::TOKEN_CACHE_FILE_PATH.
: "${TOKEN_CACHE_FILE:=data/cache/tv-token-cache}"
# Operator override — paste a JWT directly if the cache is stale or
# the script is being run from outside the tickvault working dir.
: "${TV_DHAN_ACCESS_TOKEN:=}"
: "${AUDIT_LOG:=data/logs/dhan-ip-changes.log}"
: "${TERRAFORM_DIR:=deploy/aws/terraform}"

# Flags
DRY_RUN=true
ALLOW_MODIFY=false
TARGET_IP=""
IP_FLAG="PRIMARY"

usage() {
    grep '^#' "$0" | sed 's/^# \{0,1\}//' | head -50
}

die() {
    printf 'ERROR: %s\n' "$*" >&2
    exit "${2:-1}"
}

# ---------------------------------------------------------------------------
# Flag parsing
# ---------------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
    case "$1" in
        --apply)     DRY_RUN=false ;;
        --modify)    ALLOW_MODIFY=true ;;
        --ip)        TARGET_IP="$2"; shift ;;
        --secondary) IP_FLAG="SECONDARY" ;;
        --help|-h)   usage; exit 0 ;;
        *)           die "unknown flag: $1" ;;
    esac
    shift
done

# ---------------------------------------------------------------------------
# Pure-function helpers (testable via shell test harness)
# ---------------------------------------------------------------------------

# Strict IPv4 dotted-quad validator. Rejects IPv6, trailing whitespace,
# ports, leading zeros. Exits 0 if valid, 1 otherwise.
is_valid_ipv4() {
    local ip="${1:-}"
    # 4 dotted decimals, each 1-3 digits 0-255, no leading zeros except "0"
    if [[ "$ip" =~ ^(0|[1-9][0-9]?|1[0-9][0-9]|2[0-4][0-9]|25[0-5])\.(0|[1-9][0-9]?|1[0-9][0-9]|2[0-4][0-9]|25[0-5])\.(0|[1-9][0-9]?|1[0-9][0-9]|2[0-4][0-9]|25[0-5])\.(0|[1-9][0-9]?|1[0-9][0-9]|2[0-4][0-9]|25[0-5])$ ]]; then
        return 0
    fi
    return 1
}

# Parse the Dhan-API modifyDate (e.g. "2026-05-18T10:30:00.000") and
# return the cooldown remaining in days (rounded down). Returns "0" if
# the date is empty or unparseable. Pure function; reads no env or files.
days_since_iso_date() {
    local iso="${1:-}"
    [[ -z "$iso" ]] && { echo 0; return; }
    local now_epoch then_epoch
    now_epoch=$(date -u +%s)
    # Try GNU date first; fall back to BSD date format on Mac.
    then_epoch=$(date -u -d "$iso" +%s 2>/dev/null || date -u -j -f "%Y-%m-%dT%H:%M:%S" "${iso%.*}" +%s 2>/dev/null || echo 0)
    if [[ "$then_epoch" == "0" ]] || [[ -z "$then_epoch" ]]; then
        echo 0
        return
    fi
    echo $(( (now_epoch - then_epoch) / 86400 ))
}

# ---------------------------------------------------------------------------
# Step 1 — Resolve target IP
# ---------------------------------------------------------------------------
echo "==> Step 1/6 — resolve target IP"

if [[ -z "$TARGET_IP" ]]; then
    # Prefer terraform output for source-of-truth EIP.
    if [[ -d "$TERRAFORM_DIR" ]] && command -v terraform >/dev/null 2>&1; then
        TARGET_IP=$(terraform -chdir="$TERRAFORM_DIR" output -raw elastic_ip 2>/dev/null || true)
    fi
    if [[ -z "$TARGET_IP" ]]; then
        die "could not auto-resolve target IP. Pass --ip A.B.C.D or run from a repo with terraform state." 1
    fi
    echo "    auto-resolved via terraform output: $TARGET_IP"
else
    echo "    operator-provided: $TARGET_IP"
fi

is_valid_ipv4 "$TARGET_IP" || die "target IP '$TARGET_IP' is not a valid IPv4 dotted-quad" 1

# ---------------------------------------------------------------------------
# Step 2 — Resolve credentials
#   - Client ID:    SSM SecureString (seeded by aws-seed-ssm-parameters.sh)
#   - Access token: TV_DHAN_ACCESS_TOKEN env override, OR the on-disk JWT
#                   cache the running app maintains. We do NOT shell out
#                   to TOTP generation here — that's the app's job; this
#                   script piggybacks on whatever the app has cached.
# ---------------------------------------------------------------------------
echo "==> Step 2/6 — resolve credentials"

command -v aws >/dev/null 2>&1 || die "aws CLI not found in PATH" 1
command -v jq  >/dev/null 2>&1 || die "jq not found in PATH"  1

CLIENT_ID=$(aws ssm get-parameter \
    --region "$AWS_REGION" \
    --name "$SSM_CLIENT_ID_PATH" \
    --with-decryption \
    --query 'Parameter.Value' \
    --output text 2>/dev/null) \
    || die "could not read $SSM_CLIENT_ID_PATH from SSM. Run scripts/aws-seed-ssm-parameters.sh first." 1
[[ -n "$CLIENT_ID" ]] || die "client ID is empty" 1

# Token resolution order: env override -> on-disk cache.
if [[ -n "$TV_DHAN_ACCESS_TOKEN" ]]; then
    ACCESS_TOKEN="$TV_DHAN_ACCESS_TOKEN"
    echo "    access token source: TV_DHAN_ACCESS_TOKEN env override"
elif [[ -f "$TOKEN_CACHE_FILE" ]]; then
    # Cache format: JSON { access_token: "...", expires_at: <epoch> }.
    # Pull the access_token field with jq. Fail loudly if the cache
    # exists but is malformed — pasting "{}" would otherwise sail
    # through and produce a confusing 401 from Dhan.
    ACCESS_TOKEN=$(jq -r '.access_token // empty' "$TOKEN_CACHE_FILE" 2>/dev/null) \
        || die "$TOKEN_CACHE_FILE exists but is not valid JSON. Re-run the app to regenerate the cache, OR export TV_DHAN_ACCESS_TOKEN=<jwt>." 1
    [[ -n "$ACCESS_TOKEN" ]] || die "$TOKEN_CACHE_FILE has no access_token field. Re-run the app to regenerate." 1
    echo "    access token source: $TOKEN_CACHE_FILE"
else
    die "no Dhan access token available. Either: \
(a) run the tickvault app at least once on this host so the JWT lands in $TOKEN_CACHE_FILE, or \
(b) export TV_DHAN_ACCESS_TOKEN=<jwt> before re-running this script." 1
fi

[[ "$ACCESS_TOKEN" =~ ^eyJ ]] || die "access token does not look like a JWT (no eyJ prefix)" 1

echo "    client ID:           $CLIENT_ID"
echo "    access token tail:   ...${ACCESS_TOKEN: -8}"

# ---------------------------------------------------------------------------
# Step 3 — Pre-flight GET to check current state
# ---------------------------------------------------------------------------
echo "==> Step 3/6 — GET /v2/ip/getIP (pre-flight)"

CURRENT_JSON=$(curl -sS -X GET "${DHAN_API_BASE}/ip/getIP" \
    -H "access-token: ${ACCESS_TOKEN}" \
    -H "Accept: application/json") \
    || die "GET /v2/ip/getIP failed" 2

# Capture fields. Empty string if absent (GET on virgin account returns {}).
CURRENT_IP=$(echo "$CURRENT_JSON" | jq -r '.ip // ""')
ORDERS_ALLOWED=$(echo "$CURRENT_JSON" | jq -r '.ordersAllowed // false')
MODIFY_DATE_PRIMARY=$(echo "$CURRENT_JSON" | jq -r '.modifyDatePrimary // ""')

echo "    current registered IP: ${CURRENT_IP:-<none>}"
echo "    ordersAllowed:         $ORDERS_ALLOWED"
echo "    last primary modify:   ${MODIFY_DATE_PRIMARY:-<never>}"

# ---------------------------------------------------------------------------
# Step 4 — Decide POST vs PUT vs no-op
# ---------------------------------------------------------------------------
echo "==> Step 4/6 — decide action"

if [[ "$CURRENT_IP" == "$TARGET_IP" ]]; then
    echo "    NO-OP: registered IP already matches target. Nothing to do."
    if [[ "$ORDERS_ALLOWED" == "true" ]]; then
        echo "    SUCCESS: ordersAllowed=true. Trading can proceed."
        exit 0
    else
        die "registered IP matches but ordersAllowed=false. Contact Dhan support." 4
    fi
fi

if [[ -z "$CURRENT_IP" ]]; then
    HTTP_METHOD="POST"
    ENDPOINT_PATH="/ip/setIP"
    ACTION="first-time-register"
    echo "    DECISION: $ACTION via POST /v2/ip/setIP (no cooldown applies)"
else
    HTTP_METHOD="PUT"
    ENDPOINT_PATH="/ip/modifyIP"
    ACTION="modify-registered-ip"
    DAYS_SINCE=$(days_since_iso_date "$MODIFY_DATE_PRIMARY")
    echo "    DECISION: $ACTION via PUT /v2/ip/modifyIP"
    echo "    WARNING:  current IP $CURRENT_IP is registered."
    echo "    WARNING:  last primary modify was $DAYS_SINCE days ago."
    echo "    WARNING:  Dhan enforces a 7-day cooldown after any modifyIP call."
    if [[ "$DAYS_SINCE" -lt 7 ]]; then
        if [[ "$ALLOW_MODIFY" == "false" ]]; then
            die "cooldown breach refused — last modify was $DAYS_SINCE days ago (< 7). Pass --modify ONLY if you have read .claude/rules/dhan/authentication.md rule 7 and accept the 7-day lockout risk." 3
        fi
        echo "    OPERATOR ACK (via --modify): proceeding inside cooldown window."
    fi
    if [[ "$ALLOW_MODIFY" == "false" ]]; then
        die "modifyIP requires explicit --modify acknowledgement (7-day cooldown is destructive)." 3
    fi
fi

# ---------------------------------------------------------------------------
# Step 5 — Execute or preview
# ---------------------------------------------------------------------------
REQUEST_BODY=$(jq -nc \
    --arg dhanClientId "$CLIENT_ID" \
    --arg ip           "$TARGET_IP" \
    --arg ipFlag       "$IP_FLAG" \
    '{dhanClientId:$dhanClientId, ip:$ip, ipFlag:$ipFlag}')

echo "==> Step 5/6 — $HTTP_METHOD ${DHAN_API_BASE}${ENDPOINT_PATH}"
echo "    body: $REQUEST_BODY"

if [[ "$DRY_RUN" == "true" ]]; then
    echo
    echo "    DRY-RUN — no API call made. Re-run with --apply to execute."
    exit 0
fi

RESPONSE=$(curl -sS -X "$HTTP_METHOD" "${DHAN_API_BASE}${ENDPOINT_PATH}" \
    -H "access-token: ${ACCESS_TOKEN}" \
    -H "Content-Type: application/json" \
    -d "$REQUEST_BODY") \
    || die "$HTTP_METHOD ${ENDPOINT_PATH} failed" 2

echo "    response: $RESPONSE"

# ---------------------------------------------------------------------------
# Step 6 — Post-action verify + audit log
# ---------------------------------------------------------------------------
echo "==> Step 6/6 — verify + audit"

# Give Dhan a moment to propagate (observed in field as ~3s for setIP,
# can take 30s+ for modifyIP).
sleep 5

VERIFY_JSON=$(curl -sS -X GET "${DHAN_API_BASE}/ip/getIP" \
    -H "access-token: ${ACCESS_TOKEN}" \
    -H "Accept: application/json") \
    || die "post-action GET /v2/ip/getIP failed" 2

VERIFY_IP=$(echo "$VERIFY_JSON" | jq -r '.ip // ""')
VERIFY_ALLOWED=$(echo "$VERIFY_JSON" | jq -r '.ordersAllowed // false')

# Append audit row regardless of outcome.
mkdir -p "$(dirname "$AUDIT_LOG")"
{
    printf '%s\taction=%s\tmethod=%s\ttarget_ip=%s\tip_flag=%s\tverified_ip=%s\torders_allowed=%s\n' \
        "$(date -u +%FT%TZ)" \
        "$ACTION" \
        "$HTTP_METHOD" \
        "$TARGET_IP" \
        "$IP_FLAG" \
        "${VERIFY_IP:-<empty>}" \
        "$VERIFY_ALLOWED"
} >> "$AUDIT_LOG"

if [[ "$VERIFY_IP" != "$TARGET_IP" ]]; then
    die "post-action verify FAILED: registered IP is '$VERIFY_IP', expected '$TARGET_IP'. Dhan may be slow to propagate; re-run GET in 30s." 4
fi

if [[ "$VERIFY_ALLOWED" != "true" ]]; then
    die "post-action verify FAILED: registered IP matches but ordersAllowed=false. Check account status with Dhan support." 4
fi

echo
echo "SUCCESS — IP $TARGET_IP registered as $IP_FLAG, ordersAllowed=true."
echo "Audit row appended to $AUDIT_LOG"
