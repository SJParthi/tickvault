-- PR #3 (2026-05-19) — Drop retired Greeks pipeline tables.
--
-- Run ONCE on each QuestDB instance after deploying the PR #3 binary.
-- Idempotent: each statement uses `DROP TABLE IF EXISTS` so re-runs are
-- safe. Operator runs this manually via QuestDB web console at
-- http://localhost:9000 OR via SSM RunCommand on AWS prod.
--
-- Context:
-- - PR #3 retires the entire greeks compute pipeline (Black-Scholes,
--   IV solver, PCR, Buildup classification) because under the 4-IDX_I
--   LOCKED_UNIVERSE (NIFTY/BANKNIFTY/SENSEX/INDIA VIX) there are no
--   live option contracts on the WebSocket to compute Greeks from.
-- - The 5 in-memory ParsedTick fields (iv, delta, gamma, theta, vega)
--   stay defined in `crates/common/src/tick_types.rs` — they remain
--   `f64::NAN` for indices/equities and act as zero-cost padding.
-- - The QuestDB column definitions on `candles_1s` and `ticks`
--   (iv/delta/gamma/theta/vega DOUBLE) are KEPT ON DISK per SEBI
--   5-year retention — historical rows produced before this PR have
--   real values; new rows simply have NULL in those columns.
-- - Only the 4 greeks-specific TABLES are dropped here:
--     option_greeks         (per-contract Greeks snapshot, 1 row/contract/min)
--     pcr_snapshots         (per-underlying PCR snapshot)
--     dhan_option_chain_raw (raw Dhan option-chain JSON capture)
--     greeks_verification   (computed-vs-Dhan-reported cross-check audit)
-- - Option Chain REST overlay (PR #8) will create its own table
--   `option_chain_minute_snapshot` for Dhan-computed greeks. That table
--   ALREADY exists (Wave-5 §K-L13); this script does NOT touch it.

-- =========================================================================

DROP TABLE IF EXISTS option_greeks;

DROP TABLE IF EXISTS pcr_snapshots;

DROP TABLE IF EXISTS dhan_option_chain_raw;

DROP TABLE IF EXISTS greeks_verification;

-- =========================================================================
-- Sanity check (operator runs after the DROPs):
--   SELECT table_name
--   FROM tables()
--   WHERE table_name IN ('option_greeks', 'pcr_snapshots',
--                        'dhan_option_chain_raw', 'greeks_verification');
-- Expected result: 0 rows.
--
-- The `candles_1s` and `ticks` tables retain their iv/delta/gamma/theta/vega
-- DOUBLE columns; that's intentional. Verify with:
--   SHOW COLUMNS FROM candles_1s WHERE column IN
--     ('iv', 'delta', 'gamma', 'theta', 'vega');
-- Expected: 5 rows.
