# =============================================================================
# Order-update WebSocket reconnect-STORM alarm (flapper class) — 2026-07-06
# =============================================================================
# THE GAP THIS CLOSES (zero-page incident leg 3): the existing
# tv-<env>-order-update-ws-inactive alarm (app-alarms.tf) needs
# Minimum(tv_order_update_ws_active) < 1 over a full 60s period — a socket
# that FLAPS (reconnects within each minute, ~90s cycle on 2026-07-06) never
# shows a whole minute of Minimum=0, so the durable-down alarm stays OK while
# fill confirmations arrive late or duplicated all session.
#
# ROUTE DECISION (verified 2026-07-06): the counter
# `tv_order_update_reconnections_total` (order_update_connection.rs:86,
# incremented :254 before every backoff sleep, no labels) is NOT in the CW
# agent's EMF metric_selectors allowlist — and that list is ratcheted at
# exactly 21 names (cloudwatch_app_alarms_wiring.rs). Every 60s scrape still
# ships it as a plain-JSON event (with $.host) into /tickvault/<env>/metrics —
# the exact surface the tv_boot_completed_fallback filter uses
# (metrics-log-metric-filters.tf). So: log metric filter on /metrics; NO
# allowlist edit, NO Rust pin rename.
#
# Future-compatible: if the counter is ever EMF-allowlisted, the same metric
# identity dual-publishes harmlessly (tv_boot_completed precedent).
# =============================================================================

resource "aws_cloudwatch_log_metric_filter" "order_update_reconnections_fallback" {
  name = "tv-${var.environment}-order-update-reconnections-fallback"
  # Agent-created group, referenced by name (house precedent:
  # metrics-log-metric-filters.tf; adopted into terraform by log-retention.tf).
  log_group_name = "/tickvault/${var.environment}/metrics"
  pattern        = "{ $.tv_order_update_reconnections_total = * }"
  metric_transformation {
    name      = "tv_order_update_reconnections_total"
    namespace = "Tickvault/Prod"
    value     = "$.tv_order_update_reconnections_total"
    dimensions = {
      host = "$.host" # /metrics events DO carry host (prometheus scrape label)
    }
    # Deliberately no default_value.
  }
}

resource "aws_cloudwatch_metric_alarm" "order_update_reconnect_storm" {
  alarm_name          = "tv-${var.environment}-order-update-reconnect-storm"
  alarm_description   = "Order-update WebSocket is FLAPPING: more than 5 reconnect attempts in 5 minutes during market hours. The durable-down alarm (order-update-ws-inactive, Minimum<1 over 2x60s) cannot see a socket that reconnects within each minute (2026-07-06 incident class). Fill confirmations may arrive late or duplicated. Check the errors stream for order-update disconnect reasons; cross-check Dhan status + tv_token_remaining_seconds. Runbook: .claude/rules/project/wave-2-error-codes.md (WS-GAP family) + docs/dhan-ref/10-live-order-update-websocket.md"
  comparison_operator = "GreaterThanThreshold"
  threshold           = 5
  evaluation_periods  = 1
  treat_missing_data  = "notBreaching"
  # Actions OFF by default; the market-hours gate Lambda
  # (market-hours-liveness-alarm.tf ALARM_NAMES) flips them ON 09:20-15:35 IST
  # Mon-Fri. Rationale: 15:30-close churn is routine (<=3 attempts before the
  # WS-GAP-04 dormant sleep, below threshold 5), and the gate's daily
  # set_alarm_state(OK) on open immunizes this alarm against the latched-state
  # incident class. Terraform re-asserting actions_enabled=false on apply is
  # harmless: applies are banned 09:00-15:45 IST and post-close applies
  # coincide with the gate-disabled window (existing 4-alarm precedent).
  actions_enabled = false
  alarm_actions   = local.app_alarm_actions
  ok_actions      = local.app_alarm_ok

  metric_query {
    id = "recon_cum"
    metric {
      metric_name = "tv_order_update_reconnections_total"
      namespace   = local.app_namespace
      period      = 300
      stat        = "Maximum"
      dimensions  = local.app_dimensions # { host = "tickvault-prod" }
    }
    return_data = false
  }
  metric_query {
    id = "recon_5m_increase"
    # Per-5-min increment of the cumulative counter (simpler than RATE*PERIOD,
    # same semantics). Counter reset on app restart -> ONE negative datapoint
    # -> below threshold -> cannot false-page; a storm masked by a mid-window
    # restart is missed for one 5-min window only (the restart itself is owned
    # by the liveness alarms).
    expression  = "DIFF(recon_cum)"
    label       = "order-update reconnects per 5 min"
    return_data = true
  }
}
