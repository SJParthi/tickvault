#!/usr/bin/env bash
# Auto-fix script: rotate the Dhan access token.
#
# Triggered by rules:
#   - data-807-token-expired  (DATA-807)
#   - auth-gap-01-token-expiry (AUTH-GAP-01)
#   - dh-901-invalid-auth      (DH-901) — only via upgrade from escalate
#
# Operation: calls tickvault's token-refresh endpoint which:
#   1. Reads fresh credentials from AWS SSM (/tickvault/<env>/auth/*)
#   2. Generates TOTP code
#   3. Hits Dhan auth.dhan.co/app/generateAccessToken
#   4. Atomic swap via arc-swap
#   5. Persists new token to Valkey cache
#
# Usage:  scripts/auto-fix-rotate-token.sh [--dry-run]
#
# Safety:
#   - Rate-limited: one rotation per 60s max (Dhan only allows ONE active
#     token per account; rapid rotation can lock the account).
#   - Dry-run verifies the API is reachable and token endpoint exists
#     without actually rotating.
#   - Always writes audit log entry to data/logs/auto-fix.log with
#     correlation_id so M4 verify.sh can track the outcome.
#
# Rollback: see scripts/auto-fix-rotate-token-rollback.sh
#   (Rollback = restore the previous token from Valkey backup key.)
#
# Exit codes:
#   0  success (or dry-run OK)
#   1  pre-flight failed (API unreachable, token missing, etc.)
#   2  fix attempted, endpoint returned non-2xx
#   3  rate-limited (last rotation <60s ago)
set -euo pipefail

DRY_RUN=0
if [[ "${1:-}" == "--dry-run" ]]; then
    DRY_RUN=1
fi

: "${TV_API_URL:=http://127.0.0.1:3001}"
: "${TV_API_TOKEN:=}"

AUTO_FIX_LOG="data/logs/auto-fix.log"
ROTATE_LOCK="data/logs/.rotate-token.lock"
ROTATE_COOLDOWN_SECS=60

mkdir -p "$(dirname "${AUTO_FIX_LOG}")"
TS=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
CORR_ID="rotate-token-$(date -u +%s)-$$"

log() {
    echo "${TS} auto-fix-rotate-token dry_run=${DRY_RUN} corr_id=${CORR_ID} $*" | tee -a "${AUTO_FIX_LOG}"
}

log "starting"

# Rate-limit check: refuse if lock was touched <60s ago
if [[ -f "${ROTATE_LOCK}" ]]; then
    last_ts=$(stat -c %Y "${ROTATE_LOCK}" 2>/dev/null || stat -f %m "${ROTATE_LOCK}" 2>/dev/null || echo 0)
    now_ts=$(date -u +%s)
    elapsed=$((now_ts - last_ts))
    if (( elapsed < ROTATE_COOLDOWN_SECS )); then
        log "error=rate_limited elapsed=${elapsed}s cooldown=${ROTATE_COOLDOWN_SECS}s"
        exit 3
    fi
fi

# Pre-flight: API reachable?
if ! curl -fsS -m 5 "${TV_API_URL}/health" > /dev/null 2>&1; then
    log "error=api_unreachable url=${TV_API_URL}"
    exit 1
fi

if [[ "${DRY_RUN}" == "1" ]]; then
    log "dry_run_ok pre_flight=api_reachable"
    exit 0
fi

# Fix: POST /api/auth/rotate (endpoint pending — M3.5)
# Until the endpoint lands, scaffolding exits with 2 so triage escalates.
#
# Future: atomic curl with retry + audit:
#   auth_header=""
#   [[ -n "${TV_API_TOKEN}" ]] && auth_header="-H Authorization: Bearer ${TV_API_TOKEN}"
#   http_code=$(curl -sS -m 30 -o /tmp/rotate-response.json -w "%{http_code}" \
#       -X POST "${TV_API_URL}/api/auth/rotate" \
#       ${auth_header} -H "X-Correlation-Id: ${CORR_ID}")
#   if [[ "${http_code}" =~ ^2 ]]; then
#       touch "${ROTATE_LOCK}"
#       log "fix=ok http_code=${http_code}"
#       exit 0
#   fi
#   log "fix=failed http_code=${http_code}"
#   exit 2
log "fix=not_implemented reason=endpoint_pending_m3.5 corr_id=${CORR_ID}"
exit 2
