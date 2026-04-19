#!/usr/bin/env bash
# Rollback for auto-fix-restart-depth.sh.
#
# restart-depth is already idempotent (re-subscribes the depth feed
# without disconnecting the main feed). The rollback is a no-op —
# the restart IS the recovery; there is nothing to revert.
#
# Provided to satisfy the M4 contract that every auto-fix-*.sh has a
# matching *-rollback.sh.
#
# Usage:
#   scripts/auto-fix-restart-depth-rollback.sh <correlation_id> [--dry-run]
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

AUTO_FIX_LOG="data/logs/auto-fix.log"
mkdir -p "$(dirname "${AUTO_FIX_LOG}")"
TS=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

log() {
    echo "${TS} auto-fix-restart-depth-rollback dry_run=${DRY_RUN} corr_id=${CORR_ID} $*" | tee -a "${AUTO_FIX_LOG}"
}

log "starting rollback"

if [[ "${DRY_RUN}" == "1" ]]; then
    log "dry_run_ok plan=noop_restart_is_already_recovery"
    exit 0
fi

log "rollback=noop reason=restart_is_idempotent_recovery corr_id=${CORR_ID}"
exit 0
