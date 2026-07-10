# Implementation Plan: In-window Dhan 429 → reconnect-in-place (stop the self-inflicted restart/429 loop)

**Status:** APPROVED
**Date:** 2026-06-30
**Approved by:** Parthiban (operator)

## Problem (the audit's HIGH finding, root cause confirmed)

In `crates/app/src/main.rs` the pool-watchdog Halt arm classifies an all-down
pool via `is_bare_reset_class(healths, token_valid, questdb_reachable)`, which
requires `rate_limit_streak == 0` on EVERY connection (main.rs ~3341-3345). So
the moment a Dhan main-feed HTTP 429 sets a connection's `rate_limit_streak`,
the subsequent `>300s all-down` Halt is classified GENUINE-FATAL →
`std::process::exit(2)` (BOOT-ON) → cold re-subscribe of the locked 1 main-feed
conn → another 429 → loop. Observed 06-30: 61 main-feed 429s in the market
window; the restart loop also 429-starves the independent Groww feed's auth.

This is the SAME self-inflicted-storm shape WS-GAP-09's bare-reset ride-out
already fixes for streak==0 RST storms — but the streak>0 (429) case, which is
the EXACT case #1265's WS-GAP-08 persisted cooldown exists to absorb in-place,
still exits. Exiting on a 429 is precisely wrong: the per-connection `run()`
loops already keep running through a 429 (they wait out the
`compute_rate_limit_floor_ms` floor and retry, preserving subs via
SubscribeRxGuard); only the watchdog's `process::exit` turns that benign
in-place wait into a cold re-subscribe that earns the next 429.

## Design

Add a sibling pure classifier and widen the Halt-arm ride-out decision in
`crates/app/src/main.rs`:

1. New pure fn `is_in_window_429_rideout_class(healths, in_market_hours,
   token_valid, questdb_reachable) -> bool`. Returns `true` ONLY when ALL hold:
   - `in_market_hours` (09:00–15:30 IST) — the same gate WS-GAP-09 already uses.
   - `token_valid` AND `questdb_reachable` — identical genuine-fatal guards as
     `is_bare_reset_class` (a dead token / dead DB still needs the restart path).
   - non-empty `healths` AND NO connection `saw_non_reconnectable` (a genuine-fatal
     Dhan disconnect code still exits).
   - at least one connection has `rate_limit_streak > 0` — i.e. this IS the 429
     class (distinguishes it from the streak==0 bare-RST class that
     `is_bare_reset_class` already handles). It does NOT require streak==0 — that
     is the whole point: it admits the 429.
2. New pure fn `should_reconnect_in_place(healths, in_market_hours, token_valid,
   questdb_reachable) -> bool` = `is_bare_reset_class(...) ||
   is_in_window_429_rideout_class(...)`. Single decision the watchdog calls, so
   the two ride-out classes share ONE ceiling + ONE skip path.
3. Watchdog Halt arm (main.rs ~3550-3625): replace the `bare_reset` local with
   `ride_out = should_reconnect_in_place(&healths, in_market_hours, token_valid,
   questdb_reachable)`. When `ride_out && !ceiling_exceeded`:
   - honor the persisted 429 cooldown: the per-connection loops ALREADY wait out
     the in-memory floor; the watchdog does NOT itself sleep/connect (it owns no
     socket). It calls `pool.reset_watchdog()` (restart the 300s AllDown window)
     so the per-connection reconnect loops keep running their 429-floored backoff
     + SubscribeRxGuard — exactly the in-place wait WS-GAP-08 intends. No exit,
     no cold re-subscribe, no new 429.
   - emit `WS-GAP-09` `error!` + `tv_ws_watchdog_reconnect_in_place_total` with a
     NEW reason label `in_window_429_ride_out` (vs the existing `bare_dhan_reset`)
     when the admit was via the 429 class, so the two classes are observably
     distinct. `continue` (skip genuine-fatal).
   - The ceiling (`reconnect_in_place_ceiling_exceeded`, 15 min) is UNCHANGED and
     covers BOTH classes: a truly-stuck 429 feed still falls back to the
     genuine-fatal `process::exit`/lane-teardown after 15 min — never WORSE than
     today (today exits at 5 min; this exits at 15 min only if zero recovery).
   - The `ceiling_exceeded` fallback `error!` is widened to fire for either class.
4. Genuine-fatal guards untouched: token dead, QuestDB down, any
   `saw_non_reconnectable` → `ride_out == false` → existing `process::exit(2)` /
   lane teardown exactly as today.

Reuse-first: `is_bare_reset_class`, `reconnect_in_place_ceiling_exceeded`,
`POOL_RECONNECT_IN_PLACE_CEILING_SECS`, the WS-GAP-09 ErrorCode, the
`tv_ws_watchdog_reconnect_in_place_total` counter, `pool.reset_watchdog()`, and
the WS-GAP-08 cooldown are ALL existing — this PR only adds the streak>0 admit
path + one new reason label. No new ErrorCode (satisfies tag-guard / cross-ref
by reusing WS-GAP-09, whose rule section we extend). No `types.rs` change
(ConnectionHealth already carries `rate_limit_streak` + `saw_non_reconnectable`).

## Edge Cases

- Mixed pool: one conn streak>0 (429), another streak==0 → `is_bare_reset_class`
  false (it requires all==0), but `is_in_window_429_rideout_class` true (>=1
  streak>0, none non-reconnectable) → ride out. Correct: a 429 on any conn is
  the 429 class.
- streak==0 RST storm (no 429): `is_bare_reset_class` true → ride out via the
  existing path; `is_in_window_429_rideout_class` false (no streak>0). Either
  way `should_reconnect_in_place` true → unchanged behaviour for the existing case.
- Out of market hours: `should_act` is false, the Halt arm's ride-out block is
  never reached (expected-idle branch). `in_market_hours` arg keeps the classifier
  honest if ever called directly / reused, and returns false there.
- Empty pool (no conns): both classifiers false → genuine-fatal (never ride out
  an absent pool).
- Ceiling: a 429 that never recovers for 15 min → `ceiling_exceeded` true →
  genuine-fatal exit. Bounded worst case = 15 min (vs 5 min today), never WORSE
  in outcome (still restarts), strictly better in the common recoverable case.

## Failure Modes

- Token expires DURING a 429 ride-out: next 5s poll re-evaluates `token_valid`
  (read fresh each tick from `token_handle`); when it flips invalid →
  `should_reconnect_in_place` false → genuine-fatal exit. No wedge.
- QuestDB dies during ride-out: same — `health.questdb_reachable()` is refreshed
  every 5s by the existing watchdog probe; flips false → genuine-fatal exit.
- A genuine non-reconnectable (807/auth) arrives mid-429: `saw_non_reconnectable`
  is sticky → both classifiers false → genuine-fatal exit. No wedge.
- Worst case (truly stuck 429): ceiling at 15 min → genuine-fatal exit, identical
  to today's restart, just later. NEVER fails to exit on a genuine fatal.

## Test Plan

Pure-function unit tests in `crates/app/src/main.rs::tests` (no I/O, deterministic),
reusing the existing `down_health(id, streak, saw_non_reconnectable)` helper:
- `test_in_window_429_rideout_class_streak_positive_true` — in-window, token+QDB
  ok, no non-reconnectable, >=1 streak>0 → true (the core fix).
- `test_in_window_429_rideout_class_out_of_hours_false` — not in market hours → false.
- `test_in_window_429_rideout_class_token_invalid_false` — token dead → false.
- `test_in_window_429_rideout_class_questdb_down_false` — QDB down → false.
- `test_in_window_429_rideout_class_non_reconnectable_false` — saw_non_reconnectable → false.
- `test_in_window_429_rideout_class_no_streak_false` — all streak==0 → false.
- `test_in_window_429_rideout_class_empty_false` — empty pool → false.
- `test_should_reconnect_in_place_admits_429_in_window` — combined regression.
- `test_should_reconnect_in_place_admits_bare_rst` — existing behaviour preserved.
- `test_should_reconnect_in_place_genuine_fatal_token_dead` — token dead → exit.
- `test_should_reconnect_in_place_genuine_fatal_questdb_down` — QDB down → exit.
- `test_should_reconnect_in_place_genuine_fatal_non_reconnectable` — non-reconnectable → exit.
- `test_should_reconnect_in_place_ceiling_still_exits` — past ceiling → genuine-fatal.

Run `cargo test -p tickvault-app`. `types.rs` is NOT changed, so tickvault-core
is unchanged.

## Rollback

Single-crate, additive logic change. Revert the PR commit: `git revert <sha>` —
restores the pre-change Halt arm (the `bare_reset`-only ride-out + genuine-fatal
exit on streak>0). No schema, no config, no migration, no data. The two new pure
fns are dead-code-free (called from the watchdog) and have no external surface.

## Observability

- `tv_ws_watchdog_reconnect_in_place_total{reason="in_window_429_ride_out"}` — NEW
  reason label on the EXISTING counter; non-zero in market hours = the system is
  absorbing a Dhan 429 storm in-place (NOT restarting).
- `WS-GAP-09` `error!` line (existing ErrorCode) with the new reason in the message
  → routes to Telegram via the 5-sink pipeline + `errors.jsonl`.
- `tv_pool_self_halts_total` now increments ONLY on a genuine-fatal restart.
- Rule-file: extend `.claude/rules/project/wave-2-error-codes.md` WS-GAP-09 section
  to document the `in_window_429_ride_out` reason (required by
  error_code_rule_file_crossref + keeps the runbook honest).

## Plan Items

- [x] Add `is_in_window_429_rideout_class` + `should_reconnect_in_place` pure fns
  - Files: crates/app/src/main.rs
  - Tests: test_in_window_429_rideout_class_streak_positive_true, test_in_window_429_rideout_class_out_of_hours_false, test_in_window_429_rideout_class_token_invalid_false, test_in_window_429_rideout_class_questdb_down_false, test_in_window_429_rideout_class_non_reconnectable_false, test_in_window_429_rideout_class_no_streak_false, test_in_window_429_rideout_class_empty_false, test_should_reconnect_in_place_admits_429_in_window, test_should_reconnect_in_place_admits_bare_rst, test_should_reconnect_in_place_genuine_fatal_token_dead, test_should_reconnect_in_place_genuine_fatal_questdb_down, test_should_reconnect_in_place_genuine_fatal_non_reconnectable, test_should_reconnect_in_place_ceiling_still_exits

- [x] Wire `should_reconnect_in_place` into the watchdog Halt arm + new reason label
  - Files: crates/app/src/main.rs
  - Tests: post_market_pool_halt_guard (existing ratchet must stay green)

- [x] Extend WS-GAP-09 rule section with the `in_window_429_ride_out` reason
  - Files: .claude/rules/project/wave-2-error-codes.md
  - Tests: error_code_rule_file_crossref (existing, must stay green)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | In-market Dhan 429 storm (streak>0), token+QDB ok | Ride out in place, no exit, no 429 cold re-subscribe; Groww unaffected |
| 2 | In-market bare RST storm (streak==0) | Ride out in place (existing behaviour preserved) |
| 3 | Token dies mid-429 | Genuine-fatal exit (no wedge) |
| 4 | QuestDB dies mid-429 | Genuine-fatal exit (no wedge) |
| 5 | Non-reconnectable (807/auth) mid-429 | Genuine-fatal exit (no wedge) |
| 6 | 429 never recovers for 15 min | Ceiling → genuine-fatal exit (never worse than today) |
| 7 | Out of market hours all-down | Expected-idle path, no page/exit (unchanged) |

## Guarantee Matrix (cross-reference)

This item carries the per-item guarantees by cross-reference — see
`.claude/rules/project/per-wave-guarantee-matrix.md` for the canonical 15-row
100% guarantee matrix and the 7-row resilience demand matrix. How this item
satisfies them:

- **100% code coverage / testing / extreme check:** 13 deterministic
  pure-function unit tests (Test Plan above) cover every branch of both new
  classifiers (admit-429, out-of-hours, token-dead, QDB-down, non-reconnectable,
  no-streak, empty, combined ride-out, all genuine-fatal cases, ceiling
  fallback). The existing `post_market_pool_halt_guard.rs` ratchet pins the Halt
  arm's `if !should_act`-gated reset + the `process::exit` market-hours gate.
- **100% logging / monitoring / alerting:** WS-GAP-09 `error!` (existing
  ErrorCode) with the new `in_window_429_ride_out` reason routes to Telegram via
  the 5-sink pipeline; `tv_ws_watchdog_reconnect_in_place_total{reason}` counter;
  `tv_pool_self_halts_total` now sharpened to genuine restarts only.
- **100% code review:** adversarial self-review (hostile re-read of the diff for
  the "genuine fatal fails to exit" worst case, hot-path alloc, guard wiring,
  ceiling intact, zero-tick-loss).
- **Zero ticks lost / WS resilience:** no new tick-drop path — the per-connection
  loops + WAL/ring/spill/DLQ + SubscribeRxGuard are untouched; the change only
  AVOIDS a self-inflicted restart, which strictly REDUCES tick loss.
- **O(1) latency:** both classifiers are O(connections) over the ≤1-conn locked
  pool, on the 5s cold-path watchdog tick — no hot-path allocation.
- **Uniqueness + dedup:** N/A — no QuestDB write, no new table, no new key.

## Honest envelope (per operator-charter §F / zero-loss-guarantee-charter §1)

100% inside the tested envelope, with ratcheted regression coverage: an
in-MARKET-HOURS Dhan 429 storm (token valid + QuestDB reachable + no
non-reconnectable code) is RIDDEN OUT in place (the per-connection loops honor
the WS-GAP-08 60s→5m cooldown floor, subscriptions preserved by
SubscribeRxGuard) instead of a `process::exit` that earns the next 429; a
truly-stuck feed still falls back to the genuine-fatal restart at the unchanged
15-min ceiling, so the worst case is NEVER worse than today's 5-min restart.
This does NOT stop Dhan's server-side 429s (an upstream/account matter); it
stops the SELF-INFLICTED restart/429 loop those 429s were triggering, and it
NEVER fails to exit on a genuine fatal (dead token, dead DB, non-reconnectable
disconnect) — each re-evaluated fresh on every 5s poll.
