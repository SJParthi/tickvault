-- PR #2 (2026-05-18) one-shot migration — drop all movers tables.
--
-- The movers pipeline (top_movers + option_movers + 22-TF cascade)
-- was retired in PR #2 of the AWS-lifecycle 14-PR sequence. Under
-- the 4-IDX_I-only universe (NIFTY/BANKNIFTY/SENSEX/INDIA VIX) the
-- top-N gainers/losers/most-active queries are meaningless.
--
-- This script drops the legacy QuestDB tables that the deleted code
-- populated. Run ONCE against the prod QuestDB instance after PR #2
-- merges and before re-deploying the application binary.
--
-- Idempotent: `DROP TABLE IF EXISTS` — safe to re-run.
--
-- Companion code change: `crates/storage/src/partition_manager.rs`
-- removed all of these table names from `HOUR_PARTITIONED_TABLES`
-- so the partition-retention worker no longer expects them. The
-- 4 audit tables (depth_rebalance_audit, phase2_audit, etc.) and
-- the candles_* family are unaffected.

DROP TABLE IF EXISTS stock_movers;
DROP TABLE IF EXISTS option_movers;

-- 22-TF movers cascade (Phase 8 — also retired in PR #2)
DROP TABLE IF EXISTS movers_1s;
DROP TABLE IF EXISTS movers_5s;
DROP TABLE IF EXISTS movers_10s;
DROP TABLE IF EXISTS movers_15s;
DROP TABLE IF EXISTS movers_30s;
DROP TABLE IF EXISTS movers_1m;
DROP TABLE IF EXISTS movers_2m;
DROP TABLE IF EXISTS movers_3m;
DROP TABLE IF EXISTS movers_4m;
DROP TABLE IF EXISTS movers_5m;
DROP TABLE IF EXISTS movers_6m;
DROP TABLE IF EXISTS movers_7m;
DROP TABLE IF EXISTS movers_8m;
DROP TABLE IF EXISTS movers_9m;
DROP TABLE IF EXISTS movers_10m;
DROP TABLE IF EXISTS movers_11m;
DROP TABLE IF EXISTS movers_12m;
DROP TABLE IF EXISTS movers_13m;
DROP TABLE IF EXISTS movers_14m;
DROP TABLE IF EXISTS movers_15m;
DROP TABLE IF EXISTS movers_30m;
DROP TABLE IF EXISTS movers_1h;
