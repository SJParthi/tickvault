# =============================================================================
# Groww sidecar stall-restart pager (counts EVERY restart) — 2026-07-06 (r3)
# =============================================================================
# THE GAP THIS CLOSES (round-3 review, 2026-07-06): the errcode-feed-stall-01
# log-filter alarm (error-code-alarms.tf) can only see ERROR-level
# FEED-STALL-01 lines — and in groww_sidecar_supervisor.rs the PER-RESTART
# emission is warn! (invisible to the ERROR-only errors.jsonl sink); only the
# sidecar's own STORM escalation (>5 restarts within a 300s sliding window,
# i.e. the 6th+ rapid restart) is error!. So the errcode alarm's real paging
# floor is a <~50s flap cycle. Flap cycles between ~50s and ~5 min (e.g. the
# ~90s stall-flap analog of the 2026-07-06 order-update incident cadence)
# produced ZERO ERROR lines and therefore ZERO pages, while the docs claimed
# ">=3 stall-restarts in 15 min pages".
#
# THE FIX: alarm on the counter instead. tv_feed_sidecar_stall_restart_total
# (groww_sidecar_supervisor.rs:1118) increments exactly ONCE per stall-restart
# — warn!- and error!-level alike — so this pager sees EVERY restart. Same
# route as the order-update reconnect-storm alarm: the counter is NOT in the
# ratcheted 21-name EMF metric_selectors allowlist, but every 60s scrape still
# ships it as a plain-JSON event (with $.host) into /tickvault/<env>/metrics;
# a log metric filter extracts the per-scrape delta.
#
# COUNTER SHAPE: identical model + identical honest residual as
# order-update-reconnect-storm-alarm.tf — the CW agent's prometheus pipeline
# converts COUNTER samples to PER-SCRAPE DELTAS, so `Sum` over the window =
# restarts in the window. Not live-verified from this sandbox; if the field
# ever proved CUMULATIVE, Sum overcounts and pages too eagerly (fail-loud,
# never a silent miss) and this alarm must be reworked to DIFF(Maximum).
#
# FEED LABEL: the Prometheus series carries feed="groww"; the JSON event
# carries it as a field. The filter matches on the metric field only and
# dimensions only on host, so a future second feed's sidecar restarts SUM
# into the same metric — a deliberately feed-agnostic pager, matching the
# runbook's feed-agnostic watchdog design.
#
# NO market-hours gate needed: should_restart_on_stall() requires
# market_open, so the counter can only increment 09:00-15:30 IST — there is
# no post-close churn to suppress (unlike the order-update reconnect loop).
#
# TUNING (honest, aligned-window semantics): Sum >= 3 per ONE 900s period.
# CloudWatch evaluates aligned tumbling 15-min windows, not a sliding window:
# a burst of exactly 3 restarts straddling a window boundary (2+1) pages one
# window later — or not at all if the flap stops at exactly 3. A SUSTAINED
# flap keeps accumulating and always pages. Detection floor: flap cycles
# slower than ~5 min (<3 restarts per 15-min window) do NOT page — stated
# residual; a single self-heal restart never pages (the runbook's own
# operator-action bound). Fast flaps (<~50s cycle) additionally trip the
# errcode-feed-stall-01 storm-escalation tripwire within ~5 min.
#
# Future EMF-allowlisting: if the counter is ever added to the EMF
# metric_selectors allowlist, REMOVE this log metric filter in the same PR
# (or switch the alarm to a deduplicated source): both pipelines would
# publish the same per-scrape delta into the same metric identity, and under
# this alarm's Sum statistic every restart would count TWICE — halving the
# effective threshold (over-pages; fail-loud, never a silent miss). The
# tv_boot_completed dual-publish precedent is harmless ONLY for its
# Maximum-stat consumers — it does not transfer to Sum.
# =============================================================================

resource "aws_cloudwatch_log_metric_filter" "feed_stall_restarts_fallback" {
  name = "tv-${var.environment}-feed-stall-restarts-fallback"
  # Agent-created group, referenced by name (house precedent:
  # metrics-log-metric-filters.tf; adopted into terraform by log-retention.tf).
  log_group_name = "/tickvault/${var.environment}/metrics"
  pattern        = "{ $.tv_feed_sidecar_stall_restart_total = * }"
  metric_transformation {
    name      = "tv_feed_sidecar_stall_restart_total"
    namespace = "Tickvault/Prod"
    value     = "$.tv_feed_sidecar_stall_restart_total"
    dimensions = {
      host = "$.host" # /metrics events DO carry host (prometheus scrape label)
    }
    # Deliberately no default_value (sparse metric; notBreaching alarm).
  }
}

resource "aws_cloudwatch_metric_alarm" "feed_stall_restarts" {
  alarm_name          = "tv-${var.environment}-feed-stall-restarts"
  alarm_description   = "FEED-STALL-01 flap: >=3 Groww sidecar stall-restarts within one 15-min window (Sum of the agent's per-scrape deltas of tv_feed_sidecar_stall_restart_total - the counter increments once per restart, warn!- and error!-level alike, so THIS pager sees every restart; the errcode-feed-stall-01 alarm sees only the sidecar's own >5-per-5-min STORM escalation ERROR lines). A single self-heal restart never pages. Honest floor: flap cycles slower than ~5 min (<3 restarts per aligned 15-min window) do not page - stated residual. The provider keeps closing the socket - check credential/entitlement. Runbook: .claude/rules/project/feed-stall-watchdog-error-codes.md"
  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 3
  evaluation_periods  = 1
  metric_name         = "tv_feed_sidecar_stall_restart_total"
  namespace           = local.app_namespace
  # 900s aligned window: Sum of per-scrape deltas = restarts in the window
  # (see COUNTER SHAPE + TUNING header blocks; boundary-straddling 2+1 bursts
  # page one window later or, if the flap stops at exactly 3, not at all).
  period             = 900
  statistic          = "Sum"
  dimensions         = local.app_dimensions # { host = "tickvault-prod" }
  treat_missing_data = "notBreaching"
  # Always-armed (no market-hours gate): the Rust watchdog only restarts when
  # should_restart_on_stall() sees market_open=true, so the counter cannot
  # increment outside 09:00-15:30 IST - there is no off-hours churn to gate.
  alarm_actions = local.app_alarm_actions
  ok_actions    = local.app_alarm_ok
}
