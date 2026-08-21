# =============================================================================
# Order-side observability alarms (cluster C) — 2026-07-14
# =============================================================================
# THE GAP THIS CLOSES (2026-07-14 order-side audit): the order path had ONE
# alarm (app-alarms.tf #10 orders-rejected — dead for single-rejection
# sessions until this PR's main.rs pre-registration, and paging a bogus OK
# on every off-hours transition until its ok_actions strip) and ZERO
# placement-side / P&L-side / fill-side coverage. This file adds:
#   1. orders-placed-storm  — ARMED TODAY (valid in paper AND live mode; a
#      runaway strategy loop places paper orders at the same rate).
#   2. daily-loss-breach    — ARMED AND LIVE since 2026-08-10 (tv_daily_pnl
#      gained its emit site; see the block below).
#   3. order-fill-lag-high  — SHIPS DISARMED (actions_enabled = false; the
#      arming contract is in its description).
#
# COST (see aws-budget.md COST NOTE 2026-07-14): +3 alarms ≈ $0.30/mo;
# +1 derived custom-metric series (tv_orders_placed_delta_total) ≈ $0.30/mo;
# of the 2 new EMF allowlist names, tv_daily_pnl went LIVE 2026-08-10 (its
# emit site landed) so its ≈ +$0.30/mo is now real; tv_order_fill_lag_seconds
# stays DORMANT at $0.00 until Phase-1 ships its emitter.
#
# EMIT-SITE OWNERSHIP: tv_daily_pnl's emitter LANDED 2026-08-10 —
# RiskEngine::daily_loss_state publishes the gauge (the shared helper both
# the order gate and the standalone halt evaluation call, so the published
# value is by construction the value the halt decided on).
# tv_order_fill_lag_seconds still has NO Rust emit site — Phase-1 owns it.
#
# ⚠ CHANGED 2026-08-21: "its alarm arms on data arrival with zero further tf
# changes" is NO LONGER TRUE, and the arming PR must know why. The name was
# REMOVED from the EMF `metric_selectors` allowlist (both copies) on that date
# to free bytes in the EC2 user-data budget, which is a hard 16,384-byte AWS
# limit that terraform refuses a PLAN above. It was the only selector entry in
# the whole allowlist with zero emit sites anywhere in `crates/*/src`, so it
# was shipping a name nothing could ever publish while three live counters —
# depth row drops, depth persist errors, and the new ILP overflow bound — had
# no CloudWatch presence at all.
#
# Nothing observable changed: a selector entry for a metric with no emitter
# publishes nothing either way. What changed is the arming contract. The
# Phase-1 arming PR MUST now restore `tv_order_fill_lag_seconds` to BOTH
# selector copies (`deploy/aws/cloudwatch-agent.json` and
# `deploy/aws/terraform/user-data.sh.tftpl`, pinned byte-identical by
# cw_agent_selector_lockstep_guard.rs) in the same change as the emit site —
# and must re-check the user-data byte budget, since restoring it costs those
# bytes back. Arming the alarm without the selector entry produces a
# permanently-INSUFFICIENT_DATA pager: the exact dead-monitor shape this
# alarm's own dormancy note was written to avoid.
#
# The emit-site guard (cloudwatch_app_alarms_wiring.rs
# test_every_alarm_metric_has_a_rust_emit_site) deliberately does NOT scan
# this file — the dormant names would fail it; the shape guard
# crates/app/tests/order_side_paging_wiring_guard.rs covers this file
# instead.
#
# COUNTER SHAPE (house residual, seal-drop-alarm.tf verbatim): the CW
# agent's prometheus pipeline converts COUNTER samples to PER-SCRAPE
# DELTAS, so `Sum` over the window = orders placed in the window. Not
# live-verified from this sandbox; if the field ever proved CUMULATIVE,
# Sum overcounts and the storm alarm pages too eagerly (fail-loud, never a
# silent miss) and must be reworked to DIFF(Maximum).
#
# SHIPPING-LEG CAVEAT (the 2026-07-06 collect_list incident class): the
# storm alarm rides the /tickvault/<env>/metrics log group. If that
# shipping leg is degraded, this alarm is blind with it — the order-side
# sink's Telegram events (OrderRejected / RateLimitExhausted / circuit
# transitions / Critical RiskHalt) are the independent fallback route.
#
# FIRST-SAMPLE BASELINE (the feed-stall round-5 lesson): the delta
# pipeline drops each counter series' first sample as its baseline.
# tv_orders_placed_total{mode} and tv_orders_rejected_total are therefore
# PRE-REGISTERED at 0 in main.rs immediately after the metrics recorder
# installs (next to the tv_seal_writer_drain_total registration), so the
# dropped baseline is the harmless 0 and both series are dense from boot.
# Ratchet: crates/app/tests/order_side_paging_wiring_guard.rs.
#
# =============================================================================
# ⚠ DORMANT-ALARM LEDGER — 2026-08-09 (observability-blind-spot audit)
# =============================================================================
# TWO of the three alarms in this file watch metrics with ZERO production
# emit sites anywhere in crates/*/src (only #[cfg(test)] fixtures mention
# them). Verified 2026-08-09 by source scan:
#
#   | alarm                        | metric                     | emit sites |
#   |------------------------------|----------------------------|------------|
#   | tv-<env>-daily-loss-breach   | tv_daily_pnl               | 0 (prod)   |
#   | tv-<env>-order-fill-lag-high | tv_order_fill_lag_seconds  | 0 (prod)   |
#
# This is INTENTIONAL pre-wiring (arm-on-arrival, see the per-alarm blocks) —
# it is NOT a defect and these resources MUST NOT be deleted. What follows is
# the honest statement of what that costs today, so no future reader mistakes
# their console state for a health signal:
#
# ⚠ THE FALSE-GREEN: `treat_missing_data = "notBreaching"` means a metric that
#   NEVER publishes evaluates as **OK**. On the CloudWatch console, in an alarm
#   strip, and in any "are we green?" sweep, these two alarms are visually
#   INDISTINGUISHABLE from two genuinely-healthy alarms. In particular
#   **the daily-loss kill signal does not exist today**: there is no
#   tv_daily_pnl datapoint, so a real daily-loss breach would page NOTHING
#   from this route. The live protections are the risk engine's own halt
#   (RISK-GAP-01) + the Critical RiskHalt Telegram event; this alarm is the
#   REDUNDANT counter-side route that is currently absent, not the primary.
#
# WHY treat_missing_data IS NOT CHANGED HERE (deliberate, not an oversight):
#   - "breaching" is REJECTED — it would page continuously for the entire
#     dry-run period, i.e. trade a silent gap for permanent alert fatigue.
#   - "missing" is the technically-honest value (it holds the alarm in
#     INSUFFICIENT_DATA — visibly NOT green — and still fires no action,
#     because insufficient_data_actions is unset). It is NOT applied in this
#     change because `treat_missing_data="notBreaching"` on daily_loss_breach
#     is PINNED by crates/app/tests/order_side_paging_wiring_guard.rs
#     (test_daily_loss_alarm_is_armed_with_no_ok_actions_and_not_breaching),
#     so flipping it requires editing that app-crate guard in the same PR.
#     FLAGGED FOLLOW-UP for whoever owns crates/app next: flip both alarms to
#     treat_missing_data = "missing" + update that guard, and the dormancy
#     becomes self-evident in the console instead of only in this comment.
#
# The dormancy claim above is not a promise — it is RATCHETED by
# crates/storage/tests/cloudwatch_dormant_alarms_guard.rs, which fails the
# build in BOTH directions: if the DORMANT markers vanish from the two
# alarm_descriptions while the metrics still have no emitter, and (more
# importantly) the moment either metric GAINS a production emit site, so the
# arming PR is forced to revisit this ledger rather than inherit a stale one.
# =============================================================================
#
# FUTURE EMF-ALLOWLISTING: if tv_orders_placed_total is ever added to the
# EMF metric_selectors allowlist, the DERIVED filter name
# (tv_orders_placed_delta_total) keeps the identities separate (no
# double-count) — but re-point the storm alarm at the EMF-published metric
# and REMOVE the log filter in the same PR to avoid paying for both.
# =============================================================================

# ---------------------------------------------------------------------------
# 1. Derived placed-orders delta (metrics-log filter — the house pattern)
# ---------------------------------------------------------------------------
# Mode-agnostic pattern: matches BOTH mode="paper" and mode="live" series
# events; their per-scrape deltas SUM under the one derived name — correct
# for a storm alarm (a runaway loop is a runaway loop in either mode).
resource "aws_cloudwatch_log_metric_filter" "orders_placed_delta" {
  name = "tv-${var.environment}-orders-placed-delta"
  # Agent-created group, referenced by name (house precedent:
  # metrics-log-metric-filters.tf; adopted into terraform by log-retention.tf).
  log_group_name = "/tickvault/${var.environment}/metrics"
  pattern        = "{ $.tv_orders_placed_total = * }"
  metric_transformation {
    name      = "tv_orders_placed_delta_total" # DERIVED name — see header
    namespace = "Tickvault/Prod"
    value     = "$.tv_orders_placed_total"
    dimensions = {
      host = "$.host" # /metrics events DO carry host (prometheus scrape label)
    }
    # Deliberately no default_value (that knob emits datapoints for
    # NON-matching events — never wanted). The series is DENSE while the
    # app runs thanks to the main.rs pre-registrations.
  }
}

# ---------------------------------------------------------------------------
# 2. Orders-placed storm — runaway strategy pager (ARMED, paper AND live)
# ---------------------------------------------------------------------------
# NOT window-gated: an off-hours placement storm indicates a session-gate
# bug in the strategy/OMS chain — gating this alarm would blind exactly
# that failure mode.
resource "aws_cloudwatch_metric_alarm" "orders_placed_storm" {
  alarm_name          = "tv-${var.environment}-orders-placed-storm"
  alarm_description   = "Runaway-strategy pager: 50+ orders placed in 5 minutes (Sum of the agent's per-scrape deltas of tv_orders_placed_total, both modes). Valid in PAPER and LIVE mode - a runaway signal loop places paper orders at the same rate, and catching it in paper is the point. Always armed (an off-hours storm = a session-gate bug). Triage: check the strategy evaluator + OMS logs; the order_audit table has one row per placement."
  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 50
  evaluation_periods  = 1
  metric_name         = "tv_orders_placed_delta_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions # { host = "tickvault-prod" }
  treat_missing_data  = "notBreaching"
  alarm_actions       = local.app_alarm_actions
  # NO ok_actions: a storm subsiding is not an all-clear — the placed
  # orders exist; the operator decides (Rule-11 false-recovery).
  ok_actions = []
}

# ---------------------------------------------------------------------------
# 3. Daily-loss breach — ARMED AND RECEIVING DATA since 2026-08-10
# ---------------------------------------------------------------------------
# ARMED 2026-08-10. tv_daily_pnl (gauge, INR, realized + unrealized) is now
# published by RiskEngine::daily_loss_state — the shared helper that both
# the order-gate check and the standalone halt evaluation call, so what
# CloudWatch sees is by construction the number the halt decided on.
#
# treat_missing_data RE-DECIDED at arming (the dormancy ratchet in
# crates/storage/tests/cloudwatch_dormant_alarms_guard.rs forces this to be
# a conscious choice, not an inherited default): it stays "notBreaching".
# Missing data is the NORMAL state here, not a fault — the box runs weekdays
# 08:30-17:30 IST and the gauge only publishes while the order runtime is
# evaluating, so "breaching" would page every single evening and all
# weekend. HONEST LIMIT, stated rather than implied: notBreaching means this
# alarm CANNOT detect an in-session data outage — a dead app reads as OK
# here. That gap is covered by the market-hours liveness alarm and the boot
# heartbeat, which exist for exactly that purpose; this alarm's job is the
# BREACH, not the silence.
#
# Redundant counter-side route for the sink's Critical RiskHalt Telegram
# page (the AGGREGATOR-DROP-01 dual-route precedent) — a breach pages even
# if the app-side Telegram leg is degraded.
resource "aws_cloudwatch_metric_alarm" "daily_loss_breach" {
  alarm_name          = "tv-${var.environment}-daily-loss-breach"
  alarm_description   = "Daily P&L breached the loss limit (tv_daily_pnl Minimum below -1 x daily_loss_alarm_inr over 5 minutes). ARMED 2026-08-10: the tv_daily_pnl gauge is emitted by RiskEngine::daily_loss_state, the same helper the halt decides on. treat_missing_data stays notBreaching because the box is off outside 08:30-17:30 IST weekdays, so no-data is normal here - this alarm reports a BREACH, not silence; a dead app is the liveness alarm/boot heartbeat job. Redundant with the risk engine's own halt (RISK-GAP-01 + the Critical RiskHalt Telegram page). NO OK page: the risk-engine halt latch outlives an intraday mark-to-market bounce - an auto-OK would say recovered while trading stays blocked (Rule-11)."
  comparison_operator = "LessThanThreshold"
  threshold           = -1 * var.daily_loss_alarm_inr
  evaluation_periods  = 1
  metric_name         = "tv_daily_pnl"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Minimum"
  dimensions          = local.app_dimensions
  treat_missing_data  = "notBreaching"
  alarm_actions       = local.app_alarm_actions
  # NO ok_actions — see description (halt latch outlives an MTM bounce).
  ok_actions = []
}

# ---------------------------------------------------------------------------
# 4. Order fill lag — SHIPS DISARMED (arming contract in the description)
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "order_fill_lag_high" {
  alarm_name          = "tv-${var.environment}-order-fill-lag-high"
  alarm_description   = "[DORMANT 2026-08-09 - NO PRODUCTION EMIT SITE: tv_order_fill_lag_seconds is written by ZERO non-test code paths. This alarm additionally ships with actions_enabled=false, so it can page nobody regardless of state.] ARMED at Phase-1 live promotion (phase-0-architecture.md promotion criterion #5) — in dry-run the metric never publishes and an armed alarm would be a permanently-INSUFFICIENT_DATA dead pager. The arming PR must also decide: join the market-hours window gate (12→13 + dated doc corrections) OR make the emitter publish in-session-only (the gauge holds its last value across scrapes)."
  comparison_operator = "GreaterThanThreshold"
  threshold           = 10
  evaluation_periods  = 1
  metric_name         = "tv_order_fill_lag_seconds"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Maximum"
  dimensions          = local.app_dimensions
  treat_missing_data  = "notBreaching"
  # DISARMED until Phase-1 (see description — the one-line flip at arming
  # is `actions_enabled = true`). tv_order_fill_lag_seconds has NO emit
  # site in this PR (cluster A / Phase-1 ships it).
  actions_enabled = false
  alarm_actions   = local.app_alarm_actions
  ok_actions      = []
}

# ---------------------------------------------------------------------------
# 5. SEBI FORENSIC CHAIN LOSING ROWS — ONE composite alarm over FIVE counters
#    (2026-08-19, operator Quote 17)
# ---------------------------------------------------------------------------
# The order/position audit tables are the SEBI five-year forensic chain. Five
# counters report a row failing to reach them, and until now every one of them
# was EMF-shipped to CloudWatch, billed, and consumed by NOTHING — not an
# alarm, not a dashboard widget. A regulator-facing record was being lost with
# no signal at all.
#
# WHY ONE ALARM AND NOT FIVE. The five counters are five MECHANISMS for one
# CONDITION: "an order/position audit row did not make it into the chain".
# The operator's response is the same in every case — go look at the coded
# lines and find out which rows were lost — so five separate pagers would
# deliver five different names for one incident and, per
# dhan-rest-only-noise-lock-2026-07-14.md §2.3a, "a family of eleven pagers
# for one subsystem trains an operator to ignore all of them". A metric-math
# SUM pages ONCE and the description says how to tell the mechanisms apart.
# It is also cheaper: one alarm, not five.
#
# EMIT SITES VERIFIED IN SOURCE 2026-08-19 (this repo forbids alarms on
# metrics nothing emits — a filter that can never match is a permanently-green
# dead monitor):
#   tv_order_audit_rows_discarded_total          -> crates/storage/src/order_audit_persistence.rs:479
#   tv_order_audit_persist_errors_total          -> crates/app/src/order_observability.rs:564,575
#                                                   crates/app/src/dhan_order_push_observability.rs:195,206
#   tv_order_update_events_dropped_total         -> crates/app/src/dhan_order_push_observability.rs:257
#                                                   crates/trading/src/oms/groww/push/order_events.rs:384
#   tv_order_update_events_persist_errors_total  -> crates/app/src/order_update_events_boot.rs:344,358,381,395
#   tv_order_update_events_rows_discarded_total  -> crates/storage/src/order_update_events_persistence.rs:588
#                                                   crates/storage/src/position_update_events_persistence.rs:473
# All five are already in the EMF metric_selectors list in
# user-data.sh.tftpl, so this adds NO new metric name and no new EMF cost —
# it consumes five series we were already paying to ship.
#
# HONEST SCOPE: the order push channels run in PAPER mode today
# (`[dhan_order_push] enabled`, `[groww_orders] order_push_enabled`, both
# receive-only; GROWW_ORDER_LIVE_FIRE is false). So a breach today means the
# PAPER forensic chain lost a row — which is exactly the rehearsal you want to
# have working before live orders, not a reason to leave it unwatched.
resource "aws_cloudwatch_metric_alarm" "order_audit_chain_loss" {
  alarm_name = "tv-${var.environment}-order-audit-chain-loss"
  # NOTE: AWS caps alarm_description at 1024 characters (terraform validate
  # failure, 2026-08-19). Long-form reasoning belongs in comments like this
  # one, which has no cap; the description is the pager text.
  alarm_description = "SEBI order/position forensic chain LOST A ROW. Sums five counters: order_audit rows discarded, order_audit persist errors, order/position events dropped pre-persistence, order_update_events persist errors, order_update_events rows discarded. Five-year retention - these tables are the only record of what the system did with an order, and the broker event is not replayed. DO: (1) grep /tickvault/<env>/app for coded ORDER-AUDIT / ORDER-UPDATE lines - they name which mechanism, the stage (append vs flush) and the reason. (2) persist_errors points at QuestDB (ILP flush latency, WAL-suspended gauge); rows may still be re-appendable. (3) rows_discarded or events_dropped means it is already gone - record the window. Order push is receive-only PAPER today, so a breach now is the rehearsal failing, not live-order data."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  # notBreaching: the box is stopped outside 08:30-17:30 IST weekdays and the
  # push channels only publish in-session, so no-data is the normal overnight
  # state. `breaching` here would page every single night. This alarm reports
  # LOST ROWS, never silence; a dead app is the boot-heartbeat and
  # market-hours-liveness alarms' job.
  treat_missing_data = "notBreaching"
  alarm_actions      = local.app_alarm_actions
  # NO ok_actions. These are cumulative loss counters: a delta returning to
  # zero means no ADDITIONAL rows were lost, never that the lost ones came
  # back. An OK page would tell the operator a regulator-facing record was
  # restored when it was not (Rule 11, no false recovery).
  ok_actions = []

  # Metric math: SUM of the five, with each leg's own metric NOT returned, so
  # only the total drives the alarm state and CloudWatch charges for one alarm.
  # FILL(m, 0) on each leg is load-bearing: an expression over five series
  # evaluates to no-data if ANY leg is missing, and a counter that never
  # incremented in the window legitimately has no sample — without FILL, the
  # arithmetic would be silenced by the four healthy legs.
  metric_query {
    id          = "audit_loss_total"
    expression  = "FILL(m1,0)+FILL(m2,0)+FILL(m3,0)+FILL(m4,0)+FILL(m5,0)"
    label       = "order/position audit rows lost (all five mechanisms)"
    return_data = true
  }

  metric_query {
    id          = "m1"
    return_data = false
    metric {
      metric_name = "tv_order_audit_rows_discarded_total"
      namespace   = local.app_namespace
      period      = 300
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }

  metric_query {
    id          = "m2"
    return_data = false
    metric {
      metric_name = "tv_order_audit_persist_errors_total"
      namespace   = local.app_namespace
      period      = 300
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }

  metric_query {
    id          = "m3"
    return_data = false
    metric {
      metric_name = "tv_order_update_events_dropped_total"
      namespace   = local.app_namespace
      period      = 300
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }

  metric_query {
    id          = "m4"
    return_data = false
    metric {
      metric_name = "tv_order_update_events_persist_errors_total"
      namespace   = local.app_namespace
      period      = 300
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }

  metric_query {
    id          = "m5"
    return_data = false
    metric {
      metric_name = "tv_order_update_events_rows_discarded_total"
      namespace   = local.app_namespace
      period      = 300
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }
}
