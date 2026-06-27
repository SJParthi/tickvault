# Active Plan — fix(api): /api/quote queries real `ticks` columns + feed-aware + null-safe OHLC

**Status:** IN_PROGRESS
**Crate:** `crates/api` (`crates/api/src/handlers/quote.rs`)
**Guarantee matrix cross-ref:** `.claude/rules/project/per-wave-guarantee-matrix.md`
(15-row + 7-row matrices apply; this is an HTTP cold-path read — not a hot path,
so the DHAT/Criterion rows are `N/A — HTTP read path`).

## Design

The live `GET /api/quote/{security_id}` handler (registered in
`crates/api/src/lib.rs:168-169`, so it is LIVE, not dead) SELECTs columns that do
NOT exist in the current `ticks` DDL (`crates/storage/src/tick_persistence.rs`
`TICKS_CREATE_DDL`): `exchange_segment_code`, `last_traded_quantity`,
`total_traded_volume`, `day_open/high/low/close`. The query therefore fails at
runtime for BOTH feeds.

The real `ticks` columns are: `feed`, `segment` (SYMBOL string, e.g. `"NSE_EQ"` —
NOT a numeric code), `security_id`, `ltp`, `open`, `high`, `low`, `close`,
`volume`, `oi`, `avg_price`, `last_trade_qty`, `total_buy_qty`, `total_sell_qty`,
`exchange_timestamp`, `received_at`, `payload_hash`, `capture_seq`, `ts`.

**Fix:** rewrite the SELECT to query the columns that exist, map the response
struct to them. **Feed-aware (design choice (a) — latest tick across feeds, labelled):**
the `ticks` table is shared by Dhan + Groww (both write it, distinguished by
`feed`). The `LATEST ON ts PARTITION BY security_id` returns the single most
recent row regardless of feed; we expose its `feed` label so the caller knows the
source. Choice (a) over (b `?feed=` param) because it is the simpler correct
default and matches the handler's existing single-row shape — the caller always
gets the freshest observation and is told which feed produced it.

**Null-safe:** Groww writes only a 9-of-19 column subset; the rest
(`open/high/low/close/oi/avg_price/last_trade_qty/total_buy_qty/total_sell_qty`)
are NULL by design. The response must surface those as JSON `null` (Rust
`Option<T>`), NOT a misleading `0.0`/`0`. So a Groww latest row honestly reports
`"open": null` instead of faking a 0 OHLC.

## Edge Cases

- Groww latest row → OHLC/OI/avg_price/qty fields are NULL → `None` → JSON `null`.
- Dhan latest row → all fields populated → `Some(..)`.
- `security_id == 0` → 400 (unchanged guard).
- Empty dataset / malformed JSON / row too short → `None` → 404 / 503 (unchanged).
- `ltp`, `security_id`, `ts`, `feed` are mandatory (`?`); optional numeric fields
  use `as_f64()`/`as_u64()` mapped to `Option` (null or absent → `None`, never panic).

## Failure Modes

- QuestDB unreachable → 503 (unchanged connectivity check).
- QuestDB reachable, no rows → 404 (unchanged).
- A NULL in an `Option` field → `None` (no `unwrap`/`expect` that could panic).
- No secret is logged (handler logs nothing sensitive).

## Test Plan

- Extend the unit/integration tests in `quote.rs`:
  - real DDL column count/order pinned by the mock-dataset shape.
  - response carries `feed`.
  - a Groww-style row with NULL OHLC maps to `None`/`null`, NOT `0.0`.
  - a Dhan-style fully-populated row maps to `Some(..)`.
- Update existing mock-dataset tests to the new column order.
- `cargo test -p tickvault-api` green; `cargo fmt --check`; build 0 warnings.
- Bump `.claude/hooks/.test-count-baseline` for net-new tests.

## Rollback

Single-file logic change (+ tests + baseline bump). Revert the commit;
the endpoint returns to its (broken) prior state. No schema/migration/data change.

## Observability

HTTP read endpoint. No new metric/counter required (cold path, per-request).
The fix makes the endpoint actually return data instead of silently failing, and
the `feed` field + nullable OHLC make a Groww quote self-describing to the caller.
