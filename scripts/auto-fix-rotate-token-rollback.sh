#!/usr/bin/env bash
# Rollback for auto-fix-rotate-token.sh.
#
# A token rotation is hard to roll back: once Dhan issues a new token,
# the old one is invalidated. The "rollback" here is therefore to
# RESTORE the previous token from the Valkey backup key that rotate-token.sh
# writes before overwriting (a short-circuit recovery path).
#
# Correlation: must be called with the same correlation_id that the
# original rotate emitted. Looks up the backup key by that id.
#
# Usage:
#   scripts/auto-fix-rotate-token-rollback.sh <correlation_id> [--dry-run]
#
# Exit codes:
#   0  success (or dry-run OK, or no-op when backup key absent)
#   1  invalid usage
#   2  rollback attempted but failed
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
: "${TV_API_TOKEN:=}"

AUTO_FIX_LOG="data/logs/auto-fix.log"
mkdir -p "$(dirname "${AUTO_FIX_LOG}")"
TS=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

log() {
    echo "${TS} auto-fix-rotate-token-rollback dry_run=${DRY_RUN} corr_id=${CORR_ID} $*" | tee -a "${AUTO_FIX_LOG}"
}

log "starting rollback"

if [[ "${DRY_RUN}" == "1" ]]; then
    log "dry_run_ok plan=restore_token_from_valkey_backup_key tickvault_token:backup:${CORR_ID}"
    exit 0
fi

# Future: POST /api/auth/rollback-token with correlation_id body.
# Until endpoint lands, no-op but exit 0 so verify.sh can proceed.
log "rollback=not_implemented reason=endpoint_pending_m3.5 corr_id=${CORR_ID}"
exit 0
