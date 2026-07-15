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
#   2. daily-loss-breach    — ARMED, structurally silent in dry-run
#      (arm-on-arrival: tv_daily_pnl has NO emit site in this PR).
#   3. order-fill-lag-high  — SHIPS DISARMED (actions_enabled = false; the
#      arming contract is in its description).
#
# COST (see aws-budget.md COST NOTE 2026-07-14): +3 alarms ≈ $0.30/mo;
# +1 derived custom-metric series (tv_orders_placed_delta_total) ≈ $0.30/mo;
# the 2 new EMF allowlist names (tv_daily_pnl, tv_order_fill_lag_seconds)
# are DORMANT — zero datapoints = $0.00 until cluster A / Phase-1 ships
# their emit sites, then ≈ +$0.60/mo (pre-noted there).
#
# EMIT-SITE OWNERSHIP (the tv_daily_pnl hard boundary): tv_daily_pnl and
# tv_order_fill_lag_seconds have NO Rust emit site in this PR — their
# emitters ship with cluster A (daily-pnl gauge) / Phase-1 (fill lag). The
# alarms here arm on data arrival with zero further tf changes. The
# emit-site guard (cloudwatch_app_alarms_wiring.rs
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
# 3. Daily-loss breach — ARMED, arm-on-arrival (tv_daily_pnl = cluster A)
# ---------------------------------------------------------------------------
# EMIT-SITE OWNERSHIP: tv_daily_pnl (gauge, INR, realized + unrealized) has
# NO emit site in this PR — cluster A ships it. Until then the metric never
# publishes: missing data + notBreaching = structurally silent (a dormant
# armed alarm costs $0.10/mo and needs ZERO tf changes to go live).
# Redundant counter-side route for the sink's Critical RiskHalt Telegram
# page (the AGGREGATOR-DROP-01 dual-route precedent) — a breach pages even
# if the app-side Telegram leg is degraded.
resource "aws_cloudwatch_metric_alarm" "daily_loss_breach" {
  alarm_name          = "tv-${var.environment}-daily-loss-breach"
  alarm_description   = "Daily P&L breached the loss limit (tv_daily_pnl Minimum below -1 x daily_loss_alarm_inr over 5 minutes). EMIT-SITE OWNERSHIP: the tv_daily_pnl gauge ships with cluster A - this alarm arms on data arrival with zero tf changes. Redundant with the risk engine's own halt (RISK-GAP-01 + the Critical RiskHalt Telegram page). NO OK page: the risk-engine halt latch outlives an intraday mark-to-market bounce - an auto-OK would say recovered while trading stays blocked (Rule-11)."
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
  alarm_description   = "ARMED at Phase-1 live promotion (phase-0-architecture.md promotion criterion #5) — in dry-run the metric never publishes and an armed alarm would be a permanently-INSUFFICIENT_DATA dead pager. The arming PR must also decide: join the market-hours window gate (12→13 + dated doc corrections) OR make the emitter publish in-session-only (the gauge holds its last value across scrapes)."
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
