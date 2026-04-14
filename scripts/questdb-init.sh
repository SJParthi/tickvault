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

    # 1. ticks
    execute_ddl "ticks" \
        "CREATE TABLE IF NOT EXISTS ticks (segment SYMBOL, security_id LONG, ltp DOUBLE, open DOUBLE, high DOUBLE, low DOUBLE, close DOUBLE, volume LONG, oi LONG, avg_price DOUBLE, last_trade_qty LONG, total_buy_qty LONG, total_sell_qty LONG, exchange_timestamp LONG, received_at TIMESTAMP, ts TIMESTAMP) TIMESTAMP(ts) PARTITION BY HOUR WAL"

    # 2. market_depth
    execute_ddl "market_depth" \
        "CREATE TABLE IF NOT EXISTS market_depth (segment SYMBOL, security_id LONG, level LONG, bid_qty LONG, ask_qty LONG, bid_orders LONG, ask_orders LONG, bid_price DOUBLE, ask_price DOUBLE, received_at TIMESTAMP, ts TIMESTAMP) TIMESTAMP(ts) PARTITION BY HOUR WAL"

    # 3. previous_close
    execute_ddl "previous_close" \
        "CREATE TABLE IF NOT EXISTS previous_close (segment SYMBOL, security_id LONG, prev_close DOUBLE, prev_oi LONG, ts TIMESTAMP) TIMESTAMP(ts) PARTITION BY DAY WAL"

    # 4. candles_1s (base for all materialized views)
    execute_ddl "candles_1s" \
        "CREATE TABLE IF NOT EXISTS candles_1s (segment SYMBOL, security_id LONG, open DOUBLE, high DOUBLE, low DOUBLE, close DOUBLE, volume LONG, oi LONG, tick_count INT, ts TIMESTAMP) TIMESTAMP(ts) PARTITION BY DAY WAL"

    # 5. historical_candles
    execute_ddl "historical_candles" \
        "CREATE TABLE IF NOT EXISTS historical_candles (segment SYMBOL, timeframe SYMBOL, security_id LONG, open DOUBLE, high DOUBLE, low DOUBLE, close DOUBLE, volume LONG, oi LONG, ts TIMESTAMP) TIMESTAMP(ts) PARTITION BY DAY WAL"

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

    execute_ddl "ticks DEDUP" \
        "ALTER TABLE ticks DEDUP ENABLE UPSERT KEYS(ts, security_id, segment)"

    execute_ddl "market_depth DEDUP" \
        "ALTER TABLE market_depth DEDUP ENABLE UPSERT KEYS(ts, security_id, segment, level)"

    execute_ddl "previous_close DEDUP" \
        "ALTER TABLE previous_close DEDUP ENABLE UPSERT KEYS(ts, security_id, segment)"

    execute_ddl "candles_1s DEDUP" \
        "ALTER TABLE candles_1s DEDUP ENABLE UPSERT KEYS(ts, security_id, segment)"

    execute_ddl "historical_candles DEDUP" \
        "ALTER TABLE historical_candles DEDUP ENABLE UPSERT KEYS(ts, security_id, timeframe, segment)"

    execute_ddl "instrument_build_metadata DEDUP" \
        "ALTER TABLE instrument_build_metadata DEDUP ENABLE UPSERT KEYS(timestamp)"

    execute_ddl "fno_underlyings DEDUP" \
        "ALTER TABLE fno_underlyings DEDUP ENABLE UPSERT KEYS(timestamp, underlying_symbol)"

    execute_ddl "derivative_contracts DEDUP" \
        "ALTER TABLE derivative_contracts DEDUP ENABLE UPSERT KEYS(timestamp, underlying_symbol, expiry_date, strike_price, option_type, security_id)"

    execute_ddl "subscribed_indices DEDUP" \
        "ALTER TABLE subscribed_indices DEDUP ENABLE UPSERT KEYS(timestamp, symbol)"

    execute_ddl "nse_holidays DEDUP" \
        "ALTER TABLE nse_holidays DEDUP ENABLE UPSERT KEYS(ts, name)"

    execute_ddl "index_constituents DEDUP" \
        "ALTER TABLE index_constituents DEDUP ENABLE UPSERT KEYS(ts, index_name, symbol)"

    # Greeks tables (12-15)
    execute_ddl "option_greeks DEDUP" \
        "ALTER TABLE option_greeks DEDUP ENABLE UPSERT KEYS(ts, security_id, segment)"

    execute_ddl "pcr_snapshots DEDUP" \
        "ALTER TABLE pcr_snapshots DEDUP ENABLE UPSERT KEYS(ts, underlying_symbol, expiry_date)"

    execute_ddl "dhan_option_chain_raw DEDUP" \
        "ALTER TABLE dhan_option_chain_raw DEDUP ENABLE UPSERT KEYS(ts, security_id, segment)"

    execute_ddl "greeks_verification DEDUP" \
        "ALTER TABLE greeks_verification DEDUP ENABLE UPSERT KEYS(ts, security_id, segment)"

    echo ""
}

# ---------------------------------------------------------------------------
# Phase 3: Materialized Views (18 views, dependency order)
# ---------------------------------------------------------------------------
create_materialized_views() {
    echo "  Phase 3: Materialized Views (18)"
    echo "  ---------------------------------"

    # Tier 1: From candles_1s (5 views)
    execute_ddl "candles_5s" \
        "CREATE MATERIALIZED VIEW IF NOT EXISTS candles_5s AS (SELECT ts, security_id, segment, first(open) AS open, max(high) AS high, min(low) AS low, last(close) AS close, sum(volume) AS volume, last(oi) AS oi, sum(tick_count) AS tick_count FROM candles_1s SAMPLE BY 5s ALIGN TO CALENDAR WITH OFFSET '00:00')"

    execute_ddl "candles_10s" \
        "CREATE MATERIALIZED VIEW IF NOT EXISTS candles_10s AS (SELECT ts, security_id, segment, first(open) AS open, max(high) AS high, min(low) AS low, last(close) AS close, sum(volume) AS volume, last(oi) AS oi, sum(tick_count) AS tick_count FROM candles_1s SAMPLE BY 10s ALIGN TO CALENDAR WITH OFFSET '00:00')"

    execute_ddl "candles_15s" \
        "CREATE MATERIALIZED VIEW IF NOT EXISTS candles_15s AS (SELECT ts, security_id, segment, first(open) AS open, max(high) AS high, min(low) AS low, last(close) AS close, sum(volume) AS volume, last(oi) AS oi, sum(tick_count) AS tick_count FROM candles_1s SAMPLE BY 15s ALIGN TO CALENDAR WITH OFFSET '00:00')"

    execute_ddl "candles_30s" \
        "CREATE MATERIALIZED VIEW IF NOT EXISTS candles_30s AS (SELECT ts, security_id, segment, first(open) AS open, max(high) AS high, min(low) AS low, last(close) AS close, sum(volume) AS volume, last(oi) AS oi, sum(tick_count) AS tick_count FROM candles_1s SAMPLE BY 30s ALIGN TO CALENDAR WITH OFFSET '00:00')"

    execute_ddl "candles_1m" \
        "CREATE MATERIALIZED VIEW IF NOT EXISTS candles_1m AS (SELECT ts, security_id, segment, first(open) AS open, max(high) AS high, min(low) AS low, last(close) AS close, sum(volume) AS volume, last(oi) AS oi, sum(tick_count) AS tick_count FROM candles_1s SAMPLE BY 1m ALIGN TO CALENDAR WITH OFFSET '00:00')"

    # Tier 2: From candles_1m (3 views)
    execute_ddl "candles_2m" \
        "CREATE MATERIALIZED VIEW IF NOT EXISTS candles_2m AS (SELECT ts, security_id, segment, first(open) AS open, max(high) AS high, min(low) AS low, last(close) AS close, sum(volume) AS volume, last(oi) AS oi FROM candles_1m SAMPLE BY 2m ALIGN TO CALENDAR WITH OFFSET '00:00')"

    execute_ddl "candles_3m" \
        "CREATE MATERIALIZED VIEW IF NOT EXISTS candles_3m AS (SELECT ts, security_id, segment, first(open) AS open, max(high) AS high, min(low) AS low, last(close) AS close, sum(volume) AS volume, last(oi) AS oi FROM candles_1m SAMPLE BY 3m ALIGN TO CALENDAR WITH OFFSET '00:00')"

    execute_ddl "candles_5m" \
        "CREATE MATERIALIZED VIEW IF NOT EXISTS candles_5m AS (SELECT ts, security_id, segment, first(open) AS open, max(high) AS high, min(low) AS low, last(close) AS close, sum(volume) AS volume, last(oi) AS oi FROM candles_1m SAMPLE BY 5m ALIGN TO CALENDAR WITH OFFSET '00:00')"

    # Tier 3: From candles_5m (2 views)
    execute_ddl "candles_10m" \
        "CREATE MATERIALIZED VIEW IF NOT EXISTS candles_10m AS (SELECT ts, security_id, segment, first(open) AS open, max(high) AS high, min(low) AS low, last(close) AS close, sum(volume) AS volume, last(oi) AS oi FROM candles_5m SAMPLE BY 10m ALIGN TO CALENDAR WITH OFFSET '00:00')"

    execute_ddl "candles_15m" \
        "CREATE MATERIALIZED VIEW IF NOT EXISTS candles_15m AS (SELECT ts, security_id, segment, first(open) AS open, max(high) AS high, min(low) AS low, last(close) AS close, sum(volume) AS volume, last(oi) AS oi FROM candles_5m SAMPLE BY 15m ALIGN TO CALENDAR WITH OFFSET '00:00')"

    # Tier 4: From candles_15m (1 view)
    execute_ddl "candles_30m" \
        "CREATE MATERIALIZED VIEW IF NOT EXISTS candles_30m AS (SELECT ts, security_id, segment, first(open) AS open, max(high) AS high, min(low) AS low, last(close) AS close, sum(volume) AS volume, last(oi) AS oi FROM candles_15m SAMPLE BY 30m ALIGN TO CALENDAR WITH OFFSET '00:00')"

    # Tier 5: From candles_30m (1 view)
    execute_ddl "candles_1h" \
        "CREATE MATERIALIZED VIEW IF NOT EXISTS candles_1h AS (SELECT ts, security_id, segment, first(open) AS open, max(high) AS high, min(low) AS low, last(close) AS close, sum(volume) AS volume, last(oi) AS oi FROM candles_30m SAMPLE BY 1h ALIGN TO CALENDAR WITH OFFSET '00:00')"

    # Tier 6: From candles_1h (3 views)
    execute_ddl "candles_2h" \
        "CREATE MATERIALIZED VIEW IF NOT EXISTS candles_2h AS (SELECT ts, security_id, segment, first(open) AS open, max(high) AS high, min(low) AS low, last(close) AS close, sum(volume) AS volume, last(oi) AS oi FROM candles_1h SAMPLE BY 2h ALIGN TO CALENDAR WITH OFFSET '00:00')"

    execute_ddl "candles_3h" \
        "CREATE MATERIALIZED VIEW IF NOT EXISTS candles_3h AS (SELECT ts, security_id, segment, first(open) AS open, max(high) AS high, min(low) AS low, last(close) AS close, sum(volume) AS volume, last(oi) AS oi FROM candles_1h SAMPLE BY 3h ALIGN TO CALENDAR WITH OFFSET '00:00')"

    execute_ddl "candles_4h" \
        "CREATE MATERIALIZED VIEW IF NOT EXISTS candles_4h AS (SELECT ts, security_id, segment, first(open) AS open, max(high) AS high, min(low) AS low, last(close) AS close, sum(volume) AS volume, last(oi) AS oi FROM candles_1h SAMPLE BY 4h ALIGN TO CALENDAR WITH OFFSET '00:00')"

    # Tier 7: From candles_1h (1 view)
    execute_ddl "candles_1d" \
        "CREATE MATERIALIZED VIEW IF NOT EXISTS candles_1d AS (SELECT ts, security_id, segment, first(open) AS open, max(high) AS high, min(low) AS low, last(close) AS close, sum(volume) AS volume, last(oi) AS oi FROM candles_1h SAMPLE BY 1d ALIGN TO CALENDAR WITH OFFSET '00:00')"

    # Tier 8: From candles_1d (2 views)
    execute_ddl "candles_7d" \
        "CREATE MATERIALIZED VIEW IF NOT EXISTS candles_7d AS (SELECT ts, security_id, segment, first(open) AS open, max(high) AS high, min(low) AS low, last(close) AS close, sum(volume) AS volume, last(oi) AS oi FROM candles_1d SAMPLE BY 7d ALIGN TO CALENDAR WITH OFFSET '00:00')"

    execute_ddl "candles_1M" \
        "CREATE MATERIALIZED VIEW IF NOT EXISTS candles_1M AS (SELECT ts, security_id, segment, first(open) AS open, max(high) AS high, min(low) AS low, last(close) AS close, sum(volume) AS volume, last(oi) AS oi FROM candles_1d SAMPLE BY 1M ALIGN TO CALENDAR WITH OFFSET '00:00')"

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
    table_count=$(echo "${response}" | grep -o '"table_name"' | wc -l 2>/dev/null || echo "?")

    echo "  Tables in QuestDB: ${table_count}"
    echo ""
    echo "  ============================================"
    echo "  Summary: ${PASS} OK / ${FAIL} FAIL"
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
create_materialized_views
show_dashboard
