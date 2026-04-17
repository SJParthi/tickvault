# Historical Candles & Cross-Verification Rules

> **Authority:** CLAUDE.md > this file.
> **Scope:** Any file touching historical candle fetching, cross-verification, or candle persistence.
> **Ground truth:** `docs/dhan-ref/05-historical-data.md`

## Historical Candle Fetch

### What Gets Fetched
- **Indices** (IDX_I): ALL 5 major indices — 90 days of daily + intraday candles
- **Stock equities** (NSE_EQ): ALL ~206 stocks — 90 days of daily + intraday candles
- **Derivatives**: SKIPPED — Dhan historical API does not support F&O contracts
- **Timeframes**: 1m, 5m, 15m, 60m, 1d

### When It Runs
- **At boot** (cold path): fetches 90 days of history for all eligible instruments
- **Post-market** (after 15:30 IST): re-fetches today's candles for cross-verification
- **Never blocks live data**: runs in background, separate from WebSocket pipeline

### Retry Mechanism
- Per-timeframe: up to `max_retries` (default 3-5) with linear backoff
- Per-instrument: up to 100 retry waves with exponential sleep (30s to 300s per wave)
- DH-904 (rate limit): exponential backoff 10s → 20s → 40s → 80s, then fail single timeframe
- DH-805 (too many connections): pause ALL requests for 60 seconds
- Token expiry (807/901): wait 30s for token refresh, retry up to 10 times
- Total retry budget: ~8 hours of cumulative backoff

### Metrics
- `tv_historical_candles_fetched_total` — candles successfully persisted
- `tv_historical_api_errors_total` — ALL error increments (transient + permanent, per retry)
- Note: 119K errors is normal if Dhan rate-limits heavily — most are transient DH-904 retries

### Skip Reasons
- `IndexDerivative` / `StockDerivative` categories → always skipped (Dhan API limitation)
- These account for ~99% of "skipped" instruments in the notification

### Idempotency
- QuestDB DEDUP UPSERT KEYS(ts, security_id, timeframe) prevents duplicate candles
- Restart mid-fetch: re-fetches everything (no checkpoint), DEDUP handles overlap

## Cross-Verification

### Purpose
Compares **historical candles** (Dhan REST API) against **live candles** (materialized
views from WebSocket ticks: `candles_1m`, `candles_5m`, etc.) to detect:
1. Missed ticks (historical exists, live doesn't)
2. Price drift (OHLC values differ)
3. Volume/OI discrepancies

### When It Runs
- After post-market historical candle re-fetch completes
- Only meaningful when BOTH historical AND live data exist for the same day
- If live ticks = 0 (e.g., fresh start after market close), cross-match reports "OK"
  because there's nothing to compare against

### Tolerance: ZERO (Exact Match)
```rust
const CROSS_MATCH_PRICE_EPSILON: f64 = 0.0;           // EXACT price match
const CROSS_MATCH_VOLUME_TOLERANCE_PCT: f64 = 0.0;    // EXACT volume match
const CROSS_MATCH_OI_TOLERANCE_PCT: f64 = 0.0;        // EXACT OI match
```
**NO tolerance. NO epsilon. NO ±10%. Every value must match exactly.**

### Telegram Messages

**When ALL candles match:**
```
Historical vs Live cross-match OK
Timeframes: 5 | Candles compared: 237805
All OHLCV values match exactly (zero tolerance, precision-verified)
```

**When mismatches found:**
```
Historical vs Live cross-match FAILED
Compared: 237805 | Mismatches: 42
Missing live: 15

Mismatches:
RELIANCE NSE_EQ 1m 2026-04-16 10:15 — open: hist=2847.50 live=2847.00
INFY NSE_EQ 1m 2026-04-16 11:30 — high: hist=1520.00 live=1519.50
NIFTY IDX_I 5m 2026-04-16 09:20 — volume: hist=5000000 live=4999800
... +39 more
```

Each mismatch shows: symbol, segment, timeframe, exact IST timestamp, which field
differs, historical value vs live value. Up to 10 details in Telegram, full list in logs.

`Missing live: N` = N timestamps had historical candles but NO live WebSocket candle.
These represent missed ticks during the trading session.

### Verification Checks (5 types)
1. **Per-timeframe candle count**: each timeframe has expected minimum candles
2. **OHLC consistency**: high >= low for all candles
3. **Data integrity**: no non-positive prices (open/high/low/close <= 0)
4. **Timestamp bounds**: intraday candles within 09:15-15:29 IST
5. **Weekend violations**: no candles on Saturday/Sunday

## Timestamp Rules

- **Historical REST API**: returns UTC epoch seconds → add +19800 (IST offset) before storing
- **Live WebSocket**: `exchange_timestamp` is IST epoch seconds → store directly, NO offset
- **QuestDB `ts`**: always IST epoch nanos (both sources produce same representation)

## Backfill Worker: DELETED (2026-04-17)

The BackfillWorker that injected synthetic ticks from historical candles into the
live `ticks` table has been **permanently DELETED** — not just disabled. The
source file `crates/core/src/historical/backfill.rs` (836 lines) was removed
along with the DHAT test `crates/core/tests/dhat_backfill_synth.rs`.

Historical candles go to the `historical_candles` table ONLY. The `ticks` table
contains ONLY live WebSocket-sourced data.

**Hard enforcement** (per Parthiban directive 2026-04-17):
- `.claude/rules/project/live-feed-purity.md` — authoritative rule file.
- `.claude/hooks/banned-pattern-scanner.sh` category 6 — blocks commits that
  re-introduce `BackfillWorker`, `run_backfill`, `synthesize_ticks`,
  `TickPersistenceWriter` or `append_tick` in any historical/backfill/synth path.
- `crates/storage/tests/live_feed_purity_guard.rs` — 6 runtime tests that fail
  the build if `backfill.rs` is recreated, if `pub mod backfill;` reappears in
  `historical/mod.rs`, or if any banned symbol appears inside the historical
  flow.

## Key Files

| File | Purpose |
|------|---------|
| `crates/core/src/historical/candle_fetcher.rs` | 90-day fetch, retry logic, rate limit handling |
| `crates/core/src/historical/cross_verify.rs` | Cross-match + data integrity checks |
| `crates/storage/src/candle_persistence.rs` | QuestDB write (historical_candles + live candles) |
| `crates/core/src/notification/events.rs` | Telegram message formatting |

## Trigger

This rule activates when editing files matching:
- `crates/core/src/historical/*.rs`
- `crates/storage/src/candle_persistence.rs`
- Any file containing `cross_verify`, `cross_match`, `candle_fetcher`, `historical_candles`,
  `CandleCrossMatchPassed`, `CandleCrossMatchFailed`, `CROSS_MATCH_PRICE_EPSILON`,
  `HistoricalFetchComplete`, `CandleVerificationPassed`, `CandleVerificationFailed`
