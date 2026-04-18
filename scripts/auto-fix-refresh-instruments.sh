#!/usr/bin/env bash
# Phase 6 of active-plan.md — auto-fix script: rebuild instrument master.
#
# Triage rule `data-813-invalid-security-id-rebuild` invokes this when
# the summary writer sees recurring DATA-813 (invalid SecurityId)
# events, indicating a stale instrument master CSV.
#
# Operation: POST /api/instruments/rebuild, which is already exposed
# and idempotent. It re-downloads the Dhan CSV, parses, filters, and
# atomically swaps the InstrumentRegistry.
#
# Usage:
#   scripts/auto-fix-refresh-instruments.sh [--dry-run]
#
# Exit codes:
#   0  success (or dry-run OK)
#   1  pre-flight failed
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
    echo "${TS} auto-fix-refresh-instruments dry_run=${DRY_RUN} $*" \
        | tee -a "${AUTO_FIX_LOG}"
}

log "starting"

if ! curl -fsS -m 5 "${TV_API_URL}/health" > /dev/null 2>&1; then
    log "error=api_unreachable url=${TV_API_URL}"
    exit 1
fi

if [[ "${DRY_RUN}" == "1" ]]; then
    log "dry_run_ok"
    exit 0
fi

AUTH_ARG=()
if [[ -n "${TV_API_TOKEN}" ]]; then
    AUTH_ARG=(-H "Authorization: Bearer ${TV_API_TOKEN}")
fi

HTTP_STATUS=$(curl -fsS -m 60 -o /tmp/rebuild-response.json -w "%{http_code}" \
    -X POST "${TV_API_URL}/api/instruments/rebuild" \
    "${AUTH_ARG[@]}" || echo "000")

if [[ "${HTTP_STATUS}" =~ ^2 ]]; then
    log "fix=ok http_status=${HTTP_STATUS}"
    exit 0
else
    log "fix=failed http_status=${HTTP_STATUS} response=$(head -c 200 /tmp/rebuild-response.json 2>/dev/null || echo '')"
    exit 2
fi
