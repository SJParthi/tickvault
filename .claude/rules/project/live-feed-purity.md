---
paths:
  - "crates/core/src/historical/**/*.rs"
  - "crates/core/src/pipeline/**/*.rs"
  - "crates/storage/src/tick_persistence.rs"
  - "crates/storage/src/candle_persistence.rs"
  - "crates/app/src/main.rs"
---

# Live Feed Purity — Hard Ban on Historical→Ticks Backfill

> **Directive:** Parthiban, 2026-04-17 (verbatim):
>
> "in our live market feed websockets nowhere the backfill should happen,
>  make it as hard enforcement ... live market feed should contain only
>  live market feed data alone ... historical candle data fetch is a
>  separate functionality".
>
> **Authority:** CLAUDE.md > this file > defaults.
> **Scope:** Every file that writes to the `ticks` QuestDB table or the
> `candles_*` materialized views. Every file under
> `crates/core/src/historical/` and any new `backfill`/`synth` module.

## The Rule — one line

**The `ticks` QuestDB table is populated EXCLUSIVELY from live WebSocket
frames. Historical REST-API data NEVER crosses into `ticks`.**

## The bug class this rule prevents

Before 2026-04-17 the codebase contained `crates/core/src/historical/backfill.rs`
which:

1. Subscribed to ERROR-level tick-gap events from `TickGapTracker`.
2. Called Dhan's `/charts/intraday` REST API for the missing time window.
3. Synthesised fake `ParsedTick` structs from the 1-minute OHLCV candles.
4. Fed them into `TickPersistenceWriter::append_tick` so they ended up
   in the `ticks` table alongside genuine WebSocket-sourced ticks.

This was wrong because:

- SEBI audit trail expects one data source per table. Mixing live
  WebSocket ticks and synthetic REST-derived ticks in `ticks` makes
  reconstruction ambiguous.
- Downstream candle aggregation (materialized views over `ticks`) would
  produce candles from synthetic input, polluting `candles_1m` etc.
- Cross-verification (historical vs live) becomes a tautology: comparing
  historical data against a table that already contains historical data.

On 2026-04-17 the BackfillWorker module and its DHAT test were **DELETED**
and replaced with the hard bans in this rule.

## Mechanical rules

1. **`crates/core/src/historical/backfill.rs`** MUST NOT exist. Recreating
   it is blocked by the pre-commit pattern scanner (category 6 in
   `.claude/hooks/banned-pattern-scanner.sh`).

2. **`pub mod backfill;`** MUST NOT be added to
   `crates/core/src/historical/mod.rs` (or any other module). The scanner
   blocks on this literal.

3. **`TickPersistenceWriter`, `append_tick()`, `BackfillWorker`,
   `run_backfill`, `synthesize_ticks`** MUST NOT appear inside any file
   whose path contains `historical/` or `backfill`/`synth`. Scanner blocks.

4. **Historical data — RETIRED 2026-05-26.** PR-E deleted the entire
   Dhan historical fetch chain (`candle_fetcher.rs`,
   `cross_verify.rs`, `gap_fill_*`, `preopen_*`,
   `candle_persistence.rs`) AND dropped the `historical_candles`
   QuestDB table. The only remaining "historical" surface is
   `candles_*` materialized views aggregated from live ticks in
   `ticks`. The pre-market yesterday's-1d fetch exception (rule 8
   below) is also retired — there is no REST historical fetcher in
   the codebase anymore.

5. **`ticks` table** has ONE write path: `run_tick_processor` in
   `crates/core/src/pipeline/tick_processor.rs`, which consumes the WAL
   / SPSC from the WebSocket connection pool. Any NEW writer to `ticks`
   requires Parthiban approval.

6. **`TickGapTracker`** still detects gaps for observability (metrics +
   Telegram), but the gap event NO LONGER triggers any backfill. Missing
   ticks stay missing. That is correct behaviour — forged ticks are
   worse than absent ticks for a regulated trading system.

7. **Cross-verification — RETIRED 2026-05-26.** PR-C deleted
   `cross_verify.rs`. The live-vs-historical diff no longer runs.

8. **Pre-market yesterday's-1d fetch — RETIRED 2026-05-26, then NARROWLY
   RE-ALLOWED 2026-06-01 (see rule 9).** PR-E deleted the OLD broad chain;
   it stays deleted. A strictly bounded replacement is re-allowed below.

9. **Bounded prev-day 1-candle fetch — RE-ALLOWED 2026-06-01 (operator
   directive).** Operator quote 2026-06-01: *"once authentication is
   successful and instruments get loaded successfully then instantly we
   planned to pull historical one day previous day data of all these
   subscribed symbols"* → chose "Re-add full prev-day OHLCV fetch",
   operator-confirmed scope = **one day, one daily candle per symbol**.

   RE-ALLOWS a STRICTLY BOUNDED REST fetch, distinct from the deleted chain:
   - **Scope:** yesterday's SINGLE daily candle (O/H/L/C/V) per subscribed
     SID (~243), fetched ONCE at boot after instruments load.
   - **Destination:** a SEPARATE `prev_day_ohlcv` QuestDB table — **NEVER
     `ticks`** (the hard ban in rules 1-6 stands; no synthesized ticks).
   - **Source:** Dhan REST `POST /v2/charts/historical` (daily; fromDate =
     prev trading day, non-inclusive toDate).
   - **Fail-soft:** a symbol REST can't return is skipped + logged; boot
     never blocks (cold path).
   - **NOT** the deleted `candle_fetcher.rs`/`cross_verify.rs` chain, NOT
     under `crates/core/src/historical/`, no 90-day range, no intraday, no
     gap-fill, no cross-verify. Lives in its own `prev_day_ohlcv` module.

   What STAYS banned (this rule does NOT relax): synthesized ticks →
   `ticks`, `BackfillWorker`, `run_backfill`, `synthesize_ticks`,
   `TickPersistenceWriter`/`append_tick` in any historical/synth path.

10. **1d timeframe is HISTORICAL-ONLY — never tick-calculated (operator
    directive 2026-06-02).** Operator quote: *"for 1 day timeframe alone it
    should never ever be calculated, it should always be pulled once and only
    from historical 1 day timeframe."*

    - The live multi-TF aggregator STILL seals all 21 timeframes internally
      (fixed 21-slot arrays + the `42 = 2×21` aggregator unit test stay
      intact), but the two seal **write boundaries** in
      `crates/app/src/main.rs` (per-tick + IST-midnight force-seal) DROP the
      `TfIndex::D1` seal — so **`candles_1d` / `candles_1d_shadow` are NOT
      written by the live path**. They remain CREATEd (DDL/self-heal intact)
      but unwritten.
    - The 1d daily candle is the prev-day row in **`prev_day_ohlcv`** (now
      carrying an `oi` column), pulled ONCE each morning on AWS auto-start via
      the rule-9 bounded historical fetch.
    - The **prev-OI cache** (`crates/core/src/pipeline/prev_oi_cache.rs`)
      reads OI from `prev_day_ohlcv` (was `candles_1d`) at boot AND at every
      IST midnight — so `oi_pct_from_prev_day` keeps populating.
    - The **bar-cache loader** SELECTs 8 shadow tables (not 9) — it no longer
      reads `candles_1d_shadow`.
    - Why: a tick-built 1d candle is just a sample of the day; the historical
      daily candle is NSE's authoritative O/H/L/C/V — the same exact-match
      reason the operator cited for the intraday chart-vs-tick gap.

11. **Post-market 1-minute cross-verification — RE-ALLOWED 2026-06-02 (operator
    directive, narrowed).** Operator quote: *"put back the historical cross
    verification at precise 3.31 pm onwards … dhan historical intraday … one
    minute candle OHLCV … among our candles_1m … csv … exact match … only for
    one min alone … for all those subscribed spot instruments"* + *"csv and
    exact match cross verification is needed."*

    RE-ALLOWS a STRICTLY BOUNDED post-market read-only compare, distinct from
    the deleted `cross_verify.rs` chain:
    - **When:** 15:31:00 IST, once per trading day, cold path, market-hours
      gated (audit Rule 3).
    - **What:** for every subscribed **spot** SID, fetch Dhan **intraday**
      1-minute candles (`POST /v2/charts/intraday`, interval `"1"`) and compare
      OHLCV timestamp-by-timestamp, **EXACT**, against our live `candles_1m`
      (READ-only SELECT).
    - **Output:** mismatches → `cross_verify_1m_audit` QuestDB table (DEDUP
      `(trading_date_ist, security_id, segment, minute_ts_ist, field)` —
      I-P1-11) + `data/cross-verify/cross-verify-1m-YYYY-MM-DD.csv` + Telegram
      count via `error!(code = CROSS-VERIFY-1M-01/02)`.
    - **NOT** the deleted 90-day chain: 1-minute only, spot only, today only,
      post-market only, no other timeframe, no synthesized ticks into `ticks`
      (rules 1–6 stand), lives in `crates/app/src/cross_verify_1m_boot.rs` +
      `crates/storage/src/cross_verify_1m_audit_persistence.rs` (NOT under
      `crates/core/src/historical/`).
    - **Reads `candles_1m`** (allowed — "Reading `ticks`/`candles_*` from
      pipeline code is ALLOWED"); writes ONLY to the new audit table + CSV.

## Test ratchet

- Pre-commit hook (banned-pattern-scanner.sh, category 6) — fails the
  commit if any of the banned symbols reappear in a historical/ path.
- `crates/storage/tests/live_feed_purity_guard.rs` — (to be added) scans
  the source tree at test time for the same violations, so `cargo test`
  catches them even if the pre-commit hook was bypassed.

## What this rule does NOT forbid

- Reading `ticks` from pipeline code — ALLOWED.
- Aggregating `ticks` into `candles_*` materialized views — ALLOWED.

PR-E (2026-05-26) retired every other historical surface:
`candle_fetcher.rs`, `cross_verify.rs`, `candle_persistence.rs`, and
the `historical_candles` table are all deleted. The rule's only
surviving teeth are now "no synthesised ticks into the `ticks` table",
which remains enforced.

## Historical context

- Commit `TBD` (2026-04-17) — DELETED `crates/core/src/historical/backfill.rs`
  (836 lines) + `crates/core/tests/dhat_backfill_synth.rs` (119 lines).
  Removed 9 `tv_backfill_*` Prometheus metrics from the catalog.
- Rule file created same day.
- Banned-pattern scanner category 6 added same day.
