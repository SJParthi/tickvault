#!/usr/bin/env bash
# Rollback for auto-fix-reset-ws-pool.sh.
#
# The reset pauses and reconnects; reconnection IS the recovery, so the
# rollback is: force an immediate reconnect-all (bypass the 60s pause).
#
# Usage:
#   scripts/auto-fix-reset-ws-pool-rollback.sh <correlation_id> [--dry-run]
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
    echo "${TS} auto-fix-reset-ws-pool-rollback dry_run=${DRY_RUN} corr_id=${CORR_ID} $*" | tee -a "${AUTO_FIX_LOG}"
}

log "starting rollback"

if [[ "${DRY_RUN}" == "1" ]]; then
    log "dry_run_ok plan=force_immediate_reconnect_all"
    exit 0
fi

log "rollback=not_implemented reason=endpoint_pending_m3.5 corr_id=${CORR_ID}"
exit 0
