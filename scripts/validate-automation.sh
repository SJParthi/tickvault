#!/usr/bin/env bash
# Phase 9.2 of active-plan.md — end-to-end validation of the zero-touch chain.
#
# Runs every mechanical guarantee we've built and prints a single
# pass/fail board. Operator or CI can run this to get an unambiguous
# "is automation still wired correctly" answer in 30 seconds.
#
# Usage:
#   scripts/validate-automation.sh [--verbose]
#
# Exit codes:
#   0  every check passed
#   1  at least one check failed (see the board for which)
#
# What this validates:
#   1. Workspace compiles clean
#   2. All 9 zero-touch guard suites pass
#   3. Architecture doc has no broken runbook paths
#   4. Triage YAML rules all reference existing ErrorCode variants
#   5. Auto-fix scripts exist and are executable
#   6. observability-architecture.md keywords still present
#   7. errors.jsonl layer has flatten_event(true) in main.rs
set -uo pipefail

VERBOSE=0
if [[ "${1:-}" == "--verbose" ]]; then
    VERBOSE=1
fi

PASS=0
FAIL=0
FAILED_CHECKS=()

run_check() {
    local name="$1"
    shift
    if [[ "${VERBOSE}" == "1" ]]; then
        echo "  [running] ${name}"
        if "$@"; then
            echo "  [PASS] ${name}"
            PASS=$((PASS + 1))
        else
            echo "  [FAIL] ${name}"
            FAIL=$((FAIL + 1))
            FAILED_CHECKS+=("${name}")
        fi
    else
        if "$@" >/dev/null 2>&1; then
            printf "  [PASS] %s\n" "${name}"
            PASS=$((PASS + 1))
        else
            printf "  [FAIL] %s\n" "${name}"
            FAIL=$((FAIL + 1))
            FAILED_CHECKS+=("${name}")
        fi
    fi
}

echo "============================================================"
echo "  tickvault — zero-touch automation validation"
echo "  $(date -u +'%Y-%m-%dT%H:%M:%SZ')"
echo "============================================================"

echo ""
echo "--- compile gates ---"
run_check "workspace compiles" \
    cargo check --workspace

echo ""
echo "--- observability guards ---"
run_check "error_code unit tests" \
    cargo test -p tickvault-common --lib error_code
run_check "error_code_rule_file_crossref" \
    cargo test -p tickvault-common --test error_code_rule_file_crossref
run_check "error_code_tag_guard" \
    cargo test -p tickvault-common --test error_code_tag_guard
run_check "triage_rules_guard" \
    cargo test -p tickvault-common --test triage_rules_guard
run_check "error_level_meta_guard" \
    cargo test -p tickvault-storage --test error_level_meta_guard
run_check "zero_tick_loss_alert_guard" \
    cargo test -p tickvault-storage --test zero_tick_loss_alert_guard
run_check "summary_writer unit tests" \
    cargo test -p tickvault-core --lib notification::summary_writer
run_check "observability library tests" \
    cargo test -p tickvault-app --lib observability
run_check "observability_chain_e2e" \
    cargo test -p tickvault-app --test observability_chain_e2e

echo ""
echo "--- file-level invariants ---"
run_check "architecture doc exists" \
    test -f .claude/rules/project/observability-architecture.md
run_check "triage rules YAML exists" \
    test -f .claude/triage/error-rules.yaml
run_check "claude-loop prompt exists" \
    test -f .claude/triage/claude-loop-prompt.md
run_check "auto-fix restart-depth executable" \
    test -x scripts/auto-fix-restart-depth.sh
run_check "auto-fix refresh-instruments executable" \
    test -x scripts/auto-fix-refresh-instruments.sh
run_check "auto-fix clear-spill executable" \
    test -x scripts/auto-fix-clear-spill.sh
run_check "error-triage hook executable" \
    test -x .claude/hooks/error-triage.sh

echo ""
echo "--- source-code invariants ---"
run_check "flatten_event(true) on errors.jsonl layer" \
    grep -q "flatten_event(true)" crates/app/src/main.rs
run_check "schema self-heal ALTER TABLE pattern" \
    grep -q "ADD COLUMN IF NOT EXISTS" crates/storage/src/instrument_persistence.rs
run_check "must-use deny in every crate" \
    bash -c 'for c in common storage core trading api app; do \
        grep -q "deny(unused_must_use)" crates/$c/src/lib.rs || exit 1; \
    done'

echo ""
echo "============================================================"
echo "  PASS: ${PASS}    FAIL: ${FAIL}"
echo "============================================================"

if [[ "${FAIL}" -gt 0 ]]; then
    echo ""
    echo "Failed checks:"
    for check in "${FAILED_CHECKS[@]}"; do
        echo "  - ${check}"
    done
    exit 1
fi

echo ""
echo "All zero-touch automation checks green."
exit 0
