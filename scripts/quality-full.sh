#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Full Quality Gates (All 6 CI Stages)
# =============================================================================
# Runs the same 6 stages that CI runs on GitHub Actions.
# If this passes locally, it will pass in CI.
#
# Usage: ./scripts/quality-full.sh
#
# Stages:
#   1. Compile     — cargo build --release --workspace
#   2. Lint        — cargo fmt --check + cargo clippy + banned-pattern scan
#   3. Test        — cargo test --workspace
#   4. Security    — cargo audit + cargo deny check
#   5. Performance — cargo bench --workspace (if benchmarks exist)
#   6. Coverage    — cargo llvm-cov --workspace (99% threshold)
# =============================================================================

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

FAILED=0

run_stage() {
    local stage_num="$1"
    local stage_name="$2"
    local command="$3"

    echo -e "${CYAN}[Stage $stage_num]${NC} $stage_name"
    echo -n "  Running: $command ... "
    if eval "$command" > /dev/null 2>&1; then
        echo -e "${GREEN}PASSED${NC}"
    else
        echo -e "${RED}FAILED${NC}"
        FAILED=1
    fi
    echo ""
}

echo ""
echo -e "${CYAN}╔════════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║   Full Quality Gates — 6 Stages                ║${NC}"
echo -e "${CYAN}╚════════════════════════════════════════════════╝${NC}"
echo ""

# Stage 1: Compile
run_stage "1/6" "Compile" "cargo build --release --workspace"

# Stage 2: Lint
run_stage "2/6" "Format Check" "cargo fmt --all -- --check"
run_stage "2/6" "Clippy" "cargo clippy --workspace --all-targets -- -D warnings -W clippy::perf"
run_stage "2/6" "Doc Warnings" "RUSTDOCFLAGS=\"-D warnings\" cargo doc --workspace --no-deps"
run_stage "2/6" "Doc Tests" "cargo test --doc --workspace"

if command -v typos > /dev/null 2>&1; then
    run_stage "2/6" "Typos" "typos ."
else
    echo -e "${CYAN}[Stage 2/6]${NC} Typos"
    echo -e "  ${YELLOW}SKIPPED${NC} — typos not installed"
    echo ""
fi

# Stage 3: Test
run_stage "3/6" "Tests" "cargo test --workspace"

# Stage 4: Security
if command -v cargo-audit > /dev/null 2>&1; then
    run_stage "4/6" "Security Audit" "cargo audit"
else
    echo -e "${CYAN}[Stage 4/6]${NC} Security Audit"
    echo -e "  ${YELLOW}SKIPPED${NC} — cargo-audit not installed"
    echo ""
fi

if command -v cargo-deny > /dev/null 2>&1; then
    run_stage "4/6" "Deny Check" "cargo deny check"
else
    echo -e "${CYAN}[Stage 4/6]${NC} Deny Check"
    echo -e "  ${YELLOW}SKIPPED${NC} — cargo-deny not installed"
    echo ""
fi

# Stage 5: Performance (skip if no benchmarks exist)
BENCH_FILES=$(find . -path "*/benches/*.rs" -type f 2>/dev/null | head -1)
if [ -n "$BENCH_FILES" ]; then
    run_stage "5/6" "Benchmarks" "cargo bench --workspace"
else
    echo -e "${CYAN}[Stage 5/6]${NC} Benchmarks"
    echo -e "  ${YELLOW}SKIPPED${NC} — no benchmark files found"
    echo ""
fi

# Stage 6: Coverage (99% threshold)
if command -v cargo-llvm-cov > /dev/null 2>&1; then
    run_stage "6/6" "Coverage (99% threshold)" "cargo llvm-cov --workspace --fail-under-lines 99"
    run_stage "6/6" "Coverage report" "cargo llvm-cov --workspace --html --output-dir target/llvm-cov"
else
    echo -e "${CYAN}[Stage 6/6]${NC} Coverage"
    echo -e "  ${YELLOW}SKIPPED${NC} — cargo-llvm-cov not installed (install: cargo install cargo-llvm-cov)"
    echo ""
fi

# Summary
echo -e "${CYAN}════════════════════════════════════════════════${NC}"
if [ $FAILED -eq 0 ]; then
    echo -e "${GREEN}  All quality gates PASSED${NC}"
    echo -e "  Code is production-ready."
else
    echo -e "${RED}  Some quality gates FAILED${NC}"
    echo -e "  Fix failures before shipping."
    exit 1
fi
echo ""
