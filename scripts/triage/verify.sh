#!/usr/bin/env bash
# M4 verify harness: confirm an auto-fix actually cleared the symptom.
#
# Called by Claude's /loop runbook after any auto_fix/auto_restart rule
# runs. Polls Prometheus (via the MCP endpoint resolution) for up to
# TIMEOUT_SECS, re-evaluating the PROMQL predicate every 5s. If the
# predicate is ever "clear" (returns 0), emits pass and exits 0. If
# timeout reached without ever clearing, exits 1 so the /loop runbook
# triggers rollback.
#
# Usage:
#   scripts/triage/verify.sh <correlation_id> <promql_predicate> [timeout_secs]
#
# Example:
#   scripts/triage/verify.sh drain-spill-17654-1234 \
#     'sum(tv_ticks_spilled_files_total) == 0' 120
#
# Predicate semantics:
#   - Query Prometheus /api/v1/query with the predicate
#   - Parse the vector result
#   - If ANY sample has value==1 (predicate evaluates true), declare "clear"
#   - If predicate is comparison-free (raw metric), declare "clear" when value==0
#
# Endpoint resolution: reads TICKVAULT_PROMETHEUS_URL, falls back to
# 127.0.0.1:9090. (Future: read config/claude-mcp-endpoints.toml via
# a Python helper; for now env-var-only to keep the harness pure bash.)
#
# Exit codes:
#   0  verified clear within timeout
#   1  predicate never cleared
#   2  usage / pre-flight error
set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "usage: $0 <correlation_id> <promql_predicate> [timeout_secs]" >&2
    exit 2
fi

CORR_ID="$1"
PROMQL="$2"
TIMEOUT_SECS="${3:-120}"
POLL_INTERVAL_SECS=5

: "${TICKVAULT_PROMETHEUS_URL:=http://127.0.0.1:9090}"
AUTO_FIX_LOG="data/logs/auto-fix.log"
mkdir -p "$(dirname "${AUTO_FIX_LOG}")"

log() {
    local ts
    ts=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    echo "${ts} triage-verify corr_id=${CORR_ID} $*" | tee -a "${AUTO_FIX_LOG}"
}

log "starting predicate=${PROMQL} timeout=${TIMEOUT_SECS}s"

# Pre-flight: Prometheus reachable?
if ! curl -fsS -m 5 "${TICKVAULT_PROMETHEUS_URL}/-/healthy" > /dev/null 2>&1; then
    log "error=prometheus_unreachable url=${TICKVAULT_PROMETHEUS_URL}"
    exit 2
fi

deadline=$(( $(date -u +%s) + TIMEOUT_SECS ))
poll=0

while (( $(date -u +%s) < deadline )); do
    poll=$(( poll + 1 ))
    # URL-encode the PromQL (minimal — only spaces)
    encoded=$(printf '%s' "${PROMQL}" | sed 's/ /%20/g')
    response=$(curl -sS -m 5 "${TICKVAULT_PROMETHEUS_URL}/api/v1/query?query=${encoded}" 2>/dev/null || echo '{}')

    # Parse: extract the first sample value. If status != success or
    # no samples, treat as "not clear yet".
    status=$(printf '%s' "${response}" | grep -o '"status":"[^"]*"' | head -1 | cut -d'"' -f4)
    if [[ "${status}" != "success" ]]; then
        log "poll=${poll} status=${status:-unknown} retry_in=${POLL_INTERVAL_SECS}s"
        sleep "${POLL_INTERVAL_SECS}"
        continue
    fi

    # Extract first value. Prometheus result format:
    # {"status":"success","data":{"resultType":"vector","result":[{"metric":{...},"value":[<ts>,"<val>"]}]}}
    first_value=$(printf '%s' "${response}" | grep -oE '"value":\[[^]]*\]' | head -1 | grep -oE '"[0-9.e+-]+"' | tail -1 | tr -d '"')

    if [[ -z "${first_value}" ]]; then
        log "poll=${poll} result=empty_vector"
    else
        log "poll=${poll} value=${first_value}"
        # Comparison PromQL returns 1 when true, metric returns its raw value.
        # Accept "1" (true) OR "0" (metric=0, i.e. symptom cleared).
        if [[ "${first_value}" == "1" || "${first_value}" == "0" ]]; then
            log "result=clear value=${first_value} polls=${poll} elapsed=$(($(date -u +%s) - (deadline - TIMEOUT_SECS)))s"
            exit 0
        fi
    fi
    sleep "${POLL_INTERVAL_SECS}"
done

log "result=timeout polls=${poll} elapsed=${TIMEOUT_SECS}s predicate=${PROMQL}"
exit 1
