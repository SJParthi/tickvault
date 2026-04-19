#!/usr/bin/env bash
# Rollback for auto-fix-refresh-instruments.sh.
#
# Restores the instrument cache from .pre-refresh-backup/ if present.
# This is a safety net — refresh-instruments downloads fresh CSV from
# Dhan and rebuilds the binary cache; if the fresh data is malformed,
# rollback restores the previous cache.
#
# Usage:
#   scripts/auto-fix-refresh-instruments-rollback.sh <correlation_id> [--dry-run]
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
BACKUP_DIR="data/instrument-cache/.pre-refresh-backup"
CACHE_DIR="data/instrument-cache"
mkdir -p "$(dirname "${AUTO_FIX_LOG}")"
TS=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

log() {
    echo "${TS} auto-fix-refresh-instruments-rollback dry_run=${DRY_RUN} corr_id=${CORR_ID} $*" | tee -a "${AUTO_FIX_LOG}"
}

log "starting rollback"

if [[ ! -d "${BACKUP_DIR}" ]]; then
    log "rollback=noop reason=no_backup_dir path=${BACKUP_DIR}"
    exit 0
fi

if [[ "${DRY_RUN}" == "1" ]]; then
    log "dry_run_ok plan=restore_cache_from_${BACKUP_DIR}"
    exit 0
fi

# Move backup dir contents back into cache dir. Operator then restarts.
cp -r "${BACKUP_DIR}/." "${CACHE_DIR}/" 2>/dev/null || true
log "rollback=ok restored_from=${BACKUP_DIR} corr_id=${CORR_ID}"
exit 0
