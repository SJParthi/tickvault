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

4. **Historical data** lives in two places only:
   - `historical_candles` table — Dhan REST `/charts/historical` results.
   - `candles_*` materialized views — aggregated live ticks from `ticks`.

5. **`ticks` table** has ONE write path: `run_tick_processor` in
   `crates/core/src/pipeline/tick_processor.rs`, which consumes the WAL
   / SPSC from the WebSocket connection pool. Any NEW writer to `ticks`
   requires Parthiban approval.

6. **`TickGapTracker`** still detects gaps for observability (metrics +
   Telegram), but the gap event NO LONGER triggers any backfill. Missing
   ticks stay missing. That is correct behaviour — forged ticks are
   worse than absent ticks for a regulated trading system.

7. **Cross-verification** (`crates/core/src/historical/cross_verify.rs`)
   READS from both `historical_candles` and `candles_*` views. It never
   WRITES to either. That file is allowed to continue using historical
   data because it only compares, never produces.

8. **Pre-market yesterday's-1d fetch exception (operator-locked 2026-05-25)** —
   Parthiban verbatim 2026-05-25 14:30 IST:
   > "see this should always happen onl yat the time of pre market dude
   >  okay? Fetch yesterday's 1d for 4 IDX_I SIDs"
   This is an EXPLICIT override of the 2026-04-22 directive
   ("historical fetch must NEVER run pre-market on a trading day") for
   the narrow case of yesterday's-1d-only verification:
   - **Scope:** 4 IDX_I SIDs (`LOCKED_UNIVERSE`) × 1 timeframe (1d) × 1 day (yesterday) — at most ~20 Dhan REST calls (tiny vs. 90-day post-market hammer)
   - **Trigger:** 09:00:30 IST every trading day (operator-locked, `DAILY_1D_FIRE_SECS_OF_DAY_IST` constant)
   - **Purpose:** confirm yesterday's 1d candle is intact in `historical_candles` BEFORE today's session starts; forensic record via JSONL + Telegram only on mismatch
   - **Marker:** separate marker file from the existing 15:30 IST post-market full-historical-fetch marker. Pre-market fetch does NOT block the post-market full fetch.
   - **Bulk historical fetch (1m/5m/15m/60m × 90 days)** remains POST-MARKET ONLY per the original 2026-04-22 directive. The exception applies ONLY to the narrow yesterday's-1d case.

## Test ratchet

- Pre-commit hook (banned-pattern-scanner.sh, category 6) — fails the
  commit if any of the banned symbols reappear in a historical/ path.
- `crates/storage/tests/live_feed_purity_guard.rs` — (to be added) scans
  the source tree at test time for the same violations, so `cargo test`
  catches them even if the pre-commit hook was bypassed.

## What this rule does NOT forbid

- Fetching historical candles (`candle_fetcher.rs`) — ALLOWED.
- Storing historical candles in `historical_candles` — ALLOWED.
- Cross-verifying (`cross_verify.rs`) — ALLOWED.
- Reading `ticks` from pipeline code — ALLOWED.
- Reading `historical_candles` from dashboards — ALLOWED.

Only synthesising ticks from historical data and writing them to `ticks`
is banned.

## Historical context

- Commit `TBD` (2026-04-17) — DELETED `crates/core/src/historical/backfill.rs`
  (836 lines) + `crates/core/tests/dhat_backfill_synth.rs` (119 lines).
  Removed 9 `tv_backfill_*` Prometheus metrics from the catalog.
- Rule file created same day.
- Banned-pattern scanner category 6 added same day.
