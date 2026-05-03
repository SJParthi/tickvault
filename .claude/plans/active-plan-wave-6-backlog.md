# Wave-6 Backlog — Resilience-Envelope Hardening

**Status:** BACKLOG (not approved for implementation)
**Date:** 2026-05-03
**Source:** Audit findings #2 + #3 in `docs/operator/aws-readiness-audit-2026-05-03.md`
**Why backlog vs done in Wave-5/Non-AWS:** these items require either >4h chaos test wall-clock simulation (W6-2) or careful boot-time wiring (W6-1); the operator opted to ship the §8 honest reword + Wave-6 backlog now, then prioritize implementation later.

## W6-1 — Pin `TICK_BUFFER_CAPACITY` value at the type system

**Status:** PROPOSED. Effort: ~2h. Priority: MEDIUM.

### Problem

`TICK_BUFFER_CAPACITY = 600_000` is a single `pub const` in
`crates/common/src/constants.rs:1425`. The constant is referenced by
`zero_tick_loss_alert_guard.rs` (which asserts `>= 100_000` lower bound)
and by `tick_resilience.rs` (which uses it directly in test loops).

What is NOT pinned today: the EXACT value. A future PR could lower it
to `200_000` and the lower-bound guard at `>= 100_000` would still
pass — silently shrinking the burst-absorption envelope.

### Acceptance criteria

1. Add `crates/storage/tests/tick_buffer_capacity_pinned_guard.rs` with
   `assert_eq!(TICK_BUFFER_CAPACITY, 600_000)` so any change requires
   editing the ratchet test.
2. Add an overflow test that produces exactly `TICK_BUFFER_CAPACITY + 1`
   ticks under a paused QuestDB and asserts the +1 tick lands in the
   spill file (currently asserted in `chaos_rescue_ring_overflow.rs`
   but not by exact-boundary count).
3. Update `wave-4-shared-preamble.md` §8 to cite the new exact-value
   ratchet test alongside the existing `zero_tick_loss_alert_guard.rs`.

## W6-2 — Long-weekend / holiday dormant sleep chaos test (>65h)

**Status:** PROPOSED. Effort: ~4h. Priority: MEDIUM.

### Problem

`crates/core/tests/ws_sleep_resilience.rs:93,173` simulates a 65-hour
Fri 16:00 IST → Mon 09:00 IST weekend dormant sleep. This covers the
COMMON case but not the long-weekend case described in
`disaster-recovery.md` Scenario 15:

> Wed close → Tue 09:00 IST across Republic Day-class holiday, ~92h sleep

Today this scenario is reasoned about only in docs. There is no chaos
test that proves:
- `secs_until_next_market_open` returns the correct 92h value
- `force_renewal_if_stale` fires on wake even when token has been
  renewed multiple times during the long sleep
- `WebSocketSleepResumed` event fires correctly after a 92h gap
- All audit tables (`ws_reconnect_audit`, `boot_audit`,
  `selftest_audit`) show consistent pre/post-holiday entries

### Acceptance criteria

1. New test: `crates/core/tests/chaos_long_weekend_dormant.rs`
2. Simulates Wed 15:30 IST close + Thu (Republic Day) holiday +
   Fri/Sat/Sun + Mon (Bridging holiday) + Tue 09:00 IST wake
3. Mocks the wall-clock via `tokio::time::pause`/`advance` (no real
   92h wait — tests must run in <30s)
4. Asserts the same recovery primitives that `ws_sleep_resilience.rs`
   asserts for 65h, plus:
   - Token renewal fired ≥3 times during dormant window (one per 23h)
   - `force_renewal_if_stale` fired exactly once on wake
   - All audit tables have consistent entries
5. Once shipped, update `wave-4-shared-preamble.md` §8 to drop the
   "Outstanding (Wave-6)" qualifier and cite this new test.

## W6-3 — Per-step boot timeouts with named alerts

**Status:** PROPOSED. Effort: ~3h. Priority: MEDIUM.

### Problem

Audit Finding #7 (2026-05-03) caught that 12 of 15 boot steps in
`crates/app/src/main.rs` have NO explicit per-step timeout — only the
global `BOOT_TIMEOUT_SECS = 120s` umbrella. When the umbrella fires,
the operator sees "boot took 121s" but does not know which step blew.

### What's already in place (post Item 6 of non-aws-readiness PR)

- Step 6 (Dhan auth): `TOKEN_INIT_TIMEOUT_SECS = 90s` (was 300s)
- Step 7 (QuestDB DDL): `BOOT_DEADLINE_SECS = 60s`
- Both pinned by `crates/common/tests/boot_timeout_consistency_guard.rs`
  to be `<= BOOT_TIMEOUT_SECS`.

### What W6-3 adds

1. Wrap each of the remaining 12 boot steps (1, 2, 3, 4, 5, 8, 9, 10,
   11, 12, 13, 14) in `tokio::time::timeout` with a per-step named
   timeout constant.
2. New error event `NotificationEvent::BootStepTimeout { step_name:
   String, elapsed_secs: u64, timeout_secs: u64 }` — Severity::Critical
   so the operator pages with a NAMED step.
3. New constants in `crates/common/src/constants.rs`:
   - `BOOT_STEP_CONFIG_LOAD_TIMEOUT_SECS`
   - `BOOT_STEP_OBSERVABILITY_TIMEOUT_SECS`
   - `BOOT_STEP_LOGGING_TIMEOUT_SECS`
   - `BOOT_STEP_NOTIFICATION_INIT_TIMEOUT_SECS`
   - `BOOT_STEP_DOCKER_HEALTH_TIMEOUT_SECS`
   - `BOOT_STEP_IP_VERIFY_TIMEOUT_SECS`
   - `BOOT_STEP_UNIVERSE_BUILD_TIMEOUT_SECS`
   - `BOOT_STEP_WS_POOL_TIMEOUT_SECS`
   - `BOOT_STEP_TICK_PROCESSOR_TIMEOUT_SECS`
   - `BOOT_STEP_HISTORICAL_SPAWN_TIMEOUT_SECS`
   - `BOOT_STEP_ORDER_WS_TIMEOUT_SECS`
   - `BOOT_STEP_API_SERVER_TIMEOUT_SECS`
   - `BOOT_STEP_TOKEN_RENEWAL_SPAWN_TIMEOUT_SECS`
4. Each new constant gets a row in `boot_timeout_consistency_guard.rs`
   asserting `<= BOOT_TIMEOUT_SECS`.

### Acceptance criteria

1. All 12 remaining boot steps wrapped in `tokio::time::timeout`.
2. On timeout, event fires with the actual step name (not just "boot").
3. New `BootStepTimeout` event added to `events.rs` + 5 ratchet tests.
4. Existing observability ratchets (operator-health dashboard, alerts.yml)
   updated to surface the new counter `tv_boot_step_timeouts_total{step}`.
5. Live boot test on a Sunday cold start verifies no false positives.

## W6-4 — End-to-end DHAT for `run_tick_processor` per-tick path

**Status:** PROPOSED. Effort: ~6h. Priority: MEDIUM.

### Problem

`crates/core/src/pipeline/tick_processor.rs::run_tick_processor` is the
single public entry point of the tick-processing pipeline. It is a
~5000-LoC async function that takes 13 parameters and runs an indefinite
`while let Some(...) = frame_receiver.recv().await` loop inside which
the per-tick logic lives inline.

The 11 existing DHAT tests cover the per-packet parsers, the dispatcher,
the dedup ring, and the persistence writer — but NOT the per-tick logic
inside `run_tick_processor` end-to-end. Audit Finding #11 (2026-05-03)
caught 5 unmarked cold-path allocations (now fixed) but the underlying
gap remains: there is no DHAT ratchet that pins the per-tick inner loop
allocations.

### What Item 10 of non-aws-readiness PR shipped instead

A meta-guard test
(`crates/core/tests/tick_processor_alloc_meta_guard.rs`) that scans the
production code and asserts every alloc-pattern line carries an
inline `O(1) EXEMPT` / `TEST-EXEMPT` / `APPROVED` / `cold path`
exemption. Catches NEW unmarked allocations on the next PR. Not as
strong as a real DHAT but does not require a refactor.

### What W6-4 adds

1. Refactor `run_tick_processor`: extract the per-tick inner block as
   a sync function `fn process_one_tick(...)` taking the same
   pipeline-state references (registry, writers, broadcast, etc.) by
   `&mut`. Async I/O stays at the outer `run_tick_processor` level.
2. New test `crates/core/tests/dhat_tick_processor_e2e.rs` calls
   `process_one_tick` 10,000 times with a synthetic Quote+OI+Full
   sequence and asserts `dhat::HeapStats::get().total_blocks <= 4`.
3. Add Criterion bench `crates/core/benches/process_one_tick.rs` and
   pin a budget in `quality/benchmark-budgets.toml` (target ≤10μs).

### Acceptance criteria

1. `process_one_tick` extracted as a sync function.
2. DHAT test passes with ≤4 alloc blocks across 10K calls.
3. Criterion bench gated at 5% regression by `scripts/bench-gate.sh`.
4. The existing meta-guard (Item 10 of this PR) stays in place as
   defence-in-depth.

## Tracking

When either item lands, archive the corresponding line from §8 of
`wave-4-shared-preamble.md` and `per-wave-guarantee-matrix.md` Section 7,
then mark this backlog item DONE here and update the backlog status.

## Operator decision log

| Date | Decision | Rationale |
|---|---|---|
| 2026-05-03 | Defer W6-1 + W6-2 to Wave-6 | Operator wants non-AWS readiness fixes shipped this week; Wave-6 follows after AWS provisioning |
