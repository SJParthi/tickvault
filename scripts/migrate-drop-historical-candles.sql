-- PR-E (2026-05-26): Drop the orphaned `historical_candles` table.
--
-- The Dhan historical fetch chain was deleted across PRs #803 (pre-market
-- buffer), #804 (gap_fill), #805 (cross_verify), #806 (candle_fetcher),
-- and this PR (candle_persistence + historical_candles table).
--
-- The `historical_candles` table has no writers and no readers after this
-- series. SEBI 5-year retention does NOT apply: the table only ever held
-- Dhan REST `/charts/historical` results (replayable from Dhan on demand
-- if forensic reconstruction is ever needed), never broker-traded data.
--
-- Run this against the prod QuestDB instance ONCE after this PR is
-- deployed. The boot DDL no longer creates the table.
--
-- Idempotent: `DROP TABLE IF EXISTS` succeeds whether or not the table
-- exists. Safe to re-run.

DROP TABLE IF EXISTS historical_candles;
