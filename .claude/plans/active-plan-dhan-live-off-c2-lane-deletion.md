# Implementation Plan: Dhan Live-WS Retirement — PR-C2 (delete the Dhan live-feed lane runtime)

**Status:** APPROVED
**Date:** 2026-07-13
**Approved by:** Parthiban (operator) — 2026-07-13 directive ("now remove this entire Dhan live websocket feed instruments subscription even entire live websocket feed itself...") + the "agreed" rulings relayed via the coordinator session (SLO publisher PARK/delete with its terraform; tick-gap detector deletion deferred to C3; WS-GAP banner edits deferred to C4). Authority chain: `.claude/rules/project/websocket-connection-scope-lock.md` "2026-07-13 Amendment" §A/§B (Phase C inventory) + `dhan-lane-error-codes.md` retirement banner.

> **Guarantee matrices:** this plan cross-references the 15-row + 7-row matrices in
> `.claude/rules/project/per-wave-guarantee-matrix.md` (mandatory per-item matrix). This is a
> DELETION PR — the per-row proof is "no new code path": no new tick-drop path (the Groww
> WAL→ring→spill→DLQ chain and the order-update WS are untouched), no new hot-path allocation
> (only cold boot/lane code deleted), DEDUP keys untouched, composite `(security_id,
> exchange_segment)` uniqueness untouched, and every surviving invariant stays pinned by its
> ratchet (adjusted with dated comments, never silently deleted). Crates touched:
> **tickvault-app**, **tickvault-api**, **tickvault-core** (+ **tickvault-common** only if a
> shared type loses its last consumer; terraform under `deploy/aws/terraform/`).

## Design

Delete the Dhan live-feed LANE RUNTIME per the §B inventory, now that PR-C1 (#1514) rewired
the order-update WS into `dhan_rest_stack` (the sole surviving `run_order_update_connection`
call site) and Phase A flipped `dhan_enabled=false` + revoked the runtime ON-half:

1. **main.rs FAST crash-recovery boot arm** — the `fast_cache.filter(is_market_hours &&
   dhan_enabled)` region through its `return run_shutdown_fast(...)`, including the legacy
   order-update spawn, the post-market spawn, and the pool spawn. `run_shutdown_fast` itself
   dies with its only caller.
2. **main.rs lane fns** — `start_dhan_lane`, `stop_dhan_lane`, `teardown_dhan_lane_tasks`,
   `run_dhan_lane_runtime*`, `DhanLaneRunHandles`, and the `if config.feeds.dhan_enabled`
   lane gate: the Dhan-OFF else-arm (REST-stack spawn + READY + boot-completed emit) becomes
   the ONLY arm — the shared prefix flows straight into it. READY still always fires; the
   boot-completed pinger stays unconditional (systemd_boot_notify_guard needles updated to
   the simplified gate where structure legitimately changed, semantics preserved).
3. **D2b runtime cold-start** — the `run_dhan_lane_runtime_supervisor` spawn + fn,
   `crates/app/src/dhan_activation.rs` (file), and `LaneState`/`next_lane_state` in
   `crates/api/src/feed_state.rs`. `FeedRuntimeState`, `dhan_config_enabled`, and the /feeds
   409-on-enable arm are KEPT (the toggle now permanently 409s a Dhan enable).
4. **Main-feed WS machinery in crates/core/src/websocket/** — `connection.rs` (main-feed),
   `connection_pool.rs`, `subscription_builder.rs`, `pool_watchdog.rs`,
   `activity_watchdog.rs` (main-feed-only consumers verified), `rate_limit_cooldown.rs` +
   its boot-wait call sites, the main.rs pool watchdog + WS-GAP-09 helpers
   (`is_bare_reset_class` / `is_in_window_429_rideout_class` / `should_reconnect_in_place` /
   `reconnect_in_place_ceiling_exceeded` / `spawn_pool_watchdog_task`). KEEP:
   `order_update_connection.rs` (live), `ws_frame_spill` consumers (WAL is process-global),
   `market_hours_gate.rs`, `tls.rs`, and any `types.rs` items used by survivors (each
   verified by grep before deletion).
5. **`spawn_post_market_tasks`** — zero callers after the lane + fast arm die; delete it and
   its lane-only residents' spawn wiring (EOD digest, orphan watchdog, the 15:31 1m
   cross-verify SPAWN — the `cross_verify_1m_boot.rs` FILE deletion is C3). The REST stack's
   family-claim comment updates to sole-claimant reality (guard kept, smaller diff).
6. **SLO publisher (PARK ruling)** — delete `spawn_slo_publisher_task` /
   `spawn_supervised_slo_publisher` + the spawn site; delete `slo_score.rs` if zero other
   consumers (dependency-evidence call). Terraform: retire the guarantee-degraded +
   guarantee-critical alarms, drop the SLO alarms from the window-gate Lambda's armed list,
   drop `tv_realtime_guarantee_*` dashboard panels + EMF allowlist rows. Dated retirement
   note added to `wave-3-d-error-codes.md` (SLO-01/02/03 retirement authorized 2026-07-13;
   ErrorCode variants deleted in C4). CloudWatch wiring guard pins updated with dated
   rationale.
7. **Re-home `tv_order_update_ws_active`** (C1 L2 residual) — the gauge writes move into the
   order-update connection loop so the existing `tv-<env>-order-update-ws-inactive` alarm
   has a live writer.
8. **C1 MEDIUM follow-up** — the Dhan-OFF liveness emit becomes non-blocking (spawned, not
   inline-awaited before `run_process_runloop`), preserving boot_completed_metric_guard +
   notify-parity semantics.
9. **C1 LOW follow-up** — the double-WS comment overclaim fixed (stack = sole spawn site) +
   a ratchet pinning exactly ONE `run_order_update_connection(` call site in the workspace +
   a ratchet that no `api-feed.dhan.co` runtime connect path exists in `crates/` (the
   constant may stay in `constants.rs` as historical/audit with no runtime consumer).

## Edge Cases

- **Dhan-OFF else-arm becomes the only arm:** the branch simplification must keep the exact
  behaviors: raw-TOML tripwire (dhan_enabled=true in TOML refuses the REST stack), REST-stack
  spawn, READY notify always firing, unconditional boot-completed pinger, Groww-only /
  no-feed logging. Verified by systemd_boot_notify_guard + boot_path_notify_parity (needles
  updated where the gate structure legitimately simplified).
- **`types.rs` shared items:** order_update_connection + ws_frame_spill + tests consume some
  `websocket/types.rs` items — each candidate deletion is grepped for surviving consumers
  first; survivors stay.
- **Feature modes:** `tickvault-core` builds with and without `daily_universe_fetcher` —
  both modes compiled + tested.
- **/feeds Dhan toggle:** with `LaneState` gone the 409-on-enable arm must remain coherent
  (handler + tests) — a Dhan enable is refused; a Dhan disable of an already-off feed stays
  a no-op success.
- **Ratchet baselines:** test-count + pub-fn baselines DECREASE (deletions) — synced DOWN in
  their own commit citing the AWS-lifecycle precedent + this deletion PR.

## Failure Modes

- **Silent loss of a live invariant:** prevented by adjusting (never deleting) every failing
  guard with a dated comment, and by the full 3-crate test sweep + workspace lib sweep.
- **Orphaned observability:** the SLO alarms would go missing-data-silent if left — they are
  retired in the SAME PR (terraform + Lambda armed-list + dashboard + allowlist), per the
  operator's PARK ruling.
- **Order-update alarm blind:** `tv_order_update_ws_active` loses its last writer with the
  lane — re-homed in the same PR (item 7) so `tv-<env>-order-update-ws-inactive` keeps a
  live writer.
- **READY regression:** systemd READY / boot-completed emission is pinned by
  systemd_boot_notify_guard + boot_completed_metric_guard + notify-parity tests — updated,
  not weakened.

## Test Plan

- `cargo fmt --check`; `cargo clippy --workspace --no-deps -- -D warnings -W clippy::perf`
- `cargo test -p tickvault-app -p tickvault-api -p tickvault-core` (full; core's 2
  `_without_real_ssm` sandbox failures are pre-existing — confirmed unchanged)
- `cargo test -p tickvault-common -p tickvault-storage -p tickvault-trading --lib`
  (rule-file crossref + scope guards)
- Both feature modes for `tickvault-core`
- Hooks: banned-pattern scanner, plan-gate, plan-verify
- New ratchets: exactly-one `run_order_update_connection(` call site; no runtime
  `api-feed.dhan.co` connect path in `crates/`

## Rollback

Single-branch staged commits — `git revert` of the PR's merge commit restores the lane
byte-identically (the deleted code has no schema/data migration; QuestDB tables untouched).
Until merge, the branch can be abandoned with zero prod impact (prod already runs
`dhan_enabled=false` — the deleted code is dead at runtime since Phase A).

## Observability

- The SLO retirement is LOUD: dated note in `wave-3-d-error-codes.md`, terraform alarm
  removal in the same PR, window-gate armed-list updated (count corrected to reality).
- `tv_order_update_ws_active` re-homed keeps the order-update alarm live.
- Boot-completed metric + READY notify semantics preserved (guards updated, not weakened).
- No new error codes; DHAN-LANE-01..04 / SLO-03 emit sites die with their code (enum-variant
  deletion is C4 per the retirement banners, keeping the crossref tests green both ways —
  variants stay mentioned in rule files until C4).

## Plan Items

- [ ] Archive C1 plan; create this plan (Files: .claude/plans/*)
- [ ] Delete FAST crash-recovery boot arm + run_shutdown_fast (Files: crates/app/src/main.rs; Tests: systemd_boot_notify_guard, boot_path_notify_parity, fast_boot_token_validation_wiring_guard removal/adjust)
- [ ] Delete lane fns + gate simplification (Files: crates/app/src/main.rs; Tests: d2a/d2_stage2 guards adjusted)
- [ ] Delete D2b supervisor + dhan_activation.rs + LaneState (Files: crates/app/src/main.rs, crates/app/src/dhan_activation.rs, crates/api/src/feed_state.rs; Tests: chaos_feed_state, feed handler tests)
- [ ] Delete main-feed WS machinery (Files: crates/core/src/websocket/*; Tests: core suite both feature modes)
- [ ] Delete spawn_post_market_tasks + wire family-claim comment (Files: crates/app/src/main.rs, crates/app/src/dhan_rest_stack.rs; Tests: tick_conservation_wiring_guard adjusted)
- [ ] SLO publisher deletion + terraform retirement + rule-file note (Files: crates/app/src/main.rs, crates/core/src/instrument/slo_score.rs, deploy/aws/terraform/*, .claude/rules/project/wave-3-d-error-codes.md; Tests: cloudwatch wiring guards adjusted)
- [ ] Re-home tv_order_update_ws_active (Files: crates/core/src/websocket/order_update_connection.rs; Tests: gauge write pin)
- [ ] Non-blocking Dhan-OFF liveness emit (Files: crates/app/src/main.rs; Tests: boot_completed_metric_guard)
- [ ] New ratchets + baseline syncs (Files: crates/app/tests/*, quality baselines)
