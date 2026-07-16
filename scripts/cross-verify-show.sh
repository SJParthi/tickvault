#!/bin/bash
# cross-verify-show.sh — the human-visible window into the post-market
# 1-minute cross-verification (operator directive 2026-06-02).
#
# It answers "as a human, how do I SEE the cross-verification?" with no
# illusion: it shows the real CSV if one exists, and otherwise tells you
# plainly WHY there's nothing yet and where else to look.
set -uo pipefail

DIR="data/cross-verify"
TABLE="cross_verify_1m_audit"

echo "================ Post-market 1-minute cross-verification ================"
echo "What it does: at 15:31 IST each trading day it compares OUR candles_1m"
echo "against Dhan's authoritative 1-minute candles, EXACT match, per spot SID."
echo "-------------------------------------------------------------------------"

latest=""
if [ -d "$DIR" ]; then
  # newest cross-verify-1m-YYYY-MM-DD.csv by name (dates sort lexically)
  latest="$(ls -1 "$DIR"/cross-verify-1m-*.csv 2>/dev/null | sort | tail -1)"
fi

if [ -n "$latest" ] && [ -f "$latest" ]; then
  rows="$(($(wc -l < "$latest") - 1))"   # minus header
  [ "$rows" -lt 0 ] && rows=0
  echo "Latest report : $latest"
  echo "Mismatch rows : $rows   (0 = every minute matched exactly — good)"
  echo "-------------------------------------------------------------------------"
  echo "First lines (open in Excel for the full grid):"
  head -11 "$latest" | sed 's/^/  /'
  echo "-------------------------------------------------------------------------"
  echo "All reports:"
  ls -1t "$DIR"/cross-verify-1m-*.csv 2>/dev/null | sed 's/^/  /'
else
  echo "No report on disk yet. That is EXPECTED if any of these are true:"
  echo "  - The app has not run through a 15:31 IST trading-day close yet."
  echo "  - You are on a dev box with no live Dhan feed / QuestDB candles."
  echo ""
  echo "To produce one on demand (needs a booted app + live auth + QuestDB):"
  echo "    make cross-verify-now"
  echo ""
  echo "Other places the same result shows up:"
  echo "  - QuestDB table : $TABLE  (open 'make questdb' → localhost:9000)"
  echo "  - Telegram      : a per-day summary at ~15:31 IST (compared / mismatches / missing)"
  echo "  - Logs          : CROSS-VERIFY-1M-01/-02 lines exist only in HISTORICAL errors.jsonl"
  echo "                    (the cross-verify module retired 2026-07-13 with the Dhan live WS;"
  echo "                    its ErrorCode variants were deleted in the C4 sweep 2026-07-15)"
fi
echo "========================================================================="
