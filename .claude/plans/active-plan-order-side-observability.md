# Implementation Plan: Order-Side Observability (Cluster C)

**Status:** APPROVED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) — cluster-C directive, relayed via the coordinator session, 2026-07-14

> Guarantee matrices: See per-wave-guarantee-matrix.md — all 15 + 7 rows apply to every item below.

Touched crates: **storage**, **app**, **core**, **trading** (2-line coded-emit fix only). NOT touched: common (test-only count bump), api.

## Design
Rebuild the SEBI order_audit + pnl_audit tables in crates/storage (modern ILP-over-HTTP
template, DEDUP keys `ts, trading_date_ist, order_id, leg, event, feed` and
`ts, trading_date_ist, security_id, exchange_segment, snapshot_kind, feed`); wire the
existing-but-dead OmsAlertSink/RiskAlertSink traits to Telegram via one crates/app bridge
module (order_observability.rs) feeding a bounded mpsc(1024) consumer task that also writes
the audit rows; capture placements/cancels at the crates/app trading_pipeline call sites
(zero trading-crate hook surface); one OnEod pnl heartbeat row per trading day from the
existing market-close Notify + a counters-vs-rows daily reconcile (OMS-GAP-02 on mismatch);
CloudWatch: orders-placed derived metric + storm alarm (armed), daily-loss + fill-lag alarms
arm-on-arrival in a standalone order-side-alarms.tf, EMF allowlist 27→29 (both names dormant
— emit sites ship with cluster A / Phase-1: tv_daily_pnl, tv_order_fill_lag_seconds), alarm
#10 ok_actions strip + order-counter pre-registrations in crates/app main.rs; one operator-
dashboard widget row. Zero new ErrorCode variants (AUDIT-06 / STORAGE-GAP-03 / OMS-GAP-02
reused; RISK-GAP-01 gains its first coded emit in crates/trading risk/engine.rs).

## Edge Cases
Ensure-DDL failure → documented duplicate-row window (HTTP-CLIENT-01 class), re-ensured next
boot; non-finite price/pnl clamped to 0.0 + counted; reject/rate-limit storm → mpsc(1024)
absorbs, overflow = loud coded drop, AlertPacer bounds Telegram to 1/kind/300s with suppressed
counts; mid-day restart → process-internal reconcile only (DB count informational — no false
mismatch); weekend/holiday manual run → trading-day gate skips OnEod + reconcile; dhan-off
boot (today's prod default) → the WHOLE subsystem is DORMANT — both pipeline spawn sites are
Dhan-lane-gated, so NO consumer, NO ensure-DDL, NO OnEod heartbeat, NO reconcile run (and
nothing false-pages: nothing runs); the subsystem is code-ready + fully wired and activates
whenever the Dhan lane / live trading runs (C1 truth-sync, 2026-07-14 hostile review); same-ns
duplicate events survive via event-in-key; notifier NoOp mode → notify() self-counts, audit
rows unaffected; pre-registration must follow recorder install (first-sample-baseline).

## Failure Modes
QuestDB down at boot → staged coded error! + counters, rows discard-on-flush-failure (best-
effort forensic contract; never blocks boot or orders); QuestDB down at 15:30 → OnEod flush
fails → reconcile verdict Mismatch → OMS-GAP-02 error! (heartbeat absence is loud, never
silent); consumer task dies → every subsequent fire() = coded Closed-drop error + counter
(supervised respawn is a flagged follow-up); channel full → coded drop per event; Telegram
down → NotificationService retry + TELEGRAM-01 counters (existing) + the daily-loss CW alarm
is the redundant Critical route; reconcile /exec query fails → verdict Unverified
(db_rows=unknown — Rule 11, never false-OK); alarm #10 first-sample bug → fixed by
pre-registration; CW counter-delta shape not live-verified → fail-loud residual (house).

## Test Plan
Storage: per-module DDL/dedup-key(ts-first, feed, event|snapshot_kind, arity-6)/wire-string/
ILP-conf(!9009)/append-shape/flush-discard/mock-HTTP-ensure suites + financial-guard boundary
tests (zero quantity, non-finite price→0.0, i64::MAX quantity, zero pnl, extreme negative
realized pnl) + test_dedup_key_uses_order_id_not_security_id + meta-guard comment update.
App: map_oms_alert exhaustive; plain_risk_reason total; AlertPacer first-send/suppress/fold/
re-arm; classify_order_side_reconcile verdict matrix; bridge Full/Closed drops;
order_side_wiring_guard.rs (both set_alert_sink calls, both OrderSideWiring constructions,
RISK-GAP-01 code field in risk/engine.rs); order_side_paging_wiring_guard.rs (pre-reg order,
tf shapes from real literals, daily-loss armed + ok_actions=[], fill-lag disarmed + arming
sentence, HCL-stripper self-test). Core: wording-test updates + Dhan-badge test. Common tests:
EMF count 27→29 bump. Trading: existing halt tests cover the 2-line diff. cargo test on every
touched crate; FULL_QA before push.

## Rollback
Purely additive; no config knob needed (sinks fire only on real events). git revert of the PR:
sinks return to permanent-None (today's behavior), tables remain (SEBI — never DELETE;
orphaned tables inert), terraform apply removes filter/alarms/widget cleanly, EMF regex revert
is additive-safe (the alarm resources revert in the same commit), pre-registration removal
only re-opens the first-sample hole, alarm #10 ok_actions one-line revert restores prior
(noisier) behavior. No schema migration to unwind.

## Observability
7-layer per component: counters (tv_order_audit_persist_errors_total{stage},
tv_order_audit_rows_total{event}, tv_order_audit_rows_discarded_total, tv_pnl_audit_*,
tv_order_alert_dropped_total{reason}) · coded error! (AUDIT-06, STORAGE-GAP-03, OMS-GAP-02,
RISK-GAP-01 — all log-sink-only, delivery boundary documented in rule files) · Telegram typed
events (OrderRejected/CircuitBreaker*/RateLimitExhausted High-Medium, RiskHalt Critical, all
Dhan-badged, paced, session-gated except Critical) · audit tables (order_audit + pnl_audit,
SEBI 5y, DEDUP self-healing DDL) · CW alarms (orders-rejected fixed — NOTE the C4 rejection-
class split: the alarm's counter moves at the place-time API-error arm (C4 fix) AND the
WS-reported REJECTED transition, while the OrderRejected Telegram/audit row fires ONLY at the
place-time arm — two routes, not one signal chain; orders-placed-storm armed, daily-loss
armed-dormant, fill-lag disarmed-with-arming-contract) · dashboard widget ·
daily reconcile info!/error! line. Honest 100% claim: 100% inside the tested envelope, with
ratcheted regression coverage: best-effort forensic subsystem — bounded mpsc(1024) with loud
coded drops, discard-pending poisoned-buffer defense (duplicates cannot arise from re-appends
— per-message wall-clock-ns `ts` in both DEDUP keys and NO replay source exists for order
events; `event`-in-key is defense-in-depth, and DEDUP absorbs only an exact same-ts duplicate
key — M3 truth-sync, 2026-07-14),
first-sample pre-registrations ratcheted by source-order scans; a failure here never touches
tick capture, candles, the (paper) order path, or feed recovery. NOT claimed: fill-side rows/
lag (structurally absent in dry-run — armed-on-arrival), broker-book reconciliation (needs
live mode + a no-rest-except-live-feed §3 ruling), paging for the log-sink-only codes.

## Plan Items
- [x] order_audit persistence rebuild — Files: crates/storage/src/order_audit_persistence.rs,
  crates/storage/src/lib.rs — Tests: test_order_audit_create_ddl_contains_expected_columns,
  test_order_audit_dedup_key_ts_first_event_and_feed_in_key, test_append_order_audit_row_nonfinite_price_clamped_to_zero_boundary
- [x] pnl_audit persistence rebuild — Files: crates/storage/src/pnl_audit_persistence.rs,
  crates/storage/tests/dedup_segment_meta_guard.rs — Tests: test_pnl_audit_dedup_key_ts_first_snapshot_kind_and_feed_in_key,
  test_append_pnl_audit_row_zero_pnl_boundary
- [x] core wording + badges — Files: crates/core/src/notification/events.rs — Tests:
  test_risk_halt_notification, test_circuit_breaker_notify_on_open, test_oms_risk_events_are_dhan_badged
- [x] RISK-GAP-01 coded emit — Files: crates/trading/src/risk/engine.rs — Tests: existing halt tests + test_risk_halt_error_is_coded_risk_gap_01 (app guard)
- [x] bridge + consumer + wiring — Files: crates/app/src/order_observability.rs,
  crates/app/src/trading_pipeline.rs, crates/app/src/main.rs — Tests: test_map_oms_alert_exhaustive,
  test_alert_pacer_suppresses_within_cooldown_and_folds_count, test_classify_order_side_reconcile_verdict_matrix,
  order_side_wiring_guard.rs (all)
- [x] terraform + EMF + dashboard — Files: deploy/aws/terraform/order-side-alarms.tf,
  deploy/aws/terraform/app-alarms.tf, deploy/aws/terraform/user-data.sh.tftpl,
  deploy/aws/cloudwatch-agent.json, deploy/aws/terraform/variables.tf, deploy/aws/terraform/dashboard.tf,
  crates/common/tests/cloudwatch_app_alarms_wiring.rs — Tests: order_side_paging_wiring_guard.rs (all),
  test_emf_metric_selectors_name_count_is_twenty_nine
- [x] rules + cost note + plan hygiene — Files: .claude/rules/project/wave-2-error-codes.md,
  .claude/rules/project/gap-enforcement.md, .claude/rules/project/aws-budget.md, .claude/plans/* — Tests: error_code_rule_file_crossref (existing)
