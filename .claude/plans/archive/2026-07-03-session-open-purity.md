# Implementation Plan: Session-open purity — gate the day-OHLC tracker to [09:15, 15:30) IST

**Status:** VERIFIED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator directive 2026-07-03, verbatim intent): "Session window is 09:15:00 IST inclusive → 15:30:00 IST exclusive. For Dhan or Groww it doesn't matter — never ever take or set the pre-market-open data into the 9:15 AM open candle data; ensure the 9:15 AM starting ticks are the 9:15 open tick."
**Branch:** `claude/session-open-purity`
**Changed crates:** `app` (crates/app), `trading` (crates/trading)

> Guarantee matrices: carried by cross-reference to
> `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row, mandatory
> per `per-item-guarantee-check.sh`). Dominant guarantee here: **strictly no
> worse than today** — the candle aggregator's session gate is UNCHANGED; this
> plan only adds the SAME gate to the one consumer that lacked it (day-OHLC),
> plus boundary ratchet tests. No tick-drop path is touched (`ticks` table
> persistence is a separate consumer, untouched).

## Design

**The bug (audit-verified):** `crates/app/src/day_ohlc_orchestrator.rs::spawn_day_ohlc_tick_consumer`
calls `DayOhlcTracker::update_tick()` on EVERY IDX_I broadcast tick with NO
session gate. Dhan ingestion admits ticks from 09:00 IST
(`TICK_PERSIST_START_SECS_OF_DAY_IST = 32_400`, `crates/common/src/constants.rs:1237`),
so a 09:00–09:14 pre-open indicative index tick auto-arms `day_open` (and
high/low/close) to a PRE-MARKET price — violating the operator directive that
the 09:15 open must be the 09:15 tick.

**The fix:** a small pure function in `day_ohlc_orchestrator.rs`:

```rust
pub fn day_ohlc_session_accepts(exchange_timestamp_ist_secs: u32) -> bool {
    let secs_of_day = exchange_timestamp_ist_secs % SECONDS_PER_DAY;
    g1_exchange_gate_accepts(i64::from(secs_of_day) * NANOS_PER_SECOND)
}
```

called in the consumer loop BEFORE `tracker.update_tick(...)`; out-of-session
ticks `continue` after a single static-label counter increment
(`tv_day_ohlc_session_gate_skipped_total`).

**Gate-choice rationale (constants reuse over new magic numbers):** the app
crate cannot see `crates/trading`'s `pub(crate)` `MARKET_OPEN_SECS_OF_DAY_IST` /
`MARKET_CLOSE_SECS_OF_DAY_IST` (tf_index.rs:35,41) without widening their
visibility. Instead the fix REUSES the already-`pub` canonical session gate
`tickvault_common::constants::g1_exchange_gate_accepts`
(constants.rs:2675 — half-open `[09:15:00.000, 15:30:00.000)` in IST
nanos-of-day, pinned by the existing `test_g1_exchange_gate_*` ratchets),
converting the tick's IST-epoch-seconds → nanos-of-day. Zero new session
constants; zero edits to `crates/common`; the day-OHLC window is BY
CONSTRUCTION identical to the G1 exchange gate. The aggregator's own duplicate
seconds-form constants in `tf_index.rs` get a drift-pin test against the
common-crate nanos constants so the two representations can never diverge.

**What is deliberately NOT changed:** the candle aggregator gate in
`multi_tf_aggregator.rs::consume_tick` (already correct, both feeds), the
`TfIndex::bucket_start` 09:15 clamp, the `ticks`-table persist window
[09:00, 15:30), and the Groww bridge. Both feeds' candle paths already fold
through the shared gated `consume_tick`.

**Hot-path budget:** the gate is one `%`, one multiply, two integer compares —
O(1), zero allocation, no per-tick log. The skip-path counter uses a static
label (same style as the existing `tv_day_ohlc_tick_consumer_lagged_total` in
this file). The consumer only sees IDX_I broadcast ticks (~a few/sec × 4 SIDs).

## Edge Cases

- **09:14:59 (secs_of_day 33_299):** rejected — last pre-open second.
- **09:15:00 (33_300) exactly:** accepted — this tick IS the day open (inclusive open boundary).
- **15:29:59 (55_799):** accepted — last session second.
- **15:30:00 (55_800) exactly:** rejected — close is EXCLUSIVE, matching the aggregator gate and `g1_exchange_gate_accepts`.
- **09:07 pre-open indicative tick (32_820):** rejected — the original bug scenario; tracker stays disarmed.
- **Multi-day epoch values:** `% SECONDS_PER_DAY` reduces any IST epoch second correctly (Dhan LTT is IST epoch seconds per `data-integrity.md`).
- **Post-close snapshot ticks (e.g. 15:35, overnight):** rejected — day_close stays the last in-session LTP instead of drifting on stale post-close snapshots.
- **exchange_timestamp = 0 (malformed):** secs_of_day 0 → rejected (fail-closed; previously it would have armed day_open).
- **Always-on SIDs (GIFT Nifty, operator lock 2026-06-01 §30):** EXEMPT from the window via `day_ohlc_gate_allows` — GIFT is an IDX_I index (~21 h NSE-IX session), so its legitimate out-of-window ticks pass the gate instead of being skipped + inflating `tv_day_ohlc_session_gate_skipped_total`. Reuses the SAME boot-computed set the candle aggregator + tick processor use (`tickvault_common::always_on::current()` at the spawn site, passed explicitly for testability — mirrors `with_always_on`). A same-SID-different-segment tick is NOT exempt (I-P1-11 composite key).

## Failure Modes

- **Gate too wide/narrow (regression):** pinned by 5 boundary unit tests on `day_ohlc_session_accepts` + 4 aggregator exact-boundary ratchets + the constant drift-pin test. Any drift between the trading-crate seconds constants and the common-crate nanos constants fails the build.
- **No ticks between 09:15 and close (dead feed):** tracker stays disarmed — same behaviour as today for a dead feed; FEED-STALL/SELFTEST codes own that page, not this gate.
- **Counter emission failure:** `metrics::counter!` is infallible (no-op without a recorder); the gate never blocks or panics.
- **Rollback:** revert the commit — the consumer returns to ungated behaviour (pre-open arm bug returns but nothing else changes). No schema, no config, no persisted state involved.

## Test Plan

- `crates/app/src/day_ohlc_orchestrator.rs::tests`:
  - `test_day_ohlc_session_gate_rejects_preopen_0907`
  - `test_day_ohlc_session_gate_rejects_0914_59`
  - `test_day_ohlc_session_gate_accepts_0915_00_exact_open`
  - `test_day_ohlc_session_gate_accepts_15_29_59`
  - `test_day_ohlc_session_gate_rejects_15_30_00_exclusive_close`
  - `test_preopen_tick_never_arms_day_open_then_0915_tick_is_the_open` (real `DayOhlcTracker` through the gated predicate: 09:07 tick → disarmed; 09:15:00 tick → armed, `day_open == that LTP`)
- `crates/trading/src/candles/multi_tf_aggregator.rs::tests`:
  - `test_0914_59_tick_never_seeds_0915_open`
  - `test_0915_00_first_tick_is_the_0915_open`
  - `test_15_29_59_tick_is_consumed`
  - `test_15_30_00_tick_is_skipped`
- `crates/trading/src/candles/tf_index.rs::tests`:
  - `test_session_constants_pinned_and_agree_with_common_crate` (33_300 / 55_800 == `MARKET_OPEN_IST_NANOS` / `MARKET_CLOSE_IST_NANOS` ÷ 1e9)
- Scoped runs: `cargo test -p tickvault-trading`, `cargo test -p tickvault-app --lib`; fmt + clippy `-D warnings -W clippy::perf`; banned-pattern scanner; plan-gate; plan-verify.

## Rollback

Single-commit revert per file group (fix commit / test commit / docs commit are
independent). No migration, no config change, no persisted-state change — the
gate is stateless pure logic in one consumer task. Reverting restores today's
behaviour exactly.

## Observability

- New static-label counter `tv_day_ohlc_session_gate_skipped_total` — one increment per skipped out-of-session IDX_I tick (mirrors the existing counter style in the same file). A sustained non-zero rate during 09:15–15:30 would indicate timestamp corruption upstream (BOOT-03 class); non-zero rates 09:00–09:15 and post-close are EXPECTED (that is the gate working).
- No new ErrorCode: the skip is normal, expected behaviour, not a failure (per `wave-4-shared-preamble.md` §4 the 7-layer stack applies to failure modes; this is a filter with a counter).
- Docs: dated 2026-07-03 update in `.claude/rules/project/index-day-ohlc-tracker-error-codes.md` §0 + module-doc update in `day_ohlc_orchestrator.rs`.

## Plan Items

- [x] Item 1 — Session-gate the day-OHLC consumer via `day_ohlc_session_accepts` (reusing `g1_exchange_gate_accepts`), + skip counter + module-doc update
  - Files: crates/app/src/day_ohlc_orchestrator.rs
  - Tests: test_day_ohlc_session_gate_rejects_preopen_0907, test_day_ohlc_session_gate_rejects_0914_59, test_day_ohlc_session_gate_accepts_0915_00_exact_open, test_day_ohlc_session_gate_accepts_15_29_59, test_day_ohlc_session_gate_rejects_15_30_00_exclusive_close, test_preopen_tick_never_arms_day_open_then_0915_tick_is_the_open

- [x] Item 2 — Aggregator exact-boundary ratchets (09:14:59 / 09:15:00 / 15:29:59 / 15:30:00)
  - Files: crates/trading/src/candles/multi_tf_aggregator.rs
  - Tests: test_0914_59_tick_never_seeds_0915_open, test_0915_00_first_tick_is_the_0915_open, test_15_29_59_tick_is_consumed, test_15_30_00_tick_is_skipped

- [x] Item 3 — Session-constant drift pin (trading seconds constants vs common nanos constants)
  - Files: crates/trading/src/candles/tf_index.rs
  - Tests: test_session_constants_pinned_and_agree_with_common_crate

- [x] Item 4 — Docs: dated 2026-07-03 update in index-day-ohlc-tracker-error-codes.md §0
  - Files: .claude/rules/project/index-day-ohlc-tracker-error-codes.md
  - Tests: (docs only)

- [x] Item 5 — Review fix (MEDIUM): always-on (§30 GIFT Nifty) exemption in the day-OHLC gate via `day_ohlc_gate_allows`, reusing `tickvault_common::always_on::current()` at the spawn site (same source as the aggregator); + doc nit (Dhan-only LTT wording — Groww never reaches this consumer)
  - Files: crates/app/src/day_ohlc_orchestrator.rs, crates/app/src/main.rs
  - Tests: test_day_ohlc_gate_allows_exempts_always_on_sid_at_2000_ist, test_day_ohlc_session_accepts_is_the_gate_when_always_on_empty

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | 09:07 IST pre-open indicative NIFTY tick | Skipped; tracker disarmed; counter +1 |
| 2 | 09:14:59 tick | Skipped (both day-OHLC and candle grid) |
| 3 | 09:15:00 first tick | Arms day_open AND opens the 09:15 candle at that LTP |
| 4 | 15:29:59 tick | Consumed by both paths |
| 5 | 15:30:00 tick | Skipped by both paths (exclusive close); aggregator watermark still advances |
| 6 | Constants drift (someone edits 33_300 in one crate) | Drift-pin test + const asserts fail the build |
