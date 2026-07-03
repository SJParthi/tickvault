# Implementation Plan: B3 — Token-headroom wiring + SLO/self-test de-noise

**Status:** VERIFIED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — B3 directive, this session ("wire to the real gauge, delete the dead field; switch tick_freshness to liquid-cohort or p95-gap with ratchet tests")

> **Evidence-driven scope adjustment (research reports, verified file:line):**
> (i) The "dead" field `SystemHealthStatus.token_remaining_secs` has LIVE
> production readers — `GET /health` (`crates/api/src/handlers/health.rs:69`),
> the READY Telegram (`crates/app/src/main.rs:6665`) and the EOD digest
> (`main.rs:10868`) — so the honest fix is wiring the MISSING WRITER (the 10s
> SLO loop already computes the real value), NOT deleting the field. The
> sibling `token_valid` field also has live readers (`health.rs:71`,
> `state.rs:221 overall_status`) and zero production writers → wired in the
> same spot.
> (ii) The tick_freshness aggregation fix ALREADY MERGED as #1342 (commit
> `49eccfc`, `compute_tick_freshness = 1 − silent/universe` + 6 ratchet
> tests). The residual is doc-sync (stale binary-worst-gap docstring + rule
> row) plus the SIBLING binary worst-gap still feeding the market-open
> self-test's `recent_tick` sub-check (SELFTEST-02) — same false-alarm class.

## Plan Items

- [x] Item 1 — Wire the real token headroom (and validity) into `SystemHealthStatus` from the 10s SLO loop, un-ghosting READY / EOD / `GET /health`.
  - Files: crates/app/src/main.rs
  - Tests: test_slo_loop_writes_token_health_state (source-scan guard, crates/api/tests/token_headroom_wired_guard.rs)
  - Impl: crates/app/src/main.rs — SLO loop Token_freshness block (`slo_health.set_token_remaining_secs(token_secs)` + `slo_health.set_token_valid(token_secs > 0)` immediately after `let token_secs = gauge_token_headroom_secs(&slo_feed_runtime);`)

- [x] Item 2 — Doc-sync the tick_freshness fractional-coverage definition (code fix already merged as #1342 / 49eccfc; do NOT reimplement).
  - Files: crates/core/src/instrument/slo_score.rs, .claude/rules/project/wave-3-d-error-codes.md
  - Tests: existing 12 slo_score evaluator tests + 6 compute_tick_freshness ratchets (unchanged, re-run green)
  - Impl: `SloInputs.tick_freshness` docstring rewritten (fractional coverage, VIX excluded, off-hours pinned 1.0); wave-3-d dimensions-table row updated

- [x] Item 3 — De-noise the market-open self-test `recent_tick` sub-check: feed it the FEED-LEVEL freshest tick age (min age across the universe) instead of the worst per-SID gap, so ~33 always-silent illiquid SIDs can no longer fail SELFTEST-02 at 09:16:30 IST.
  - Files: crates/app/src/main.rs, crates/core/src/pipeline/tick_gap_detector.rs, crates/core/src/instrument/market_open_self_test.rs
  - Tests: test_freshest_tick_age_secs_single_sid_returns_its_age, test_freshest_tick_age_secs_saturates_to_zero_when_now_before_last_seen (tick_gap_detector.rs); existing test_freshest_tick_age_secs_none_when_empty + test_freshest_tick_age_secs_returns_most_recent_across_instruments; test_self_test_recent_tick_uses_feed_level_freshest_age (guard)
  - Impl: self-test wiring in main.rs now calls `freshest_tick_age_secs` (already-existing pub fn, `tick_gap_detector.rs:131`, gains an O(N)-honesty note) with `unwrap_or(u64::MAX)` fail-safe; `MarketOpenSelfTestInputs.last_tick_age_secs` docstring updated

- [x] Item 4 — Ratchet guard tests (build-failing on regression) pinning both wirings.
  - Files: crates/api/tests/token_headroom_wired_guard.rs (new; mirrors health_counter_fix7_guard.rs cross-crate source-scan style)
  - Tests: test_slo_loop_writes_token_health_state, test_self_test_recent_tick_uses_feed_level_freshest_age

## Design

The 10s SLO scheduler in `crates/app/src/main.rs` already computes the
authoritative token headroom every 10 seconds:
`let token_secs = gauge_token_headroom_secs(&slo_feed_runtime);` →
`TokenManager::seconds_until_expiry()` (live lane-owned manager preferred,
global fallback). `slo_health` is a clone of the process-wide
`SystemHealthStatus` and is already captured by that task. Item 1 adds two
O(1) atomic stores right after that line: `set_token_remaining_secs(token_secs)`
and `set_token_valid(token_secs > 0)`. Every downstream reader (READY Telegram
at 09:14 IST, EOD digest at 15:31:30 IST, `GET /health` token block,
`overall_status()`) becomes live with zero reader changes and zero message-format
changes.

Item 3 replaces the self-test's `scan_gaps_top_n(now, 1)` (worst per-SID gap,
no exclusions — fails on a single permanently-illiquid SID) with the existing
`TickGapDetector::freshest_tick_age_secs(now)` (MIN age across all tracked
SIDs = seconds since ANY subscribed SID last ticked). This matches the
documented sub-check meaning ("last tick > 60s old — silent socket likely" =
FEED-level liveness, mirroring the feed-stall watchdog's feed-level last-tick
design). `None` (no detector installed, or map empty = no tick and no seed
ever observed) maps to `u64::MAX` → the sub-check FAILS (fail-safe: an
in-market self-test with zero tick evidence must not pass). The seeded-at-boot
map means "subscribed but zero ticks ever" also fails honestly (all ages =
time since boot seed > 60s at 09:16:30). `evaluate_self_test` logic and the
`RECENT_TICK_DEGRADED_THRESHOLD_SECS = 60` threshold are UNCHANGED.

Item 2 is documentation-only: the shipped #1342 fractional-coverage measure
(`1 − silent/universe`, INDIA VIX excluded from the silent numerator,
off-hours pinned 1.0) is described where the stale binary definition remains.

Crates touched: app (`crates/app/src/main.rs`), core
(`crates/core/src/pipeline/tick_gap_detector.rs`,
`crates/core/src/instrument/slo_score.rs`,
`crates/core/src/instrument/market_open_self_test.rs`), api
(`crates/api/tests/token_headroom_wired_guard.rs`; `crates/api/src/state.rs`
is READ-only reference — no state.rs change).

## Edge Cases

- Token manager not yet ready / no live lane: `gauge_token_headroom_secs`
  returns 0 → field reads 0 → "Token headroom: 0.0h" is now an HONEST 0 (and
  `token_valid=false`), consistent with the existing Token_freshness dimension.
- Token expired mid-session: next 10s tick writes 0 → `/health` flips to
  "invalid" + `overall_status` "degraded" honestly.
- READY fires at 09:14:00 IST; the SLO loop starts at boot (08:3x) with 10s
  cadence → value is at most 10s stale at read time (honest staleness envelope).
- Self-test: single illiquid SID silent for hours while liquid SIDs tick →
  min age ≈ 0 → `recent_tick` PASSES (the de-noise goal).
- Self-test: ALL SIDs silent > 60s in-market (real dead socket) → min age >
  60 → FAILS (detection preserved).
- Self-test: detector missing or map empty → `u64::MAX` → FAILS (fail-safe;
  strictly safer than the previous `unwrap_or(0)` false-pass).
- `now < last_seen` clock edge: `saturating_duration_since` → 0 age → passes
  (no underflow panic).

## Failure Modes

- SLO loop dies → the token fields stop updating and hold the LAST-written
  value (staleness envelope: unbounded if the task dies; the task is the same
  one computing the SLO score, so its death already surfaces via missing
  `tv_realtime_guarantee_score` gauges). No new failure path is introduced —
  before this change the fields were permanently 0.
- `set_token_*` are lock-free `AtomicU64/AtomicBool` stores — cannot block,
  cannot panic, no unwrap/expect, no allocation, no secret material (only a
  seconds count crosses).
- Self-test wiring failure mode: `freshest_tick_age_secs` is O(N) over ≤1200
  entries in a once-per-day cold task — cannot affect the tick hot path
  (`record_tick` untouched).
- Regression risk (someone removes the wiring) → the two new source-scan
  guard tests fail the build.

## Test Plan

- `cargo test -p tickvault-core --lib` — tick_gap_detector (incl. 2 new unit
  tests: single-SID age; saturating zero), slo_score (12 evaluator tests
  unchanged), market_open_self_test (thresholds/logic unchanged).
- `cargo test -p tickvault-app --lib` — existing compute_tick_freshness
  ratchets (6) + main.rs in-module source-scan tests still green.
- `cargo test -p tickvault-api` — state/health tests + NEW
  token_headroom_wired_guard.rs (2 guards).
- `cargo clippy -p tickvault-app -p tickvault-core -p tickvault-api -- -D warnings -W clippy::perf`.
- `cargo fmt --all`, banned-pattern scanner, pub-fn-test guard, plan-verify.

## Rollback

Single revert of the one commit on branch `claude/token-headroom-slo`
restores: (a) the un-wired (permanently-0) token fields, (b) the worst-gap
self-test input, (c) the stale docstrings. No schema change, no config
change, no data migration — pure in-process wiring + docs, so `git revert`
is complete rollback. The guard tests live in the same commit, so the revert
does not strand failing ratchets.

## Observability

- The wired fields are THEMSELVES observability surfaces: `GET /health`
  token block, `overall_status()`, READY Telegram, EOD digest all become
  live. Message formats unchanged (Telegram 10-commandments untouched —
  plain-English text already compliant; only the value becomes real).
- Existing gauges unchanged: `tv_token_remaining_seconds` (renewal loop) and
  the SLO `token_freshness` dimension remain the metric-side truth; this
  change aligns the HTTP/Telegram side with them.
- Self-test outcome still logs `PROOF: market-open self-test fired` with
  the same fields and increments `tv_self_test_total{result}`; SELFTEST-02
  routing unchanged — only the `recent_tick` input becomes feed-level.
- Ratchets: `test_slo_loop_writes_token_health_state` and
  `test_self_test_recent_tick_uses_feed_level_freshest_age` fail the build
  if either wiring is removed.

## Per-Item Guarantee Matrix

This plan cross-references the canonical 15-row + 7-row matrices in
`.claude/rules/project/per-wave-guarantee-matrix.md`; every item below
inherits them.

| Item | Proof artifact (build-failing) |
|---|---|
| 1 — token headroom wiring | `test_slo_loop_writes_token_health_state` (crates/api/tests/token_headroom_wired_guard.rs) |
| 2 — tick_freshness doc-sync | existing `test_compute_tick_freshness_*` (6, main.rs) + 12 slo_score evaluator tests stay green |
| 3 — feed-level recent_tick | `test_self_test_recent_tick_uses_feed_level_freshest_age` + `test_freshest_tick_age_secs_*` (4, tick_gap_detector.rs) |
| 4 — ratchets themselves | both guards are source-scan tests that fail on regression |

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Healthy token (23h left) at 09:14 IST READY | READY Telegram shows real headroom (~23.0h), not 0.0h |
| 2 | Token manager dead / SLO task dead | Fields hold last-written value; honest staleness envelope — before the fix they were ALWAYS 0; task death already visible via missing SLO gauges |
| 3 | One illiquid SID silent all day, liquid SIDs ticking | `recent_tick` sub-check PASSES (min age ≈ 0s) |
| 4 | ALL SIDs silent > 60s in-market (dead socket) | `recent_tick` FAILS → SELFTEST-02 (detection preserved) |
| 5 | Empty detector map in-market (no tick/seed ever) | `u64::MAX` age → `recent_tick` FAILS (fail-safe, no false-OK) |
| 6 | Token genuinely expired mid-session | `/health` token = "invalid", headroom 0s, `overall_status` = "degraded" — honest |
