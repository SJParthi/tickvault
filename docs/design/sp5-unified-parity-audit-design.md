# SP5 — ONE unified `feed_parity_1m_audit` table + writer (design-first)

> **Plan:** `.claude/plans/active-plan-groww-live-backtest-parity.md` SP5 (line 85)
> **Authority chain:** CLAUDE.md > operator-charter-forever.md > the parity plan > this doc
> **Status:** APPROVED-design (mirrors the candle-table NULL-backfill template that already shipped)

## Design

SP5 merges the two byte-identical-schema 1-minute parity audit modules into ONE
feed-parameterized table + writer, with `feed` promoted INTO the DEDUP key.

| Today (siloed) | After SP5 (unified) |
|---|---|
| `cross_verify_1m_audit` (Dhan; `our_value`/`dhan_value`; `feed` is a LABEL, not a key) | `feed_parity_1m_audit` (any feed; `live_value`/`backtest_value`; `feed` IN the DEDUP key) |
| `groww_cross_verify_1m_audit` (Groww; `live_value`/`backtest_value`; `feed` label) | (same table) |
| `CrossVerify1mAuditWriter` + `CrossVerify1mMismatch` + `MismatchField` | `FeedParity1mAuditWriter` + `FeedParity1mMismatch` (+ reuse `tickvault_common::feed_parity::MismatchField`) |
| `GrowwCrossVerify1mAuditWriter` + `GrowwParityMismatchRow` + `GrowwMismatchField` | (merged into the above) |

**Why `live_value`/`backtest_value` (not `our_value`/`dhan_value`):** the unified
column names are feed-agnostic — "live" vs "backtest" is the universal parity
axis for EVERY feed (Dhan REST tape is the backtest; Groww historical is the
backtest). The Dhan `our_value`→`live_value`, `dhan_value`→`backtest_value`
rename is a pure label change (semantically: our live `candles_1m` vs the
authoritative backtest).

### New unified table schema

```sql
CREATE TABLE IF NOT EXISTS feed_parity_1m_audit (
    ts               TIMESTAMP,   -- when the parity run executed (IST nanos)
    trading_date_ist TIMESTAMP,   -- the trading day verified (IST midnight)
    feed             SYMBOL,      -- broker source: 'dhan' / 'groww' (IN the DEDUP key)
    segment          SYMBOL,
    security_id      LONG,
    minute_ts_ist    TIMESTAMP,   -- the mismatched 1-min bucket (IST nanos)
    field            SYMBOL,      -- open / high / low / close / volume
    live_value       DOUBLE,      -- our candles_1m (feed=<this feed>) value
    backtest_value   DOUBLE       -- the feed's authoritative backtest value
) timestamp(ts) PARTITION BY DAY
  DEDUP UPSERT KEYS(ts, trading_date_ist, feed, security_id, segment, minute_ts_ist, field);
```

### DEDUP key (MUST include `feed` per the per-feed-identity contract)

```
DEDUP_KEY_FEED_PARITY_1M_AUDIT =
    "ts, trading_date_ist, feed, security_id, segment, minute_ts_ist, field"
```

- `ts` FIRST — QuestDB requires the designated timestamp in every DEDUP key
  (2026-05-18 HTTP-400 regression). Pinned.
- `feed` — promoted INTO the key (NEW vs the two old tables, which kept `feed`
  as a label only). This is mandatory because the two feeds now share ONE
  physical table: without `feed` in the key, a Dhan mismatch row and a Groww
  mismatch row for the same `(trading_date, security_id, segment, minute, field)`
  would collide and silently overwrite. This is the candle-table per-feed
  identity rule applied to the audit table.
- `(security_id, segment)` — I-P1-11 composite identity.
- `(trading_date_ist, minute_ts_ist, field)` — discriminators so re-running the
  parity check for a day collapses to one row per `(feed, instrument, minute, field)`.

`dedup_segment_meta_guard` scans this constant; it mentions `ts` AND `segment`
(its existing assertions), and the per-feed test list **does NOT** add this table
(see "Meta-guard interaction" below) — but the new module's OWN unit test asserts
`feed` is in the key.

### Idempotent DDL + self-heal (greenfield + brownfield)

```
1. CREATE TABLE IF NOT EXISTS feed_parity_1m_audit (...)   -- includes feed col + feed-in-key DEDUP
2. ALTER TABLE feed_parity_1m_audit ADD COLUMN IF NOT EXISTS feed SYMBOL   -- self-heal pre-existing table
3. UPDATE feed_parity_1m_audit SET feed = 'dhan' WHERE feed IS NULL        -- NULL-backfill (HIGH finding) BEFORE re-key
4. ALTER TABLE feed_parity_1m_audit DEDUP ENABLE UPSERT KEYS(... feed ...) -- re-apply feed-in-key DEDUP
```

Steps 2-4 mirror the shipped candle-table template
(`shadow_persistence.rs::ensure_shadow_candle_tables` lines 242-262) EXACTLY:
ADD COLUMN → NULL-backfill `WHERE feed IS NULL` → DEDUP ENABLE. Idempotent &
free on every subsequent boot (`WHERE feed IS NULL` matches nothing once
backfilled; re-enabling the same key is a no-op).

## Migration / cutover — no SEBI audit data silently dropped

The two OLD tables are SEPARATE physical QuestDB tables; the unified table is a
NEW physical table. The honest, non-destructive cutover:

| Old table | Rows today | Cutover treatment |
|---|---|---|
| `cross_verify_1m_audit` | possibly populated (Dhan path is live since 2026-06-02) | **NEVER DROPPED** (SEBI 5y retention). Stays on disk + in `partition_manager` retention sweep, queryable forever. New Dhan mismatches route to `feed_parity_1m_audit` going forward. |
| `groww_cross_verify_1m_audit` | EMPTY — only ever `ensure`'d at boot; no production writer was ever constructed (verified: `GrowwCrossVerify1mAuditWriter` has zero call sites outside its own module). | **NEVER DROPPED**. Stays on disk. No data to migrate (it was never written). |

**Why no row-copy migration is needed:** the Groww table is empty (no writer
existed). The Dhan table's historical rows stay queryable in place under the old
table name — they are NOT lost, just not relocated (relocating SEBI rows across
tables would itself be a risk). The NULL-backfill in step 3 above protects the
NEW `feed_parity_1m_audit` table against a Dhan row written before this self-heal
re-keyed it (defence-in-depth for a partial-deploy re-run), exactly as the HIGH
finding (plan line 106) demands.

**Honest envelope:** queries that want the full Dhan parity history span TWO
table names (`cross_verify_1m_audit` for pre-SP5 rows, `feed_parity_1m_audit` for
SP5-onward). This is the cost of not relocating SEBI rows. The old table is
documented as retained, not orphaned. `partition_manager` keeps both names in
the retention sweep (the new name is ADDED; the two old names STAY).

## Call sites that migrate

| Call site | Today | After SP5 |
|---|---|---|
| `crates/app/src/cross_verify_1m_boot.rs:34-36` (import) | `cross_verify_1m_audit_persistence::{CrossVerify1mAuditWriter, CrossVerify1mMismatch, MismatchField, ...}` | `feed_parity_1m_audit_persistence::{FeedParity1mAuditWriter, FeedParity1mMismatch}` + `tickvault_common::feed_parity::MismatchField` |
| `cross_verify_1m_boot.rs:246` (build mismatch) | `CrossVerify1mMismatch { feed: Dhan, our_value, dhan_value, ... }` | `FeedParity1mMismatch { feed: Dhan, live_value, backtest_value, ... }` |
| `cross_verify_1m_boot.rs:527` (`final_flush`) | `&mut CrossVerify1mAuditWriter` | `&mut FeedParity1mAuditWriter` |
| `cross_verify_1m_boot.rs:566` (ensure) | `ensure_cross_verify_1m_audit_table` | `ensure_feed_parity_1m_audit_table` |
| `cross_verify_1m_boot.rs:608` (construct) | `CrossVerify1mAuditWriter::new` | `FeedParity1mAuditWriter::new` |
| `cross_verify_1m_boot.rs` tests | `CrossVerify1mAuditWriter::for_test`, `CrossVerify1mMismatch{...}` | unified equivalents |
| `crates/app/src/groww_activation.rs:238` (ensure) | `ensure_groww_cross_verify_1m_audit_table` | `ensure_feed_parity_1m_audit_table` (the ONE table both feeds ensure; idempotent) |
| `crates/storage/src/lib.rs:85,109` (mod decls) | `pub mod cross_verify_1m_audit_persistence; pub mod groww_cross_verify_audit_persistence;` | `pub mod feed_parity_1m_audit_persistence;` (delete the two old mods) |
| `crates/storage/src/partition_manager.rs:60,80` | `"cross_verify_1m_audit"`, `"groww_cross_verify_1m_audit"` | KEEP both (SEBI retention) + ADD `"feed_parity_1m_audit"` |

**CSV path:** `cross_verify_1m_boot.rs` also uses `csv_header` + `mismatch_to_csv_line`
from the Dhan module. Those pure helpers move into the unified module
(`live_value`/`backtest_value` column names; the CSV header changes
`our_value,dhan_value` → `live_value,backtest_value` for the unified file). The
Dhan CSV file name (`data/cross-verify/cross-verify-1m-YYYY-MM-DD.csv`) is owned
by the boot module, not the persistence module — left as-is in SP5 (renaming the
CSV path is an SP7 orchestrator concern).

## Module deletion plan (safe-delete gating)

Delete the two old modules + their `lib.rs` entries ONLY after EVERY call site is
migrated and `cargo test -p tickvault-storage` + the app crate compile/test green.
- `cross_verify_1m_audit_persistence.rs` — its writer/types had ONE consumer
  (`cross_verify_1m_boot.rs`), fully migrated. SAFE TO DELETE.
- `groww_cross_verify_audit_persistence.rs` — its writer/types had ZERO consumers
  (only the boot `ensure` call). SAFE TO DELETE.
- The DDL constants `CROSS_VERIFY_1M_AUDIT_TABLE` / `GROWW_CROSS_VERIFY_1M_AUDIT_TABLE`
  are referenced as STRING LITERALS in `partition_manager.rs` (not via the const),
  so deleting the modules does not break the retention sweep.

## Meta-guard interaction (must stay green)

- `dedup_segment_meta_guard::audit DEDUP keys must include ts` — the new
  `DEDUP_KEY_FEED_PARITY_1M_AUDIT` includes `ts` (pinned first). GREEN.
- `dedup_segment_meta_guard::per_feed_market_data_dedup_keys_must_include_feed`
  scans a FIXED list `FEED_KEYED_MARKET_DATA_KEYS` (ticks/candles/prev_day/ws_event)
  — it does NOT include any parity-audit key, so adding `feed` to our key does
  NOT change that test's required-list, and removing the two old keys does NOT
  break it. The guard's comment explicitly excludes `cross_verify_1m_audit` /
  `groww_cross_verify_1m_audit` as "label, not key" tables. The NEW table puts
  `feed` IN the key (stronger), which the guard tolerates (it only asserts the
  4 named keys CONTAIN feed; it never asserts other keys must NOT). GREEN.
- The new module's OWN unit test asserts `feed` ∈ `DEDUP_KEY_FEED_PARITY_1M_AUDIT`
  (the per-feed-identity contract for the shared table).

## Edge Cases
- Pre-existing `feed_parity_1m_audit` from a partial earlier deploy with NULL-feed
  Dhan rows → step-3 NULL-backfill stamps `feed='dhan'` BEFORE re-key, no overwrite.
- Old `cross_verify_1m_audit` table populated → untouched, queryable, retained.
- Both feeds write the same `(date, sid, segment, minute, field)` but DIFFERENT
  feed → BOTH rows kept (feed in key). Tested.
- Re-run the same feed's parity for a day → collapses to one row per cell (DEDUP).
- QuestDB unreachable at writer construction → lazy `sender=None`, buffer locally,
  `flush` Errs until reconnect (mirrors both old writers).

## Failure Modes
- DDL/ALTER/backfill HTTP non-2xx → `error!` (Telegram-routable), boot continues
  (audit can't persist, reported; never blocks the parity run). Mirrors both old
  modules.
- ILP flush while disconnected → `Err`, never panic. Tested.
- Module deleted while a call site still references it → compile error (caught
  pre-merge), never a silent runtime gap.

## Test Plan
- `DEDUP_KEY_FEED_PARITY_1M_AUDIT` contains `ts`, `feed`, `segment`, `security_id`,
  `minute_ts_ist`, `field` (per-feed-identity + I-P1-11 + designated-ts).
- CREATE DDL contains all columns + `feed` in DEDUP + PARTITION BY DAY.
- `append_mismatch` with `feed=dhan` AND `feed=groww` → both land on the wire with
  correct table + feed label; pending increments.
- Migration DDL strings: ADD COLUMN feed, `UPDATE ... WHERE feed IS NULL`, DEDUP
  ENABLE present + ordered (backfill BEFORE re-key).
- `for_test` writer disconnected + empty; disconnected `flush` Errs not panics.
- CSV header/line column count matches; symbol sanitization.
- `dedup_segment_meta_guard` whole-suite stays green.
- `cargo test -p tickvault-storage --lib --tests`; app crate compiles + its
  cross_verify_1m_boot tests pass.

## Rollback
Additive new module + new table; reverting the SP5 commit restores the two siloed
modules. The new `feed_parity_1m_audit` table stays on disk (harmless, SEBI-safe);
the old `cross_verify_1m_audit` table was never touched, so the Dhan path reverts
cleanly. No data loss either direction.

## Observability
The audit table IS the observability layer for parity (per plan §Observability +
the cross-verify-1m rule). The Dhan boot path already emits `CROSS-VERIFY-1M-01/02`
+ Telegram + CSV; SP5 keeps that wiring intact (only the persistence target table
+ writer type change). Generic `PARITY-01/02` error codes are an SP7 concern.

## Z+ / honest-envelope note
"One unified table, both feeds, `feed` in DEDUP, no SEBI row dropped" is true
INSIDE this envelope: the OLD Dhan table's pre-SP5 rows are retained in place
(queryable under the old name), not relocated — a deliberate no-row-copy choice
to avoid the risk of moving SEBI audit rows across tables. New rows (both feeds)
go to the unified table. Full-history Dhan parity queries span two table names
until/unless a separate, explicitly-approved row-copy migration runs.
