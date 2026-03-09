#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Flaky Test Detector
# =============================================================================
# Runs the test suite multiple times to detect intermittent failures.
# Race conditions, timing bugs, and non-determinism show up as tests that
# pass sometimes and fail sometimes. This catches them.
#
# Usage:
#   ./scripts/flaky-detect.sh          # 3 runs (default)
#   ./scripts/flaky-detect.sh 5        # 5 runs
#
# Cost: Local only. Zero CI minutes. Zero budget impact.
#
# Exit codes:
#   0 — All runs consistent (all pass or all fail the same way)
#   1 — Flaky test detected (inconsistent results between runs)
# =============================================================================

set -uo pipefail

RUNS=${1:-3}
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

if [ "$RUNS" -lt 2 ]; then
  echo -e "${RED}Need at least 2 runs to detect flakiness${NC}"
  exit 1
fi

echo -e "${CYAN}╔════════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║   Flaky Test Detector — $RUNS runs               ║${NC}"
echo -e "${CYAN}╚════════════════════════════════════════════════╝${NC}"
echo ""

RESULTS_DIR=$(mktemp -d)
trap 'rm -rf "$RESULTS_DIR"' EXIT

PASS_COUNT=0
FAIL_COUNT=0

for i in $(seq 1 "$RUNS"); do
  echo -e "${CYAN}[Run $i/$RUNS]${NC} cargo test --workspace..."

  # Capture test output, extract test result lines
  if cargo test --workspace 2>&1 | tee "$RESULTS_DIR/run-$i-full.txt" | \
     grep -E '^test .+ \.\.\. (ok|FAILED|ignored)' > "$RESULTS_DIR/run-$i.txt" 2>/dev/null; then
    PASS_COUNT=$((PASS_COUNT + 1))
    echo -e "  ${GREEN}PASSED${NC}"
  else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    echo -e "  ${RED}FAILED${NC}"
  fi
  echo ""
done

# Analyze: if all runs had the same result, no flakiness
echo -e "${CYAN}════════════════════════════════════════════════${NC}"

if [ "$PASS_COUNT" -eq "$RUNS" ]; then
  echo -e "${GREEN}  All $RUNS runs passed consistently — no flakiness detected${NC}"
  exit 0
elif [ "$FAIL_COUNT" -eq "$RUNS" ]; then
  echo -e "${RED}  All $RUNS runs failed consistently — deterministic failure (not flaky, just broken)${NC}"
  echo "  Fix the failing tests."
  exit 1
else
  echo -e "${RED}  FLAKY TESTS DETECTED${NC}"
  echo -e "  $PASS_COUNT/$RUNS passed, $FAIL_COUNT/$RUNS failed"
  echo ""

  # Diff the test results to find inconsistent tests
  echo -e "${YELLOW}  Inconsistent tests between runs:${NC}"
  if [ -f "$RESULTS_DIR/run-1.txt" ]; then
    for i in $(seq 2 "$RUNS"); do
      if [ -f "$RESULTS_DIR/run-$i.txt" ]; then
        DIFF=$(diff "$RESULTS_DIR/run-1.txt" "$RESULTS_DIR/run-$i.txt" 2>/dev/null || true)
        if [ -n "$DIFF" ]; then
          echo "  --- Differences between run 1 and run $i ---"
          echo "$DIFF" | head -20
          echo ""
        fi
      fi
    done
  fi

  echo -e "${RED}  Flaky tests are dangerous in a live trading system.${NC}"
  echo -e "${RED}  Fix race conditions before production.${NC}"
  exit 1
fi
