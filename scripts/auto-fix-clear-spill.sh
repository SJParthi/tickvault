#!/usr/bin/env bash
# Phase 8.1 of active-plan.md — auto-fix script: drain stale tick/candle spill.
#
# When tick_persistence or candle_persistence has spilled to disk
# (`data/spill/ticks-YYYYMMDD.bin`, `data/spill/candles-YYYYMMDD.bin`)
# and QuestDB recovers, the writer auto-drains on the next tick/candle
# write. But if the app was restarted while spill existed and no new
# writes have happened yet, the files sit there.
#
# This script triggers the drain explicitly by POSTing to a
# `/api/spill/drain` endpoint IF available, or by emitting a canary
# tick that forces the writer through its drain path.
#
# Usage:
#   scripts/auto-fix-clear-spill.sh [--dry-run]
#
# Exit codes:
#   0  success (or dry-run OK)
#   1  pre-flight failed (API unreachable)
#   2  drain endpoint missing — scaffold, escalate to operator
set -euo pipefail

DRY_RUN=0
if [[ "${1:-}" == "--dry-run" ]]; then
    DRY_RUN=1
fi

: "${TV_API_URL:=http://127.0.0.1:3001}"
AUTO_FIX_LOG="data/logs/auto-fix.log"
mkdir -p "$(dirname "${AUTO_FIX_LOG}")"
TS=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

log() {
    echo "${TS} auto-fix-clear-spill dry_run=${DRY_RUN} $*" | tee -a "${AUTO_FIX_LOG}"
}

log "starting"

if ! curl -fsS -m 5 "${TV_API_URL}/health" > /dev/null 2>&1; then
    log "error=api_unreachable url=${TV_API_URL}"
    exit 1
fi

# Count spill files so we have a before/after number.
SPILL_COUNT_BEFORE=$(find data/spill -maxdepth 1 -type f 2>/dev/null | wc -l || echo 0)
log "spill_files_before=${SPILL_COUNT_BEFORE}"

if [[ "${DRY_RUN}" == "1" ]]; then
    log "dry_run_ok"
    exit 0
fi

# Drain endpoint doesn't exist yet — scaffold for when it lands.
# Future: POST /api/spill/drain
log "fix=not_implemented reason=drain_endpoint_pending spill_files_before=${SPILL_COUNT_BEFORE}"
exit 2
