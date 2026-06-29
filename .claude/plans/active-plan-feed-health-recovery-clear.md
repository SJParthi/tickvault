# Implementation Plan: Clear the auth-rejected health flag on feed recovery

**Status:** APPROVED
**Date:** 2026-06-29
**Approved by:** Parthiban (operator) — grounded directive, this session

## Design

The shared `FeedHealthRegistry` (`crates/common/src/feed_health.rs`) carries a
per-feed `auth_rejected: [AtomicBool; Feed::COUNT]` flag. The verdict engine
hard-prioritises `auth_rejected → Down("auth rejected — refresh the Groww SSM
api-key")` (it wins over disconnected / no-ticks reasons, after `Disabled` /
not-started / not-instrumented).

The flag is SET true on any alert-class Groww sidecar line
(`crates/app/src/groww_sidecar_supervisor.rs::spawn_pipe_drain` ~L511, edge-
triggered via the `alerted` latch) and on a confirmed `run_groww_auth_smoke_check`
`Rejected` outcome (`crates/app/src/groww_activation.rs` ~L279). The ONLY site
that CLEARS it to false is `groww_activation.rs` ~L271 — and that fires only on
the START edge (a new activation cycle / feed enable).

**The bug (Medium, confirmed):** once an alert-class sidecar line trips the flag,
there is NO path that clears it back to false on **same-session recovery**. The
sidecar can recover and stream ticks fine, yet `/feeds` shows a permanent
false-RED "auth rejected — refresh the SSM api-key" Down, because the verdict
engine reads the stale flag.

**The fix (minimal, edge-triggered, feed-agnostic):** clear `auth_rejected` on a
genuine RECOVERY signal — the next SUCCESSFUL tick flow. `record_ticks(feed, n,
ts)` with `n > 0` is unambiguous proof the feed recovered (rows actually flushed
to QuestDB — the Groww bridge's `live_writer.flush()` return value, NOT in-memory
appends). I clear the flag there, guarded edge-triggered: only transition
true→false (one `compare_exchange`), so there is NO per-tick store when already
clear (the steady state). I apply the SAME clear in `record_tick` (the single-tick
path) for symmetry.

This lives in `feed_health.rs` (the common crate, the flag's home) so it is
feed-agnostic and symmetric: it clears whichever feed's flag on that feed's own
recovery. The Dhan path never SETS `auth_rejected(true)` (grep confirms the only
two `set_auth_rejected(…, true)` callers are both Groww), so there is no
parallel Dhan gap — but the fix protects Dhan for free if a future Dhan path ever
sets it.

The SET path (alert-class → true) is UNCHANGED. Only the missing
clear-on-recovery is added. The activation-cycle clear at `groww_activation.rs`
~L271 stays (it covers the enable edge; this adds the same-session recovery edge).

**Why `record_ticks`/`record_tick` and not `set_connected`:** "connected" can be
true while the feed still delivers nothing (the verdict already treats
connected-but-silent as Down). Only ROWS FLOWING (`n > 0`) is unambiguous proof
the auth-reject condition is over — it can never prematurely clear a real
persistent auth failure, because a feed that is genuinely auth-rejected by the
provider produces ZERO ticks.

## Edge Cases

- **Steady-state hot path (already-clear):** `record_ticks`/`record_tick` load
  `auth_rejected` and only `compare_exchange` true→false. When the flag is
  already false (the overwhelming common case), the CAS is not even attempted
  (the `load` short-circuits) — O(1), no store, no churn.
- **`n == 0`:** existing early-return in `record_ticks` means a 0-row flush never
  reaches the clear — correct (no false "recovered" signal; audit Rule 11).
- **Persistent auth failure:** a genuinely auth-rejected feed produces 0 ticks,
  so `record_ticks(n>0)` never fires and the flag stays Down — no premature
  clear / no hiding a real failure.
- **Flapping:** if the provider rejects again after recovery, the supervisor's
  `alerted` latch is per-child; on the next child the SET path fires again and
  re-Downs the feed. The clear is edge-triggered (true→false once), the set is
  edge-triggered (false→true once per child) — no per-tick or per-line storms.
- **Concurrent set + clear:** `AtomicBool` `compare_exchange` (Relaxed) — last
  writer wins atomically; the registry is advisory health, no torn state.

## Failure Modes

- A clear racing a fresh set (child re-rejects in the same instant a stray
  buffered tick flushes): both are atomic single-flag ops. Worst case the verdict
  flips by one refresh cycle (the page re-polls); self-corrects on the next
  signal. No data-correctness impact — this is advisory health only.
- The clear is NOT on the tick-CAPTURE path (WAL/ring/spill) — it is on the
  health registry's `record_ticks`, already called once per flush batch. No new
  tick-drop path, no new allocation, no hot-loop store.

## Test Plan

`crates/common/src/feed_health.rs` unit tests (run `cargo test -p tickvault-common
feed_health`):
- `test_registry_record_ticks_clears_auth_rejected_on_recovery` — set true, then
  `record_ticks(n>0)` clears it to false → verdict resolves to Ok.
- `test_registry_record_ticks_zero_does_not_clear_auth_rejected` — `record_ticks(0)`
  does NOT clear (stays Down) — no false-recovery.
- `test_registry_record_tick_clears_auth_rejected_on_recovery` — the single-tick
  path also clears.
- Existing `test_registry_set_auth_rejected_round_trip_and_clear` still passes
  (explicit `set_auth_rejected(false)` clear unchanged).

## Rollback

Pure additive change to two methods in one file + tests. Revert the single commit
(`git revert <sha>`) restores the exact prior behaviour — the flag goes back to
"only cleared on the activation edge". No schema, no config, no migration, no
data touched. No feature flag needed (the change has no off-state to gate — it is
a bug fix that makes the existing flag self-heal).

## Observability

No new counter/log/Telegram needed: the clear's EFFECT is already observable —
`/feeds` (`crates/api/src/handlers/feeds_page.rs`) re-reads the live
`auth_rejected` via `snapshot()` each refresh and the verdict recomputes to
Ok/live the instant ticks flow again. The `record_ticks` call site already bumps
`ticks_total` (the visible "ticks" count) on the same flush, so the operator sees
the count climb AND the RED clear in the same refresh. The existing
`GrowwSidecarRejected` Telegram event (the SET edge) is untouched.

## Per-Item Guarantee Matrix

See `.claude/rules/project/per-wave-guarantee-matrix.md` — all 15 rows of the
100% guarantee matrix and all 7 rows of the resilience demand matrix apply to
this single item. Specifics:

- 100% code coverage: 3 new unit tests cover the new true→false clear branch on
  both `record_ticks` and `record_tick`, plus the `n==0` no-clear branch.
- 100% testing coverage: unit (the registry round-trip tests).
- 100% code performance / O(1) latency: the clear is a `load` + at-most-one
  `compare_exchange` (Relaxed) — O(1), zero-alloc, NO store in the already-clear
  steady state (Zero ticks lost row: no new tick-drop path; Never
  slow/locked/hanged row: no hot-path allocation, load-then-conditional-CAS).
- Uniqueness + dedup: per-feed slot indexed by `Feed::index()` — unchanged.
- Real-time proof: `/feeds` snapshot recomputes the verdict each refresh from the
  live flag.

### Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the
auth-rejected flag now self-heals on the next successful tick flow
(`record_ticks(n>0)` / `record_tick`), proven by `feed_health` unit tests; the
clear is edge-triggered true→false (one CAS, no per-tick store when already
clear); a genuinely auth-rejected feed produces 0 ticks so the clear can never
prematurely hide a real persistent failure. Beyond the envelope (a flap where the
provider re-rejects mid-recovery), the SET edge re-Downs the feed on the next
child and the page self-corrects on the next refresh.
