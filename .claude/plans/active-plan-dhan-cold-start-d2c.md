# Implementation Plan: D2c — wake-renewal targets the lane-owned TokenManager (closes C4)

**Status:** IN_PROGRESS
**Date:** 2026-06-27
**Approved by:** operator ("yes go ahead" — D2b security-review C4 follow-up)

> Closes the **C4** finding deferred from D2b
> (`active-plan-dhan-cold-start-d2b.md`, security review): the WebSocket
> sleep-wake `force_renewal_if_stale` path reads the GLOBAL `OnceLock`
> TokenManager (`crates/core/src/auth/token_manager.rs` —
> `set_global_token_manager` no-ops on the 2nd set). After a runtime Dhan-lane
> STOP (drops manager-A) → re-START (lane-owned manager-B with a fresh JWT), the
> wake-renewal still reads dead manager-A → renews the wrong token while the live
> pool uses manager-B → stale token on wake after a re-start.

## Design

Thread the **lane-owned** `Arc<TokenManager>` into every `WebSocketConnection`
so the sleep-wake renewal targets the LIVE lane's manager, NOT the stale global
`OnceLock`. Implementation (mirrors the existing `with_feed_enable_flag` /
`with_wal_spill` injection patterns — no new global, no `OnceLock` mutation):

- `crates/core/src/websocket/connection.rs`:
  - new field `token_manager: Option<Arc<TokenManager>>` (default `None`).
  - new builder `with_token_manager(Arc<TokenManager>) -> Self`.
  - new private helper `wake_renewal_token_manager() -> Option<&Arc<TokenManager>>`
    returning the INJECTED lane manager if present, else falling back to
    `global_token_manager()` (preserves boot-ON behaviour for callers that do not
    inject — the fast crash-recovery arm).
  - the wake-renewal block (`~1920`) calls the helper instead of
    `global_token_manager()` directly.
- `crates/core/src/websocket/connection_pool.rs`: thread
  `Option<Arc<TokenManager>>` through `new_with_optional_wal` and apply
  `with_token_manager` to each pooled connection (same shape as `feed_enable_flag`).
- `crates/app/src/main.rs`: `create_websocket_pool` gains a
  `lane_token_manager: Option<Arc<TokenManager>>` param. In `start_dhan_lane`
  (the SLOW / canonical cold path used by every runtime cold-start) pass
  `Some(token_manager.clone())` — the lane-owned manager. The FAST crash-recovery
  arm passes `None` (it only ever runs at BOOT with a valid cache, never from a
  runtime toggle), so boot-ON behaviour is byte-identical (still uses the global).

This is COLD path (fires at most once per connect/disconnect/sleep cycle, never
per tick) — O(1), zero hot-path allocation, no `.unwrap()`/`.expect()`/`println!`.
The 2-WS lock + the `dhan_disable_allowed` gate are untouched (lifecycle only).

## Edge Cases

| # | Case | Behaviour |
|---|---|---|
| 1 | boot-ON (fast or slow arm), never toggled | **slow arm:** injects the lane manager → wake-renewal renews that live manager. **fast crash-recovery arm:** injects `None`, AND it `return run_shutdown_fast`s (`~main.rs:2074`) BEFORE the inline spine's `set_global_token_manager` (`~main.rs:4991`), so the global `OnceLock` is UNSET → `wake_renewal_token_manager()` returns `None` → wake-renewal silently NO-OPS. The fast arm has NO proactive wake-renewal; it relies on its cached boot token (benign, pre-existing — NOT a D2c regression). Corrected 2026-06-27: the earlier "fast arm → global fallback, both renew the same real manager" wording was WRONG (the global is never set on the fast arm). |
| 2 | runtime cold-start after a prior STOP (the C4 case) | the new lane builds manager-B; the pool's connections hold manager-B via injection → wake-renewal renews manager-B, never the dead global manager-A. |
| 3 | injected manager present but token still fresh | `force_renewal_if_stale` short-circuits `Ok(false)` exactly as today — no renewal, no Telegram. |
| 4 | no manager available (test binaries / no global set, no injection) | helper returns `None` → wake-renewal block skipped (identical to today's `if let Some(tm) = global…`). |
| 5 | lane STOP drops the lane manager | the WS connections (and their `Arc<TokenManager>`) are torn down with the lane; the next cold-start injects a fresh one. |

## Failure Modes

- Injected manager's `force_renewal_if_stale` returns `Err` → logged at `error!`
  with `code = AUTH-GAP-03` exactly as today; wake falls through to reconnect
  retry. No new failure path.
- Helper never panics: it is a pure `Option` selection (`as_ref().or_else(global)`),
  no unwrap.
- If a future caller forgets to inject on a runtime cold-start, the helper falls
  back to the global (degraded-but-safe — the pre-D2c behaviour), never a panic.

## Test Plan

Scoped to `crates/core` + `crates/app` (per `testing-scope.md`). New ratchet
unit tests in `connection.rs::tests`:
- `wake_renewal_token_manager_prefers_injected_over_global` (C4 — the injected
  lane manager wins even when a different global is installed).
- `wake_renewal_token_manager_falls_back_to_global_when_not_injected` (boot-ON
  fast-arm preservation).
- `with_token_manager_sets_the_field` (builder wiring).
Existing `token_manager.rs` idempotency test
(`test_set_global_token_manager_is_idempotent`) stays green (we do NOT add a 2nd
`set_global_token_manager` call). `pub-fn-test-guard` + `pub-fn-wiring-guard` for
the new `pub fn with_token_manager` (real call site in `connection_pool.rs`).
`cargo build`/`cargo test` for `tickvault-core` + `tickvault-app`; `cargo fmt`.

## Rollback

Single-feature, additive injection. Revert the branch commit(s): the
`Option<Arc<TokenManager>>` params default-thread `None`, so reverting restores
the global-only behaviour with no schema/migration impact. No QuestDB table, no
config flag, no boot-order change.

## Observability

No new metric/table needed — the wake-renewal already emits
`WebSocketTokenForceRenewedOnWake` (Telegram) on `Ok(true)` and `error!` with
`code = AUTH-GAP-03` on `Err`. After the fix those signals now describe the
CORRECT (lane-owned) manager, removing the silent "renewed the wrong token"
class. The honest-envelope note: the SLO/self-test global readers
(`main.rs` ~5945/~6289) keep reading the global as a process-level health
approximation — out of C4 scope (they do not renew, only gauge remaining secs).

## Per-Item Guarantee Matrix (15-row + 7-row)

Cross-references `.claude/rules/project/per-wave-guarantee-matrix.md`. This is a
cold control-plane correctness fix: **code coverage** (3 new unit ratchets +
existing idempotency test), **testing coverage** (unit), **code checks**
(banned-pattern + pub-fn-test + pub-fn-wiring), **performance** (cold path, no
hot-path alloc — DHAT N/A per the lifecycle-plan honest envelope), **logging**
(`error!` AUTH-GAP-03 preserved), **functionalities** (new pub fn has a call site
+ test). 7-row resilience: no new tick-drop path; SubscribeRxGuard/pool watchdog
untouched; no hot-path allocation; QuestDB self-heal untouched; O(1) cold path;
composite uniqueness untouched; real-time proof via the preserved Telegram +
AUTH-GAP-03 signals.

## Honest envelope (charter §F)

100% inside the tested envelope: a runtime Dhan-lane stop→re-start now renews the
manager the live pool actually uses (lane-owned injection), with ratcheted
regression coverage (`wake_renewal_token_manager_prefers_injected_over_global`).
The boot-ON fast crash-recovery arm is byte-identical (global fallback — and,
honestly, that "fallback" is a NO-OP on the fast arm: the global is never set
there, so the fast arm has no proactive wake-renewal and relies on its cached
boot token; pre-existing, benign).

## D2c PR follow-up (2026-06-27) — two review findings closed in this PR

**MEDIUM (gauge fix — FULL fix taken, not deferred):** the two PROCESS-level
health gauges (market-open self-test `token_headroom_secs`, SLO `token_freshness`)
previously read the global `OnceLock` directly — after a runtime stop→re-start
they showed the dead boot-time manager's remaining seconds (a misleading reading
on the exact path D2c targets). Now both read via a single helper
`gauge_token_headroom_secs(feed_runtime)` that PREFERS the live lane-owned manager
(a new `Mutex<Option<Arc<TokenManager>>>` slot on `FeedRuntimeState`, SET on
`start_dhan_lane` success, CLEARED at every lane→Off transition) and falls back to
the global only when no lane runs (boot-OFF / Groww-only). The set/clear lifecycle
is co-located with the existing `set_dhan_lane_running` calls (1 set + 3 clears),
so it is mechanical, not the "non-trivial/risky" case — the full fix was taken.
Off-hot-path (self-test once/day, SLO every 10s); zero per-tick impact. Ratchets:
`slo_token_gauge_prefers_live_lane_manager` (source-scan — both gauges go through
the helper; the global is read exactly once, in the helper's fallback) +
`gauge_token_headroom_falls_back_to_global_when_no_live_lane` (behavioural) +
3 `feed_state` slot tests.

**LOW (comment fix):** the fast-arm comments (`~main.rs:1413`, `~main.rs:2766`) and
this plan's Edge Cases #1 previously claimed the fast arm "uses the global
`OnceLock` set at boot". Corrected: the fast arm returns BEFORE
`set_global_token_manager`, so the global is UNSET there and wake-renewal no-ops
(cached token; benign, pre-existing — NOT a D2c regression).
