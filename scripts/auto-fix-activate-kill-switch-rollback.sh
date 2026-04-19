#!/usr/bin/env bash
# Rollback for auto-fix-activate-kill-switch.sh.
#
# Deactivates the kill switch. Trading resumes only after the operator
# manually re-enables orders — rollback here just clears the Dhan-side
# block, not the operator hold.
#
# Usage:
#   scripts/auto-fix-activate-kill-switch-rollback.sh <correlation_id> [--dry-run]
set -euo pipefail

if [[ $# -lt 1 ]]; then
    echo "usage: $0 <correlation_id> [--dry-run]" >&2
    exit 1
fi

CORR_ID="$1"
DRY_RUN=0
if [[ "${2:-}" == "--dry-run" ]]; then
    DRY_RUN=1
fi

: "${TV_API_URL:=http://127.0.0.1:3001}"

AUTO_FIX_LOG="data/logs/auto-fix.log"
mkdir -p "$(dirname "${AUTO_FIX_LOG}")"
TS=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

log() {
    echo "${TS} auto-fix-activate-kill-switch-rollback dry_run=${DRY_RUN} corr_id=${CORR_ID} $*" | tee -a "${AUTO_FIX_LOG}"
}

log "starting rollback"

if [[ "${DRY_RUN}" == "1" ]]; then
    log "dry_run_ok plan=POST_v2_killswitch_DEACTIVATE"
    exit 0
fi

log "rollback=not_implemented reason=endpoint_pending_m3.5 corr_id=${CORR_ID}"
exit 0
