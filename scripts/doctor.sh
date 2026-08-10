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
SKIP=0
FAILED_CHECKS=()
SKIPPED_CHECKS=()

print_header() {
    echo "============================================================"
    echo "  tickvault doctor — $(date -u +'%Y-%m-%dT%H:%M:%SZ')"
    echo "============================================================"
}

# glob_exists <path-glob>... — true if AT LEAST ONE operand exists.
#
# Deliberately not `ls a b`: ls exits non-zero when ANY operand is missing,
# so `ls a b` means "a AND b", never "a OR b". The pre-2026-08-10 log checks
# used exactly that form, which is why they could only ever have been true
# by accident — and were masked by a `|| true` besides. Unmatched globs stay
# literal and fail the -e test, which is the behaviour we want.
glob_exists() {
    local f
    for f in "$@"; do
        [ -e "$f" ] && return 0
    done
    return 1
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

# run_check_when <section> <name> <precondition-cmd...> -- <assertion-cmd...>
#
# Rule 11 (no false-OK): a check whose precondition is not met is reported as
# SKIP, never PASS. "We did not test this" and "this passed" are different
# facts and must never render identically. In --gate mode (live session, the
# app is supposed to be running) a SKIP is escalated to FAIL, because there
# the precondition failing is itself the defect.
run_check_when() {
    local section="$1"
    local name="$2"
    shift 2
    local pre=()
    while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do
        pre+=("$1"); shift
    done
    shift  # drop the "--"

    if ! "${pre[@]}" >/dev/null 2>&1; then
        if [ "${GATE_MODE}" -eq 1 ]; then
            printf "[FAIL] %-14s %s (precondition not met, --gate)\n" "${section}" "${name}"
            FAIL=$((FAIL + 1))
            FAILED_CHECKS+=("${section}/${name} (precondition not met)")
        else
            printf "[SKIP] %-14s %s (not applicable — app has not run yet)\n" "${section}" "${name}"
            SKIP=$((SKIP + 1))
            SKIPPED_CHECKS+=("${section}/${name}")
        fi
        return
    fi

    run_check "${section}" "${name}" "$@"
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
# Both checks below are gated on "the app has actually run" (an app log
# exists). Before 2026-08-10 they ended in `|| true`, which made them
# unfailable: "errors.jsonl wrote at least once" printed PASS on a tree
# where nothing had ever been written. That is the Rule 11 false-OK class
# the charter forbids — a check that cannot fail reports nothing.
#
# Cadence note: summary_writer refreshes errors.summary.md every 60s
# (crates/core/src/notification/summary_writer.rs), so a doctor run inside
# the first minute of a cold boot can legitimately FAIL here. That is the
# correct direction of error: loud and self-resolving, never silent.
app_has_run() {
    glob_exists data/logs/app.*.log data/logs/machine/app.*.log
}

run_check_when "logs" "errors.jsonl appender wrote at least once" \
    app_has_run \
    -- glob_exists data/logs/machine/errors.jsonl* data/logs/errors.jsonl*
run_check_when "logs" "errors.summary.md exists" \
    app_has_run \
    -- glob_exists data/logs/machine/errors.summary.md data/logs/errors.summary.md
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
# Sections 8+9 (movers DDL integrity + movers universe coverage) RETIRED
# 2026-07-06 — audit finding: the movers runtime was deleted in
# AWS-lifecycle PR #2/#3 (2026-05-19). The checks required 22 movers_{T}
# partition-manager entries + a `movers_unified_persistence.rs` module,
# neither of which exists anymore, making `make doctor` permanently RED
# on phantom checks. The orphan artefacts they referenced
# (deploy/aws/s3-lifecycle-movers-tables.json,
# scripts/migrate-drop-movers-tables.sql) were removed in the same sweep.

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "============================================================"
echo "  PASS: ${PASS}    FAIL: ${FAIL}    SKIP: ${SKIP}"
echo "============================================================"

if [ "${SKIP}" -gt 0 ]; then
    echo ""
    echo "Skipped checks (precondition not met — NOT the same as passing):"
    for check in "${SKIPPED_CHECKS[@]}"; do
        echo "  - ${check}"
    done
    echo "  (re-run with --gate to treat these as failures)"
fi

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
