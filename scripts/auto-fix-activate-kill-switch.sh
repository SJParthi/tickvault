#!/usr/bin/env bash
# Auto-fix script: activate Dhan kill switch.
#
# Triggered by rules (upgrade path from escalate):
#   - oms-gap-06-dry-run-safety (OMS-GAP-06, Critical — dry_run violation)
#   - risk-gap-01-pretrade      (RISK-GAP-01, High — halt conditions)
#   - risk-gap-02-position-pnl  (RISK-GAP-02, High — P&L breach)
#
# Operation:
#   1. Close all open positions FIRST (Dhan requires no open positions
#      to activate kill switch — per .claude/rules/dhan/traders-control.md)
#   2. Call POST /v2/killswitch?killSwitchStatus=ACTIVATE
#   3. Verify via GET /v2/killswitch
#   4. Telegram CRITICAL alert
#
# Safety:
#   - This is a one-way door: kill switch blocks ALL trading for the day.
#     Once activated, rollback requires manual operator action.
#   - Never runs in dry_run=true mode of the app itself (belt-and-braces).
#   - Cooldown: 1h between activations (if it keeps re-firing, operator
#     intervention is already needed).
#
# Rollback: see scripts/auto-fix-activate-kill-switch-rollback.sh
#   (Rollback = deactivate kill switch via POST /v2/killswitch?killSwitchStatus=DEACTIVATE.
#    Operator must still manually resume trading.)
#
# Exit codes:
#   0  success (or dry-run OK)
#   1  pre-flight failed (open positions, API unreachable, etc.)
#   2  activation returned non-2xx
#   3  cooldown / already-activated
set -euo pipefail

DRY_RUN=0
if [[ "${1:-}" == "--dry-run" ]]; then
    DRY_RUN=1
fi

: "${TV_API_URL:=http://127.0.0.1:3001}"
: "${TV_API_TOKEN:=}"

AUTO_FIX_LOG="data/logs/auto-fix.log"
KS_LOCK="data/logs/.kill-switch.lock"
KS_COOLDOWN_SECS=3600

mkdir -p "$(dirname "${AUTO_FIX_LOG}")"
TS=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
CORR_ID="kill-switch-$(date -u +%s)-$$"

log() {
    echo "${TS} auto-fix-activate-kill-switch dry_run=${DRY_RUN} corr_id=${CORR_ID} $*" | tee -a "${AUTO_FIX_LOG}"
}

log "starting"

# Cooldown
if [[ -f "${KS_LOCK}" ]]; then
    last_ts=$(stat -c %Y "${KS_LOCK}" 2>/dev/null || stat -f %m "${KS_LOCK}" 2>/dev/null || echo 0)
    now_ts=$(date -u +%s)
    elapsed=$((now_ts - last_ts))
    if (( elapsed < KS_COOLDOWN_SECS )); then
        log "error=cooldown elapsed=${elapsed}s cooldown=${KS_COOLDOWN_SECS}s"
        exit 3
    fi
fi

if ! curl -fsS -m 5 "${TV_API_URL}/health" > /dev/null 2>&1; then
    log "error=api_unreachable url=${TV_API_URL}"
    exit 1
fi

if [[ "${DRY_RUN}" == "1" ]]; then
    log "dry_run_ok plan=exit_positions_then_POST_v2_killswitch_ACTIVATE"
    exit 0
fi

# Future: POST /api/kill-switch/activate (tickvault wrapper that first
# DELETE /v2/positions then POST /v2/killswitch?killSwitchStatus=ACTIVATE).
# Scaffold returns 2 so triage escalates to operator until the wrapper exists.
log "fix=not_implemented reason=endpoint_pending_m3.5 corr_id=${CORR_ID}"
exit 2
