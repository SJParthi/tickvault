#!/usr/bin/env bash
# M4 verify harness: confirm an auto-fix actually cleared the symptom.
#
# Called by Claude's /loop runbook after any auto_fix/auto_restart rule
# runs. Polls the app's Prometheus-format /metrics endpoint (the local
# exporter, NOT a Prometheus server) for up to TIMEOUT_SECS, re-evaluating
# the metric predicate every 5s. If the predicate is ever "clear", emits
# pass and exits 0. If timeout reached without ever clearing, exits 1 so
# the /loop runbook triggers rollback.
#
# Usage:
#   scripts/triage/verify.sh <correlation_id> <metric_predicate> [timeout_secs]
#
# Example:
#   scripts/triage/verify.sh drain-spill-17654-1234 \
#     'sum(tv_ticks_spilled_files_total) == 0' 120
#
# Predicate semantics (interim /metrics-scrape evaluator — replaces the
# retired Prometheus PromQL path; #O5 2026-05-30, Prometheus container
# removed in #O3):
#   - Supported forms:
#       sum(METRIC) == N   → clear when the summed value of all series
#                            of METRIC across labels equals N
#       sum(METRIC)        → clear when that sum equals 0
#       METRIC == N        → same, single/aggregated metric vs N
#       METRIC             → clear when its value equals 0
#   - The exporter renders one text line per series:
#       METRIC{labels} <value>   or   METRIC <value>
#     We sum <value> across every series whose name matches METRIC.
#   - Only sum()/raw-metric/`== N` predicates are supported. Richer PromQL
#     (rate(), histogram_quantile(), label selectors) needs a query engine;
#     the full CloudWatch GetMetricData version lands in the AWS phase once
#     CloudWatch exists. Unsupported predicates exit 2 (pre-flight error).
#
# Endpoint resolution: reads TICKVAULT_METRICS_URL, falls back to
# 127.0.0.1:9091/metrics (the exporter port from config/base.toml
# `metrics_port = 9091` / observability.rs).
#
# Exit codes:
#   0  verified clear within timeout
#   1  predicate never cleared
#   2  usage / pre-flight error (incl. unsupported predicate form)
set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "usage: $0 <correlation_id> <metric_predicate> [timeout_secs]" >&2
    exit 2
fi

CORR_ID="$1"
PREDICATE="$2"
TIMEOUT_SECS="${3:-120}"
POLL_INTERVAL_SECS=5

: "${TICKVAULT_METRICS_URL:=http://127.0.0.1:9091/metrics}"
AUTO_FIX_LOG="data/logs/auto-fix.log"
mkdir -p "$(dirname "${AUTO_FIX_LOG}")"

log() {
    local ts
    ts=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    echo "${ts} triage-verify corr_id=${CORR_ID} $*" | tee -a "${AUTO_FIX_LOG}"
}

# Parse the predicate into a bare METRIC name + threshold N.
# Strips an optional sum(...) wrapper and an optional `== N` comparison.
metric_name=$(printf '%s' "${PREDICATE}" \
    | sed -E 's/^[[:space:]]*sum[[:space:]]*\(//; s/\)[[:space:]]*//; s/[[:space:]]*==[[:space:]]*[0-9.]+[[:space:]]*$//' \
    | tr -d '[:space:]')
# `|| true` so a predicate with no `== N` (raw-metric form) doesn't trip
# `set -e`/`pipefail` when grep finds no match — it just defaults to 0.
threshold=$(printf '%s' "${PREDICATE}" | grep -oE '==[[:space:]]*[0-9.]+' | grep -oE '[0-9.]+' | head -1 || true)
threshold="${threshold:-0}"

# Validate the metric name looks like a Prometheus identifier (reject
# unsupported PromQL like rate(...) / quantiles / label selectors).
if [[ ! "${metric_name}" =~ ^[a-zA-Z_:][a-zA-Z0-9_:]*$ ]]; then
    log "error=unsupported_predicate predicate=${PREDICATE} parsed_metric=${metric_name}"
    echo "verify.sh: unsupported predicate '${PREDICATE}' — only sum(METRIC)[== N] / METRIC[== N] are supported by the /metrics-scrape evaluator" >&2
    exit 2
fi

log "starting predicate=${PREDICATE} metric=${metric_name} threshold=${threshold} timeout=${TIMEOUT_SECS}s url=${TICKVAULT_METRICS_URL}"

# Pre-flight: is the /metrics exporter reachable?
if ! curl -fsS -m 5 "${TICKVAULT_METRICS_URL}" > /dev/null 2>&1; then
    log "error=metrics_endpoint_unreachable url=${TICKVAULT_METRICS_URL}"
    exit 2
fi

# Sum every series of metric_name in the exporter's text output.
# Prometheus text format: `name{labels} value` or `name value`.
scrape_sum() {
    curl -fsS -m 5 "${TICKVAULT_METRICS_URL}" 2>/dev/null \
        | awk -v m="${metric_name}" '
            /^#/ { next }                       # skip HELP/TYPE comments
            {
                name = $1
                br = index(name, "{")
                if (br > 0) { name = substr(name, 1, br - 1) }
                if (name == m) { s += $NF; matched = 1 }
            }
            END { if (matched) { printf "%s", s } else { printf "NA" } }
        '
}

deadline=$(( $(date -u +%s) + TIMEOUT_SECS ))
poll=0

while (( $(date -u +%s) < deadline )); do
    poll=$(( poll + 1 ))
    value=$(scrape_sum)

    if [[ "${value}" == "NA" || -z "${value}" ]]; then
        log "poll=${poll} result=metric_absent metric=${metric_name}"
    else
        log "poll=${poll} value=${value} threshold=${threshold}"
        # Clear when the summed value equals the threshold (float-safe).
        if awk -v v="${value}" -v t="${threshold}" 'BEGIN { exit !(v == t) }'; then
            log "result=clear value=${value} threshold=${threshold} polls=${poll} elapsed=$(($(date -u +%s) - (deadline - TIMEOUT_SECS)))s"
            exit 0
        fi
    fi
    sleep "${POLL_INTERVAL_SECS}"
done

log "result=timeout polls=${poll} elapsed=${TIMEOUT_SECS}s predicate=${PREDICATE}"
exit 1
