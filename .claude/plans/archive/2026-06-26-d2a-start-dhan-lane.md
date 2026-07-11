# Implementation Plan: D2a — extract `start_dhan_lane` from the (now-contiguous) Dhan lane gate

**Status:** APPROVED
**Date:** 2026-06-26
**Approved by:** coordinator (D2 design `active-plan-dhan-cold-start-d2.md` §1.3 / D2a row; this is the pure-extraction sub-PR)
**Branch:** `claude/d2a-start-dhan-lane-v2`
**Crate:** `crates/app`

> Builds to the D2 design (`.claude/plans/active-plan-dhan-cold-start-d2.md`,
> commit `cfd27ca6`) — D2a row of the sub-PR table. Stage 2 (the shared-infra
> hoist, #1209) merged + deployed, so the Dhan lane is a single contiguous block
> inside `if config.feeds.dhan_enabled { … }`. This sub-PR is the BEHAVIOUR-
> IDENTICAL pure extraction only — NO runtime cold-start path, NO `LaneState`
> FSM, NO config flag, NO `stop_dhan_lane` (those are D2b).

## Design

Lift the contiguous slow-arm Dhan-lane body VERBATIM out of the inline
`if config.feeds.dhan_enabled { … }` gate in `main()` into a callable
`async fn start_dhan_lane(ctx: DhanLaneContext<'_>) -> Result<DhanLaneRunHandles, StartLaneError>`.

- `DhanLaneContext<'a>` — the EXHAUSTIVE set of `main()`-scope values the lane
  body reads (produced from a real compile, no elision): `config`, `notifier`,
  `health_status`, `tick_broadcast_sender`, `order_update_sender`,
  `feed_runtime`, `feed_health`, `trading_calendar`, `ws_frame_spill` (all
  borrowed Arc/refs), the Copy clock flags `is_market_hours` / `is_trading` /
  `is_muhurat` / `is_mock_trading`, `boot_start`, and the two OWNED WAL-replay
  buffers `ws_wal_replay_live_feed` / `ws_wal_replay_order_update` (moved in, as
  the lane `std::mem::take`s them). `ctx` is taken by value so the owned buffers
  can be moved out; the borrowed fields keep `main`'s `config` alive for the
  run-loop after the lane returns (design C4: lane owns its `TokenManager`, built
  inside the auth step and returned in `DhanLaneRunHandles`; the global
  `OnceLock` set-once boot path is unchanged in D2a).
- `StartLaneError` — `BootAbortClean` (the inline gate's `return Ok(())` boot
  aborts: IP/auth/dual-lock/static-IP) and `BootAbortErr(anyhow::Error)` (the
  inline gate's `return Err(err)` / `bail!` aborts). The caller maps them back to
  `main()`'s EXACT prior returns (`return Ok(())` / `return Err(err)`), so the
  extraction is behaviour-identical.
- The gate becomes: build `DhanLaneContext`, then
  `let dhan_lane = if config.feeds.dhan_enabled { match start_dhan_lane(lane_ctx).await { Ok(h)=>Some(h), Err(BootAbortClean)=>return Ok(()), Err(BootAbortErr(e))=>return Err(e) } } else { None };`.

The body is relocated unchanged except 4 mechanical, semantics-preserving edits:
(1) 7× `return Ok(())` → `BootAbortClean`; (2) 1× `return Err(err)` + 3×
`anyhow::bail!` → `BootAbortErr`; (3) 2× `.context(..)?` → `.map_err(BootAbortErr)?`;
(4) 5× bare `&config` arg → `config` (the lane re-binds `config: &ApplicationConfig`).
Plus 3 spawn pre-extractions: `effective_main_feed_pool_size(scope, max)` is
computed BEFORE each of the 3 `'static` `tokio::spawn`s (readiness/heartbeat/SLO)
so they capture an owned Copy result rather than the lane-lifetime `&config` (the
inline gate relied on Rust-2021 disjoint Copy captures of an OWNED `config`;
the result is identical because the config is immutable).

## Edge Cases

- Dhan-OFF boot: gate `else` → `None`; lane never starts (no auth, no instrument
  fetch, no Dhan WS) — OFF-isolation preserved by construction.
- The 7 boot-abort sites: each maps to the same `main()` return as before
  (clean exit code 0, or error exit with the chain).
- WAL-replay buffers populated regardless of `dhan_enabled`; when OFF they are
  simply dropped unused (same as before Stage 2's gate `else`).
- Fast crash-recovery arm: UNTOUCHED (byte-identical; D2d unifies it).

## Failure Modes

- A boot gate refusing → `Err(StartLaneError::BootAbortClean)` → `main()` clean
  exit (unchanged). Instrument-load / BOOT-02 / BOOT-03 / pre-market-profile →
  `Err(StartLaneError::BootAbortErr)` → `main()` error exit (unchanged chain).
- WAL init failure still `std::process::exit(1)` BEFORE the lane (in `main`),
  unchanged.

## Test Plan

- `cargo build -p tickvault-app` (0 warnings), `cargo test -p tickvault-app`
  green (incl. `feed_toggle_lifecycle_guard`, `d2_stage2_hoist_guard`,
  `run_loop_split_guard`, `shutdown_sequence_guard`, `per_feed_boot_isolation_guard`).
- NEW `crates/app/tests/d2a_start_dhan_lane_guard.rs` (4 source-scan tests):
  the fn + context types exist; the lane is `start_dhan_lane`-extracted + still
  gated by `dhan_enabled`; the boot-abort → `main()`-return mapping is present;
  the 2-WS helper call sites survive.
- `per_feed_boot_isolation_guard.rs` updated: auth + instrument-load now asserted
  INSIDE `start_dhan_lane` (called only from the gate).
- Whitespace-ignoring diff of the relocated body vs the original confirms ONLY
  the 4 intended transform categories + fmt reflow (behaviour-identical proof).
- `fmt --check`, banned-pattern scanner, pub-fn-test + pub-fn-wiring guards exit 0.

## Rollback

Single-commit pure relocation — `git revert` restores the inline gate. No new
runtime path, no config flag, no FSM; nothing to feature-flag off. Boot-ON and
boot-OFF behaviour are byte-identical to pre-D2a.

## Observability

No new telemetry in D2a (pure extraction). The lane's existing counters /
tracing / Telegram / audit rows are relocated unchanged. The D2 7-layer lane-
lifecycle telemetry (`tv_dhan_lane_*`, `DHAN-LANE-0N`, `dhan_lane_audit`) lands
in D2b/D2c per the design.

## Per-Item Guarantee Matrix

Carries the 15-row + 7-row matrices by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md`. D2a-specific: code
performance / O(1) = N/A axis (cold control-plane; the hot tick path is
UNTOUCHED); zero-tick-loss WAL→ring→spill→DLQ + `SubscribeRxGuard` reused
unchanged; 2-WS Dhan lock held; uniqueness/dedup unaffected; behaviour-identical
(ratcheted by the source-scan guard + the body diff).

## Honest envelope

> "100% inside the tested envelope: D2a is a BEHAVIOUR-IDENTICAL extraction of
> the contiguous slow-arm Dhan lane into a callable `start_dhan_lane`, ratcheted
> by a source-scan guard and proven by a whitespace-ignoring body diff showing
> ONLY the boot-abort→return remap, the `&config`→`config` re-bind, and the
> spawn Copy-capture pre-extractions. NO runtime cold-start path, NO FSM, NO
> flag, NO `stop_dhan_lane` (D2b). The fast crash-recovery arm is NOT folded in
> (D2d). Boot-ON and boot-OFF are byte-identical."
