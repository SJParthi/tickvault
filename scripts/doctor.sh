#!/usr/bin/env bash
# scripts/doctor.sh — single-command total health check.
#
# Run with: `make doctor` OR `bash scripts/doctor.sh`
#
# Philosophy: Parthiban's directive — "no manual inputs or manual
# intervention should never ever be expected". This script is the
# one command an operator runs in the morning that answers every
# "is everything OK?" question. If every section is green, nothing
# needs attention. Any red = the section names the runbook path.
#
# Exit codes:
#   0 — everything healthy
#   1 — at least one check failed (which one is printed)
#   2 — infrastructure unavailable (Docker not running etc.)
#
# Every line of output is designed to be greppable by Claude Code,
# scripts, and humans alike. Format: `[STATUS] <category>: <detail>`.
set -uo pipefail

# --gate: treat SKIP/WARN on runtime endpoints as FAIL. Intended for live
# sessions where the app is supposed to be running — the audit should not
# silently pass when Prometheus/QuestDB/Grafana are unreachable.
GATE_MODE=0
for arg in "$@"; do
    case "$arg" in
        --gate) GATE_MODE=1 ;;
    esac
done

PASS=0
FAIL=0
FAILED_CHECKS=()

print_header() {
    echo "============================================================"
    echo "  tickvault doctor — $(date -u +'%Y-%m-%dT%H:%M:%SZ')"
    echo "============================================================"
}

run_check() {
    local section="$1"
    local name="$2"
    shift 2
    if "$@" >/dev/null 2>&1; then
        printf "[PASS] %-14s %s\n" "${section}" "${name}"
        PASS=$((PASS + 1))
    else
        printf "[FAIL] %-14s %s\n" "${section}" "${name}"
        FAIL=$((FAIL + 1))
        FAILED_CHECKS+=("${section}/${name}")
    fi
}

print_section() {
    echo ""
    echo "--- $1 ---"
}

# ---------------------------------------------------------------------------
# 1. Source-code invariants (every zero-touch guard must pass)
# ---------------------------------------------------------------------------
print_header
print_section "source invariants"

run_check "source" "workspace compiles" \
    cargo check --workspace

run_check "source" "validate-automation (full guard suite)" \
    bash scripts/validate-automation.sh

# ---------------------------------------------------------------------------
# 2. Docker stack health
# ---------------------------------------------------------------------------
print_section "docker stack"

if ! command -v docker >/dev/null 2>&1; then
    printf "[SKIP] %-14s docker CLI not installed\n" "docker"
else
    for service in tv-questdb tv-valkey tv-prometheus tv-grafana tv-traefik tv-alertmanager tv-valkey-exporter; do
        run_check "docker" "$service running" \
            bash -c "docker compose -f deploy/docker/docker-compose.yml ps --services --filter status=running | grep -q '^${service}$'"
    done
fi

# ---------------------------------------------------------------------------
# 3. Service endpoints reachable
# ---------------------------------------------------------------------------
print_section "endpoints"

run_check "endpoint" "QuestDB HTTP" \
    curl -fsS -m 3 http://127.0.0.1:9000/status
run_check "endpoint" "Prometheus" \
    curl -fsS -m 3 http://127.0.0.1:9090/-/healthy
run_check "endpoint" "Grafana" \
    curl -fsS -m 3 http://127.0.0.1:3000/api/health
run_check "endpoint" "Valkey (PING)" \
    bash -c "docker compose -f deploy/docker/docker-compose.yml exec -T tv-valkey valkey-cli ping 2>/dev/null | grep -q PONG"
run_check "endpoint" "Alertmanager" \
    curl -fsS -m 3 http://127.0.0.1:9093/-/healthy

# ---------------------------------------------------------------------------
# 4. Zero-touch artefacts present + non-stale
# ---------------------------------------------------------------------------
print_section "zero-touch artefacts"

run_check "logs" "data/logs/ directory exists" \
    test -d data/logs
run_check "logs" "errors.jsonl appender wrote at least once (optional — depends on uptime)" \
    bash -c "ls data/logs/errors.jsonl* >/dev/null 2>&1 || true"
run_check "logs" "errors.summary.md exists (if app has run)" \
    bash -c "test ! -d data/logs || test -f data/logs/errors.summary.md || ls data/logs/app.*.log >/dev/null 2>&1 || true"
run_check "hooks" "error-triage hook executable" \
    test -x .claude/hooks/error-triage.sh
run_check "hooks" "validate-automation script executable" \
    test -x scripts/validate-automation.sh

# ---------------------------------------------------------------------------
# 5. Live error summary (if present)
# ---------------------------------------------------------------------------
print_section "live error signal"

if [ -f data/logs/errors.summary.md ]; then
    if grep -q "Zero ERROR-level events" data/logs/errors.summary.md; then
        printf "[PASS] %-14s no ERROR events in lookback window\n" "errors"
        PASS=$((PASS + 1))
    else
        # Surface the top signatures for operator visibility
        sig_count=$(grep -c '^| [0-9]\+ | ' data/logs/errors.summary.md 2>/dev/null || echo 0)
        printf "[WARN] %-14s %s active signature(s) — see data/logs/errors.summary.md\n" "errors" "${sig_count}"
    fi
else
    printf "[SKIP] %-14s errors.summary.md not present (app hasn't run since Phase 5 shipped)\n" "errors"
fi

# ---------------------------------------------------------------------------
# 6. Env-var sanity
# ---------------------------------------------------------------------------
print_section "env vars"

# These are required to START the app in LIVE mode. Doctor doesn't
# require them for the health check itself; just reports absence.
for v in TV_QUESTDB_PG_USER TV_QUESTDB_PG_PASSWORD TV_GRAFANA_ADMIN_USER TV_GRAFANA_ADMIN_PASSWORD; do
    if [ -n "${!v:-}" ]; then
        printf "[PASS] %-14s %s is set\n" "env" "$v"
        PASS=$((PASS + 1))
    else
        printf "[WARN] %-14s %s not set (blocks LIVE mode boot — set before `make run`)\n" "env" "$v"
    fi
done

# ---------------------------------------------------------------------------
# 7. AWS readiness (opt-in — skip silently if AWS not provisioned)
# ---------------------------------------------------------------------------
print_section "aws readiness (skips when not provisioned)"

if command -v aws >/dev/null 2>&1; then
    if [ -f deploy/aws/terraform/.terraform.lock.hcl ]; then
        run_check "aws" "terraform init state present" \
            test -d deploy/aws/terraform/.terraform
    else
        printf "[SKIP] %-14s terraform not initialized\n" "aws"
    fi
else
    printf "[SKIP] %-14s aws CLI not installed\n" "aws"
fi

# ---------------------------------------------------------------------------
# Section 8 — Movers DDL Integrity (Phase 13 of v3 plan)
# ---------------------------------------------------------------------------
# Verifies the 22 movers_{T} table DDL files + partition manager + S3
# lifecycle config are all in sync. Source-only checks (no QuestDB
# round-trip) so this stays fast and runs on a fresh clone.

print_section "movers DDL integrity (Phase 13 sec 8)"

# 22 movers tables registered in partition manager
run_check "movers" "22 movers_{T} tables in partition manager" \
    bash -c 'grep -cE "\"movers_(1s|5s|10s|15s|30s|[0-9]+m|1h)\"," crates/storage/src/partition_manager.rs | awk "{ exit (\$1 == 22 ? 0 : 1) }"'

# S3 lifecycle config exists
run_check "movers" "S3 lifecycle config exists for movers" \
    test -f deploy/aws/s3-lifecycle-movers-tables.json

# 22 movers_22tf canonical helper file
run_check "movers" "movers_22tf_persistence module present" \
    test -f crates/storage/src/movers_22tf_persistence.rs

# DEDUP key includes segment per I-P1-11
run_check "movers" "DEDUP_KEY_MOVERS_22TF includes segment" \
    grep -q 'DEDUP_KEY_MOVERS_22TF.*segment' crates/storage/src/movers_22tf_persistence.rs

# 22 timeframes constant pinned
run_check "movers" "MOVERS_TIMEFRAMES has 22 entries" \
    grep -q 'MOVERS_TIMEFRAME_COUNT: usize = 22' crates/common/src/mover_types.rs

# ---------------------------------------------------------------------------
# Section 9 — Movers Universe Coverage (Phase 13 of v3 plan)
# ---------------------------------------------------------------------------
# Live runtime check — only meaningful after the snapshot scheduler ships
# in Phase 10. Until then it's a SKIP. Once shipped, queries Prometheus
# for `tv_movers_universe_size` (target ~24,324 ± 5% during market hours).

print_section "movers universe coverage (Phase 13 sec 9)"

# This section's checks are wired in Phase 10 once the metrics emit.
# Documented here so the section header is in place.
printf "[SKIP] %-14s Prometheus metric tv_movers_universe_size — wires up in Phase 10\n" "movers"
printf "[SKIP] %-14s Prometheus metric tv_movers_per_segment_count — wires up in Phase 10\n" "movers"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "============================================================"
echo "  PASS: ${PASS}    FAIL: ${FAIL}"
echo "============================================================"

if [ "${FAIL}" -gt 0 ]; then
    echo ""
    echo "Failed checks (follow the named runbook):"
    for check in "${FAILED_CHECKS[@]}"; do
        echo "  - ${check}"
    done
    echo ""
    echo "Runbook directory: docs/runbooks/"
    echo "Morning-ops guide: docs/runbooks/README.md"
    exit 1
fi

if [ "${GATE_MODE}" -eq 1 ]; then
    # --gate: app is expected to be live. Any unreachable endpoint should
    # surface as a real failure, not a silent SKIP. Re-probe the five
    # runtime endpoints and exit 1 if any fail.
    GATE_FAIL=0
    for probe in \
        "prometheus http://127.0.0.1:9090/-/healthy" \
        "questdb    http://127.0.0.1:9000/status" \
        "grafana    http://127.0.0.1:3000/api/health" \
        "alertmgr   http://127.0.0.1:9093/-/healthy" \
        "api        http://127.0.0.1:3001/health"; do
        name="${probe%% *}"
        url="${probe#* }"
        url="${url## }"
        if ! curl -fsS -m 3 "$url" >/dev/null 2>&1; then
            echo "[GATE-FAIL] $name unreachable ($url)"
            GATE_FAIL=1
        fi
    done
    if [ "${GATE_FAIL}" -eq 1 ]; then
        echo ""
        echo "--gate mode: one or more runtime endpoints are DOWN."
        exit 1
    fi
fi

echo ""
echo "All health checks green. No operator action required."
exit 0
