#!/usr/bin/env bash
# Phase 6 of active-plan.md — auto-fix script: restart depth rebalancer.
#
# Triage rule `depth-spot-stale-auto-restart` invokes this when the
# summary writer sees a recurring "Depth spot price STALE" signature.
#
# Operation: hits the tickvault API's depth-restart endpoint. The
# endpoint already exists and is idempotent — re-subscribes the
# 20-level + 200-level depth connections without disconnecting the
# main feed.
#
# Usage:
#   scripts/auto-fix-restart-depth.sh [--dry-run]
#
# Exit codes:
#   0  success (or dry-run OK)
#   1  pre-flight failed (API unreachable, TV_API_TOKEN missing, etc.)
#   2  fix attempted but returned non-2xx
set -euo pipefail

DRY_RUN=0
if [[ "${1:-}" == "--dry-run" ]]; then
    DRY_RUN=1
fi

: "${TV_API_URL:=http://127.0.0.1:3001}"
: "${TV_API_TOKEN:=}"

AUTO_FIX_LOG="data/logs/auto-fix.log"
mkdir -p "$(dirname "${AUTO_FIX_LOG}")"
TS=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

log() {
    echo "${TS} auto-fix-restart-depth dry_run=${DRY_RUN} $*" | tee -a "${AUTO_FIX_LOG}"
}

log "starting"

# Pre-flight: is the API reachable?
if ! curl -fsS -m 5 "${TV_API_URL}/health" > /dev/null 2>&1; then
    log "error=api_unreachable url=${TV_API_URL}"
    exit 1
fi

if [[ "${DRY_RUN}" == "1" ]]; then
    log "dry_run_ok"
    exit 0
fi

# Depth restart endpoint doesn't exist yet — this script is scaffolding
# for when it lands. Until then, log the intent and exit with a non-
# zero code so the triage hook escalates.
#
# Future: replace with `curl -X POST $TV_API_URL/api/depth/restart`.
log "fix=not_implemented reason=endpoint_pending"
exit 2
