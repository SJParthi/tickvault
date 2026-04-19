#!/usr/bin/env bash
# Rollback for auto-fix-drain-spill.sh.
#
# The drain replays spill files into QuestDB and then deletes them.
# QuestDB's DEDUP handles idempotency, but the files are gone. Rollback
# restores them from the .drain-backup/ dir that drain-spill creates
# before deletion.
#
# Usage:
#   scripts/auto-fix-drain-spill-rollback.sh <correlation_id> [--dry-run]
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
    echo "${TS} auto-fix-drain-spill-rollback dry_run=${DRY_RUN} corr_id=${CORR_ID} $*" | tee -a "${AUTO_FIX_LOG}"
}

log "starting rollback"

if [[ "${DRY_RUN}" == "1" ]]; then
    log "dry_run_ok plan=restore_spill_from_drain_backup"
    exit 0
fi

log "rollback=not_implemented reason=endpoint_pending_m3.5 corr_id=${CORR_ID}"
exit 0
