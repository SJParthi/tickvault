# Dhan Historical Data Enforcement

> **Ground truth:** `docs/dhan-ref/05-historical-data.md`
> **Scope:** Any file touching historical OHLCV fetching, candle storage, intraday data chunking, or timestamp conversion.
> **Cross-reference:** `docs/dhan-ref/08-annexure-enums.md` (ExchangeSegment, InstrumentType, ExpiryCode, rate limits, timestamps)

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any historical data fetcher, OHLCV parser, or candle storage: `Read docs/dhan-ref/05-historical-data.md`.

2. **Two endpoints — exact URLs.**
   - Daily: `POST https://api.dhan.co/v2/charts/historical`
   - Intraday: `POST https://api.dhan.co/v2/charts/intraday`
   - Both require `Content-Type: application/json` and `access-token` header.

3. **Response is columnar parallel arrays, NOT row objects.**
   ```json
   { "open": [...], "high": [...], "low": [...], "close": [...], "volume": [...], "timestamp": [...], "open_interest": [...] }
   ```
   All arrays same length. Index `i` across all arrays = one candle. Always validate array lengths match before processing.

4. **Request fields — exact types.**
   - `securityId`: STRING (`"1333"`)
   - `exchangeSegment`: STRING enum (`"NSE_EQ"`)
   - `instrument`: STRING enum (`"EQUITY"`, `"FUTIDX"`, etc.)
   - `expiryCode`: INTEGER (`0`, `1`, `2`) — optional
   - `oi`: BOOLEAN — optional
   - `fromDate` (daily): `"YYYY-MM-DD"`
   - `toDate` (daily): `"YYYY-MM-DD"` — **NON-INCLUSIVE** (toDate Feb 8 → last candle = Feb 7)
   - `fromDate` (intraday): `"YYYY-MM-DD HH:MM:SS"`
   - `toDate` (intraday): `"YYYY-MM-DD HH:MM:SS"`

5. **Interval is a STRING for intraday.** Exactly `"1"`, `"5"`, `"15"`, `"25"`, or `"60"`. Not integers. Not any other values.

6. **Intraday max range = 90 days per request.** Validate before sending. For longer periods, chunk into 90-day windows with sliding dates.

7. **Intraday data available for last 5 years.** Daily data available back to instrument inception.

8. **Timestamps are UNIX epoch SECONDS** (see `dhan-annexure-enums.md` rule 13 for base format).
   - Daily candles: stamped at IST midnight = UTC 18:30 previous day (e.g., `1326220200` = IST 2012-01-11 00:00:00)
   - Intraday candles: stamped at candle open time in UTC (e.g., `1328845500` = IST 09:15:00)
   - QuestDB needs microseconds: multiply by 1,000,000

9. **`toDate` is NON-INCLUSIVE for daily.** Requesting `fromDate: "2022-01-08"` to `toDate: "2022-02-08"` gives data from Jan 8 to Feb 7 inclusive.

10. **Rate limit: Data API category.** Sleep 200ms between calls during heavy fetching. See `dhan-api-introduction.md` rules 7-8 for exact limits and DH-904 backoff.

11. **OI array may be all zeros.** For equity instruments, `open_interest` is always `[0,0,0,...]`. Only meaningful for F&O.

12. **Store data locally.** Dhan recommends caching. Don't re-fetch the same date ranges repeatedly.

13. **serde field naming: `camelCase`.** Use `#[serde(rename_all = "camelCase")]` for request structs. Response fields are `snake_case` (`open_interest`).

## What This Prevents

- Row-object parsing on columnar response → deserialization failure or wrong data mapping
- Integer interval → API rejects request → no data fetched
- >90 day intraday request → error response
- Timestamp confusion (secs vs millis vs micros) → dates off by 1000x
- Inclusive toDate assumption → missing one day of data
- Re-fetching same data → wasting daily quota

## Trigger

This rule activates when editing files matching:
- `crates/core/src/historical/*.rs`
- `crates/storage/src/candle_persistence.rs`
- `crates/core/src/pipeline/candle_aggregator.rs`
- `crates/core/tests/deterministic_replay.rs`
- Any file containing `OhlcvResponse`, `HistoricalDataResponse`, `HistoricalDailyRequest`, `HistoricalIntradayRequest`, `charts/historical`, `charts/intraday`, `candle_fetcher`, `candle_persistence`, `expiryCode`, `Candle`, `to_candles`
