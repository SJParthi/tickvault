# Runbook: Human-readable QuestDB console queries (`ticks_named` / `candles_named`)

> **What this answers:** "How do I, as a human, query ticks and candles
> BY NAME (NIFTY, RELIANCE, …) in the QuestDB console — without
> hand-writing the composite instrument join every time?"
> **Authority:** `.claude/plans/archive/2026-07-08-questdb-named-views.md`,
> `.claude/rules/project/http-client-error-codes.md` (§1
> `named_views_ensure` degrade row),
> `.claude/rules/project/security-id-uniqueness.md` (I-P1-11),
> `.claude/rules/project/data-integrity.md` (feed-in-key).
> **Code:** `crates/storage/src/console_views.rs::ensure_named_views`,
> wired in BOTH Dhan boot paths of `crates/app/src/main.rs` (after the
> base-table ensure join) AND in the Groww activation
> (`crates/app/src/groww_activation.rs::activate_groww_lane`) so
> Groww-only boots (`feeds.dhan_enabled = false`) create them too.

## TL;DR

| Question | Answer |
|---|---|
| What are they? | Two plain (non-materialized) QuestDB views — `ticks_named` and `candles_named` — that LEFT JOIN the raw `ticks` / `candles_1m` tables against the `instrument_lifecycle` master so every row carries `symbol_name`, `display_name`, `instrument_type`. |
| When are they created? | At every feed-enabled boot (either feed): the Dhan lane creates them right after the base-table ensure join, and the Groww activation creates them after its own base-table ensures — so a Groww-only boot (`feeds.dhan_enabled = false`) is covered too. `CREATE OR REPLACE VIEW` — every boot **converges** the deployed definition to the code (a definition change self-heals, no manual step). Fail-soft — a failure retries next boot. A boot with BOTH feeds disabled creates nothing (nothing streams in that mode). |
| Where do I see them? | `make questdb` → `localhost:9000` — the console sidebar lists views alongside tables (`table_type = 'V'`). |
| Candle scope? | **1-minute ONLY** (`candles_1m` base). Other timeframes: query the `candles_<tf>` base tables directly. |
| NULL `symbol_name`? | **A diagnostic, not a bug** — the streaming instrument is absent from the lifecycle master (fresh DB, pre-reconcile boot, or genuinely unmapped). The row is kept, never dropped. |
| Hot-path impact? | ZERO. Cold-path analyst tooling; the join is computed at SELECT time — O(N), never claimed O(1). |

## Copy-paste console queries

```sql
-- Today's NIFTY ticks by name
SELECT * FROM ticks_named
WHERE symbol_name = 'NIFTY'
ORDER BY ts DESC LIMIT 100;

-- Today's 1m candles for a stock, by name
SELECT ts, symbol_name, open, high, low, close, volume, feed
FROM candles_named
WHERE symbol_name = 'RELIANCE' AND ts > dateadd('d', -1, now())
ORDER BY ts;

-- Which streaming instruments have NO name mapping? (diagnostic)
SELECT DISTINCT security_id, segment, feed
FROM ticks_named
WHERE symbol_name IS NULL;

-- Per-feed comparison for the same instrument (Dhan vs Groww rows coexist)
SELECT ts, feed, close, volume FROM candles_named
WHERE symbol_name = 'NIFTY' ORDER BY ts DESC LIMIT 20;

-- DETECTOR: duplicate lifecycle dimension rows (any hit = the views
-- N-fold-multiply that instrument's rows; see "Failure modes" below).
-- QuestDB does not support standard SQL HAVING — this is the
-- documented subquery + WHERE equivalent.
SELECT * FROM (
    SELECT security_id, exchange_segment, feed, count() AS n
    FROM instrument_lifecycle
    GROUP BY security_id, exchange_segment, feed
) WHERE n > 1;
```

## What the views are (and are not)

- **Plain views, computed at query time.** No storage, no refresh
  machinery, no write path. They are read-only projections over
  `ticks` / `candles_1m` LEFT-joined against a dimension subquery of
  `instrument_lifecycle` filtered `dry_run = false` (dry-run rows are
  isolated per the daily-universe lock §27).
- **Duplicate-safety honest envelope — one dimension row per key WHILE
  DEDUP is live:** the lifecycle master pins its designated `ts` to a
  constant (epoch 0), so its DEDUP collapses it to exactly ONE row per
  `(security_id, exchange_segment, feed)` and the join cannot multiply
  rows **in that state**. Two documented degraded windows break the
  precondition with persistent duplicates that a later `DEDUP ENABLE`
  does NOT retro-collapse (it applies to future writes only): (1) the
  lifecycle HTTP-CLIENT-01 auto-create window (`lifecycle_ensure` site
  — table ILP-created WITHOUT dedup keys, reconcile UPSERTs append
  duplicate sets), and (2) a partial feed self-heal boot (NULL→'dhan'
  backfill failed, `DEDUP ENABLE` succeeded — the next boot's backfill
  mints same-key duplicates). With N lifecycle rows for one key, every
  matching view row appears exactly N times. Run the detector below if
  counts look inflated.
- **`feed` is in the join predicate** — a Dhan and a Groww row for the
  same instrument stay distinct, and Groww's bit-62 synthetic index
  ids resolve only under `feed = 'groww'`.
- **SAMPLE BY caveat:** the view result carries **no designated
  timestamp**, so `SAMPLE BY` does not work on the views — use the base
  tables (`ticks`, `candles_<tf>`) for SAMPLE BY aggregations.

## Fallback: the raw JOIN SQL (if the views are ever dropped)

If a boot degraded (HTTP-CLIENT-01 `named_views_ensure` row in the
rule file) or someone dropped the views, paste this directly:

```sql
SELECT t.ts, il.symbol_name, il.display_name, il.instrument_type,
       t.ltp, t.open, t.high, t.low, t.close, t.volume, t.oi,
       t.avg_price, t.last_trade_qty, t.total_buy_qty, t.total_sell_qty,
       t.feed, t.segment, t.security_id, t.exchange_timestamp,
       t.received_at, t.capture_seq
FROM ticks t
LEFT JOIN (
    SELECT security_id, exchange_segment, feed,
           symbol_name, display_name, instrument_type
    FROM instrument_lifecycle WHERE dry_run = false
) il
ON t.security_id = il.security_id
AND t.segment = il.exchange_segment
AND t.feed = il.feed
WHERE il.symbol_name = 'NIFTY'
ORDER BY t.ts DESC LIMIT 100;
```

For candles, swap `FROM ticks t` → `FROM candles_1m c` (alias `c`) and
the column list to
`c.ts, il.symbol_name, il.display_name, il.instrument_type, c.open,
c.high, c.low, c.close, c.volume, c.oi, c.tick_count, c.feed,
c.segment, c.security_id, c.change_pct, c.close_pct_from_prev_day,
c.open_pct, c.open_gap_pct`.

## Introspection + definition changes

| Task | Command (QuestDB console) |
|---|---|
| List views | `SELECT * FROM views();` (or `SHOW TABLES` / `tables()` — views carry `table_type = 'V'`) |
| View status | `SELECT view_name, view_status FROM views();` |
| Show a view's DDL | `SHOW CREATE VIEW ticks_named;` |
| **Change a view's definition** | Nothing to do — the DDL is `CREATE OR REPLACE VIEW` (probe-Verified on the pinned QuestDB 9.3.5), so the next boot after a deploy **converges** the deployed definition to the code automatically. No manual `DROP VIEW` (consistent with `aws-budget.md` rule 8: no manual configuration on AWS). |
| Rollback (remove entirely) | `DROP VIEW IF EXISTS ticks_named; DROP VIEW IF EXISTS candles_named;` + revert the three `ensure_named_views` call sites (two Dhan paths in `main.rs` + the Groww activation). Views are stateless — zero data risk. |

## Failure modes

| Symptom | Meaning | Action |
|---|---|---|
| `named view DDL non-2xx` warn at boot | QuestDB rejected the statement (e.g. base table missing on a degraded boot) | None — retries next boot; use the fallback SQL meanwhile |
| HTTP-CLIENT-01 with site `named_views_ensure` | reqwest client build failed (fd/TLS/resolver pressure) | Follow `.claude/rules/project/http-client-error-codes.md` triage; views self-heal next boot |
| NULL `symbol_name` on live instruments | Lifecycle master empty/partial (fresh DB, pre-reconcile) | Expected pre-reconcile; sustained NULLs = check the daily-universe orchestrator (INSTR-FETCH-*) |
| View counts exactly N× inflated per instrument (e.g. every NIFTY minute appears twice) | Duplicate `instrument_lifecycle` rows for one `(security_id, exchange_segment, feed)` key — the HTTP-CLIENT-01 auto-create window or a partial feed self-heal left pre-DEDUP duplicates that `DEDUP ENABLE` never retro-collapses | Run the detector query above to confirm + name the keys. Base tables (`ticks`, `candles_1m`) are UNAFFECTED — the multiplication is join-side only. Remediation = the manual dedup sweep already named by `http-client-error-codes.md` §1 — an explicit operator decision (lifecycle rows are never auto-deleted, daily-universe lock §5) |
