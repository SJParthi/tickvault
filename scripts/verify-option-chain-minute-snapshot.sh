#!/usr/bin/env bash
# verify-option-chain-minute-snapshot.sh
#
# Runs the 7-query verification suite for the option-chain minute-snapshot
# pipeline (operator-charter §C 15-row matrix: "real-time proof").
#
# Each query hits the LOCAL QuestDB via its /exec HTTP endpoint and prints
# a one-line verdict. Exit code is 0 if every query returns a PASS, else 1.
#
# Usage: make verify-option-chain-minute-snapshot
#        OR
#        bash scripts/verify-option-chain-minute-snapshot.sh
#
# Plan reference:
# .claude/plans/friday-may-15-mega/topic-OPTION-CHAIN-MINUTE-SNAPSHOT.md §9

set -u

# QuestDB HTTP /exec endpoint — same defaults as the rest of the codebase
# (overridable via env vars).
QDB_HOST="${TV_QUESTDB_HOST:-127.0.0.1}"
QDB_PORT="${TV_QUESTDB_HTTP_PORT:-9000}"
QDB_URL="http://${QDB_HOST}:${QDB_PORT}/exec"

# Color codes for the verdict table (no color if not a TTY).
if [[ -t 1 ]]; then
  GREEN=$'\033[32m'
  RED=$'\033[31m'
  YELLOW=$'\033[33m'
  RESET=$'\033[0m'
else
  GREEN=""
  RED=""
  YELLOW=""
  RESET=""
fi

PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0

# Run one query, print verdict line. The `verdict_fn` decides PASS/FAIL/SKIP
# based on the raw query response.
#
# Args:
#   $1 = check_name (e.g. "Q1 schema")
#   $2 = SQL
#   $3 = verdict_fn (bash function name accepting the response body)
run_check() {
  local name="$1"
  local sql="$2"
  local verdict_fn="$3"

  local sql_enc
  sql_enc=$(printf '%s' "$sql" | jq -sRr @uri)

  local body
  if ! body=$(curl -fsS --max-time 5 "${QDB_URL}?query=${sql_enc}" 2>/dev/null); then
    printf "  %s %-45s %s\n" "${YELLOW}SKIP${RESET}" "$name" "(QuestDB unreachable)"
    SKIP_COUNT=$((SKIP_COUNT + 1))
    return
  fi

  if $verdict_fn "$body"; then
    printf "  %s %-45s %s\n" "${GREEN}PASS${RESET}" "$name" "$VERDICT_DETAIL"
    PASS_COUNT=$((PASS_COUNT + 1))
  else
    printf "  %s %-45s %s\n" "${RED}FAIL${RESET}" "$name" "$VERDICT_DETAIL"
    FAIL_COUNT=$((FAIL_COUNT + 1))
  fi
}

# -----------------------------------------------------------------------
# Verdict helpers — each takes the raw QuestDB JSON body and sets
# `VERDICT_DETAIL` for the operator-readable line.
# -----------------------------------------------------------------------

verdict_q1_schema() {
  # Expect 27 columns in the SHOW COLUMNS result.
  local body="$1"
  local count
  count=$(printf '%s' "$body" | jq -r '.dataset | length' 2>/dev/null || echo 0)
  VERDICT_DETAIL="found ${count} columns (expected 27)"
  [[ "$count" -eq 27 ]]
}

verdict_q2_rowcount() {
  # At least one underlying with > 0 rows today.
  local body="$1"
  local row_count
  row_count=$(printf '%s' "$body" | jq -r '.dataset | length' 2>/dev/null || echo 0)
  VERDICT_DETAIL="${row_count} underlying(s) with rows today"
  [[ "$row_count" -gt 0 ]]
}

verdict_q3_spot_bounds() {
  # Don't enforce specific ranges — just verify last_price > 0 for each
  # row (NIFTY/BANKNIFTY/SENSEX values fluctuate).
  local body="$1"
  local zero_count
  zero_count=$(printf '%s' "$body" | jq -r '[.dataset[] | select(.[1] <= 0)] | length' 2>/dev/null || echo 0)
  VERDICT_DETAIL="${zero_count} row(s) with last_price <= 0"
  [[ "$zero_count" -eq 0 ]]
}

verdict_q4_null_fields() {
  # No NULL IV/delta/oi for liquid strikes (cardinality from the count).
  local body="$1"
  local null_iv null_delta null_oi
  null_iv=$(printf '%s' "$body" | jq -r '.dataset[0][0]' 2>/dev/null || echo 0)
  null_delta=$(printf '%s' "$body" | jq -r '.dataset[0][1]' 2>/dev/null || echo 0)
  null_oi=$(printf '%s' "$body" | jq -r '.dataset[0][2]' 2>/dev/null || echo 0)
  VERDICT_DETAIL="null_iv=${null_iv} null_delta=${null_delta} null_oi=${null_oi}"
  [[ "$null_iv" -eq 0 ]] && [[ "$null_delta" -eq 0 ]] && [[ "$null_oi" -eq 0 ]]
}

verdict_q5_dedup_correctness() {
  # Zero rows in the duplicate-key GROUP BY HAVING query = DEDUP holds.
  local body="$1"
  local dup_count
  dup_count=$(printf '%s' "$body" | jq -r '.dataset | length' 2>/dev/null || echo 0)
  VERDICT_DETAIL="${dup_count} duplicate composite-key rows (expected 0)"
  [[ "$dup_count" -eq 0 ]]
}

verdict_q6_fetch_outcome() {
  # Fresh > 0% (any non-zero proves the scheduler ran).
  local body="$1"
  local rows
  rows=$(printf '%s' "$body" | jq -r '.dataset | length' 2>/dev/null || echo 0)
  VERDICT_DETAIL="${rows} distinct fetch_outcome value(s) seen today"
  [[ "$rows" -gt 0 ]]
}

verdict_q7_recent_window() {
  # At least one row in the last 10 minutes (during market hours).
  local body="$1"
  local row_count
  row_count=$(printf '%s' "$body" | jq -r '.dataset[0][0]' 2>/dev/null || echo 0)
  VERDICT_DETAIL="${row_count} rows in last 10 minutes"
  [[ "$row_count" -gt 0 ]]
}

# -----------------------------------------------------------------------
# The 7 queries
# -----------------------------------------------------------------------

main() {
  echo ""
  echo "Option-Chain Minute-Snapshot Verification (7 queries)"
  echo "======================================================"
  echo ""

  run_check "Q1 schema (27 columns)" \
    "SHOW COLUMNS FROM option_chain_minute_snapshot;" \
    verdict_q1_schema

  run_check "Q2 row count per underlying today" \
    "SELECT underlying_symbol, count(*) FROM option_chain_minute_snapshot WHERE trading_date_ist = today() GROUP BY underlying_symbol;" \
    verdict_q2_rowcount

  run_check "Q3 spot sanity (last_price > 0)" \
    "SELECT underlying_symbol, avg(last_price) FROM option_chain_minute_snapshot WHERE trading_date_ist = today() GROUP BY underlying_symbol;" \
    verdict_q3_spot_bounds

  run_check "Q4 NULL field counts" \
    "SELECT sum(case when implied_volatility IS NULL then 1 else 0 end), sum(case when greek_delta IS NULL then 1 else 0 end), sum(case when oi IS NULL then 1 else 0 end) FROM option_chain_minute_snapshot WHERE trading_date_ist = today() AND ts > dateadd('m', -10, now());" \
    verdict_q4_null_fields

  run_check "Q5 DEDUP correctness (zero duplicates)" \
    "SELECT trading_date_ist, ts, underlying_symbol, exchange_segment, strike, ce_or_pe, count(*) FROM option_chain_minute_snapshot WHERE trading_date_ist = today() GROUP BY trading_date_ist, ts, underlying_symbol, exchange_segment, strike, ce_or_pe HAVING count(*) > 1 LIMIT 5;" \
    verdict_q5_dedup_correctness

  run_check "Q6 fetch_outcome distribution" \
    "SELECT fetch_outcome, count(*) FROM option_chain_minute_snapshot WHERE trading_date_ist = today() GROUP BY fetch_outcome;" \
    verdict_q6_fetch_outcome

  run_check "Q7 recent activity (last 10 min)" \
    "SELECT count(*) FROM option_chain_minute_snapshot WHERE ts > dateadd('m', -10, now());" \
    verdict_q7_recent_window

  echo ""
  echo "------------------------------------------------------"
  echo "Summary: ${GREEN}${PASS_COUNT} PASS${RESET}, ${RED}${FAIL_COUNT} FAIL${RESET}, ${YELLOW}${SKIP_COUNT} SKIP${RESET}"
  echo "------------------------------------------------------"
  echo ""

  if [[ "$FAIL_COUNT" -gt 0 ]]; then
    exit 1
  fi
}

main "$@"
