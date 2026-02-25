-- =============================================================================
-- dhan-live-trader — QuestDB Verification Queries
-- =============================================================================
-- Run these in QuestDB Web Console (localhost:9000) or via psql
-- Connection: PostgreSQL wire protocol → localhost:8812 → database: qdb
-- Credentials: admin / quest (dev defaults)
-- =============================================================================

-- ---------------------------------------------------------------------------
-- 1. Connection Health Check
-- ---------------------------------------------------------------------------
-- If this returns the QuestDB version, your connection is working.
SELECT version();

-- ---------------------------------------------------------------------------
-- 2. List All Tables
-- ---------------------------------------------------------------------------
SHOW TABLES;

-- ---------------------------------------------------------------------------
-- 3. Server Status
-- ---------------------------------------------------------------------------
SELECT
    now()          AS server_time,
    systimestamp() AS system_timestamp;

-- ---------------------------------------------------------------------------
-- 4. Tick Data Queries (available after Block 01+ starts collecting)
-- ---------------------------------------------------------------------------

-- Latest 100 ticks
-- SELECT * FROM ticks
-- ORDER BY timestamp DESC
-- LIMIT 100;

-- Ticks from the last hour
-- SELECT * FROM ticks
-- WHERE timestamp > dateadd('h', -1, now())
-- ORDER BY timestamp DESC;

-- Tick count by exchange segment
-- SELECT exchange_segment, count() AS tick_count
-- FROM ticks
-- WHERE timestamp IN today()
-- GROUP BY exchange_segment
-- ORDER BY tick_count DESC;

-- ---------------------------------------------------------------------------
-- 5. 1-Minute OHLCV Candles (QuestDB SAMPLE BY)
-- ---------------------------------------------------------------------------
-- QuestDB's SAMPLE BY is purpose-built for time-series aggregation.
-- ALIGN TO CALENDAR WITH OFFSET '05:30' aligns to IST (UTC+5:30).

-- SELECT
--     timestamp,
--     first(last_traded_price) AS open,
--     max(last_traded_price)   AS high,
--     min(last_traded_price)   AS low,
--     last(last_traded_price)  AS close,
--     sum(volume)              AS volume
-- FROM ticks
-- WHERE security_id = 12345
--   AND timestamp IN today()
-- SAMPLE BY 1m
-- ALIGN TO CALENDAR WITH OFFSET '05:30';

-- ---------------------------------------------------------------------------
-- 6. Order Audit Trail Queries (available after OMS is built)
-- ---------------------------------------------------------------------------

-- All orders today
-- SELECT * FROM orders
-- WHERE created_at IN today()
-- ORDER BY created_at DESC;

-- Orders by state
-- SELECT order_state, count() AS order_count
-- FROM orders
-- WHERE created_at IN today()
-- GROUP BY order_state;

-- ---------------------------------------------------------------------------
-- 7. Data Integrity Checks
-- ---------------------------------------------------------------------------

-- Check for gaps in tick data (no ticks for >1 minute during market hours)
-- SELECT
--     timestamp,
--     datediff('s', lag(timestamp) OVER (ORDER BY timestamp), timestamp) AS gap_seconds
-- FROM ticks
-- WHERE security_id = 12345
--   AND timestamp IN today()
--   AND gap_seconds > 60;

-- Duplicate tick detection
-- SELECT security_id, timestamp, count() AS duplicate_count
-- FROM ticks
-- WHERE timestamp IN today()
-- GROUP BY security_id, timestamp
-- HAVING duplicate_count > 1;

-- ---------------------------------------------------------------------------
-- 8. Performance Monitoring
-- ---------------------------------------------------------------------------

-- Tick ingestion rate per second
-- SELECT
--     timestamp,
--     count() AS ticks_per_second
-- FROM ticks
-- WHERE timestamp > dateadd('m', -5, now())
-- SAMPLE BY 1s;

-- Table sizes
-- SELECT * FROM table_partitions('ticks');
