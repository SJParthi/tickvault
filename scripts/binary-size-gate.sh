#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Binary Size Gate
# =============================================================================
# Checks that the release binary stays under the Docker scratch constraint.
# Stores baseline; fails on >10% growth.
#
# Usage: bash scripts/binary-size-gate.sh [binary-path]
#
# Default binary: target/release/dhan-live-trader
# Max size: 15MB (scratch Docker container constraint)
# Exit 0 = within budget. Exit 1 = too large or >10% growth.
# =============================================================================

set -euo pipefail

BINARY="${1:-target/release/dhan-live-trader}"
MAX_BYTES=$((15 * 1024 * 1024))  # 15MB in bytes
MAX_GROWTH_PCT=10
BASELINE_FILE="quality/.binary-size-baseline"

if [ ! -f "$BINARY" ]; then
  echo "INFO: Release binary not found at $BINARY — skipping size gate."
  echo "  Build with: cargo build --release"
  exit 0
fi

CURRENT_BYTES=$(stat -c%s "$BINARY" 2>/dev/null || stat -f%z "$BINARY" 2>/dev/null)
CURRENT_MB=$(echo "scale=2; $CURRENT_BYTES / 1048576" | bc)

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
  echo "  FAIL: Binary exceeds ${MAX_BYTES}B (${CURRENT_MB}MB > 15MB)"
  FAILED=1
else
  echo "  PASS: Binary within absolute limit (${CURRENT_MB}MB < 15MB)"
fi

# Check growth regression against baseline
if [ -f "$BASELINE_FILE" ]; then
  BASELINE_BYTES=$(cat "$BASELINE_FILE" | tr -d ' \n')
  if [ -n "$BASELINE_BYTES" ] && [ "$BASELINE_BYTES" -gt 0 ] 2>/dev/null; then
    GROWTH_PCT=$(echo "scale=1; (($CURRENT_BYTES - $BASELINE_BYTES) * 100) / $BASELINE_BYTES" | bc)
    if [ "$(echo "$GROWTH_PCT > $MAX_GROWTH_PCT" | bc)" -eq 1 ]; then
      echo "  FAIL: Binary grew ${GROWTH_PCT}% (was ${BASELINE_BYTES}B, now ${CURRENT_BYTES}B, max ${MAX_GROWTH_PCT}%)"
      FAILED=1
    else
      echo "  PASS: Binary growth within limit (${GROWTH_PCT}%)"
      # Update baseline if binary shrank or grew within limits
      echo "$CURRENT_BYTES" > "$BASELINE_FILE"
    fi
  fi
else
  # First run — establish baseline
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
