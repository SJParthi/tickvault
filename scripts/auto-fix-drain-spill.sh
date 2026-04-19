#!/usr/bin/env bash
# Auto-fix script: drain QuestDB spill files.
#
# Extends the existing clear-spill script to cover all 3 spill types
# (tick, candle, depth) instead of just ticks.
#
# Triggered by rules:
#   - tick-spill-stuck-clear (STORAGE-GAP-01 tick spill)
#   - [future] candle-spill-stuck-clear
#   - [future] depth-spill-stuck-clear
#
# Operation: calls POST /api/spill/drain (endpoint pending — M3.5) which:
#   1. Iterates data/spill/{ticks,candles,depth}/*.bin
#   2. For each: re-opens QuestDB ILP writer, replays, deletes on success
#   3. Returns count drained + count skipped (corrupt / in-progress)
#
# Usage:  scripts/auto-fix-drain-spill.sh [--dry-run]
#
# Safety:
#   - Idempotent — replays spill files in-order, skips duplicates via
#     QuestDB DEDUP.
#   - If QuestDB is down, returns 1 immediately (pre-flight check).
#   - Cooldown 600s between runs.
#
# Rollback: see scripts/auto-fix-drain-spill-rollback.sh
#   (Rollback = restore spill files from .drain-backup/ dir.)
#
# Exit codes:
#   0  success
#   1  pre-flight failed (QuestDB unreachable)
#   2  drain attempted, some files failed (escalate)
#   3  cooldown
set -euo pipefail

DRY_RUN=0
if [[ "${1:-}" == "--dry-run" ]]; then
    DRY_RUN=1
fi

: "${TV_API_URL:=http://127.0.0.1:3001}"
: "${TV_QUESTDB_URL:=http://127.0.0.1:9000}"

AUTO_FIX_LOG="data/logs/auto-fix.log"
DRAIN_LOCK="data/logs/.drain-spill.lock"
DRAIN_COOLDOWN_SECS=600

mkdir -p "$(dirname "${AUTO_FIX_LOG}")"
TS=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
CORR_ID="drain-spill-$(date -u +%s)-$$"

log() {
    echo "${TS} auto-fix-drain-spill dry_run=${DRY_RUN} corr_id=${CORR_ID} $*" | tee -a "${AUTO_FIX_LOG}"
}

log "starting"

# Cooldown
if [[ -f "${DRAIN_LOCK}" ]]; then
    last_ts=$(stat -c %Y "${DRAIN_LOCK}" 2>/dev/null || stat -f %m "${DRAIN_LOCK}" 2>/dev/null || echo 0)
    now_ts=$(date -u +%s)
    elapsed=$((now_ts - last_ts))
    if (( elapsed < DRAIN_COOLDOWN_SECS )); then
        log "error=cooldown elapsed=${elapsed}s cooldown=${DRAIN_COOLDOWN_SECS}s"
        exit 3
    fi
fi

# Pre-flight: QuestDB reachable?
if ! curl -fsS -m 5 "${TV_QUESTDB_URL}/" > /dev/null 2>&1; then
    log "error=questdb_unreachable url=${TV_QUESTDB_URL}"
    exit 1
fi

# Count spill files so dry-run reports real numbers
tick_count=0
candle_count=0
depth_count=0
for kind in ticks candles depth; do
    dir="data/spill/${kind}"
    if [[ -d "${dir}" ]]; then
        n=$(find "${dir}" -maxdepth 1 -type f -name "*.bin" 2>/dev/null | wc -l)
        case "${kind}" in
            ticks)   tick_count=${n} ;;
            candles) candle_count=${n} ;;
            depth)   depth_count=${n} ;;
        esac
    fi
done

if [[ "${DRY_RUN}" == "1" ]]; then
    log "dry_run_ok ticks=${tick_count} candles=${candle_count} depth=${depth_count}"
    exit 0
fi

# Future: POST /api/spill/drain. Scaffold returns 2.
log "fix=not_implemented reason=endpoint_pending_m3.5 ticks=${tick_count} candles=${candle_count} depth=${depth_count} corr_id=${CORR_ID}"
exit 2
