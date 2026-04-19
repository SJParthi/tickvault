#!/usr/bin/env bash
# M4 rollback dispatcher: given a fix name + correlation_id, invoke the
# matching *-rollback.sh script.
#
# Called by Claude's /loop runbook when verify.sh returned non-zero.
#
# Usage:
#   scripts/triage/rollback.sh <fix_name> <correlation_id> [--dry-run]
#
# Example:
#   scripts/triage/rollback.sh auto-fix-rotate-token rotate-token-1234-567
#
# The dispatcher looks up scripts/<fix_name>-rollback.sh and runs it.
# Every M3 auto-fix script MUST have a matching *-rollback.sh pair
# (enforced by crates/common/tests/autonomous_ops_m4_guard.rs).
#
# Exit codes:
#   0   rollback completed (or dry-run ok)
#   1   usage error / rollback script missing
#   2   rollback script exited non-zero
set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "usage: $0 <fix_name> <correlation_id> [--dry-run]" >&2
    exit 1
fi

FIX_NAME="$1"
CORR_ID="$2"
DRY_RUN_ARG=""
if [[ "${3:-}" == "--dry-run" ]]; then
    DRY_RUN_ARG="--dry-run"
fi

AUTO_FIX_LOG="data/logs/auto-fix.log"
mkdir -p "$(dirname "${AUTO_FIX_LOG}")"
TS=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

log() {
    echo "${TS} triage-rollback corr_id=${CORR_ID} $*" | tee -a "${AUTO_FIX_LOG}"
}

ROLLBACK_SCRIPT="scripts/${FIX_NAME}-rollback.sh"

if [[ ! -x "${ROLLBACK_SCRIPT}" ]]; then
    log "error=rollback_script_missing path=${ROLLBACK_SCRIPT}"
    exit 1
fi

log "invoking=${ROLLBACK_SCRIPT} corr_id=${CORR_ID} dry_run=${DRY_RUN_ARG:-0}"

# shellcheck disable=SC2086
if "${ROLLBACK_SCRIPT}" "${CORR_ID}" ${DRY_RUN_ARG}; then
    log "rollback=ok"
    exit 0
else
    rc=$?
    log "rollback=failed exit=${rc}"
    exit 2
fi
