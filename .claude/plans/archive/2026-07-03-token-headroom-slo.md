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

- [x] Item 1 — Wire the real token headroom (and validity) into `SystemHealthStatus`, un-ghosting READY / EOD / `GET /health`. **Round-2 revision:** the writer is a DEDICATED, UNCONDITIONAL 10s lane task (`spawn_token_health_writer`), NOT the feature-gated SLO loop, and its JoinHandle is registered in `DhanLaneRunHandles` so a runtime Dhan disable aborts it + resets the block to 0/false.
  - Files: crates/app/src/main.rs
  - Tests: test_token_health_writer_is_dedicated_and_unconditional, test_lane_teardown_resets_token_health_block (source-scan guards, crates/api/tests/token_headroom_wired_guard.rs)
  - Impl: crates/app/src/main.rs — `spawn_token_health_writer` (10s `tokio::time::interval`, `TOKEN_HEALTH_WRITER_INTERVAL_SECS = 10`, two atomic stores from `gauge_token_headroom_secs`), spawned in `start_dhan_lane` Step 12′ (post-auth, outside every feature gate); `token_health_handle` + `health` fields on `DhanLaneRunHandles`; abort + 0/false reset in `teardown_dhan_lane_tasks` step 0 and in the H8 `Drop` floor

- [x] Item 2 — Doc-sync the tick_freshness fractional-coverage definition (code fix already merged as #1342 / 49eccfc; do NOT reimplement).
  - Files: crates/core/src/instrument/slo_score.rs, .claude/rules/project/wave-3-d-error-codes.md
  - Tests: existing 12 slo_score evaluator tests + 6 compute_tick_freshness ratchets (unchanged, re-run green)
  - Impl: `SloInputs.tick_freshness` docstring rewritten (fractional coverage, VIX excluded, off-hours pinned 1.0); wave-3-d dimensions-table row updated

- [x] Item 3 — De-noise the market-open self-test `recent_tick` sub-check: feed it the FEED-LEVEL freshest tick age instead of the worst per-SID gap, so ~33 always-silent illiquid SIDs can no longer fail SELFTEST-02 at 09:16:30 IST. **Round-2 revision:** the freshest age is REAL-tick-only (a dedicated `AtomicU64` written only by `record_tick`, never by NT-15 boot seeds) and O(1) (single relaxed load instead of the map min-scan).
  - Files: crates/app/src/main.rs, crates/core/src/pipeline/tick_gap_detector.rs, crates/core/src/instrument/market_open_self_test.rs
  - Tests: test_freshest_tick_age_secs_single_sid_returns_its_age, test_freshest_tick_age_secs_saturates_to_zero_when_now_before_last_seen, test_freshest_tick_age_secs_ignores_boot_seeds, test_freshest_tick_age_secs_seed_after_real_tick_does_not_refresh_it, test_freshest_tick_age_secs_out_of_order_older_tick_does_not_regress, test_freshest_tick_age_secs_cleared_by_reset_daily (tick_gap_detector.rs); existing test_freshest_tick_age_secs_none_when_empty + test_freshest_tick_age_secs_returns_most_recent_across_instruments; test_self_test_recent_tick_uses_feed_level_freshest_age (guard)
  - Impl: self-test wiring in main.rs calls `freshest_tick_age_secs` with `unwrap_or(u64::MAX)` fail-safe; detector gains `freshest_anchor: OnceLock<Instant>` + `last_real_tick_offset_plus_one_secs: AtomicU64` (`record_tick` does one relaxed `fetch_max`; `seed_subscribed` untouched; `reset_daily` clears it); `MarketOpenSelfTestInputs.last_tick_age_secs` docstring updated

- [x] Item 4 — Ratchet guard tests (build-failing on regression) pinning both wirings. **Round-2 revision:** assertions are block-scoped (anchored bounded slices) so unrelated occurrences elsewhere in main.rs cannot satisfy/trip them (LOW-1); the negative worst-gap ratchet rejects `scan_gaps_top_n(<anything>, 1)` with any variable name; a teardown-reset guard is added.
  - Files: crates/api/tests/token_headroom_wired_guard.rs (mirrors health_counter_fix7_guard.rs cross-crate source-scan style)
  - Tests: test_token_health_writer_is_dedicated_and_unconditional, test_lane_teardown_resets_token_health_block, test_self_test_recent_tick_uses_feed_level_freshest_age

## Design

**(Round-2 revision.)** The health-state token block is written by a
DEDICATED lane task, `spawn_token_health_writer(health, feed_runtime)`: a 10s
`tokio::time::interval` loop that computes
`gauge_token_headroom_secs(&feed_runtime)` (live lane-owned
`TokenManager::seconds_until_expiry()` preferred, global boot manager
fallback) and does two O(1) atomic stores — `set_token_remaining_secs(secs)`
+ `set_token_valid(secs > 0)`. It is spawned in `start_dhan_lane` post-auth,
UNCONDITIONALLY (not behind `realtime_guarantee_score` or any feature flag —
the round-1 placement inside the feature-gated SLO loop was the review HIGH),
and its `JoinHandle` is registered in `DhanLaneRunHandles` so
`teardown_dhan_lane_tasks` aborts it on a runtime Dhan disable and resets the
block to 0/false (review MEDIUM-1 — otherwise the orphan writer falls back to
the global boot manager after `clear_live_token_manager` and reports
`token_valid=true` for up to 24h while Dhan is deliberately OFF). Deliberate
lane-off → token block reads 0/false, the same as the pre-B3 perpetual state,
so no NEW alarm class. The SLO loop keeps computing `token_secs` for its own
Token_freshness dimension only. Every downstream reader (READY Telegram at
09:14 IST, EOD digest at 15:31:30 IST, `GET /health` token block,
`overall_status()`) becomes live with zero reader changes and zero
message-format changes.

Item 3 replaces the self-test's `scan_gaps_top_n(now, 1)` (worst per-SID gap,
no exclusions — fails on a single permanently-illiquid SID) with
`TickGapDetector::freshest_tick_age_secs(now)` — seconds since ANY subscribed
SID last ticked. This matches the documented sub-check meaning ("last tick >
60s old — silent socket likely" = FEED-level liveness, mirroring the
feed-stall watchdog's feed-level last-tick design). **Round-2 revision
(review MEDIUM-2):** the freshest age is REAL-tick-only and O(1) — backed by
a dedicated `AtomicU64` (seconds-since-anchor, +1, `fetch_max`, relaxed)
written ONLY by `record_tick` (one extra relaxed RMW per tick, zero alloc),
NEVER by NT-15 `seed_subscribed`, so a lane (re)start < 60s before the
09:16:30 IST self-test cannot false-PASS `recent_tick` on boot seeds with
zero real ticks (Rule 11). ≤ 1s quantization (seconds resolution) — honest
for the 60s threshold; `reset_daily` clears the atomic alongside the map.
`None` (no detector installed, or no REAL tick ever recorded) maps to
`u64::MAX` → the sub-check FAILS (fail-safe: an in-market self-test with zero
real-tick evidence must not pass). `evaluate_self_test` logic and the
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
- READY fires at 09:14:00 IST; the writer starts with the lane (boot 08:3x)
  with 10s cadence → value is at most 10s stale at read time (honest
  staleness envelope).
- **Accepted (review LOW-2):** if the Dhan lane (re)starts less than ~12s
  before the 09:14 READY sample, the token block may still read its reset
  0s/false for one render — bounded staleness ≤ the 10s writer cadence,
  self-corrects on the next tick. Accepted; no code change.
- Runtime Dhan disable: teardown aborts the writer and resets the block to
  0/false — deliberate lane-off reads exactly like the pre-B3 perpetual
  state (no NEW alarm class; `overall_status()` "degraded" while Dhan is
  intentionally OFF matches the pre-wiring behaviour).
- Lane (re)start < 60s before the self-test with zero real ticks: `recent_tick`
  FAILS (real-tick-only atomic; NT-15 seeds cannot arm it).
- Self-test: single illiquid SID silent for hours while liquid SIDs tick →
  min age ≈ 0 → `recent_tick` PASSES (the de-noise goal).
- Self-test: ALL SIDs silent > 60s in-market (real dead socket) → min age >
  60 → FAILS (detection preserved).
- Self-test: detector missing or map empty → `u64::MAX` → FAILS (fail-safe;
  strictly safer than the previous `unwrap_or(0)` false-pass).
- `now < last_seen` clock edge: `saturating_duration_since` → 0 age → passes
  (no underflow panic).

## Failure Modes

- Token-health writer dies → the token fields stop updating and hold the
  LAST-written value (staleness envelope: unbounded if the task dies; the
  loop body is two infallible atomic stores over an infallible gauge read, so
  the only death vector is runtime teardown). No new failure path is
  introduced — before this change the fields were permanently 0.
- Runtime Dhan disable → teardown aborts the writer + resets 0/false, so the
  MEDIUM-1 orphan-writer path (stale `token_valid=true` off the global boot
  manager for ≤24h) is closed.
- `set_token_*` are lock-free `AtomicU64/AtomicBool` stores — cannot block,
  cannot panic, no unwrap/expect, no allocation, no secret material (only a
  seconds count crosses).
- Self-test wiring failure mode: `freshest_tick_age_secs` is now a single
  relaxed atomic load (O(1)); `record_tick` gains ONE relaxed `fetch_max` per
  tick (zero alloc, no lock) — hot-path cost bounded and mirrored on the
  existing per-tick map write.
- Regression risk (someone removes the wiring) → the three block-scoped
  source-scan guard tests fail the build.

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

Revert the branch's commits (`git revert` newest-first) on
`claude/token-headroom-slo` restores: (a) the un-wired (permanently-0) token
fields, (b) the worst-gap self-test input, (c) the stale docstrings, (d) the
round-1 SLO-loop placement (intermediate revert point) or the pre-B3 state
(full revert). No schema change, no config change, no data migration — pure
in-process wiring + docs. The guard tests live in the same commits as the
wirings they pin, so no revert strands failing ratchets.

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
- Ratchets: `test_token_health_writer_is_dedicated_and_unconditional`,
  `test_lane_teardown_resets_token_health_block` and
  `test_self_test_recent_tick_uses_feed_level_freshest_age` fail the build
  if any wiring is removed, feature-gated, or un-registered from the lane.

## Per-Item Guarantee Matrix

This plan cross-references the canonical 15-row + 7-row matrices in
`.claude/rules/project/per-wave-guarantee-matrix.md`; every item below
inherits them.

| Item | Proof artifact (build-failing) |
|---|---|
| 1 — token headroom wiring | `test_token_health_writer_is_dedicated_and_unconditional` + `test_lane_teardown_resets_token_health_block` (crates/api/tests/token_headroom_wired_guard.rs) |
| 2 — tick_freshness doc-sync | existing `test_compute_tick_freshness_*` (6, main.rs) + 12 slo_score evaluator tests stay green |
| 3 — feed-level recent_tick | `test_self_test_recent_tick_uses_feed_level_freshest_age` + `test_freshest_tick_age_secs_*` (8, tick_gap_detector.rs) |
| 4 — ratchets themselves | all three guards are block-scoped source-scan tests that fail on regression |

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Healthy token (23h left) at 09:14 IST READY | READY Telegram shows real headroom (~23.0h), not 0.0h |
| 2 | Token manager dead / SLO task dead | Fields hold last-written value; honest staleness envelope — before the fix they were ALWAYS 0; task death already visible via missing SLO gauges |
| 3 | One illiquid SID silent all day, liquid SIDs ticking | `recent_tick` sub-check PASSES (min age ≈ 0s) |
| 4 | ALL SIDs silent > 60s in-market (dead socket) | `recent_tick` FAILS → SELFTEST-02 (detection preserved) |
| 5 | Empty detector map in-market (no tick/seed ever) | `u64::MAX` age → `recent_tick` FAILS (fail-safe, no false-OK) |
| 6 | Token genuinely expired mid-session | `/health` token = "invalid", headroom 0s, `overall_status` = "degraded" — honest |
| 7 | `realtime_guarantee_score = false` in config | Token block STILL live (dedicated unconditional writer — round-2 HIGH fix) |
| 8 | Runtime Dhan disable (dry-run, operator toggle) | Writer aborted by lane teardown; token block resets 0/false (same as pre-B3 state — no new alarm class) |
| 9 | Lane (re)start < 60s before 09:16:30 self-test, zero real ticks | `recent_tick` FAILS — boot seeds cannot arm the real-tick atomic (round-2 MEDIUM-2 fix) |
| 10 | Lane starts < ~12s before the 09:14 READY sample | READY may render 0s headroom once (≤10s writer-cadence staleness) — accepted, LOW-2 |

## Adversarial Review Round 1 (2026-07-03 — 1 HIGH / 2 MEDIUM / 2 LOW)

| # | Sev | Finding | Resolution |
|---|---|---|---|
| 1 | HIGH | Token-health stores lived inside the SLO loop, which is spawned only `if config.features.realtime_guarantee_score` — flipping that unrelated flag off would silently re-ghost READY/EOD/`GET /health` and pin `overall_status()` "degraded" | **Fixed:** stores removed from the SLO loop; dedicated UNCONDITIONAL `spawn_token_health_writer` (10s interval) spawned in `start_dhan_lane` Step 12′, outside every feature gate. Guard: `test_token_health_writer_is_dedicated_and_unconditional` (incl. scoped negative on the SLO Token_freshness block) |
| 2 | MEDIUM-1 | The SLO loop is a bare `tokio::spawn` not registered in `DhanLaneRunHandles`, so a runtime Dhan disable never aborts the writer — after `clear_live_token_manager` it falls back to the global boot manager and keeps writing `token_valid=true` for ≤24h while Dhan is deliberately OFF | **Fixed:** writer JoinHandle registered as `DhanLaneRunHandles.token_health_handle` (mirrors the 2026-07-01 per-lane-leak precedent); `teardown_dhan_lane_tasks` step 0 aborts it + resets `set_token_remaining_secs(0)` / `set_token_valid(false)`; the H8 `Drop` floor mirrors the abort+reset. Deliberate lane-off reads 0/false = the pre-B3 perpetual state (no NEW alarm class). Guard: `test_lane_teardown_resets_token_health_block` |
| 3 | MEDIUM-2 | NT-15 `seed_subscribed` populates the SAME papaya map `freshest_tick_age_secs` min-scanned, so a lane (re)start < 60s before the 09:16:30 self-test would false-PASS `recent_tick` on seeds with zero real ticks (Rule 11); the "populated ONLY from record_tick" docstring was false | **Fixed (strictly better + O(1)):** dedicated `last_real_tick_offset_plus_one_secs: AtomicU64` (+ `freshest_anchor: OnceLock<Instant>`) written ONLY by `record_tick` (one relaxed `fetch_max` per tick, zero alloc); `freshest_tick_age_secs` is now a single relaxed load (was O(N) map scan); `reset_daily` clears it; docstrings corrected; 4 new detector unit tests (seeds ignored / seed-after-tick / out-of-order / reset-daily) |
| 4 | LOW-1 | Guard strings were file-global — unrelated occurrences (e.g. the pool-online freshest caller) could satisfy/trip them; the negative worst-gap ratchet only matched one exact spelling | **Fixed:** all assertions block-scoped via anchored bounded slices (`block_between`); negative ratchet `contains_worst_gap_top1_scan` rejects `scan_gaps_top_n(<anything>, 1)` with any variable name/formatting inside the self-test block |
| 5 | LOW-2 | READY may render 0s headroom if the lane started < ~12s before 09:14 (writer hasn't ticked yet) | **Accepted + documented** (Edge Cases + Scenario 10): bounded staleness ≤ the 10s writer cadence, self-corrects next tick; no code change |
