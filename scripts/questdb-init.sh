#!/usr/bin/env bash
# =============================================================================
# QuestDB Auto-Init — Creates ALL tables + materialized views + DEDUP keys
# =============================================================================
# Runs automatically after `make docker-up`. Idempotent (IF NOT EXISTS).
# Single source of truth for QuestDB schema. Rust app DDL is defense-in-depth.
#
# Usage: ./scripts/questdb-init.sh [host] [port]
#   host: QuestDB HTTP host (default: localhost)
#   port: QuestDB HTTP port (default: 9000)
# =============================================================================

set -euo pipefail

QUESTDB_HOST="${1:-localhost}"
QUESTDB_PORT="${2:-9000}"
QUESTDB_URL="http://${QUESTDB_HOST}:${QUESTDB_PORT}/exec"

# Counters
PASS=0
FAIL=0
SKIP=0

# ---------------------------------------------------------------------------
# Helper: execute DDL via QuestDB HTTP API
# ---------------------------------------------------------------------------
execute_ddl() {
    local label="$1"
    local sql="$2"

    local http_code
    http_code=$(curl -s -o /dev/null -w "%{http_code}" \
        --max-time 10 \
        -G "${QUESTDB_URL}" \
        --data-urlencode "query=${sql}" 2>/dev/null) || {
        echo "  FAIL  ${label} (connection error)"
        FAIL=$((FAIL + 1))
        return 1
    }

    if [ "${http_code}" = "200" ]; then
        echo "  OK    ${label}"
        PASS=$((PASS + 1))
    else
        echo "  FAIL  ${label} (HTTP ${http_code})"
        FAIL=$((FAIL + 1))
    fi
}

# ---------------------------------------------------------------------------
# FIX 10 B1: never re-key an EXISTING table's dedup.
# An `ALTER TABLE ... DEDUP ENABLE UPSERT KEYS(...)` with a DIFFERENT
# (narrower) key set silently REPLACES the key — the live risk this closes:
# this script's stale 3-column ticks key would have DOWNGRADED the app's
# canonical 5-column key (ts, security_id, segment, capture_seq, feed),
# collapsing distinct Dhan/Groww ticks into one row (silent data loss).
# Policy: a dedup key is applied ONLY when the table has none yet; a table
# that already carries a key (the app's boot DDL owns several) is SKIPPED.
# ---------------------------------------------------------------------------
table_dedup_enabled() { # $1 = table name → rc 0 when dedup is already enabled
    local resp
    resp=$(curl -s --max-time 10 -G "${QUESTDB_URL}" \
        --data-urlencode "query=select dedup from tables() where table_name='${1}'" 2>/dev/null) || return 1
    echo "${resp}" | grep -q '"dataset":\[\[true\]\]'
}

apply_dedup_if_absent() { # $1 = label, $2 = table name, $3 = ALTER sql
    if table_dedup_enabled "$2"; then
        echo "  SKIP  $1 (dedup already enabled — never re-keying an existing table)"
        SKIP=$((SKIP + 1))
        return 0
    fi
    execute_ddl "$1" "$3"
}

# ---------------------------------------------------------------------------
# Wait for QuestDB to be ready
# ---------------------------------------------------------------------------
wait_for_questdb() {
    echo ""
    echo "  QuestDB Schema Init"
    echo "  ==================="
    echo ""
    echo "  Waiting for QuestDB at ${QUESTDB_HOST}:${QUESTDB_PORT}..."

    local retries=30
    local wait_sec=2
    for i in $(seq 1 ${retries}); do
        if curl -s -o /dev/null --max-time 2 "http://${QUESTDB_HOST}:${QUESTDB_PORT}/exec?query=SELECT%201" 2>/dev/null; then
            echo "  QuestDB ready (attempt ${i}/${retries})"
            echo ""
            return 0
        fi
        sleep "${wait_sec}"
    done

    echo "  ERROR: QuestDB not reachable after ${retries} attempts"
    exit 1
}

# ---------------------------------------------------------------------------
# Phase 1: Base Tables (15 tables)
# ---------------------------------------------------------------------------
create_base_tables() {
    echo "  Phase 1: Base Tables"
    echo "  --------------------"

    # 1. ticks — APP-OWNED, SKIPPED (FIX 10 B1). The app's boot DDL
    # (crates/storage/src/tick_persistence.rs) owns the canonical ticks
    # schema (feed + payload_hash + capture_seq columns) and its 5-column
    # DEDUP key (ts, security_id, segment, capture_seq, feed). This
    # script's stale column set + 3-column key must never touch it —
    # creating here forces a wrong-key window until the app boots.
    echo "  SKIP  ticks (app-owned — the app's boot DDL creates it with the canonical schema + dedup key)"
    SKIP=$((SKIP + 1))

    # 2. market_depth
    execute_ddl "market_depth" \
        "CREATE TABLE IF NOT EXISTS market_depth (segment SYMBOL, security_id LONG, level LONG, bid_qty LONG, ask_qty LONG, bid_orders LONG, ask_orders LONG, bid_price DOUBLE, ask_price DOUBLE, received_at TIMESTAMP, ts TIMESTAMP) TIMESTAMP(ts) PARTITION BY HOUR WAL"

    # 3. previous_close
    execute_ddl "previous_close" \
        "CREATE TABLE IF NOT EXISTS previous_close (segment SYMBOL, security_id LONG, prev_close DOUBLE, prev_oi LONG, ts TIMESTAMP) TIMESTAMP(ts) PARTITION BY DAY WAL"

    # 4. candles_1s (base for all materialized views)
    execute_ddl "candles_1s" \
        "CREATE TABLE IF NOT EXISTS candles_1s (segment SYMBOL, security_id LONG, open DOUBLE, high DOUBLE, low DOUBLE, close DOUBLE, volume LONG, oi LONG, tick_count INT, ts TIMESTAMP) TIMESTAMP(ts) PARTITION BY DAY WAL"

    # PR-E (2026-05-26): historical_candles DDL retired alongside the
    # deleted Dhan historical fetch chain. The migration SQL in
    # scripts/migrate-drop-historical-candles.sql drops the orphaned
    # table on existing deployments.

    # 6. instrument_build_metadata
    execute_ddl "instrument_build_metadata" \
        "CREATE TABLE IF NOT EXISTS instrument_build_metadata (csv_source SYMBOL, csv_row_count LONG, parsed_row_count LONG, index_count LONG, equity_count LONG, underlying_count LONG, derivative_count LONG, option_chain_count LONG, build_duration_ms LONG, build_timestamp TIMESTAMP, timestamp TIMESTAMP) TIMESTAMP(timestamp) PARTITION BY DAY WAL"

    # 7. fno_underlyings
    execute_ddl "fno_underlyings" \
        "CREATE TABLE IF NOT EXISTS fno_underlyings (underlying_symbol SYMBOL, price_feed_segment SYMBOL, derivative_segment SYMBOL, kind SYMBOL, underlying_security_id LONG, price_feed_security_id LONG, lot_size LONG, contract_count LONG, timestamp TIMESTAMP) TIMESTAMP(timestamp) PARTITION BY DAY WAL"

    # 8. derivative_contracts
    execute_ddl "derivative_contracts" \
        "CREATE TABLE IF NOT EXISTS derivative_contracts (underlying_symbol SYMBOL, instrument_kind SYMBOL, exchange_segment SYMBOL, option_type SYMBOL, symbol_name SYMBOL, security_id LONG, expiry_date STRING, strike_price DOUBLE, lot_size LONG, tick_size DOUBLE, display_name STRING, timestamp TIMESTAMP) TIMESTAMP(timestamp) PARTITION BY DAY WAL"

    # 9. subscribed_indices
    execute_ddl "subscribed_indices" \
        "CREATE TABLE IF NOT EXISTS subscribed_indices (symbol SYMBOL, exchange SYMBOL, category SYMBOL, subcategory SYMBOL, security_id LONG, timestamp TIMESTAMP) TIMESTAMP(timestamp) PARTITION BY DAY WAL"

    # 10. nse_holidays
    execute_ddl "nse_holidays" \
        "CREATE TABLE IF NOT EXISTS nse_holidays (name SYMBOL, holiday_type SYMBOL, ts TIMESTAMP) TIMESTAMP(ts) PARTITION BY YEAR WAL"

    # 11. index_constituents
    execute_ddl "index_constituents" \
        "CREATE TABLE IF NOT EXISTS index_constituents (index_name SYMBOL, symbol SYMBOL, isin STRING, weight DOUBLE, sector STRING, security_id LONG, ts TIMESTAMP) TIMESTAMP(ts) PARTITION BY MONTH WAL"

    # 12. option_greeks (DEDUP via ALTER TABLE in Phase 2 — QuestDB rejects inline DEDUP)
    execute_ddl "option_greeks" \
        "CREATE TABLE IF NOT EXISTS option_greeks (segment SYMBOL, security_id LONG, symbol_name SYMBOL, underlying_security_id LONG, underlying_symbol SYMBOL, strike_price DOUBLE, option_type SYMBOL, expiry_date SYMBOL, iv DOUBLE, delta DOUBLE, gamma DOUBLE, theta DOUBLE, vega DOUBLE, bs_price DOUBLE, intrinsic_value DOUBLE, extrinsic_value DOUBLE, spot_price DOUBLE, option_ltp DOUBLE, oi LONG, volume LONG, buildup_type SYMBOL, ts TIMESTAMP) TIMESTAMP(ts) PARTITION BY HOUR WAL"

    # 13. pcr_snapshots
    execute_ddl "pcr_snapshots" \
        "CREATE TABLE IF NOT EXISTS pcr_snapshots (underlying_symbol SYMBOL, expiry_date SYMBOL, pcr_oi DOUBLE, pcr_volume DOUBLE, total_put_oi LONG, total_call_oi LONG, total_put_volume LONG, total_call_volume LONG, sentiment SYMBOL, ts TIMESTAMP) TIMESTAMP(ts) PARTITION BY HOUR WAL"

    # 14. dhan_option_chain_raw
    execute_ddl "dhan_option_chain_raw" \
        "CREATE TABLE IF NOT EXISTS dhan_option_chain_raw (security_id LONG, segment SYMBOL, symbol_name SYMBOL, underlying_symbol SYMBOL, underlying_security_id LONG, underlying_segment SYMBOL, strike_price DOUBLE, option_type SYMBOL, expiry_date SYMBOL, spot_price DOUBLE, last_price DOUBLE, average_price DOUBLE, oi LONG, previous_close_price DOUBLE, previous_oi LONG, previous_volume LONG, volume LONG, top_bid_price DOUBLE, top_bid_quantity LONG, top_ask_price DOUBLE, top_ask_quantity LONG, implied_volatility DOUBLE, delta DOUBLE, theta DOUBLE, gamma DOUBLE, vega DOUBLE, ts TIMESTAMP) TIMESTAMP(ts) PARTITION BY HOUR WAL"

    # 15. greeks_verification
    execute_ddl "greeks_verification" \
        "CREATE TABLE IF NOT EXISTS greeks_verification (security_id LONG, segment SYMBOL, symbol_name SYMBOL, underlying_symbol SYMBOL, strike_price DOUBLE, option_type SYMBOL, our_iv DOUBLE, dhan_iv DOUBLE, iv_diff DOUBLE, our_delta DOUBLE, dhan_delta DOUBLE, delta_diff DOUBLE, our_gamma DOUBLE, dhan_gamma DOUBLE, gamma_diff DOUBLE, our_theta DOUBLE, dhan_theta DOUBLE, theta_diff DOUBLE, our_vega DOUBLE, dhan_vega DOUBLE, vega_diff DOUBLE, match_status SYMBOL, ts TIMESTAMP) TIMESTAMP(ts) PARTITION BY DAY WAL"

    echo ""
}

# ---------------------------------------------------------------------------
# Phase 2: DEDUP Keys (ALTER TABLE)
# ---------------------------------------------------------------------------
apply_dedup_keys() {
    echo "  Phase 2: DEDUP Keys"
    echo "  -------------------"

    # FIX 10 B1: ticks DEDUP retired here — app-owned (see Phase 1 note).
    echo "  SKIP  ticks DEDUP (app-owned canonical key — this script never touches it)"
    SKIP=$((SKIP + 1))

    # FIX 10 B1: every remaining key goes through apply_dedup_if_absent —
    # a table that ALREADY carries a dedup key (the app's boot DDL owns
    # several and may have keyed them wider) is never re-keyed.
    apply_dedup_if_absent "market_depth DEDUP" market_depth \
        "ALTER TABLE market_depth DEDUP ENABLE UPSERT KEYS(ts, security_id, segment, level)"

    apply_dedup_if_absent "previous_close DEDUP" previous_close \
        "ALTER TABLE previous_close DEDUP ENABLE UPSERT KEYS(ts, security_id, segment)"

    apply_dedup_if_absent "candles_1s DEDUP" candles_1s \
        "ALTER TABLE candles_1s DEDUP ENABLE UPSERT KEYS(ts, security_id, segment)"

    # PR-E (2026-05-26): historical_candles DEDUP retired.

    apply_dedup_if_absent "instrument_build_metadata DEDUP" instrument_build_metadata \
        "ALTER TABLE instrument_build_metadata DEDUP ENABLE UPSERT KEYS(timestamp)"

    apply_dedup_if_absent "fno_underlyings DEDUP" fno_underlyings \
        "ALTER TABLE fno_underlyings DEDUP ENABLE UPSERT KEYS(timestamp, underlying_symbol)"

    apply_dedup_if_absent "derivative_contracts DEDUP" derivative_contracts \
        "ALTER TABLE derivative_contracts DEDUP ENABLE UPSERT KEYS(timestamp, underlying_symbol, expiry_date, strike_price, option_type, security_id)"

    apply_dedup_if_absent "subscribed_indices DEDUP" subscribed_indices \
        "ALTER TABLE subscribed_indices DEDUP ENABLE UPSERT KEYS(timestamp, symbol)"

    apply_dedup_if_absent "nse_holidays DEDUP" nse_holidays \
        "ALTER TABLE nse_holidays DEDUP ENABLE UPSERT KEYS(ts, name)"

    apply_dedup_if_absent "index_constituents DEDUP" index_constituents \
        "ALTER TABLE index_constituents DEDUP ENABLE UPSERT KEYS(ts, index_name, symbol)"

    # Greeks tables (12-15)
    apply_dedup_if_absent "option_greeks DEDUP" option_greeks \
        "ALTER TABLE option_greeks DEDUP ENABLE UPSERT KEYS(ts, security_id, segment)"

    apply_dedup_if_absent "pcr_snapshots DEDUP" pcr_snapshots \
        "ALTER TABLE pcr_snapshots DEDUP ENABLE UPSERT KEYS(ts, underlying_symbol, expiry_date)"

    apply_dedup_if_absent "dhan_option_chain_raw DEDUP" dhan_option_chain_raw \
        "ALTER TABLE dhan_option_chain_raw DEDUP ENABLE UPSERT KEYS(ts, security_id, segment)"

    apply_dedup_if_absent "greeks_verification DEDUP" greeks_verification \
        "ALTER TABLE greeks_verification DEDUP ENABLE UPSERT KEYS(ts, security_id, segment)"

    echo ""
}

# ---------------------------------------------------------------------------
# Phase 3: Candle tables — owned by the app candle engine (NOT this script)
# ---------------------------------------------------------------------------
# Since the "one common candle engine" convergence (#1189), every candle
# timeframe (candles_1m … candles_1d) is a REAL TABLE that the running app
# folds directly from `ticks` (O(1) per tick) and seals via its aggregator —
# NOT a QuestDB materialized view. The app's idempotent boot DDL creates +
# DEDUP-keys those tables and drops any stale matview squatting the name.
#
# This script therefore MUST NOT create candle materialized views: doing so
# collides with the app-owned tables and returns HTTP 400 (the historical
# "13 FAIL"). Candles are intentionally left to the engine — `ticks` is the
# single source of truth, candle tables only (no matviews). Operator
# 2026-06-23: "using ticks we need all candle tables as tables only — no
# mat view".
create_candle_note() {
    echo "  Phase 3: Candle tables"
    echo "  ----------------------"
    echo "  SKIP  candles_* — owned by the app candle engine (tables folded from ticks; no matviews)"
    echo ""
}

# ---------------------------------------------------------------------------
# Phase 4: Verification Dashboard
# ---------------------------------------------------------------------------
show_dashboard() {
    echo "  Phase 4: Verification"
    echo "  ---------------------"

    # Query actual table list from QuestDB
    local response
    response=$(curl -s --max-time 5 \
        -G "${QUESTDB_URL}" \
        --data-urlencode "query=SELECT table_name FROM information_schema.tables() ORDER BY table_name" 2>/dev/null) || {
        echo "  WARN  Could not query table list"
        return
    }

    # Count tables
    local table_count
    # QuestDB /exec returns a top-level "count":N (number of dataset rows).
    # The old `grep '"table_name"'` matched only the column header → always 1.
    table_count=$(echo "${response}" | grep -oE '"count":[0-9]+' | grep -oE '[0-9]+' | tail -1 2>/dev/null || echo "?")

    echo "  Tables in QuestDB: ${table_count}"
    echo ""
    echo "  ============================================"
    echo "  Summary: ${PASS} OK / ${FAIL} FAIL / ${SKIP} SKIP"
    echo "  ============================================"

    if [ "${FAIL}" -gt 0 ]; then
        echo ""
        echo "  WARNING: ${FAIL} DDL statement(s) failed."
        echo "  Check QuestDB logs: docker logs tv-questdb"
        echo ""
        exit 1
    fi

    echo ""
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
wait_for_questdb
create_base_tables
apply_dedup_keys
create_candle_note
show_dashboard
