#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Binary Size Gate
# =============================================================================
# Checks that the release binary stays under budget.
# Stores baseline; fails on >10% growth.
#
# Usage: bash scripts/binary-size-gate.sh [binary-path]
#
# Default binary: target/release/dhan-live-trader
# Max size: 50MB (with AWS SDK + tokio + axum + reqwest + teloxide)
# Exit 0 = within budget. Exit 1 = too large or >10% growth.
# =============================================================================

set -euo pipefail

BINARY="${1:-target/release/dhan-live-trader}"
MAX_BYTES=$((50 * 1024 * 1024))  # 50MB in bytes
MAX_GROWTH_PCT=10
BASELINE_FILE="quality/.binary-size-baseline"

if [ ! -f "$BINARY" ]; then
  echo "INFO: Release binary not found at $BINARY — skipping size gate."
  echo "  Build with: cargo build --release"
  exit 0
fi

CURRENT_BYTES=$(stat -c%s "$BINARY" 2>/dev/null || stat -f%z "$BINARY" 2>/dev/null)
CURRENT_MB=$(python3 -c "print(f'{$CURRENT_BYTES / 1048576:.2f}')")

echo "╔══════════════════════════════════════════════╗"
echo "║   Binary Size Gate                            ║"
echo "╚══════════════════════════════════════════════╝"
echo ""
echo "  Binary: $BINARY"
echo "  Size:   ${CURRENT_MB}MB ($CURRENT_BYTES bytes)"
echo "  Budget: $((MAX_BYTES / 1048576))MB ($MAX_BYTES bytes)"
echo ""

FAILED=0

# Check absolute size limit
if [ "$CURRENT_BYTES" -gt "$MAX_BYTES" ]; then
  echo "  FAIL: Binary exceeds budget (${CURRENT_MB}MB > 50MB)"
  FAILED=1
else
  echo "  PASS: Binary within absolute limit (${CURRENT_MB}MB < 50MB)"
fi

# Check growth regression against baseline (CI only — baseline not committed)
if [ -f "$BASELINE_FILE" ]; then
  BASELINE_BYTES=$(cat "$BASELINE_FILE" | tr -d ' \n')
  if [ -n "$BASELINE_BYTES" ] && [ "$BASELINE_BYTES" -gt 0 ] 2>/dev/null; then
    GROWTH_RESULT=$(python3 -c "
b = $BASELINE_BYTES
c = $CURRENT_BYTES
pct = ((c - b) / b) * 100
print(f'{pct:.1f}')
")
    IS_OVER=$(python3 -c "print('yes' if float('$GROWTH_RESULT') > $MAX_GROWTH_PCT else 'no')")
    if [ "$IS_OVER" = "yes" ]; then
      echo "  FAIL: Binary grew ${GROWTH_RESULT}% (was ${BASELINE_BYTES}B, now ${CURRENT_BYTES}B, max ${MAX_GROWTH_PCT}%)"
      FAILED=1
    else
      echo "  PASS: Binary growth within limit (${GROWTH_RESULT}%)"
      echo "$CURRENT_BYTES" > "$BASELINE_FILE"
    fi
  fi
else
  # First run — establish baseline (not a failure)
  echo "$CURRENT_BYTES" > "$BASELINE_FILE"
  echo "  INFO: Binary size baseline established: ${CURRENT_BYTES}B"
fi

echo ""
if [ "$FAILED" -ne 0 ]; then
  echo "  Binary size gate FAILED."
  exit 1
else
  echo "  Binary size gate passed."
  exit 0
fi
