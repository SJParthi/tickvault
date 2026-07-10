# Implementation Plan: Pre-open guard — 09:15 candle open + DayOhlcTracker session gate

**Status:** VERIFIED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — grounded directive 2026-07-03, this session

> **Operator rule (verbatim intent, 2026-07-03):** "the trading session window
> is [09:15:00 IST inclusive, 15:30:00 exclusive) and — for BOTH dhan and groww
> feeds — PRE-MARKET data (09:00–09:15 pre-open session, or any earlier tick)
> must NEVER become the 09:15 candle's open or leak into intraday candles: the
> 09:15 open must be the first REGULAR-session tick with exchange timestamp
> ≥ 09:15:00.000."

Crates touched: `crates/trading` (candles/multi_tf_aggregator.rs — ratchet
tests only, no logic change), `crates/app` (day_ohlc_orchestrator.rs — the
one real fix).

## Design

**Verified-safe (no change needed):** the shared 21-TF aggregator session gate
at `crates/trading/src/candles/multi_tf_aggregator.rs:461-486` already
discards every non-exempt tick whose EXCHANGE timestamp (`tick.exchange_timestamp
% 86_400`) is `< MARKET_OPEN_SECS_OF_DAY_IST (33_300 = 09:15:00)` or
`>= MARKET_CLOSE_SECS_OF_DAY_IST (55_800 = 15:30:00)` — half-open, boundary
exact, exchange-ts based (never receive time). BOTH candle-writing ingest
paths route through this single gate: Dhan (`crates/app/src/main.rs:4505`,
the Engine-B tick-broadcast subscriber) and Groww
(`crates/app/src/groww_bridge.rs:1152`, same `MultiTfAggregator::consume_tick`;
Groww ms timestamps floor to IST epoch seconds in `build_parsed_tick`,
`groww_bridge.rs:336`, so 09:14:59.999 → 33,299 → rejected). No second
candle writer exists (in-loop candle aggregation was removed from
`tick_processor.rs` per #T1b). The `session_open` candle column is captured
only from gated (≥ 09:15) ticks' Quote-packet `day_open` field with a
day_open==0 no-clobber guard (`aggregator_cell.rs`, pinned by
`test_session_open_captured_from_day_open`). This plan adds RATCHET TESTS
pinning the boundary (09:14:59 rejected / 09:15:00 accepted, both feeds; a
pre-market tick never becomes the 09:15 open).

**The real gap (the fix):** `crates/app/src/day_ohlc_orchestrator.rs:52-82`
(`spawn_day_ohlc_tick_consumer`) routes EVERY IDX_I tick from the tick
broadcast into `DayOhlcTracker::update_tick()`, which auto-arms `day_open`
on the FIRST call after the IST-midnight reset
(`crates/trading/src/in_mem/day_ohlc_tracker.rs:93-99,119`). The tick
broadcast carries ticks from 09:00 IST (the persist window
`TICK_PERSIST_START_SECS_OF_DAY_IST = 32_400`, `constants.rs:1237`, gates
the pipeline at `tick_processor.rs:146`), and Dhan streams IDX_I pre-open
values during 09:00–09:15 — so `day_open` arms at a PRE-OPEN price. This
violates the operator rule AND the tracker's own documented contract
(`day_ohlc_tracker.rs:20-22`: "day_open == first traded LTP after 09:15:00
IST").

**Fix (smallest correct):** gate the routing in
`spawn_day_ohlc_tick_consumer` on the session window using the tick's
EXCHANGE timestamp, via the existing pure G1 gate
`tickvault_common::constants::g1_exchange_gate_accepts` (half-open
[09:15:00.000, 15:30:00.000), `constants.rs:2676`). A new pure helper
`should_route_day_ohlc_tick(exchange_segment_code, exchange_timestamp_secs)`
combines the existing IDX_I filter with the G1 gate so it is unit-testable;
the consumer loop calls it. No change to `DayOhlcTracker` itself (frozen-area
adjacent; `in_mem` is outside the §28 boundary but the smaller diff is in
the app-side consumer, which owns the routing policy).

## Edge Cases

- Tick at exactly 09:14:59 (secs-of-day 33,299) → rejected by both the
  aggregator gate and the new day-OHLC gate (ratchet-tested).
- Tick at exactly 09:15:00 (33,300) → accepted by both (ratchet-tested).
- Tick at exactly 15:30:00 (55,800) → rejected (exclusive close; existing
  aggregator test + new day-OHLC helper test).
- Groww millisecond timestamp 09:14:59.999 → floors to 33,299 seconds →
  rejected (the shared `ParsedTick.exchange_timestamp` is second-granular).
- Pre-market tick then first regular tick at 09:15:01 → the M1 candle open
  and the DayOhlc day_open both equal the 09:15:01 price, never the
  pre-market price (ratchet-tested on the aggregator; unit-tested on the
  helper via routing decisions).
- GIFT-Nifty always-on exemption (§30 operator lock) — untouched: the
  aggregator exemption set applies only inside `consume_tick`; the day-OHLC
  consumer only handles IDX_I SIDs which are never in the always-on set by
  default. No behaviour change for exempt instruments.
- IST-midnight reset + first next-day tick: unchanged — the first tick that
  passes the new gate (≥ 09:15) arms day_open; pre-market ticks no longer
  arm it early.

## Failure Modes

- If Dhan delivers ZERO IDX_I ticks in [09:15, 15:30) (feed outage), the
  DayOhlcTracker stays disarmed for the day — identical to today's
  feed-outage behaviour (auto-arm simply waits); no new failure path.
- A malformed/garbage exchange timestamp outside the window is now dropped
  from day-OHLC tracking instead of corrupting day_open — strictly safer.
- The gate is a pure integer comparison (O(1), zero allocation) on an
  already-hot broadcast consumer — no new latency, lock, or panic path.
- No new tick-drop path for persistence/candles: the ticks table and the
  aggregator paths are untouched; only the in-RAM day-OHLC routing narrows.

## Test Plan

- `crates/trading/src/candles/multi_tf_aggregator.rs::tests` (new ratchets):
  - `test_session_gate_boundary_091459_rejected_091500_accepted_dhan`
  - `test_session_gate_boundary_091459_rejected_091500_accepted_groww`
  - `test_pre_market_tick_never_becomes_0915_open`
- `crates/app/src/day_ohlc_orchestrator.rs`: SUPERSEDED by #1379 (merged to
  main 2026-07-03) — main's `day_ohlc_session_accepts` + `day_ohlc_gate_allows`
  (with the §30 always-on exemption) implement the same gate; this PR keeps
  main's version verbatim and its tests (`test_session_gate_*` in that file).
- Scoped runs: `cargo test -p tickvault-trading candles` +
  `cargo test -p tickvault-app day_ohlc`.

## Rollback

Single revert of this PR's commit(s) restores the previous behaviour
(pre-market ticks arming day_open). No schema change, no config change,
no persisted-data migration — the tracker is in-RAM only and resets at IST
midnight, so rollback has zero data-cleanup cost.

## Observability

- The new helper drops out-of-session IDX_I ticks silently by design
  (per-tick logging is banned on the broadcast consumer); the tracker's
  arming behaviour remains observable via the existing
  `test_day_ohlc_orchestrator_is_wired_into_main` source-scan meta-guard
  and the `tv_day_ohlc_tick_consumer_lagged_total` counter (unchanged).
- The candle-side guarantee is observability-pinned by the new ratchet
  tests (build-failing on regression) plus the existing
  `test_non_exempt_tick_outside_window_is_skipped` /
  `test_groww_pre_open_minute_is_gated_intended` ratchets.
- No new ErrorCode: no new `error!` emit site is added (routing narrowing,
  not a failure path).

## Plan Items

- [x] Item 1 — Ratchet tests pinning the aggregator session-gate boundary +
  09:15-open purity (both feeds)
  - Files: crates/trading/src/candles/multi_tf_aggregator.rs
  - Tests: test_session_gate_boundary_091459_rejected_091500_accepted_dhan,
    test_session_gate_boundary_091459_rejected_091500_accepted_groww,
    test_pre_market_tick_never_becomes_0915_open
- [x] Item 2 — Session-gate the DayOhlcTracker tick routing (the fix) —
  SUPERSEDED by #1379 (merged to main first, same session-gate + always-on
  exemption). Conflict resolved by keeping main's day_ohlc_orchestrator.rs
  verbatim; this PR contributes Item 1's aggregator ratchets only.
  - Files: crates/app/src/day_ohlc_orchestrator.rs (identical to main)
  - Tests: covered by main's day_ohlc_session_accepts / day_ohlc_gate_allows tests

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Tick 09:14:59 (either feed) | no candle bucket opens; day_open stays disarmed |
| 2 | Tick 09:15:00 exactly | first M1 bucket opens; day_open arms to that LTP |
| 3 | Pre-market tick 09:10 then 09:15:01 tick | M1 open + day_open == 09:15:01 price |
| 4 | Tick 15:30:00 exactly | rejected by both gates |
| 5 | GIFT-Nifty (always-on) tick 20:00 | still aggregates (exemption intact) |

## Per-Item Guarantee Matrix

Cross-reference: `.claude/rules/project/per-wave-guarantee-matrix.md` (the
canonical 15-row 100% Guarantee Matrix + 7-row Resilience Demand Matrix
apply to both items; this plan's specific proofs are the ratchet tests named
in the Test Plan, the untouched ring→spill→DLQ chain, the O(1) zero-alloc
gate, and the composite `(security_id, exchange_segment)` keys already used
by both the aggregator and the tracker).

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the
candle session window [09:15:00, 15:30:00) IST is enforced on the exchange
timestamp for both feeds by a single gate
(`multi_tf_aggregator.rs::consume_tick`) pinned by boundary-exact ratchet
tests; the day-OHLC `day_open` arms only from a ≥ 09:15:00 exchange-ts tick
via `g1_exchange_gate_accepts`. The `ticks` TABLE deliberately remains
capture-everything (09:00–15:30 persist window) — pre-market rows there are
by design and outside this guarantee. Beyond the envelope (garbage exchange
timestamps), the tick is excluded from candles + day-OHLC and remains
recoverable in the WAL/ticks record.
