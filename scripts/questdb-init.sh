#!/usr/bin/env bash
# =============================================================================
# QuestDB Auto-Init — Creates the LIVE base tables + DEDUP keys (pruned 2026-07-18)
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
# Phase 1: Base Tables (live set only — Track A prune, 2026-07-18)
# ---------------------------------------------------------------------------
# The 13 retired-table CREATEs (instrument/misc/greeks era + candles_1s)
# were PRUNED 2026-07-18: the boot-time retired-object sweep
# (crates/storage/src/shadow_persistence.rs::drop_legacy_candle_objects)
# drops them, and re-creating them here resurrected dropped tables on
# every `make docker-up`. Every LIVE table's authoritative DDL is its
# Rust ensure fn (boot-reachable — see
# crates/app/tests/ensure_ddl_boot_wiring_guard.rs); this script keeps
# only `ticks` (no app-side writer/ensure since the 2026-07-17 stage-2
# sweep, but the scoreboard READS it, so a fresh box gets the final
# schema + 5-key DEDUP here). SEBI tables (instrument_lifecycle*,
# index_constituency, ws_event_audit, order_audit family) are created by
# their own boot ensures and are NEVER dropped.
# ---------------------------------------------------------------------------
create_base_tables() {
    echo "  Phase 1: Base Tables"
    echo "  --------------------"

    # 1. ticks — the FINAL (pre-deletion tick_persistence.rs) schema:
    #    feed + payload_hash + capture_seq columns included so the
    #    scoreboard's feed-sliced reads work on a fresh box.
    execute_ddl "ticks" \
        "CREATE TABLE IF NOT EXISTS ticks (feed SYMBOL, segment SYMBOL, security_id LONG, ltp DOUBLE, open DOUBLE, high DOUBLE, low DOUBLE, close DOUBLE, volume LONG, oi LONG, avg_price DOUBLE, last_trade_qty LONG, total_buy_qty LONG, total_sell_qty LONG, exchange_timestamp LONG, received_at TIMESTAMP, payload_hash LONG, capture_seq LONG, ts TIMESTAMP) TIMESTAMP(ts) PARTITION BY HOUR WAL"

    echo ""
}

# ---------------------------------------------------------------------------
# Phase 2: DEDUP Keys (ALTER TABLE)
# ---------------------------------------------------------------------------
apply_dedup_keys() {
    echo "  Phase 2: DEDUP Keys"
    echo "  -------------------"

    # The final 5-key ticks DEDUP (TICK-SEQ-01 + feed-in-key) — the old
    # 3-key (ts, security_id, segment) collapsed distinct intra-second
    # arrivals and cross-feed rows.
    execute_ddl "ticks DEDUP" \
        "ALTER TABLE ticks DEDUP ENABLE UPSERT KEYS(ts, security_id, segment, capture_seq, feed)"

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
create_candle_note
show_dashboard
