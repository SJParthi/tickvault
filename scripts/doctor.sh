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
# silently pass when QuestDB or the tickvault API are unreachable.
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
    # tv-valkey removed in #O4 (2026-05-24) — see .claude/rules/project/aws-budget.md.
    for service in tv-questdb; do
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
# Valkey PING check removed in #O4 (2026-05-24).
# Grafana (#O1), Alertmanager (#O2) and Prometheus (#O3) container probes
# removed in #O5 (2026-05-30) — those containers no longer exist
# (CloudWatch-only observability). The app's /metrics exporter (port 9091)
# is scraped by the CloudWatch agent in prod, not probed here.

# ---------------------------------------------------------------------------
# 4. Zero-touch artefacts present + non-stale
# ---------------------------------------------------------------------------
print_section "zero-touch artefacts"

run_check "logs" "data/logs/ directory exists" \
    test -d data/logs
run_check "logs" "errors.jsonl appender wrote at least once (optional — depends on uptime)" \
    bash -c "ls data/logs/machine/errors.jsonl* data/logs/errors.jsonl* >/dev/null 2>&1 || true"
run_check "logs" "errors.summary.md exists (if app has run)" \
    bash -c "test ! -d data/logs || test -f data/logs/machine/errors.summary.md || test -f data/logs/errors.summary.md || ls data/logs/app.*.log >/dev/null 2>&1 || true"
run_check "hooks" "error-triage hook executable" \
    test -x .claude/hooks/error-triage.sh
run_check "hooks" "validate-automation script executable" \
    test -x scripts/validate-automation.sh

# ---------------------------------------------------------------------------
# 5. Live error summary (if present)
# ---------------------------------------------------------------------------
print_section "live error signal"

ERR_SUMMARY="data/logs/machine/errors.summary.md"
[ -f "$ERR_SUMMARY" ] || ERR_SUMMARY="data/logs/errors.summary.md"
if [ -f "$ERR_SUMMARY" ]; then
    if grep -q "Zero ERROR-level events" "$ERR_SUMMARY"; then
        printf "[PASS] %-14s no ERROR events in lookback window\n" "errors"
        PASS=$((PASS + 1))
    else
        # Surface the top signatures for operator visibility
        sig_count=$(grep -c '^| [0-9]\+ | ' "$ERR_SUMMARY" 2>/dev/null || echo 0)
        printf "[WARN] %-14s %s active signature(s) — see data/logs/machine/errors.summary.md\n" "errors" "${sig_count}"
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
for v in TV_QUESTDB_PG_USER TV_QUESTDB_PG_PASSWORD; do
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

# Movers base table + materialized views (post 2026-05-01 cleanup).
# Module file kept its `_unified_` filename to minimise the rename diff —
# the operator-visible table is `movers_1s` (not `movers_unified_1s`).
run_check "movers" "movers persistence module (movers_1s base + 24 mat views)" \
    test -f crates/storage/src/movers_unified_persistence.rs

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
        "questdb    http://127.0.0.1:9000/status" \
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
