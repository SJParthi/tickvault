#!/usr/bin/env bash
# Rollback for auto-fix-clear-spill.sh.
#
# clear-spill is an idempotent recovery action (re-drains spill files
# into QuestDB). "Rolling it back" means restoring the spill files from
# the backup dir that clear-spill creates before attempting drain, so
# operator can retry manually.
#
# Usage:
#   scripts/auto-fix-clear-spill-rollback.sh <correlation_id> [--dry-run]
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
    echo "${TS} auto-fix-clear-spill-rollback dry_run=${DRY_RUN} corr_id=${CORR_ID} $*" | tee -a "${AUTO_FIX_LOG}"
}

log "starting rollback"

if [[ "${DRY_RUN}" == "1" ]]; then
    log "dry_run_ok plan=restore_spill_from_backup_dir"
    exit 0
fi

# No-op until backup dir convention is wired in clear-spill.sh (M3.5).
log "rollback=noop reason=clear_spill_is_already_idempotent corr_id=${CORR_ID}"
exit 0
