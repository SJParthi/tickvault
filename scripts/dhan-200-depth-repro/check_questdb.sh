#!/usr/bin/env bash
# tickvault — quick QuestDB health + data-flow check.
#
# Usage:
#     ./scripts/dhan-200-depth-repro/check_questdb.sh
#
# Reports:
#   1. deep_market_depth row count by depth_type, last 5 minutes
#   2. ticks row count by segment, last 5 minutes
#   3. Per-contract depth-200 row counts, last 5 minutes

set -euo pipefail

QUESTDB_URL="${QUESTDB_URL:-http://localhost:9000}"

run_query() {
    local label="$1"
    local sql="$2"
    echo
    echo "═══════════════════════════════════════════════════════════════════"
    echo "  $label"
    echo "═══════════════════════════════════════════════════════════════════"
    curl -sG "$QUESTDB_URL/exec" --data-urlencode "query=$sql" | python3 -m json.tool
}

run_query \
    "1. deep_market_depth — rows last 5min by depth_type" \
    "SELECT depth_type, count(*) AS rows, max(ts) AS last_ts \
     FROM deep_market_depth \
     WHERE ts > dateadd('m', -5, now()) \
     GROUP BY depth_type"

run_query \
    "2. ticks — rows last 5min by segment" \
    "SELECT segment, count(*) AS rows, count_distinct(security_id) AS unique_sids \
     FROM ticks \
     WHERE ts > dateadd('m', -5, now()) \
     GROUP BY segment"

run_query \
    "3. depth-200 per-contract row counts last 5min" \
    "SELECT security_id, count(*) AS rows, max(ts) AS last_ts \
     FROM deep_market_depth \
     WHERE depth_type='200' AND ts > dateadd('m', -5, now()) \
     GROUP BY security_id \
     ORDER BY rows DESC"

echo
echo "═══════════════════════════════════════════════════════════════════"
echo "  DONE — interpretation:"
echo "    • depth_type='20'  rows > 100k  ✓ depth-20 streaming"
echo "    • depth_type='200' rows > 1M    ✓ depth-200 streaming all 4"
echo "    • depth-200 should show 4 distinct security_ids (NIFTY+BANKNIFTY × CE+PE)"
echo "═══════════════════════════════════════════════════════════════════"
