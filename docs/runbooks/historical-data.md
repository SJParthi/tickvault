# Runbook — Historical data fetch failures

**When it fires:** `HighHistoricalFetchErrors`, `DH-904` (rate limit),
`DH-908/909` (Dhan 5xx), `DATA-811/812`, plus anything referencing
`candle_fetcher.rs`.

**Consequence if not resolved:** cross-verification between live and
historical candles fails silently → potential SEBI audit gap. Strategy
backtests run on incomplete data.

## Quick facts

- Endpoints:
  - Daily: `POST https://api.dhan.co/v2/charts/historical`
  - Intraday: `POST https://api.dhan.co/v2/charts/intraday`
- Response is **columnar parallel arrays**, NOT row objects
- Max intraday range: **90 days/request**; chunk longer spans
- `toDate` is **non-inclusive** on daily endpoint
- Timestamps: UNIX seconds (daily stamped IST midnight = UTC 18:30 prev day)
- Rate limit: Data API category — see `.claude/rules/dhan/api-introduction.md`

## Telegram alert → action (60-second version)

| Alert | First action |
|---|---|
| 🟠 `HighHistoricalFetchErrors` (baseline ~10K/day) | Baseline is KNOWN NOISE due to DH-904 retries. Only investigate if > 50K/day or if `tv_historical_candles_fetched_total` flatlines. |
| 🔴 `DH-904` burst on historical | Backoff is auto. If persistent > 30 min, halt the fetch via config. |
| 🔴 `DH-908/909` (5xx or network) | Dhan-side. Check status.dhan.co. Auto-retry handles transient. |
| 🟠 `DATA-811` (invalid expiry) | Expiry date guessed instead of fetched from expirylist API — fix caller. |
| 🟠 `DATA-812` (invalid date format) | Date format drift — daily uses `YYYY-MM-DD`, intraday uses `YYYY-MM-DD HH:MM:SS`. |

## Root-cause checklist

### 1. Is the fetcher actually running?

```bash
mcp__tickvault-logs__prometheus_query("tv_historical_candles_fetched_total")
# Expected: increases during off-market hours. Flat = fetcher crashed.
```

### 2. Error rate proportion

```bash
# Error ratio
mcp__tickvault-logs__prometheus_query(
  "rate(tv_historical_fetch_errors_total[1h]) / rate(tv_historical_candles_fetched_total[1h])"
)
# Baseline 0.001–0.01. Above 0.1 = abnormal.
```

### 3. Date window sanity

Historical is expected to run in overnight batches. Check the requested
window against market holidays (`config/base.toml → trading.nse_holidays`).
Requesting data for a weekend/holiday = empty response, not an error —
but wastes the daily quota.

### 4. Cross-verification output

```bash
grep "cross-match" data/logs/app.*.log | tail -20
# Expected output: "Historical vs Live cross-match OK"
# If mismatches found, each row lists symbol/timeframe/timestamp/field
```

Zero tolerance on cross-match — any diff > 0.0 means a data corruption.

## Recovery

### Rate-limit burst won't clear

If DH-904 is persistent > 30 min:

1. Halt the fetcher via config (`config/base.toml → historical.enabled = false`)
2. Wait 1 hour for Dhan's daily quota to clear
3. Re-enable with a smaller chunk size (drop from 90-day to 30-day)

### Columnar parse failure

Symptom: log line "HistoricalDataResponse deserialization failed".
Cause: Dhan changed response shape (rare — check release notes).

```bash
# Capture the raw response for analysis
mcp__tickvault-logs__tail_errors --limit 50 --code DH-907
# Look for the full request + response body in structured field `response`
```

### Missing candles on a known trading day

```bash
# Compare against live materialized view
# QuestDB SQL (via PG wire or HTTP):
SELECT count(*) FROM historical_candles
WHERE security_id = <X> AND timeframe = '1m'
  AND ts BETWEEN <start> AND <end>;

SELECT count(*) FROM candles_1m
WHERE security_id = <X>
  AND ts BETWEEN <start> AND <end>;

# Counts should match to the minute. If historical < live = refetch.
# If historical > live = live pipeline had a gap (see zero-tick-loss.md).
```

## Never do these

- **Never inject historical candles into the `ticks` table.** Hard
  enforcement — `live_feed_purity_guard` fails the build.
- **Never guess expiry dates.** Always fetch via
  `POST /v2/optionchain/expirylist`.
- **Never use epoch milliseconds.** Dhan historical uses SECONDS.
- **Never mix daily vs intraday date formats.** Daily is `YYYY-MM-DD`,
  intraday is `YYYY-MM-DD HH:MM:SS` — not interchangeable.
- **Never retry DH-904 without backoff.** Exponential 10s→20s→40s→80s.
  Immediate retry = compounds into account-level rate ban.

## Related files

- `.claude/rules/dhan/historical-data.md` — Dhan API spec
- `crates/core/src/historical/candle_fetcher.rs` — fetch + retry
- `crates/core/src/historical/cross_verify.rs` — live-vs-historical diff
- `crates/storage/src/candle_persistence.rs` — `historical_candles` writer
- `.claude/rules/project/historical-candles-cross-verify.md` — invariants
- `deploy/docker/grafana/dashboards/data-lifecycle.json` — panel 11
