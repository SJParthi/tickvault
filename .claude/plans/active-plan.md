# Implementation Plan: Damp the QuestDB-reachable probe feeding the 429 ride-out exit gate

**Status:** VERIFIED
**Date:** 2026-07-01
**Approved by:** Parthiban (operator) — watchdog-cascade audit HIGH finding, this session 2026-07-01

Crate referenced: **tickvault-app** (change lives in `crates/app/src/main.rs`) +
`tickvault-api` (`crates/api/src/state.rs` health-status damped signal).

> Guarantee matrices: carried by cross-reference to
> `.claude/rules/project/per-wave-guarantee-matrix.md` (mandatory per
> `per-item-guarantee-check.sh`). Dominant guarantee here: **strictly no worse
> than today** — a genuine sustained QuestDB outage still forces the exit after
> N ticks; the 15-min ride-out ceiling stays; token-invalid + non-reconnectable
> still force exit unchanged. No hot-path change (the probe/counter is the 5s
> cold watchdog tick). Does NOT touch `dry_run`, budget, instance lock, WAL
> spill, `MAX_DAILY_UNIVERSE_SIZE`, `SEAL_BUFFER_CAPACITY`, or the #1280 WAL
> boot-confirm code.

## Design

The pool-watchdog (`spawn_pool_watchdog_task`, `crates/app/src/main.rs` ~3479)
runs one QuestDB liveness probe per 5s tick:
`timeout(2s, wait_for_questdb_ready(&cfg, 1))` (~main.rs:3574), stores the raw
result via `health.set_questdb_reachable(...)` (~3583), and later the Halt arm
reads `health.questdb_reachable()` (~3689) into the ride-out classifiers
`is_bare_reset_class` (~3373) and `is_in_window_429_rideout_class` (~3435), both
of which HARD-REQUIRE `questdb_reachable == true`. So a SINGLE momentary QuestDB
blip (GC pause / transient HTTP hiccup / the 2s timeout firing under load)
during a live in-market 429 storm flips the flag false → `ride_out == false` →
`process::exit(2)` → 775-SID cold re-subscribe → next 429 → the self-inflicted
restart/429 loop that #1277 exists to stop.

**Fix — DAMP only the signal that feeds the EXIT decision.** Add a pure damping
function and a separate damped atomic on the health status; the raw single-probe
signal stays untouched for `/health` + `overall_status` + observability.

1. `crates/api/src/state.rs`:
   - Add `questdb_reachable_for_exit_decision: AtomicBool` (init `true` — a
     watchdog that has never probed must not pre-force an exit) + a
     `set_/get_` pair. This is the DAMPED signal, read ONLY by the exit gate.
   - `set_questdb_reachable(...)` (raw) is UNCHANGED — `/health`,
     `overall_status`, and `handlers/health.rs` keep reading the raw probe.
2. `crates/app/src/main.rs`:
   - Add pure fn `damp_questdb_exit_signal(consecutive_failures: u32,
     threshold: u32) -> bool` returning `true` (reachable-for-exit) unless
     `consecutive_failures >= threshold`.
   - Add `const POOL_WATCHDOG_QDB_EXIT_DAMP_THRESHOLD: u32 = 2` (N=2).
   - In the watchdog task, hold a local `qdb_consecutive_failures: u32`
     alongside `reconnect_in_place_since`. Each tick, after the raw probe:
     raw `true` → reset counter to 0; raw `false` → saturating-add 1. Compute
     `damp_questdb_exit_signal(counter, N)` and push it via the new
     `set_questdb_reachable_for_exit_decision(...)`. Still push the raw result
     via the existing `set_questdb_reachable(...)`.
   - The Halt arm reads `health.questdb_reachable_for_exit_decision()` (damped)
     instead of `health.questdb_reachable()` (raw) for the two ride-out
     classifiers ONLY.

**Chosen N = 2.** Justification: N×5s = 10s of sustained unreachability before
the exit gate treats QuestDB as down — well within the ~15s self-review budget
and far below the 15-min ride-out ceiling, so a REAL outage still exits promptly
(2 consecutive failed 5s ticks). N=1 = today's undamped bug; N=3 (15s) is the
upper acceptable bound but 2 already absorbs a single blip while exiting a real
outage in 10s. 2 is the smallest N that fixes the bug — minimal behaviour change.

## Edge Cases

- First-ever tick before any probe: damped atomic inits `true`, so no
  pre-emptive false-exit; the gate already requires other predicates too.
- Single blip mid-storm: counter goes 0→1, `damp(1,2)==true` ⇒ ride-out
  continues (the fix).
- Two consecutive blips: counter 1→2, `damp(2,2)==false` ⇒ exit forced
  (genuine sustained outage — never worse than today).
- Success mid-way: a `true` probe resets counter to 0, so the NEXT single
  failure starts fresh (a real outage must be N *consecutive*).
- Off-hours / Dhan-OFF: the `should_act` gate short-circuits before the Halt
  ride-out arm, so the damped signal is irrelevant there — unchanged.
- Counter overflow: `saturating_add(1)` — can never wrap.
- `/health` and `overall_status`: read the RAW signal, so a single blip still
  shows "unreachable" for that one probe (observability NOT degraded).

## Failure Modes

- Genuine sustained QuestDB outage: 2 consecutive failed probes flip the damped
  signal false → ride-out classifiers return false → genuine-fatal exit (today's
  behaviour, +10s). Correct — never a new wedge.
- Token-invalid / non-reconnectable code: ride-out classifiers still require
  `token_valid` + no `saw_non_reconnectable` — UNCHANGED by this diff, still
  force exit.
- 15-min ceiling: `reconnect_in_place_ceiling_exceeded` is untouched — a
  ride-out that persists past 15 min still falls back to the genuine-fatal exit.
- WS-GAP-08 persisted-cooldown / WS-GAP-09 ride-out counters: unchanged.

## Test Plan

Unit tests (in `crates/app/src/main.rs` `mod tests`, next to the ride-out tests):
- `test_damp_questdb_exit_signal_single_blip_stays_reachable` — 1 failure with
  N=2 ⇒ `true` (ride-out continues).
- `test_damp_questdb_exit_signal_n_consecutive_flips` — 2 (and >2) failures with
  N=2 ⇒ `false` (exit forced).
- `test_damp_questdb_exit_signal_success_resets` — model the counter loop: a
  `true` between failures resets, so it takes N fresh consecutive failures to
  flip (proves a mid-way success resets).
- `test_damp_questdb_exit_signal_threshold_is_two` — pin N=2.

Health-status test (in `crates/api/src/state.rs` `mod tests`):
- `test_questdb_reachable_for_exit_decision_independent_of_raw` — the raw
  `set_questdb_reachable(false)` does NOT change the damped
  `questdb_reachable_for_exit_decision()` (still `true` until explicitly set),
  proving `/health`'s raw signal and the exit signal are separate.

Run `cargo test -p tickvault-app --lib` and `cargo test -p tickvault-api --lib`,
paste `test result:` lines.

## Rollback

Single-commit, self-contained. Revert the commit to restore the undamped
single-probe read (the `health.questdb_reachable()` call at the Halt arm) — no
schema, no data, no config, no dependency change; nothing to migrate. The new
`AtomicBool` and pure fn are additive and inert once the Halt-arm read reverts.

## Observability

- The damped signal drives ONLY the exit decision; `/health`,
  `overall_status`, and `handlers/health.rs` keep the RAW single-probe signal —
  observability of QuestDB reachability is NOT degraded (a blip still shows on
  `/health`).
- The existing WS-GAP-09 `tv_ws_watchdog_reconnect_in_place_total{reason}`
  counter now correctly stays on the ride-out path through a single blip instead
  of dropping to `process::exit` — so the counter's meaning sharpens (fewer
  false genuine-fatal `tv_pool_self_halts_total` increments from blips).
- No new error code, no new Telegram event, no new metric — this is a damping of
  an EXISTING gate, not a new failure mode (WS-GAP-09 already covers the ride-out
  path; no rule-file/ErrorCode addition needed).

## Plan Items

- [x] Add damped `questdb_reachable_for_exit_decision` AtomicBool + setter/getter to health status
  - Files: crates/api/src/state.rs
  - Tests: test_questdb_reachable_for_exit_decision_independent_of_raw

- [x] Add pure `damp_questdb_exit_signal` fn + N=2 threshold const; wire the watchdog counter + damped setter; switch the Halt-arm read to the damped getter
  - Files: crates/app/src/main.rs
  - Tests: test_damp_questdb_exit_signal_single_blip_stays_reachable, test_damp_questdb_exit_signal_n_consecutive_flips, test_damp_questdb_exit_signal_success_resets, test_damp_questdb_exit_signal_threshold_is_two

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | 1 blip during in-market 429 storm | ride-out continues (no exit) |
| 2 | 2 consecutive failed probes (real outage) | exit forced (genuine-fatal) |
| 3 | success between two failures | counter resets; needs 2 fresh consecutive to flip |
| 4 | `/health` during 1 blip | raw signal shows "unreachable" (observability intact) |
