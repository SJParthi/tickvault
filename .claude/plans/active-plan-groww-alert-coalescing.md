# Implementation Plan: Groww fleet-scoped alert coalescing + honest reject wording

**Status:** APPROVED
**Date:** 2026-07-06
**Approved by:** Parthiban (operator) — exam-fix directive 2026-07-06

> Crates touched: `crates/app` (tickvault-app) — `crates/app/src/groww_sidecar_supervisor.rs`,
> `crates/app/src/groww_bridge.rs`, new `crates/app/src/groww_fleet_alerts.rs`,
> `crates/app/src/lib.rs`, `crates/app/tests/groww_live_pipeline_e2e.rs` — and
> `crates/core` (tickvault-core) — `crates/core/src/notification/events.rs`
> (the `GrowwSidecarRejected` variant gains a `fleet_summary: bool` field so
> the Telegram body is fleet-aware; hardening round 2026-07-06).

## Design

During the 2026-07-06 fleet exam the operator received DOZENS of alternating
per-connection Telegram messages: each of the ~86 Groww fleet connections runs
its own sidecar supervisor (`run_groww_sidecar_supervisor` → `supervise_child`
→ `spawn_pipe_drain`, per-child `alerted` latch) and independently fires a
`[HIGH] GrowwSidecarRejected` per retry cycle, while each shard bridge
(`run_groww_bridge`) independently fires the `[LOW] FeedConnectedAwaitingTicks`
ping per connected episode. The core `TelegramCoalescer` cannot absorb this:
it bypasses `Severity::High` and `DispatchPolicy::Immediate` events by design.

Two fixes, both in `crates/app`:

1. **Fleet-scoped coalescing** — new module `groww_fleet_alerts.rs` with a
   pure decision fn `decide_fleet_alert(state, conn_id, kind, now_secs)` over
   a per-direction (`Reject` / `Connected`) 60s window
   (`FLEET_ALERT_WINDOW_SECS = 60`) plus a persistent per-connection reject
   set. Fleet connections (`conn_id == Some(n)`) aggregate into at most ONE
   Telegram per window per direction, the first event of a window emitting a
   count summary ("7 of 40 connections retrying (server session limit or
   throttle) — no reject reported from the other 33 connections" via
   `format_fleet_reject_summary` + the new
   `SidecarLineClass::summary_label()`). `conn_id == None`
   (single-connection, non-fleet path) is `Passthrough` — today's semantics
   byte-identical. A process-wide `OnceLock<Mutex<FleetAlertState>>` shares
   the window across all supervisors + bridges; the fleet reconciler
   (`spawn_groww_scale_fleet`) publishes the live fleet size via
   `set_global_fleet_size` (which ALSO prunes reject entries for
   scaled-down conn ids — ids are contiguous `0..n` by construction, so a
   GROWW-SCALE-01 rollback can never leave phantom dead connections
   inflating later summaries). The supervisor's streaming-recovery edge
   clears the reject set via `global_record_fleet_recovery` WITHOUT
   consuming the Connected window (the bridge's genuine connected ping owns
   it); the bridge's Connected (socket-connected, awaiting-first-tick) event
   deliberately does NOT clear the reject set — connected ≠ streaming.
   Suppressed events keep their `error!` logs, feed-health state, and
   `ws_event_audit` rows — only the Telegram fan-out coalesces — and a
   `Suppress` decision RE-ARMS the per-child one-shot latch so later 60s
   windows carry the accumulated count (bounded at ≤1 Telegram/window and
   ≤ fleet-size messages per outage episode). `conn_id` threads through
   `supervise_child` / `spawn_pipe_drain` (from `opts.conn_id`) and gains a
   new final parameter on `run_groww_bridge` (fleet loop passes its conn id;
   the single-conn wrapper path passes `None`).

   **Hardening round (hostile review, 2026-07-06, same directive):** the
   five CRITICAL/HIGH findings — false "others streaming normally" suffix,
   Suppress consuming the per-child latch, no reject-set pruning on
   scale-down, the single-conn total-outage trailer wrapping fleet
   summaries, and the Connected edge clearing the reject set — are fixed as
   described above plus a fleet-aware Telegram body:
   `NotificationEvent::GrowwSidecarRejected` (crates/core) gains a
   `fleet_summary: bool` field; when `true` the body says the AFFECTED
   connections keep retrying and only their prices are stalled, never
   "receiving nothing / Groww prices will not flow".

2. **Honest reject wording** — `SidecarLineClass::EntitlementRejected::alert_reason()`
   no longer asserts "account lacks a live market-data feed entitlement" (an
   unproven account claim; the exam's real trigger was server-side session
   starvation). New prose: "server is not sending data to this connection
   (session limit or throttle) — retrying with backoff". Classification logic,
   error codes, and `error!` code fields are untouched.

## Edge Cases

- Single connection (non-fleet, `conn_id == None`): every decision is
  Passthrough; no lock is taken, no state mutates — behavior unchanged.
- N connections rejecting inside one 60s window: exactly one summary; the
  rest suppress but stay tracked, so the NEXT window's summary carries the
  accumulated count (edge-triggered, no timer task — the first-event count
  can understate for that window; documented honest envelope).
- Window rollover at exactly 60s emits a fresh summary.
- Recovery edge: ONLY the supervisor's streaming/auth-OK recovery
  (`record_fleet_recovery`) clears the connection from the reject set; the
  bridge's Connected (awaiting-first-tick) event never does. The Connected
  direction has its own independent window, so a recovery ping right after a
  reject summary still reaches the operator.
- Whole fleet down (`affected >= fleet_size`): the summary drops the
  remainder suffix entirely (no false-OK); the partial-fleet suffix is
  negative-evidence-only ("no reject reported from the other N
  connections"), never a positive health claim.
- Fleet size not yet published (reconciler lag): denominator is clamped to
  `max(fleet_size, affected)` — never "3 of 2" / "1 of 0".
- Backwards wall-clock step: `saturating_sub` → Suppress (window widens,
  never double-fires). Clock read failure degrades to 0 secs (suppression
  only — fail-quiet, never a storm).
- Scale-down / ladder rollback: `set_fleet_size(n)` prunes reject entries
  with `conn_id >= n` (ids are contiguous `0..n`), so killed connections —
  exactly the ones a rollback removes because they rejected — can never
  render a later "N of N connections retrying" false whole-fleet-down claim.
- Sustained fleet outage (children alive, retrying, never streaming): the
  Suppress-rearm keeps unemitted children eligible, so each later window's
  first unlatched child emits the accumulated count — bounded at ≤1
  Telegram per 60s window and ≤ fleet-size messages per outage episode.
- Poisoned mutex (panicked holder): `PoisonError::into_inner` recovery so
  fleet alerting can never be permanently disabled by one panic.

## Failure Modes

- Coalescer state lost on process restart: acceptable — first post-restart
  event per direction emits one summary (at-most-one-per-window still holds).
- A suppressed Telegram during a genuine incident: the per-line `error!`
  (5-sink chain → CloudWatch), feed-health Down state, and `ws_event_audit`
  rows still fire per connection; the operator's eyes-on-now channel gets the
  windowed summary. Suppressions are counted by the new
  `tv_groww_fleet_alerts_suppressed_total{direction}` counter so silence is
  never invisible (audit Rule 11).
- The Connected-direction gate suppresses only the Telegram — the bridge's
  boot-connect audit row, CONNECT log, and `set_subscribed` counts are
  emitted before the gate and remain per-connection.
- No new error paths are introduced; no `error!` levels or `code =` fields
  change, so `error_level_meta_guard` / `error_code_tag_guard` are unaffected.

## Test Plan

Unit tests in `crates/app/src/groww_fleet_alerts.rs`:
- `test_single_conn_is_passthrough_and_stateless` (1 conn = passthrough)
- `test_n_conns_same_window_coalesce_to_one_summary` (N conns, same window = 1 summary)
- `test_window_rollover_emits_fresh_summary_with_accumulated_count` (rollover)
- `test_recovery_edge_clears_reject_set_and_connected_window_is_independent` (recovery edge)
- `test_backwards_clock_never_double_fires`
- `test_format_fleet_reject_summary_wording` (pins the negative-evidence
  suffix + bans any "streaming" health claim)
- `test_set_fleet_size_prunes_scaled_down_conn_ids` (rollback prune)
- `test_fleet_size_never_smaller_than_affected`
- `test_global_wrapper_end_to_end` (the one test that drives the process-wide state)

Unit tests in `crates/app/src/groww_sidecar_supervisor.rs`:
- `test_alert_reason_wording_is_honest_and_summary_labels_exist` (pins the new
  prose, bans the "account lacks" claim from returning, covers `summary_label`)
- `test_fleet_suppress_rearms_latch_and_summary_is_fleet_marked`
  (source-scan: Suppress re-arms the per-child latch; the fleet summary is
  `fleet_summary: true`, passthrough `false`)

Unit tests in `crates/core/src/notification/events.rs`:
- `test_groww_sidecar_rejected_fleet_summary_body_is_fleet_aware` (a
  fleet-summary body never carries the single-conn total-outage trailer; the
  single-conn arm keeps it)
- existing `test_groww_sidecar_rejected_*` tests updated for the new field

Scoped run: `cargo test -p tickvault-app -p tickvault-core --lib --tests`
(block-scoped per `testing-scope.md`; QuestDB-dependent e2e tests remain
`#[ignore]`-gated as before). `cargo fmt --check` + banned-pattern scanner
before push.

## Rollback

Single revert of this branch's commits restores the prior behavior exactly:
per-connection passthrough alerts + the old entitlement prose + the old
single-shape `GrowwSidecarRejected` event. No schema, no config, no
persisted-state change — the coalescer state is in-memory only, so rollback
has zero migration surface. The new `run_groww_bridge` / `supervise_child`
parameters and the `fleet_summary` event field disappear with the same
revert.

## Observability

- New counter `tv_groww_fleet_alerts_suppressed_total{direction="reject"|"connected"}`
  (static labels) — every coalesced-away Telegram is counted, so the summary
  cadence vs. underlying event rate is measurable and silence is never a
  false-OK.
- The fleet reconciler publishes fleet size each pass (`set_global_fleet_size`)
  so summaries carry an honest "N of M" denominator alongside the existing
  `tv_groww_conns_active` gauge.
- Per-connection `error!` logs (with existing `code =` fields), feed-health
  state, and `ws_event_audit` rows are deliberately NOT coalesced — full
  forensic granularity is preserved; only the operator Telegram fan-out is
  windowed (one summary per 60s per direction).

## Per-Item Guarantee Matrix

Cross-reference: `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row
+ 7-row matrices apply as written; this change adds no hot-path code, no new
tick-drop path, no DEDUP/schema change, no new pub fn without test + call
site, and every new failure surface is counted by
`tv_groww_fleet_alerts_suppressed_total`).
