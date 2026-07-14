# Implementation Plan: Order Runtime (dry-run) — revive order machinery, close the silent-stuck-position hole

**Status:** VERIFIED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) via coordinator directive 2026-07-14 (cluster A — order-side readiness audit)
**Crates:** app (`crates/app/src/order_runtime.rs` NEW, `crates/app/src/oms_wiring.rs` NEW, `crates/app/src/dhan_rest_stack.rs`, `crates/app/src/main.rs`, `crates/app/src/groww_bridge.rs`, `crates/app/src/trading_pipeline.rs`), trading (`crates/trading/src/oms/engine.rs`, `crates/trading/src/oms/types.rs`, `crates/trading/src/oms/mod.rs`, `crates/trading/src/risk/engine.rs`, `crates/trading/src/risk/types.rs`), common (`crates/common/src/config.rs` — `[order_runtime]` section)

> **Guarantee matrices:** this plan carries the 15-row + 7-row matrices by
> cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`
> (the canonical copy) — all 15 + 7 rows apply to every item in this plan,
> instantiated below in Design / Test Plan / Observability.
> Honest 100% claim (charter §F wording): 100% inside the tested envelope,
> with ratcheted regression coverage — fill delta computed in-engine
> (double-count-safe), conditional whole-dir WAL confirm (zero silent
> loss; deferred segments re-replay next boot), one relaxed AtomicBool
> load on the dominant empty-book Groww per-tick path (DHAT + Criterion
> evidence), single-owner task serialization (no reset races). NOT
> claimed: broker-side reconcile in dry-run (heartbeat says "broker
> reconcile SKIPPED" — the local Σfills==net_lots invariant is the real
> check); marks are best-effort (channel-full drops counted, next tick
> supersedes — positions exact, marks bounded-stale); I-P1-11 composite
> keys inside OMS/Risk (deferred with a loud first-seen-segment tripwire;
> the full `(u64,u8)` rewrite is a MANDATORY pre-live follow-up).

## Design

One supervised single-owner tokio task (`crates/app/src/order_runtime.rs`) owning OMS (dry_run hard-true) + RiskEngine,
spawned ONLY from `dhan_rest_stack.rs` Phase 5a (the dhan-OFF arm) replacing the discard drain, config-gated
`[order_runtime]` (serde default OFF, `config/base.toml` ON). Fill bridge = `OrderManagementSystem::handle_order_update`
return-widened to `Result<Option<FillEvent>, OmsError>` (delta computed in-engine from old vs new `traded_qty`) →
`RiskEngine::record_fill`. Marks = `Arc<AtomicBool>`-gated zero-alloc `try_send` tap at the groww_bridge consume seam →
`mpsc::channel::<MarkUpdate>(8192)` → `update_market_price` + next-mark paper filler + ≤1/sec mark-to-market
`evaluate_daily_loss_halt`. WAL: boot-collected order-update frames drained FIFO into the stack-local broadcast BEFORE
the WS spawn; conditional whole-dir `confirm_replayed` (confirm iff drain parse-clean AND `livefeed_frames_replayed ==
0`, else defer with ONE coalesced `warn!(code = WS-REINJECT-01, reason="confirm_deferred_stale_livefeed")`). Alert
sinks (`OmsAlertSink` + `RiskAlertSink`) wired for the first time to `NotificationService` via `NotifierAlertSink`
(5 existing NotificationEvent variants; ZERO new variants, ZERO new ErrorCodes). Reconcile scheduler every 300s
(market-hours-gated) + boot-once (+60s) + on order-update reconnect, with honest dry-run heartbeat classification +
local Σfills==net_lots invariant. Fixes shipped in the same PR: unrealized-P&L lot_size bug (risk/engine.rs:295 —
`PositionInfo` gains `lot_size`), uncoded `trigger_halt` error gains `code = RISK-GAP-01`, the dormant dhan-on
trading_pipeline consumes the widened return (4-line FillEvent graft). Once-daily gated paper self-test proves the
place → mark → fill → P&L → close chain end-to-end.

## Edge Cases

Duplicate/same-status cumulative updates (in-engine delta = new−old → 0 on re-delivery); replayed WAL frames landing on
an empty book (unknown order + traded_qty>0 → loud orphan `warn!(OMS-GAP-02)` + counter, never silent); empty `Source`
tolerated (serde-default reality) / `Source=="N"` filtered at the runtime consumer (WAL keeps capturing ALL frames —
SEBI); partial-lot remainder floored + `error!(OMS-GAP-01)`; 0/NaN/negative mark defers the paper fill
(`tv_paper_fills_deferred_total`, retried next mark — never fabricate a price); mark channel full → counted drop, next
tick supersedes (prices idempotent); >200 replayed frames vs broadcast(256) → warn + counter (envelope honesty);
sid-segment collision tripwire (first-seen segment per sid; divergent later mark/fill → `error!(RISK-GAP-02)` + skip);
16:00 IST reset racing an in-flight fill (single-task `select!` serialization — one reset block clears risk + oms +
mirror + tripwire + marks_wanted); holiday/off-hours/second-run self-test refusal (calendar + [09:20, 15:00) IST +
once-per-day latch + `is_dry_run` assertion); `[order_runtime]` absent or `enabled=false` → byte-identical current
dormant shape (discard drain + `tv_order_update_dormant_events_total` + `wal_spill: None`).

## Failure Modes

F1-F22 table (scratchpad final-design.md §c, summarized): F1 fills never reach RiskEngine → return-widened
`handle_order_update` + `record_fill` in the runtime AND the pipeline graft; F2 cumulative-qty double-count →
in-engine delta; F3 restart-mid-fill silent book reset → loud orphan warn + heartbeat book age; F4/F5 WS-before-drain /
subscribe-after-drain → ordering law pinned by source-order ratchet (subscribe → spawn runtime → drain → confirm → WS);
F6/F7 whole-dir confirm archiving un-reinjected live-feed frames / unbounded `replaying/` growth → conditional confirm
+ defer warn + runbook archive procedure; F8 dual-OMS split-brain → runtime spawned ONLY in the dhan-OFF rest_stack
(pipeline is dhan-ON only) + zero-`spawn_order_runtime(`-in-main.rs ratchet; F9 hot-path alloc at the Groww seam → 1
relaxed AtomicBool load + try_send of a 16-byte Copy struct (DHAT + Criterion); F10 mark channel full → counted,
bounded staleness stated; F11 I-P1-11 sid collision → tripwire (loud skip), composite rewrite = pre-live follow-up;
F12 paper-fill feedback loop → fill-once terminal + no signal source (§28 frozen) + PAPER-prefix assertion; F13 halt
flapping → idempotent `trigger_halt`, sink fires once per episode; F14 dry-run reconcile false-OK → "broker reconcile
SKIPPED" wording + REAL local invariant; F15 live reconcile error storm (future) → backoff + edge-latched OMS-GAP-02;
F16 reset race → single-task serialization; F17 self-test on holiday → calendar gates; F18 manual Dhan-app events →
Source=P filter; F19 sink I/O stall → `notify()` is sync fire-and-forget; F20 zero-mark fabrication → finite>0 gate;
F21 WAL replay burst > broadcast(256) → warn + counter; F22 understated daily-loss halt → lot_size fix in this PR.

F15 honesty amendment (fix-round 2026-07-14, L6): the live-reconcile backoff + edge-latched OMS-GAP-02 is NOT
implemented — the live path is unreachable today (dry_run hard-true) and the backoff/edge-latch is a MANDATORY
pre-live follow-up listed in the rule file §3. Additional fix-round failure modes closed: E1 (suspend-skipped 16:00
reset → epoch-keyed catch-up), C6 (dangling self-test position → timeout cancel + loud dangle report), S2
(unknown-segment tripwire bypass → fill refused), HP-3/E3 (tripwire error storm → per-sid edge latch), C11 (replayed
NDJSON lines as live marks → replay-window gate on the tap).

## Test Plan

Ratchet replacements (same PR): `test_rest_stack_spawns_order_update_ws_functional_dormant` → REPLACED by
`test_rest_stack_wires_order_runtime` (production-region source-order pins: subscribe < spawn_order_runtime < drain <
confirm arm < run_order_update_connection; the M8 `ou_wal_spill` binding-shape + WS-argument pins; family-claim < WS
spawn kept; ws_audit consumer kept; OrderUpdateAuthenticated kept; DISABLED branch retains the discard drain + exact
dormant-counter literal); `wal_replay_confirm_symmetry_guard.rs` extended §C — fix-round 2026-07-14 strengthened it to
CONTAINMENT (confirm_replayed exactly once, inside the Confirm arm slice — M1), never weakened;
`dhan_live_off_phase_a_guard.rs` untouched-and-green; NEW `order_runtime_spawn_site_guard.rs` — fix-round 2026-07-14
REWRITTEN on `tickvault_common::source_scan::production_region` (H1: full prod-region coverage incl. main.rs
post-test-module fns + all of groww_bridge.rs) with scanner self-tests + the M2 mark-tap call-site pin
(`ratchet_groww_bridge_mark_tap_call_site_pinned`); `test_oms_http_timeout_is_pinned_at_5s` scan path verified after
the `oms_wiring.rs` extraction.

E2E (H2 honesty, fix-round 2026-07-14): the FULL promised chain — place → mark → synth fill → net_lots≠0 → adverse
mark → daily-loss halt → sink fires RiskHalt exactly once → check_order rejects — is delivered by the in-module
`order_runtime::tests::test_e2e_place_fill_mark_halt_fires_risk_halt_sink`, which drives the ACTUAL production arm
bodies (`process_mark` / `handle_order_update_event` / `evaluate_daily_loss_halt` / production `RiskAlertSink` wiring).
It lives in-module because the arm bodies are private and the SPAWNED runtime places orders only via the time-gated
self-test (not deterministically drivable from a test). The integration file
`crates/app/tests/order_runtime_e2e.rs` covers the SPAWNED task black-box (liveness, orphan tolerance, mark drain,
disarmed gate). Unit tests + the deterministic-LCG walk `prop_fill_mirror_matches_risk_net_lots` (disclosed: not
proptest) per the item lists below. Perf evidence: `crates/app/tests/dhat_mark_forward.rs` (disarmed + armed-full arms
zero-alloc across 10K forwards; armed-accept excluded — amortized tokio block reuse, Criterion-budgeted) + Criterion
`order_gate/mark_forward` + `quality/benchmark-budgets.toml` entry at 100ns (design draft said 50ns; loosened with the
bench's recv-in-loop iteration shape and NOT yet run locally — the CI bench lane produces the first real numbers;
dated note in the rule file §5). Scope: `cargo test -p tickvault-trading -p tickvault-app`; config.rs touched
(common) → escalate workspace in CI.

## Rollback

`[order_runtime] enabled = false` (or delete the section — serde default OFF) restores the exact current dormant shape
(discard drain, `wal_spill: None`, dormant counter) — pinned by the disabled-branch ratchet. Full revert = `git revert`
(no schema changes, no new QuestDB tables, no data migration; WAL segments stay replayable either way — the conditional
confirm only ever archives what was cleanly drained).

## Observability

Gauges/counters (static labels only): `tv_order_runtime_up` (0/1), `tv_order_update_events_total{outcome=
"consumed"|"foreign_source"}`, `tv_oms_orphan_fill_updates_total`, `tv_risk_fills_recorded_total{kind="paper"|"live"}`,
`tv_mark_forward_dropped_total`, `tv_paper_fills_synthesized_total`, `tv_paper_fills_deferred_total`,
`tv_oms_reconcile_runs_total{mode="paper_noop"|"live"}`, `tv_oms_local_reconcile_divergence_total`,
`tv_paper_selftest_total{outcome="ok"|"failed"}`, `tv_wal_confirm_deferred_total`, `tv_wal_replay_burst_total`,
`tv_order_runtime_respawn_total{reason}`; existing `tv_realized_pnl`/`tv_unrealized_pnl` gauges start moving; existing
`tv_ws_frame_wal_reinjected_total{ws_type="order_update"}` starts moving on prod. Telegram: `RiskHalt` (Critical),
`CircuitBreakerOpened`/`Closed`, `OrderRejected`, `RateLimitExhausted` (all existing variants), self-test heartbeat
(Info). error! codes (ALL existing): OMS-GAP-01/02/03/04/06, RISK-GAP-01/02; warn! WS-REINJECT-01
(reason=`confirm_deferred_stale_livefeed`, non-paging). Rule files: NEW
`.claude/rules/project/order-runtime-dryrun.md` (code-mapping table + WAL confirm-defer semantics + stale-live-segment
archive runbook + I-P1-11 deferral justification) + dated sections in `ws-reinject-error-codes.md` +
`websocket-connection-scope-lock.md` (PR-C1 dormancy-honesty paragraph superseded for `order_runtime.enabled=true`).

## OUT OF SCOPE / OPERATOR GATE

1. **trading_pipeline spawn on dhan-off / any strategy or indicator activation** — operator §28 gate
   (`daily-universe-scope-expansion-2026-05-27.md` §28); the runtime never constructs IndicatorEngine /
   StrategyInstance; `crates/trading/src/strategy/*` and `crates/trading/src/indicator/*` are NOT touched (hash-pinned
   by `operator_boundary_indicator_strategy_guard.rs`).
2. **RiskEngine/OMS composite-key `(u64, u8)` rewrite** — deferred with the first-seen-segment tripwire; MANDATORY
   pre-live follow-up (a Dhan live order path requires it before `dry_run` ever flips) — flagged in the new rule file.
3. **Live mode anything**: `enable_live_mode` stays `#[cfg(test)]`; dry_run hard default untouched; no `mode=Live`
   path; the expired sandbox-gate date is noted, not acted on.
4. `order_audit`/`pnl_audit` QuestDB tables (never built) — flagged follow-up; NO new tables this PR.
5. `POST /api/order-runtime/*` command endpoints — cut (attack surface; heartbeat + metrics cover observability).
6. dh904 backoff wiring (Item 22a-wire) — untouched.
7. Per-record-type WAL confirm — impossible (mixed-type segment files), documented.
8. Dhan-lane (dhan-on) runtime wiring — the pipeline owns that path; only the 4-line FillEvent consumption lands there.
9. `[feeds.groww.scale]`, feed toggles, GDF — untouched.

## Plan Items

- [x] Item 1 — OMS fill bridge: `handle_order_update` return-widened to `Result<Option<FillEvent>, OmsError>` +
      `FillEvent` type + `parse_segment_chars` + partial-lot handling + re-exports. Fix-round 2026-07-14: DELTA-price
      math (C1), monotone qty copy (C2), terminal same-status guard (C3), floor-of-cumulatives remainder carry (C4),
      fill-price guard (C5), reconcile swallowed-delta error (C8)
  - Files: crates/trading/src/oms/engine.rs, crates/trading/src/oms/types.rs, crates/trading/src/oms/mod.rs
  - Tests: test_same_status_refresh_applies_delta_not_cumulative, test_duplicate_update_zero_delta_skipped,
    test_partial_lot_remainder_floors_and_errors, test_fill_sign_from_managed_order_transaction_type,
    test_segment_char_parse_matrix, test_two_slice_fill_delta_price_and_risk_avg_entry,
    test_regressing_and_negative_traded_qty_never_lower_baseline,
    test_terminal_order_higher_qty_redelivery_never_refills, test_fill_price_guard_rejects_zero_and_nan_avg_price,
    test_fill_lots_i32_clamp_and_lot_size_zero_normalized, test_active_order_count_matches_active_orders_len
- [x] Item 2 — Risk engine fixes: `PositionInfo.lot_size` + unrealized-P&L lot_size multiply,
      `evaluate_daily_loss_halt`, coded `trigger_halt`. Fix-round 2026-07-14: record_fill finiteness/positivity
      guards + realized-overflow skip + avg-entry repair (S1), normalized-lot realized product (C12), fail-closed
      halt on non-finite P&L, position_security_ids
  - Files: crates/trading/src/risk/engine.rs, crates/trading/src/risk/types.rs
  - Tests: test_unrealized_pnl_multiplies_lot_size, test_evaluate_daily_loss_halt_boundary,
    test_record_fill_rejects_nonfinite_and_nonpositive_price, test_record_fill_zero_lots_is_noop,
    test_record_fill_lot_size_zero_realized_uses_normalized,
    test_realized_overflow_skipped_keeps_accumulator_finite,
    test_evaluate_daily_loss_halt_fails_closed_on_nonfinite_pnl, test_halt_fires_risk_halt_once_per_episode,
    test_position_security_ids_iterates_tracked_rows
- [x] Item 3 — trading_pipeline FillEvent graft (4-line consumption of the widened return; dormant dhan-on path)
  - Files: crates/app/src/trading_pipeline.rs
  - Tests: NONE named (H2 honesty, fix-round 2026-07-14 — the earlier claim of a
    `test_pipeline_processes_order_updates` was FALSE; the graft is a 4-line exhaustive-match consumption on the
    dormant dhan-on path, compile-pinned by the widened return type; a dedicated pipeline-level test is a flagged
    follow-up alongside the dhan-on path's own revival)
- [x] Item 4 — Order runtime module: single-owner actor (OMS+Risk construction via oms_wiring, order-update consumer
      with Source=P filter, fill bridge, mark consumer + paper filler, reconcile scheduler + local invariant, daily
      reset/close sweeps, self-test, NotifierAlertSink). Fix-round 2026-07-14: epoch-keyed reset (E1), O(1) pending
      index (HP-2), latched tripwire (HP-3/E3), unknown-segment fill refusal (S2), sanitized log ids (S3), self-test
      timeout cancel + dangle report (C6), order-fold reconcile leg (C7), index-spot sid pick (E4), trading-day
      reconcile gate (E5), escalating respawn backoff (E8), f32_to_f64_clean marks (C14)
  - Files: crates/app/src/order_runtime.rs, crates/app/src/oms_wiring.rs, crates/app/src/lib.rs
  - Tests: test_apply_fill_reaches_risk_engine_net_lots_nonzero (apply_fill direct — the handle_order_update leg is
    covered by test_source_n_update_for_tracked_order_never_fills), test_orphan_fill_update_tolerated_book_unchanged
    (HONEST scope: the warn+counter emission sits on the exercised branch but the counter itself is not assertable
    without a metrics recorder), test_source_n_filtered_empty_tolerated (the PRODUCTION is_foreign_source predicate),
    test_source_n_update_for_tracked_order_never_fills, test_paper_fill_deferred_until_finite_positive_mark,
    test_terminal_order_never_refilled, test_selftest_single_cycle_latched,
    test_selftest_full_cycle_via_production_advance_latches_day, test_selftest_window_gate_boundaries (window only —
    the calendar/holiday gate is source-visible in drive_self_test_timers, not deterministically testable on an
    arbitrary run day), test_selftest_sid_pick_requires_index_spot_mark, test_selftest_timeout_cancels_outstanding_order,
    test_local_reconcile_invariant_holds_and_diverges, test_local_reconcile_order_fold_catches_lost_fill,
    test_daily_reset_clears_book_mirror_tripwire_flag_atomically (drives the production perform_daily_reset),
    test_reset_epoch_same_day_fire_and_new_day_catch_up, test_sid_segment_collision_skips_and_errors,
    test_tripwire_divergence_error_latched_per_sid, test_apply_fill_refuses_unknown_segment,
    test_pending_paper_index_tracks_placements_and_fills, test_alert_sink_event_mapping (PARTIAL — exercises all 5
    sink arms against a disabled NotificationService; the emitted event payloads are not capturable without a
    NotificationService seam), test_halt_fires_risk_halt_once_per_episode (trading crate, capturing sink),
    test_confirm_decision_matrix, prop_fill_mirror_matches_risk_net_lots (deterministic LCG walk, not proptest),
    test_log_safe_id_truncates_and_strips, test_e2e_place_fill_mark_halt_fires_risk_halt_sink (the H3 chain).
    DROPPED from the original list (H2 honesty): test_dry_run_reconcile_classified_heartbeat_not_ok — the heartbeat
    classification is an info!/error! wording branch with no capturable output; its state-side substance is covered
    by the two local_reconcile tests, and the wording is source-visible in run_reconcile_cycle
- [x] Item 5 — Config: `[order_runtime]` OrderRuntimeConfig (serde default OFF) + base.toml enabled=true + validation
  - Files: crates/common/src/config.rs, config/base.toml
  - Tests: test_order_runtime_config_defaults_off, test_order_runtime_config_validation
- [x] Item 6 — dhan_rest_stack Phase 5a rewrite: subscribe → spawn runtime → WAL drain → conditional confirm →
      WS spawn with `wal_spill: Some(..)`; disabled branch byte-identical dormant shape
  - Files: crates/app/src/dhan_rest_stack.rs, crates/app/src/main.rs
  - Tests: test_rest_stack_wires_order_runtime (replaces the dormant-shape ratchet)
- [x] Item 7 — groww_bridge mark tap: `marks_wanted` AtomicBool gate + `try_send(MarkUpdate)` (zero alloc/lock/await)
  - Files: crates/app/src/groww_bridge.rs, crates/app/src/main.rs
  - Tests: test_mark_forward_skips_send_when_marks_wanted_false, test_mark_forward_channel_full_never_blocks_or_panics
- [x] Item 8 — Ratchets + perf evidence: confirm-symmetry guard extension, spawn-site ratchet, DHAT mark-forward,
      Criterion bench + budget entry, e2e integration
  - Files: crates/app/tests/wal_replay_confirm_symmetry_guard.rs,
    crates/app/tests/order_runtime_spawn_site_guard.rs, crates/app/tests/dhat_mark_forward.rs,
    crates/app/benches/order_gate.rs, quality/benchmark-budgets.toml, crates/app/tests/order_runtime_e2e.rs
  - Tests: ratchet_order_runtime_spawned_only_from_rest_stack, order_runtime_e2e, dhat_mark_forward
- [x] Item 9 — Rule files: NEW order-runtime-dryrun.md + dated sections in ws-reinject-error-codes.md +
      websocket-connection-scope-lock.md
  - Files: .claude/rules/project/order-runtime-dryrun.md, .claude/rules/project/ws-reinject-error-codes.md,
    .claude/rules/project/websocket-connection-scope-lock.md
  - Tests: every_runbook_path_exists_on_disk (existing cross-ref suite stays green)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | `[order_runtime]` absent from config | Byte-identical dormant boot (discard drain, wal_spill None) |
| 2 | Paper order placed, Groww mark arrives | Next-mark fill → record_fill → net_lots ≠ 0 → tv_paper_fills_synthesized_total |
| 3 | Same TRADED update re-delivered | Delta = 0, no double-count |
| 4 | Replayed WAL fill frame on empty book | warn OMS-GAP-02 orphan + counter, book unchanged |
| 5 | Stale live-feed WAL segments present at boot | Confirm DEFERRED, warn WS-REINJECT-01, segments re-replay next boot |
| 6 | Daily loss breaches threshold on a mark | evaluate_daily_loss_halt → halt → RiskHalt Telegram (Critical) once |
| 7 | Source=N manual Dhan-app event | Filtered at consumer, counted foreign_source; WAL still captured it |
| 8 | Mark for a sid with a different segment than first seen | RISK-GAP-02 error + skip (tripwire) |
