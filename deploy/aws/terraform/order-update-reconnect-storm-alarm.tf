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
# COUNTER SHAPE (round-2 review fix, 2026-07-06): the CloudWatch agent's
# prometheus pipeline converts COUNTER-type samples to PER-SCRAPE DELTAS
# (dropping each counter's first sample) before emitting the log/EMF event —
# the AWS-documented agent behavior, and the model every sibling counter
# alarm in app-alarms.tf already assumes (spill_dropped / dlq_ticks /
# ticks_dropped / orders_rejected / disk_watcher_respawn all alarm
# `Sum > 0 per 5m` — under cumulative shipping they would latch permanently
# after the first increment, contradicting their documented in-window-rate
# semantics and the aggregator_no_seals post-close-idle fire history). An
# earlier draft alarmed on DIFF(Maximum(cumulative)) — dead on arrival under
# delta shipping (DIFF of a 0/1-per-scrape series hovers at 0/±1, never >5).
# So: plain `Sum` over the window = reconnect increments in that window.
# HONEST RESIDUAL: the delta shape is NOT live-verified from this sandbox
# (no AWS creds); the post-apply runbook verifies one /tickvault/prod/metrics
# event. If the field ever proved CUMULATIVE, Sum would OVERCOUNT and page
# too eagerly inside the gate window (fail-loud, never a silent miss) and
# this alarm must be reworked to DIFF(Maximum).
#
# TUNING: the counter increments exactly ONCE per disconnect/reconnect cycle
# (order_update_connection.rs:254 — the bottom of the reconnect loop). A
# 5-min Sum at threshold >5 could NEVER see the documented incident shape:
# a ~90s cycle yields only 300/90 ≈ 3-4 increments per 5 min, and even a 60s
# cycle yields exactly 5 (not > 5). The window is therefore 900s: a 90s cycle
# yields ~10 per 15 min (≫ 5) and a 60s cycle ~15, while the routine
# 15:30-close churn (≤3 attempts before the WS-GAP-04 dormant sleep) stays
# ≤3 per 15 min, below threshold. Honest detection floor: flap cycles slower
# than ~180s (≤5 reconnects/15 min) do NOT page — that slow-flap band is a
# stated residual (the durable-down alarm owns full outages; nothing owns
# 3-min+ cycles today). Aligned-window semantics (round-8; mirrors the
# feed-stall-restart-alarm.tf clause): CloudWatch evaluates aligned TUMBLING
# 900s windows, not a sliding window — a short burst of >5 cycles that
# STRADDLES a window boundary (≤5 on each side, e.g. 4+4 over ~12 min) pages
# one window later, or not at all if the flap stops at the boundary; a
# SUSTAINED flap keeps accumulating and always pages (the 2026-07-06
# all-session incident shape is unaffected — stated residual, not a tuning
# defect). Counter reset on app restart is absorbed by the
# agent's delta calculation (first post-restart sample dropped — at most one
# increment lost, never a latched or negative artifact; restarts themselves
# are owned by the liveness alarms).
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
# Future EMF-allowlisting (round-3 review fix, 2026-07-06): if the counter is
# ever added to the EMF metric_selectors allowlist, REMOVE this log metric
# filter in the same PR (or switch the alarm to a deduplicated source): both
# pipelines would publish the same per-scrape delta into the same metric
# identity, and under this alarm's Sum statistic every reconnect would count
# TWICE — halving the effective storm threshold to ~2.5 cycles/15 min
# (over-pages; fail-loud, never a silent miss). The tv_boot_completed
# dual-publish precedent is harmless ONLY for its Maximum-stat consumers
# (market-hours-liveness-alarm.tf + the readiness Lambda) — idempotence under
# duplicate datapoints does not transfer to Sum.
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
  alarm_description   = "Order-update WebSocket is FLAPPING: more than 5 reconnect attempts in 15 minutes during market hours (Sum of the agent's per-scrape counter deltas; catches flap cycles faster than ~3 min, incl. the ~90s 2026-07-06 incident cadence — the counter increments once per cycle). Aligned tumbling window, not sliding: a >5-cycle burst straddling the 900s boundary (<=5 per side) pages one window later or, if the flap stops at the boundary, not at all; a SUSTAINED flap always pages. The durable-down alarm (order-update-ws-inactive, Minimum<1 over 2x60s) cannot see a socket that reconnects within each minute (2026-07-06 incident class). Fill confirmations may arrive late or duplicated. Check the errors stream for order-update disconnect reasons; cross-check Dhan status + tv_token_remaining_seconds. Runbook: .claude/rules/project/wave-2-error-codes.md (WS-GAP family) + docs/dhan-ref/10-live-order-update-websocket.md"
  comparison_operator = "GreaterThanThreshold"
  threshold           = 5
  evaluation_periods  = 1
  metric_name         = "tv_order_update_reconnections_total"
  namespace           = local.app_namespace
  # 900s ALIGNED window (NOT 300s): the counter increments once per flap
  # cycle, so a 5-min window mathematically cannot exceed 5 for any cycle
  # >= 60s — see the TUNING header block. Sum over the window = reconnects in
  # the window (the agent ships per-scrape counter deltas — see COUNTER SHAPE
  # header; boundary-straddling >5 bursts page one window later or, if the
  # flap stops at the boundary, not at all — tumbling windows, not sliding).
  period             = 900
  statistic          = "Sum"
  dimensions         = local.app_dimensions # { host = "tickvault-prod" }
  treat_missing_data = "notBreaching"
  # Actions OFF by default; the market-hours gate Lambda
  # (market-hours-liveness-alarm.tf ALARM_NAMES) flips them ON 09:20-15:35 IST
  # Mon-Fri. Rationale: 15:30-close churn is routine (<=3 attempts before the
  # WS-GAP-04 dormant sleep, <=3 per 15-min window, below threshold 5), and the gate's daily
  # set_alarm_state(OK) on open immunizes this alarm against the latched-state
  # incident class. Terraform re-asserting actions_enabled=false on apply is
  # harmless: applies are banned 09:00-15:45 IST and post-close applies
  # coincide with the gate-disabled window (existing 4-alarm precedent).
  actions_enabled = false
  alarm_actions   = local.app_alarm_actions
  ok_actions      = local.app_alarm_ok
}
