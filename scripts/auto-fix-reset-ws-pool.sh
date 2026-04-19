#!/usr/bin/env bash
# Auto-fix script: reset the WebSocket connection pool.
#
# Triggered by rules:
#   - data-805-too-many-connections  (DATA-805, Critical)
#   - ws-gap-01-disconnect-classification  (upgrade from silence)
#
# Operation:
#   1. Pause all WS connections for 60s (Dhan DATA-805 recovery protocol)
#   2. Audit: count active connections via Prometheus query
#   3. Reconnect one at a time, respecting the 5-connection cap
#
# Usage:  scripts/auto-fix-reset-ws-pool.sh [--dry-run]
#
# Safety:
#   - Cooldown 300s between runs (prevents resubscribe storm)
#   - Market-hours gate: refuses to run inside 09:15-15:30 IST without
#     explicit override (TV_WS_RESET_DURING_MARKET=1). Market-hours
#     reset drops ticks for 60s — operator must approve.
#
# Rollback: see scripts/auto-fix-reset-ws-pool-rollback.sh
#   (Rollback = no-op; reconnection is the rollback.)
#
# Exit codes:
#   0  success (or dry-run OK)
#   1  pre-flight failed
#   2  fix attempted, endpoint returned non-2xx
#   3  rate-limited OR blocked by market-hours gate
set -euo pipefail

DRY_RUN=0
if [[ "${1:-}" == "--dry-run" ]]; then
    DRY_RUN=1
fi

: "${TV_API_URL:=http://127.0.0.1:3001}"
: "${TV_API_TOKEN:=}"
: "${TV_WS_RESET_DURING_MARKET:=0}"

AUTO_FIX_LOG="data/logs/auto-fix.log"
RESET_LOCK="data/logs/.reset-ws-pool.lock"
RESET_COOLDOWN_SECS=300

mkdir -p "$(dirname "${AUTO_FIX_LOG}")"
TS=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
CORR_ID="reset-ws-pool-$(date -u +%s)-$$"

log() {
    echo "${TS} auto-fix-reset-ws-pool dry_run=${DRY_RUN} corr_id=${CORR_ID} $*" | tee -a "${AUTO_FIX_LOG}"
}

log "starting"

# Cooldown check
if [[ -f "${RESET_LOCK}" ]]; then
    last_ts=$(stat -c %Y "${RESET_LOCK}" 2>/dev/null || stat -f %m "${RESET_LOCK}" 2>/dev/null || echo 0)
    now_ts=$(date -u +%s)
    elapsed=$((now_ts - last_ts))
    if (( elapsed < RESET_COOLDOWN_SECS )); then
        log "error=rate_limited elapsed=${elapsed}s cooldown=${RESET_COOLDOWN_SECS}s"
        exit 3
    fi
fi

# Market-hours gate: IST minute-of-day = (UTC minute + 330) mod 1440
utc_min=$((10#$(date -u +%H) * 60 + 10#$(date -u +%M)))
ist_min=$(( (utc_min + 330) % 1440 ))
market_open=$(( 9 * 60 + 15 ))
market_close=$(( 15 * 60 + 30 ))
if (( ist_min >= market_open && ist_min < market_close )); then
    if [[ "${TV_WS_RESET_DURING_MARKET}" != "1" ]]; then
        log "error=market_hours_gate ist_min=${ist_min} override=TV_WS_RESET_DURING_MARKET=1"
        exit 3
    fi
    log "warning=market_hours_override ist_min=${ist_min}"
fi

# Pre-flight
if ! curl -fsS -m 5 "${TV_API_URL}/health" > /dev/null 2>&1; then
    log "error=api_unreachable url=${TV_API_URL}"
    exit 1
fi

if [[ "${DRY_RUN}" == "1" ]]; then
    log "dry_run_ok ist_min=${ist_min} plan=pause_60s_audit_reconnect"
    exit 0
fi

# Future: POST /api/ws/reset-pool with 60s pause + staged reconnect.
# Scaffold returns 2 so triage escalates.
log "fix=not_implemented reason=endpoint_pending_m3.5 corr_id=${CORR_ID}"
exit 2
