# QuestDB Cleanup Migration — Drop ALL Unwanted Tables + Mat Views

> **Status:** MIGRATION PLAN (no SQL executed yet).
> **Authority:** CLAUDE.md > operator-charter-forever.md > this file.
> **Created:** 2026-05-18 in response to operator screenshot showing 39 tables + 9 mat views in LIVE QuestDB.
> **Operator question verbatim:** *"we have unwanted tables right dude why bro?"*
> **Honest answer:** the cleanup PR hasn't been executed yet. Design docs say 33 tables + 0 mat views; LIVE has 39 + 9. This doc bridges design → LIVE state with exact SQL.

---

## §0. Auto-driver one-liner

> "Sir, you saw 48 storage boxes in the warehouse — we only need 33. The other 15 are old boxes from the previous shop layout. We need to throw them out + we also need to remove 9 'auto-compute machines' (mat views) because we now compute everything in worker's head (RAM) before storing. This doc lists EVERY box to drop and every NEW box to create. After this runs: 33 boxes, 0 auto-compute, clean warehouse."

---

## §1. The LIVE state today (from operator's screenshot)

### 39 base tables

| # | Table | Verdict |
|---|---|---|
| 1 | `aggregator_seal_audit` | 🔴 DROP — info already in candle tables |
| 2 | `auth_renewal_audit` | ✅ KEEP |
| 3 | `boot_audit` | ✅ KEEP |
| 4 | `candles_15m_shadow` | 🔴 DROP — Wave 6 shadow design retired |
| 5 | `candles_1d_shadow` | 🔴 DROP |
| 6 | `candles_1h_shadow` | 🔴 DROP |
| 7 | `candles_1m_shadow` | 🔴 DROP |
| 8 | `candles_1s` | 🔴 DROP — sub-second TF dropped from scope |
| 9 | `candles_2h_shadow` | 🔴 DROP |
| 10 | `candles_30m_shadow` | 🔴 DROP |
| 11 | `candles_3h_shadow` | 🔴 DROP |
| 12 | `candles_4h_shadow` | 🔴 DROP |
| 13 | `candles_5m_shadow` | 🔴 DROP |
| 14 | `deep_market_depth` | 🔴 DROP — depth dropped per LOCK-I |
| 15 | `depth_dynamic_diff_audit` | 🔴 DROP |
| 16 | `depth_rebalance_audit` | 🔴 DROP |
| 17 | `derivative_contracts` | 🔴 DROP — no F&O in indices-only scope |
| 18 | `dhan_option_chain_raw` | ✅ KEEP (rename to `option_chain_raw` for cleanliness — optional) |
| 19 | `fno_underlyings` | 🔴 DROP |
| 20 | `greeks_verification` | 🔴 DROP |
| 21 | `historical_candles` | 🔴 DROP — replaced by per-TF candle tables |
| 22 | `index_constituents` | 🔴 DROP — no equity tracking |
| 23 | `indicator_snapshots` | 🔴 DROP — indicators in RAM only per operator lock |
| 24 | `instrument_build_metadata` | 🔴 DROP — no instrument master rebuild |
| 25 | `market_depth` | 🔴 DROP — depth dropped |
| 26 | `nse_holidays` | 🟡 OPTIONAL — keep if useful for calendar; or move to `trading_calendar.rs` const (recommended: DROP, calendar is code-driven) |
| 27 | `obi_snapshots` | 🔴 DROP — order book imbalance not in scope |
| 28 | `option_chain_minute_snapshot` | 🔴 DROP — replaced by `option_chain_snapshots` |
| 29 | `option_greeks` | 🔴 DROP — greeks live in `option_chain_snapshots` |
| 30 | `order_audit` | ✅ KEEP — SEBI 5y mandate |
| 31 | `pcr_snapshots` | 🔴 DROP — PCR computed in RAM from option_chain on-demand |
| 32 | `phase2_audit` | 🔴 DROP — Phase 2 dispatcher dropped |
| 33 | `previous_close` | 🔴 DROP — operator insight: morning 1d fetch provides yesterday's close |
| 34 | `selftest_audit` | ✅ KEEP |
| 35 | `subscribed_indices` | 🔴 DROP — universe is 4-entry compile-time const |
| 36 | `subscription_audit_log` | 🔴 DROP — boot_audit covers it |
| 37 | `ticks` | ✅ KEEP (recreate with slim schema) |
| 38 | `volume_nse_audit` | 🔴 DROP — bhavcopy + NSE volume cross-check dropped |
| 39 | `ws_reconnect_audit` | 🟡 OPTIONAL — useful for debugging reconnect storms; KEEP (slim version) |

### 9 materialized views (ALL DROP per operator lock)

| # | Mat view | Verdict |
|---|---|---|
| 1 | `candles_15m` | 🔴 DROP — recreated as plain table |
| 2 | `candles_1d` | 🔴 DROP — recreated as plain table |
| 3 | `candles_1h` | 🔴 DROP — recreated as plain table |
| 4 | `candles_1m` | 🔴 DROP — recreated as plain table |
| 5 | `candles_2h` | 🔴 DROP — recreated as plain table |
| 6 | `candles_30m` | 🔴 DROP — recreated as plain table |
| 7 | `candles_3h` | 🔴 DROP — recreated as plain table |
| 8 | `candles_4h` | 🔴 DROP — recreated as plain table |
| 9 | `candles_5m` | 🔴 DROP — recreated as plain table |

**Total to DROP: 32 tables + 9 mat views = 41 objects.**
**Total to KEEP: 7 tables (auth, boot, dhan_option_chain_raw, order, selftest, ticks, ws_reconnect_audit).**
**Total to CREATE NEW: 21 candle tables + 2 cross-verify + 2 option chain = 25 new tables.**

---

## §2. THE EXACT MIGRATION SQL

### §2.1 Step 1 — DROP all 41 unwanted objects

```sql
-- DROP all 9 materialized views FIRST (some may depend on candles_1s)
DROP MATERIALIZED VIEW IF EXISTS candles_15m;
DROP MATERIALIZED VIEW IF EXISTS candles_1d;
DROP MATERIALIZED VIEW IF EXISTS candles_1h;
DROP MATERIALIZED VIEW IF EXISTS candles_1m;
DROP MATERIALIZED VIEW IF EXISTS candles_2h;
DROP MATERIALIZED VIEW IF EXISTS candles_30m;
DROP MATERIALIZED VIEW IF EXISTS candles_3h;
DROP MATERIALIZED VIEW IF EXISTS candles_4h;
DROP MATERIALIZED VIEW IF EXISTS candles_5m;

-- DROP all 13 shadow + 1s candle tables
DROP TABLE IF EXISTS candles_15m_shadow;
DROP TABLE IF EXISTS candles_1d_shadow;
DROP TABLE IF EXISTS candles_1h_shadow;
DROP TABLE IF EXISTS candles_1m_shadow;
DROP TABLE IF EXISTS candles_2h_shadow;
DROP TABLE IF EXISTS candles_30m_shadow;
DROP TABLE IF EXISTS candles_3h_shadow;
DROP TABLE IF EXISTS candles_4h_shadow;
DROP TABLE IF EXISTS candles_5m_shadow;
DROP TABLE IF EXISTS candles_1s;

-- DROP all depth-related tables (depth dropped per LOCK-I)
DROP TABLE IF EXISTS deep_market_depth;
DROP TABLE IF EXISTS market_depth;
DROP TABLE IF EXISTS depth_dynamic_diff_audit;
DROP TABLE IF EXISTS depth_rebalance_audit;

-- DROP all stock F&O / instrument-master tables (no F&O in indices-only scope)
DROP TABLE IF EXISTS derivative_contracts;
DROP TABLE IF EXISTS fno_underlyings;
DROP TABLE IF EXISTS instrument_build_metadata;
DROP TABLE IF EXISTS subscribed_indices;
DROP TABLE IF EXISTS subscription_audit_log;
DROP TABLE IF EXISTS index_constituents;

-- DROP all greeks pipeline tables (computed in option_chain_snapshots now)
DROP TABLE IF EXISTS greeks_verification;
DROP TABLE IF EXISTS option_greeks;
DROP TABLE IF EXISTS pcr_snapshots;

-- DROP all other unwanted tables
DROP TABLE IF EXISTS aggregator_seal_audit;
DROP TABLE IF EXISTS historical_candles;
DROP TABLE IF EXISTS indicator_snapshots;
DROP TABLE IF EXISTS nse_holidays;             -- calendar moved to trading_calendar.rs const
DROP TABLE IF EXISTS obi_snapshots;
DROP TABLE IF EXISTS option_chain_minute_snapshot;
DROP TABLE IF EXISTS phase2_audit;
DROP TABLE IF EXISTS previous_close;            -- morning 1d fetch replaces this
DROP TABLE IF EXISTS volume_nse_audit;

-- VERIFY drops succeeded
SELECT table_name FROM tables() ORDER BY table_name;
-- Expected: 7 surviving tables only (auth_renewal_audit, boot_audit, dhan_option_chain_raw,
-- order_audit, selftest_audit, ticks, ws_reconnect_audit)
```

### §2.2 Step 2 — RECREATE `ticks` table with slim schema

```sql
-- Drop and recreate with PRECISE columns only
DROP TABLE IF EXISTS ticks;
CREATE TABLE ticks (
    ts TIMESTAMP,                  -- exchange tick ts (microsecond IST)
    security_id INT,               -- 13, 25, 51, 21
    exchange_segment SYMBOL CAPACITY 4 NOCACHE,  -- always 'IDX_I' at locked scope
    last_price DOUBLE,
    last_quantity INT,
    sequence_number LONG
) TIMESTAMP(ts) PARTITION BY DAY WAL
DEDUP UPSERT KEYS(ts, security_id, exchange_segment, sequence_number);
```

### §2.3 Step 3 — CREATE 21 candle tables (one per timeframe)

```sql
-- Macro: same DDL repeated for each TF
-- {tf} ∈ {1m, 2m, 3m, 4m, 5m, 6m, 7m, 8m, 9m, 10m, 11m, 12m, 13m, 14m, 15m, 30m, 1h, 2h, 3h, 4h, 1d}

CREATE TABLE candles_1m (
    ts TIMESTAMP,                  -- bar boundary ts (microsecond IST)
    security_id INT,
    exchange_segment SYMBOL CAPACITY 4 NOCACHE,
    open DOUBLE,
    high DOUBLE,
    low DOUBLE,
    close DOUBLE,
    volume LONG
) TIMESTAMP(ts) PARTITION BY DAY WAL
DEDUP UPSERT KEYS(ts, security_id, exchange_segment);

CREATE TABLE candles_2m (... same ...) ;
CREATE TABLE candles_3m (...) ;
CREATE TABLE candles_4m (...) ;
CREATE TABLE candles_5m (...) ;
CREATE TABLE candles_6m (...) ;
CREATE TABLE candles_7m (...) ;
CREATE TABLE candles_8m (...) ;
CREATE TABLE candles_9m (...) ;
CREATE TABLE candles_10m (...) ;
CREATE TABLE candles_11m (...) ;
CREATE TABLE candles_12m (...) ;
CREATE TABLE candles_13m (...) ;
CREATE TABLE candles_14m (...) ;
CREATE TABLE candles_15m (...) ;
CREATE TABLE candles_30m (...) ;
CREATE TABLE candles_1h (...) ;
CREATE TABLE candles_2h (...) ;
CREATE TABLE candles_3h (...) ;
CREATE TABLE candles_4h (...) ;
CREATE TABLE candles_1d (...) ;
```

(Auto-generated in code via macro — boot Step 4 will `ensure_*_table` idempotently for all 21.)

### §2.4 Step 4 — CREATE 3 option chain tables

```sql
-- Snapshots (parsed per-strike rows)
CREATE TABLE option_chain_snapshots (
    ts TIMESTAMP,
    underlying_id INT,             -- 13, 25, 51 (no VIX — has no options)
    expiry STRING,                 -- 'YYYY-MM-DD'
    strike DOUBLE,
    side SYMBOL CAPACITY 2 NOCACHE, -- 'CE' or 'PE'
    last_price DOUBLE,
    oi LONG,
    volume LONG,
    iv DOUBLE,
    delta DOUBLE,
    theta DOUBLE,
    gamma DOUBLE,
    vega DOUBLE,
    top_bid DOUBLE,
    top_ask DOUBLE,
    top_bid_qty INT,
    top_ask_qty INT
) TIMESTAMP(ts) PARTITION BY DAY WAL
DEDUP UPSERT KEYS(ts, underlying_id, expiry, strike, side);

-- Per-request audit
CREATE TABLE option_chain_request_audit (
    ts TIMESTAMP,
    underlying_id INT,
    expiry STRING,
    http_status SHORT,
    latency_ms INT,
    retry_count SHORT,
    outcome SYMBOL CAPACITY 8 NOCACHE
) TIMESTAMP(ts) PARTITION BY DAY WAL
DEDUP UPSERT KEYS(ts, underlying_id, expiry);

-- Raw JSON (already exists as dhan_option_chain_raw — KEEP, optionally rename)
-- If rename desired:
-- RENAME TABLE dhan_option_chain_raw TO option_chain_raw;
```

### §2.5 Step 5 — CREATE 2 cross-verify audit tables

```sql
-- Outcome per (date, SID, TF)
CREATE TABLE cross_verify_audit (
    ts_run TIMESTAMP,
    trading_date_ist DATE,
    security_id INT,
    timeframe SYMBOL CAPACITY 8 NOCACHE,  -- '1m', '5m', '15m', '1d'
    candles_compared INT,
    mismatches_count INT,
    outcome SYMBOL CAPACITY 8 NOCACHE     -- 'passed', 'failed', 'rest_unreachable'
) TIMESTAMP(ts_run) PARTITION BY DAY WAL
DEDUP UPSERT KEYS(trading_date_ist, security_id, timeframe);

-- Per-mismatch detail
CREATE TABLE cross_verify_mismatches (
    ts_run TIMESTAMP,
    trading_date_ist DATE,
    security_id INT,
    timeframe SYMBOL CAPACITY 8 NOCACHE,
    candle_ts TIMESTAMP,
    field SYMBOL CAPACITY 8 NOCACHE,      -- 'open', 'high', 'low', 'close', 'volume'
    live_value DOUBLE,
    hist_value DOUBLE
) TIMESTAMP(ts_run) PARTITION BY DAY WAL
DEDUP UPSERT KEYS(trading_date_ist, security_id, timeframe, candle_ts, field);
```

### §2.6 Step 6 — TRIM existing audit tables to slim columns

```sql
-- For each KEEP table (auth, boot, order, selftest, ws_reconnect):
-- Inspect current schema, drop columns we don't use, keep minimal set

-- Example for boot_audit (recreate to be safe):
DROP TABLE IF EXISTS boot_audit_old;
ALTER TABLE boot_audit RENAME TO boot_audit_old;
CREATE TABLE boot_audit (
    ts TIMESTAMP,
    boot_id STRING,
    step_name SYMBOL CAPACITY 16 NOCACHE,
    outcome SYMBOL CAPACITY 8 NOCACHE,     -- 'passed', 'failed', 'skipped'
    latency_ms INT,
    error_code SYMBOL CAPACITY 32 NOCACHE
) TIMESTAMP(ts) PARTITION BY DAY WAL
DEDUP UPSERT KEYS(boot_id, step_name);
INSERT INTO boot_audit SELECT ts, boot_id, step_name, outcome, latency_ms, error_code FROM boot_audit_old;
DROP TABLE boot_audit_old;

-- Repeat similar pattern for auth_renewal_audit, order_audit, selftest_audit, ws_reconnect_audit
```

### §2.7 Step 7 — VERIFY final state

```sql
-- Should return EXACTLY 32 tables (7 kept + 25 new) + 0 mat views
SELECT table_name FROM tables() ORDER BY table_name;
SELECT view_name FROM materialized_views();  -- should be empty

-- Expected tables (32):
-- auth_renewal_audit, boot_audit, candles_1d, candles_1h, candles_1m,
-- candles_2h, candles_2m, candles_3h, candles_3m, candles_4h, candles_4m,
-- candles_5m, candles_6m, candles_7m, candles_8m, candles_9m, candles_10m,
-- candles_11m, candles_12m, candles_13m, candles_14m, candles_15m,
-- candles_30m, cross_verify_audit, cross_verify_mismatches,
-- dhan_option_chain_raw (or renamed to option_chain_raw),
-- option_chain_request_audit, option_chain_snapshots,
-- order_audit, selftest_audit, ticks, ws_reconnect_audit
```

---

## §3. The migration PR — when this gets executed

This goes into PR #10 of the 14-PR sequence (`THE-FINAL-PLAN.md` §5):

| PR # | Title | Files touched |
|---|---|---|
| 10 | Cutover observability + QuestDB cleanup | docker-compose.yml + Terraform + **this migration SQL applied via `aws ssm send-command` to instance** |

### Execution procedure

```bash
# 1. Operator pauses tickvault (or schedules during off-hours)
aws ssm send-command --instance-ids $INSTANCE_ID \
  --document-name AWS-RunShellScript \
  --parameters 'commands=["systemctl stop tickvault"]'

# 2. Take EBS snapshot for rollback safety
aws ec2 create-snapshot --volume-id $EBS_VOL --description "pre-cleanup-2026-05-18"

# 3. Apply migration SQL via QuestDB PG-wire or HTTP
psql -h localhost -p 8812 -U admin -f migration-cleanup.sql

# 4. Verify (run the SELECT in §2.7)

# 5. Restart tickvault — boot Step 4 ensure_*_table will idempotently CREATE
#    any missing tables (but at this point all 32 should already exist)
aws ssm send-command --instance-ids $INSTANCE_ID \
  --document-name AWS-RunShellScript \
  --parameters 'commands=["systemctl start tickvault"]'

# 6. Wait for BootReadyConfirmation Telegram
# 7. Verify on next 15:31 cross-verify that everything works
```

### Rollback if anything goes wrong

```bash
# Restore from EBS snapshot pre-migration
aws ec2 stop-instances --instance-ids $INSTANCE_ID
# Detach old volume, attach snapshot-restored volume, restart
```

---

## §4. Why this wasn't already done

Honest answer: this session has been pure DESIGN work. The 14-PR sequence is sequenced but no PRs have been MERGED yet. The LIVE QuestDB you see is the PRE-RESTRUCTURE state.

| Doc says | LIVE reality |
|---|---|
| 32 tables, 0 mat views | 39 tables + 9 mat views = 48 objects |
| `candles_1m` is a plain table | `candles_1m` is a mat view (computed from `candles_1s` via QuestDB SQL aggregation) |
| `previous_close` table is dropped | Still exists |
| All depth tables dropped | Still exist (`market_depth`, `deep_market_depth`, `depth_*_audit`) |
| Greeks tables dropped | Still exist (`option_greeks`, `pcr_snapshots`, `greeks_verification`) |

**The cleanup PR exists in PLAN but has not been executed against the LIVE database.** PR #10 of THE-FINAL-PLAN does this.

---

## §5. The 100% guarantee for this migration

Per operator-charter §F mandatory wording:

> "100% inside the migration envelope: the 41 DROP + 25 CREATE statements in §2 are idempotent (`IF EXISTS` / `IF NOT EXISTS`). The migration is reversible via EBS snapshot taken in §3 step 2. The boot Step 4 `ensure_*_table` self-heal pattern from existing Wave-2-A code re-creates any missing table automatically on next boot. Verification SQL in §2.7 confirms final state matches design.
>
> Beyond envelope: if the migration corrupts data, EBS snapshot rollback restores to pre-migration state in ~5 minutes. The historical tick data in `ticks` table is the only DATA loss risk; mitigation = export to S3 cold archive BEFORE migration (CREATE TABLE AS SELECT into S3 via QuestDB's S3 export)."

---

## §6. Honest envelope

> "The migration is DESIGNED. It is NOT EXECUTED. Operator running this session sees stale LIVE state. After PR #10 of THE-FINAL-PLAN merges + Cowork applies the migration, the LIVE QuestDB will match the locked design exactly (32 tables, 0 mat views). Until then, operator-screenshot mismatch is expected and documented honestly."

---

## §7. Trigger / auto-load

This rule activates when editing:
- `crates/storage/src/*.rs` (any `ensure_*_table` function)
- `deploy/aws/terraform/*.tf` (if QuestDB schema is in IaC)
- Any file containing `CREATE TABLE`, `DROP TABLE`, `MATERIALIZED VIEW`
- `.claude/plans/aws-lifecycle/THE-FINAL-PLAN.md` PR #10
