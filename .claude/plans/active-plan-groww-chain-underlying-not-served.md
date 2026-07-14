# Implementation Plan: Groww chain per-underlying not-served paging edge

**Status:** VERIFIED
**Date:** 2026-07-14
**Approved by:** coordinator relay (operator-directed follow-up, 2026-07-14)

Per-item guarantee matrix: cross-reference `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row matrices).

> **Incident context (Verified, 2026-07-14):** Groww stopped serving NIFTY's
> same-day-expiring option chain at 14:54 IST (2xx, zero strikes,
> `outcome=empty`) while BANKNIFTY + SENSEX kept working (`ok=2 empty=1`
> per minute, all afternoon). NOTHING paged: the CHAIN-02 escalation edge
> (`stage="escalation"`) arms only on FULLY-failed minutes (ok == 0), so a
> single-underlying vendor cutoff is invisible to the pager. This mirrors
> the Dhan spot leg's `sid_not_served` gap that was closed 2026-07-13
> (`SPOT_1M_REST_SID_NOT_SERVED_THRESHOLD` / `SidServedTracker` in
> `crates/app/src/spot_1m_rest_boot.rs`) — the same pattern is now applied
> to the GROWW chain leg, per-underlying.

## Design

A pure per-underlying consecutive-not-served tracker + page latch in
`crates/app/src/groww_option_chain_1m_boot.rs` (mirror of the spot leg's
`SidServedTracker`, keyed on the pinned `&'static str` underlying instead
of `SecurityId`):

- **Counted minute** for underlying U = U's fetch outcome is non-OK
  (Empty OR Error/auth-skip/parse — vendor did not serve U's chain) AND
  ≥1 OTHER underlying was OK that minute (the vendor serves the endpoint
  but not U). "OK" is FETCH-level (`GrowwChainFetchOutcome::Found`) — the
  vendor-serving question, deliberately NOT persist-gated (persist
  failures are OUR side and already feed the escalation edge via the
  spot-M1 persist gate).
- **Global-failure minute** (zero OK across all underlyings) neither
  counts nor resets any streak (HOLD). General outages belong to the
  existing `FailureEdge` escalation — which requires ok == 0 while this
  edge requires ≥1 OK, so the two pages are MUTUALLY EXCLUSIVE per
  minute (no double-fire by construction). Skipped-boundary minutes
  (nothing fetched) deliberately do not touch the tracker — equivalent
  to HOLD.
- U's own OK resets U's streak; if U was latched, ONE Info recovery.
- Rising edge: streak reaches the new constant
  `GROWW_CHAIN_1M_UNDERLYING_NOT_SERVED_THRESHOLD` (= 10, same value as
  the spot `SPOT_1M_REST_SID_NOT_SERVED_THRESHOLD`) → ONE HIGH page,
  latched; no re-page while latched.
- Emissions: per counted minute
  `tv_groww_chain1m_underlying_not_served_total{underlying}` (3 static
  label values — the pinned plain symbols); rising edge
  `error!(code = CHAIN-02, stage = "underlying_not_served",
  feed = "groww", ...)` (REUSES `Chain02FetchDegraded` — NO new
  ErrorCode) + typed HIGH Telegram
  `GrowwChain1mUnderlyingNotServed { underlying, empty_minutes }`;
  falling edge `info!` + typed Info
  `GrowwChain1mUnderlyingServedRecovered { underlying, empty_minutes }`.
- **Wiring:** `fire_one_groww_chain_minute` collects the per-underlying
  served verdicts it already iterates (Found → served; Empty / Failed /
  auth-short-circuit-skipped / no-token → not served) and feeds them,
  immediately after `record_groww_chain_minute_verdict`, into a new
  `record_groww_chain_underlying_verdicts` sink (mirror of the spot
  `record_sid_served_verdicts`). Tracker lives next to `FailureEdge` in
  `run_groww_chain_minute_loop` — per-day lifetime by construction (the
  loop exits after 15:30 IST; targets rebuilt per day; a mid-day
  supervisor respawn restarts the streak — the SAME session-scoped
  envelope as `FailureEdge`, documented). Market-hours gating is
  INHERENT: the loop fires only in-session minute closes
  (09:16:00–15:30:00 IST, trading days — `next_fire_after` +
  `is_trading_day_today` gates), so no separate gate is needed.
- Cold scheduled loop only — zero hot-path involvement.

## Plan Items

- [x] Item 1 — Constant: `GROWW_CHAIN_1M_UNDERLYING_NOT_SERVED_THRESHOLD = 10`
  in the GROWW_CHAIN_1M constants block (doc style copied from
  `SPOT_1M_REST_SID_NOT_SERVED_THRESHOLD`), pinned in the existing
  chain-constants test.
  - Files: crates/common/src/constants.rs
  - Tests: test_groww_chain_1m_constants_pinned (extended)

- [x] Item 2 — Typed events: `GrowwChain1mUnderlyingNotServed` (High) +
  `GrowwChain1mUnderlyingServedRecovered` (Info) — variant + to_message
  (10-commandments body) + topic + severity + unit tests, mirroring the
  `Spot1mSidNotServed` / `Spot1mSidServedRecovered` pair.
  - Files: crates/core/src/notification/events.rs
  - Tests: test_groww_chain_underlying_not_served_event,
    test_groww_chain_underlying_served_recovered_event

- [x] Item 3 — Pure tracker + wiring: `UnderlyingServedTracker` /
  `UnderlyingEdgeAction` pure core, verdict collection in
  `fire_one_groww_chain_minute`, the
  `record_groww_chain_underlying_verdicts` emission sink (counter +
  CHAIN-02 `stage="underlying_not_served"` error + events), tracker
  state in `run_groww_chain_minute_loop`, existing fire-fn mock tests
  updated to thread + assert the tracker.
  - Files: crates/app/src/groww_option_chain_1m_boot.rs
  - Tests: test_underlying_not_served_pages_once_at_threshold_then_stays_latched,
    test_underlying_not_served_global_failure_neither_counts_nor_resets,
    test_underlying_not_served_recovery_rearms_the_latch,
    test_underlying_not_served_two_empty_simultaneously_page_independently,
    test_underlying_not_served_error_class_counts_like_empty,
    test_fire_no_token_arm_counts_misses_and_writes_forensics (extended),
    test_fire_token_path_found_and_empty_via_mock (extended),
    test_fire_token_path_auth_reject_short_circuits_via_mock (extended)

- [x] Item 4 — Rule-file edit: dated 2026-07-14 paragraph in
  `rest-1m-pipeline-error-codes.md` §2c documenting the new
  `stage="underlying_not_served"` arm, counter, threshold, the
  no-double-fire argument, the motivating incident, and the delivery
  boundary (typed HIGH Telegram is the page; CHAIN-02 stays
  log-sink-only).
  - Files: .claude/rules/project/rest-1m-pipeline-error-codes.md
  - Tests: (docs — covered by error_code_rule_file_crossref which already
    passes for Chain02FetchDegraded; no new ErrorCode variant)

## Edge Cases

- ok=2/empty=1 for 10 consecutive minutes → exactly ONE page for that
  underlying at minute 10; minutes 11+ latched, no re-page.
- Global-failure minutes (ok == 0: no-token fire, auth reject on the
  first underlying, full vendor outage) interleaved mid-streak → neither
  count nor reset (streak survives the blip).
- Recovery (U served) after the latch → exactly ONE recovery event,
  latch cleared; a later NEW streak can page again.
- Two underlyings empty simultaneously while the third is OK → both
  count, both page independently at their own thresholds.
- Streak below threshold at day end → nothing fires; fresh day = fresh
  run = fresh tracker.
- Error-class (non-2xx / parse / budget / auth-skip) not-served minutes
  count exactly like empty (vendor not serving U either way).
- Skipped-boundary minutes (fire overrun / suspend) → tracker untouched
  (HOLD — nothing was fetched for ANY underlying).
- Persist-failure minutes: fetch-level OK still counts as served (the
  escalation edge owns persist failures via the M1 gate).

## Failure Modes

- Notifier delivery failure → the existing NotificationService
  never-drop ladder + TELEGRAM-01 loudness own it; the coded `error!`
  line has already hit the 5-sink chain either way.
- Tracker state lost on a mid-day supervisor respawn → streak restarts
  (worst case the page fires up to threshold-1 minutes later); the
  per-counted-minute counter keeps rising throughout, so the condition
  is never invisible. Same documented envelope as `FailureEdge`.
- A vendor flap oscillating served/not-served around the threshold →
  each recovery emits at most one Info and re-arms; a page needs 10 NEW
  consecutive counted minutes — bounded page rate ≈ 1 per 10+ minutes
  per underlying, worst case.
- Metrics recorder not yet installed (unit tests) → `metrics::counter!`
  no-op handles; no panic paths (no unwrap/expect in prod code).

## Test Plan

- Pure-core unit tests for every edge case above (module `tests` in
  `groww_option_chain_1m_boot.rs`; no clock, no I/O — the spot
  `SidServedTracker` test style).
- Existing fire-fn mock tests extended to thread the tracker and assert
  the HOLD (no-token / auth-reject) and counted (found+empty) semantics
  end-to-end through the real fire path.
- Event unit tests in `events.rs` (topic + severity + body wording
  pins, sibling style).
- Constants pin extended in `test_groww_chain_1m_constants_pinned`.
- Gates: `cargo fmt --check`, `cargo clippy -p tickvault-app -p
  tickvault-core -p tickvault-common -- -D warnings -W clippy::perf`,
  `cargo test -p tickvault-app -p tickvault-core -p tickvault-common`,
  `bash .claude/hooks/banned-pattern-scanner.sh`.

## Rollback

Pure additive observability on the cold scheduled loop — revert the
implementation commit to restore byte-identical behaviour. No config,
no schema, no hot-path, no persisted-data change; the counter and events
simply stop being emitted. No feature flag needed (the whole chain leg
already sits behind `[groww_option_chain_1m].enabled`, which also turns
this edge off with it).

## Observability

- Counter: `tv_groww_chain1m_underlying_not_served_total{underlying}` —
  one increment per counted vendor-not-serving minute (3 static label
  values: NIFTY / BANKNIFTY / SENSEX).
- Coded log: `error!(code = "CHAIN-02", stage = "underlying_not_served",
  feed = "groww", underlying, consecutive_minutes, minute)` on the
  rising edge (edge-latched — Rule 4); `info!` on recovery.
- Typed Telegram: HIGH `GrowwChain1mUnderlyingNotServed` (the page —
  CHAIN-02 remains log-sink-only per rest-1m-pipeline-error-codes.md §3)
  + Info `GrowwChain1mUnderlyingServedRecovered`.
- Forensics: unchanged — every not-served minute already writes its
  `rest_fetch_audit` row (`outcome=empty`/`error`) at the existing emit
  sites; this feature adds the PAGING edge on top, not a new record.
- Runbook: `rest-1m-pipeline-error-codes.md` §2c (dated 2026-07-14
  paragraph added in this PR).
