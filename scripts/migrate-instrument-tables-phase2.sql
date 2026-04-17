-- Phase 2 lifecycle migration (2026-04-17)
--
-- Before this migration the three snapshot tables held only a business-key
-- schema (no `status`, `last_seen_date`, `expired_date`). Run this script
-- ONCE on the target QuestDB instance BEFORE deploying the new binary.
--
-- Because every row in these three tables is rebuilt from the Dhan instrument
-- master CSV on every app start, DROP + CREATE is safe. No user trade data
-- lives here. SEBI 5-year retention applies to `orders`, `trades`, `ticks`,
-- `candles`, `option_greeks`, etc. — NOT these derivative lookup tables.
--
-- After this script:
--   1. Deploy the new tickvault binary.
--   2. Boot. `ensure_instrument_tables()` will CREATE the new DDL with
--      lifecycle columns.
--   3. The first universe build populates all rows with `status='active'`
--      and `last_seen_date=today_ist`.
--
-- How to run:
--   curl -G 'http://<questdb-host>:9000/exec' \
--     --data-urlencode 'query=<paste-each-statement>'
--
-- Or via the QuestDB Web Console (localhost:9000) — paste and Run query.

DROP TABLE IF EXISTS fno_underlyings;
DROP TABLE IF EXISTS derivative_contracts;
DROP TABLE IF EXISTS subscribed_indices;

-- `instrument_build_metadata` is NOT dropped — it is the build-history table
-- and its accumulating-rows design is correct.
