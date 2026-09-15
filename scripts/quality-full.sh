#!/usr/bin/env bash
# =============================================================================
# tickvault — Local Quality Checks (7 Stages)
# =============================================================================
# Runs the commands listed below. This local wrapper differs from CI in
# target selection, features and coverage policy; it cannot certify a CI pass.
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
#   7. Flakiness   — run tests 3x to detect intermittent failures (local only)
# =============================================================================

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

FAILED=0
UNAVAILABLE=0

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
echo -e "${CYAN}║   Local Quality Checks — 7 Stages                ║${NC}"
echo -e "${CYAN}╚════════════════════════════════════════════════╝${NC}"
echo ""

# Stage 1: Compile
run_stage "1/7" "Compile" "cargo build --release --workspace"

# Stage 2: Lint
run_stage "2/7" "Format Check" "cargo fmt --all -- --check"
run_stage "2/7" "Clippy" "cargo clippy --workspace --all-targets -- -D warnings -W clippy::perf"
run_stage "2/7" "Doc Warnings" "RUSTDOCFLAGS=\"-D warnings\" cargo doc --workspace --no-deps"
run_stage "2/7" "Doc Tests" "cargo test --doc --workspace"

if command -v typos > /dev/null 2>&1; then
    run_stage "2/7" "Typos" "typos ."
else
    echo -e "${CYAN}[Stage 2/7]${NC} Typos"
    UNAVAILABLE=$((UNAVAILABLE + 1))
    echo -e "  ${YELLOW}UNAVAILABLE${NC} — typos not installed"
    echo ""
fi

# Stage 3: Test
run_stage "3/7" "Tests" "cargo test --workspace"

# Stage 4: Security
if command -v cargo-audit > /dev/null 2>&1; then
    run_stage "4/7" "Security Audit" "cargo audit"
else
    echo -e "${CYAN}[Stage 4/7]${NC} Security Audit"
    UNAVAILABLE=$((UNAVAILABLE + 1))
    echo -e "  ${YELLOW}UNAVAILABLE${NC} — cargo-audit not installed"
    echo ""
fi

if command -v cargo-deny > /dev/null 2>&1; then
    run_stage "4/7" "Deny Check" "cargo deny check"
else
    echo -e "${CYAN}[Stage 4/7]${NC} Deny Check"
    UNAVAILABLE=$((UNAVAILABLE + 1))
    echo -e "  ${YELLOW}UNAVAILABLE${NC} — cargo-deny not installed"
    echo ""
fi

# Stage 5: Performance (skip if no benchmarks exist)
BENCH_FILES=$(find . -path "*/benches/*.rs" -type f 2>/dev/null | head -1)
if [ -n "$BENCH_FILES" ]; then
    run_stage "5/7" "Benchmarks" "cargo bench --workspace"
else
    echo -e "${CYAN}[Stage 5/7]${NC} Benchmarks"
    UNAVAILABLE=$((UNAVAILABLE + 1))
    echo -e "  ${YELLOW}UNAVAILABLE${NC} — no benchmark files found"
    echo ""
fi

# Stage 6: Coverage (99% threshold)
if command -v cargo-llvm-cov > /dev/null 2>&1; then
    run_stage "6/7" "Coverage (99% threshold)" "cargo llvm-cov --workspace --fail-under-lines 99"
    run_stage "6/7" "Coverage report" "cargo llvm-cov --workspace --html --output-dir target/llvm-cov"
else
    echo -e "${CYAN}[Stage 6/7]${NC} Coverage"
    UNAVAILABLE=$((UNAVAILABLE + 1))
    echo -e "  ${YELLOW}UNAVAILABLE${NC} — cargo-llvm-cov not installed (install: cargo install cargo-llvm-cov)"
    echo ""
fi

# Stage 7: Flakiness detection (local only — zero CI cost)
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
if [ -x "$SCRIPT_DIR/flaky-detect.sh" ]; then
    echo -e "${CYAN}[Stage 7/7]${NC} Flakiness Detection (3 runs)"
    echo -n "  Running: ./scripts/flaky-detect.sh 3 ... "
    if "$SCRIPT_DIR/flaky-detect.sh" 3 > /dev/null 2>&1; then
        echo -e "${GREEN}PASSED${NC}"
    else
        echo -e "${RED}FAILED${NC}"
        FAILED=1
    fi
    echo ""
else
    echo -e "${CYAN}[Stage 7/7]${NC} Flakiness Detection"
    UNAVAILABLE=$((UNAVAILABLE + 1))
    echo -e "  ${YELLOW}UNAVAILABLE${NC} — scripts/flaky-detect.sh not found"
    echo ""
fi

# Summary
echo -e "${CYAN}════════════════════════════════════════════════${NC}"
if [ "$FAILED" -ne 0 ]; then
    echo -e "${RED}  One or more executed local checks FAILED${NC}"
    exit 1
elif [ "$UNAVAILABLE" -ne 0 ]; then
    echo -e "${YELLOW}  Local checks incomplete: $UNAVAILABLE unavailable${NC}"
    exit 2
else
    echo -e "${GREEN}  The executed local checks PASSED${NC}"
    echo "  CI, deployment and production behavior require their own evidence."
fi
echo ""
