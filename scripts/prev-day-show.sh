#!/bin/bash
# prev-day-show.sh — the human-visible window into the PRE-market prev-day
# OHLCV fetch + its coverage verification.
#
# Symmetric to scripts/cross-verify-show.sh (post-market). No illusion: shows
# the real coverage report if one exists, otherwise says plainly WHY there's
# nothing yet and where else to look.
set -uo pipefail

DIR="data/prev-day"
TABLE="prev_day_ohlcv"

echo "================ Pre-market prev-day OHLCV fetch (coverage) ================"
echo "What it does: at boot it pulls YESTERDAY's single daily candle (O/H/L/C/V +"
echo "OI) for every subscribed spot SID into the $TABLE table, then verifies how"
echo "many of the universe actually got one (coverage %)."
echo "---------------------------------------------------------------------------"

latest=""
if [ -d "$DIR" ]; then
  latest="$(ls -1 "$DIR"/prev-day-coverage-*.csv 2>/dev/null | sort | tail -1)"
fi

if [ -n "$latest" ] && [ -f "$latest" ]; then
  echo "Latest coverage report : $latest"
  echo "---------------------------------------------------------------------------"
  cat "$latest" | sed 's/^/  /'
  echo "---------------------------------------------------------------------------"
  echo "Reading it: outcome=ok (>=90% got yesterday's candle) is healthy;"
  echo "            degraded = some symbols missing; empty = nothing fetched (investigate)."
  echo "All reports:"
  ls -1t "$DIR"/prev-day-coverage-*.csv 2>/dev/null | sed 's/^/  /'
else
  echo "No coverage report on disk yet. That is EXPECTED if:"
  echo "  - The app has not completed a boot fetch yet, or"
  echo "  - You are on a dev box with no live Dhan auth (the fetch is skipped)."
  echo ""
  echo "Other places the same data shows up:"
  echo "  - QuestDB table : $TABLE  (open 'make questdb' → localhost:9000 →"
  echo "                    SELECT count(*) FROM $TABLE)"
  echo "  - Logs          : a 'PROOF: prev-day OHLCV coverage OK' info line, or a"
  echo "                    'coverage DEGRADED/EMPTY' warning/error in errors.jsonl"
fi
echo "==========================================================================="
