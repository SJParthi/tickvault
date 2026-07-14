# =============================================================================
# Telegram-drop pager — 2026-07-14 (REST-pipeline adversarial audit, GAP-05)
# =============================================================================
# THE GAP THIS CLOSES (audit S8, docs/audits/2026-07-14-rest-pipeline-
# adversarial-audit.md): every TYPED-event page in the system — incl. the
# REST-leg 3-minute escalation HIGH pages, the AUTH-GAP-05 self-heal pages,
# the daily scorecard — terminates at the app's own Telegram dispatcher.
# tv_telegram_dropped_total counts every drop (reason = send_failed /
# noop_mode / coalesced_sample_capped — notification/service.rs +
# coalescer.rs), but NO alarm read it: a broken bot token / Telegram API
# outage / NoOp-mode boot silently killed every typed-event page with zero
# signal. This pager makes sustained drops loud via the SNS -> telegram-
# webhook-Lambda route — a DIFFERENT delivery leg from the app's dispatcher
# (same chat, but the Lambda path works while the app-side dispatcher is
# broken; if Telegram-the-service itself is down, neither leg delivers —
# stated residual, the alarm still lands in the CloudWatch console + any
# other SNS subscriber).
#
# ROUTE (house pattern: feed-stall-restart-alarm.tf): the counter is NOT in
# the EMF metric_selectors allowlist (verified 2026-07-14,
# deploy/aws/cloudwatch-agent.json), but every 60s scrape ships it as a
# plain-JSON event (with $.host) into /tickvault/<env>/metrics; a log metric
# filter extracts the per-scrape delta. COUNTER SHAPE: identical model +
# identical honest residual as feed-stall-restart-alarm.tf — the CW agent's
# prometheus pipeline converts COUNTER samples to PER-SCRAPE DELTAS, so
# `Sum` over the window = drops in the window. Labeled series (the reason
# label) arrive as separate events per scrape, each carrying the metric
# field — Sum aggregates across reasons, deliberately reason-agnostic (any
# sustained drop class is page-worthy).
#
# FIRST-SAMPLE BASELINE (the feed-stall round-5 lesson — FLAGGED FOLLOW-UP,
# NOT fixed here): tv_telegram_dropped_total is NOT pre-registered at 0
# after the metrics recorder installs in crates/app/src/main.rs (verified
# 2026-07-14 — only the stall / never-streamed / seal-drain counters are).
# Each labeled series is therefore lazily BORN at its first drop, and the
# CW agent's delta pipeline DROPS that first sample as its baseline — the
# session's first drop per reason-series is uncounted, so the effective
# first-episode threshold is 4 (or 3 spread across >=2 reason series)
# instead of the documented 3. A sustained broken-bot episode (the class
# this pager exists for) repeat-drops every typed event and still pages;
# only a tiny first burst can slip. Fix = one `metrics::counter!(...)
# .increment(0)` per reason label in main.rs boot Step 2 (crates PR —
# blocked here by the design-first wall; flagged in the 2026-07-14 PR
# body). When that lands, the series turns DENSE and this comment + the
# ok_actions choice below should be revisited.
#
# TUNING: Sum >= 3 per ONE aligned 900s window — a one-off coalescer
# sample-cap drop or a single transient send failure never pages; a broken
# bot drops EVERY typed event, so any active session lands >=3 in some
# window quickly. Aligned-window honesty (the feed-stall round-10 math): a
# burst that stops after straddling a boundary at 2+1 never pages; only a
# sustained drop regime is guaranteed to page. Quiet-hours honesty: drops
# require typed events to fire — a bot broken overnight with zero events
# produces zero drops and pages only when traffic resumes (that is when
# pages are being lost, so detection timing matches harm).
#
# ok_actions = [] (NO recovered/OK page — Rule-11): the metric is SPARSE
# (no pre-registration), so with treat_missing_data=notBreaching the alarm
# auto-transitions to OK ~15 min after drops age out of the lookback — but
# in a low-traffic window that only means "no typed event fired lately",
# NOT "the bot recovered". A green auto-OK while the bot is still broken
# would be a false recovery. Revisit (flip to local.app_alarm_ok) once the
# pre-registration lands and the series is dense. (Contrast feed-stall:
# its counter IS dense, so its OK genuinely tracks "restarts stopped".)
#
# Future EMF-allowlisting: if tv_telegram_dropped_total is ever added to
# the EMF metric_selectors allowlist, REMOVE this log metric filter in the
# same PR — dual-publish under Sum double-counts every drop (fail-loud,
# never a silent miss; the feed-stall-restart-alarm.tf precedent note).
# =============================================================================

resource "aws_cloudwatch_log_metric_filter" "telegram_drops_fallback" {
  name = "tv-${var.environment}-telegram-drops-fallback"
  # Agent-created group, referenced by name (house precedent:
  # metrics-log-metric-filters.tf; adopted into terraform by log-retention.tf).
  log_group_name = "/tickvault/${var.environment}/metrics"
  pattern        = "{ $.tv_telegram_dropped_total = * }"
  metric_transformation {
    name      = "tv_telegram_dropped_total"
    namespace = "Tickvault/Prod"
    value     = "$.tv_telegram_dropped_total"
    dimensions = {
      host = "$.host" # /metrics events DO carry host (prometheus scrape label)
    }
    # Deliberately no default_value (that knob emits datapoints for
    # NON-matching events — never wanted). The metric is SPARSE until the
    # flagged crates-side pre-registration lands (header note above):
    # billed only in hours a drop fires — near-free.
  }
}

# AWS caps alarm_description at 1024 chars — the full honest-residuals text
# (first-sample baseline, 2+1 straddle, Telegram-down floor, no-OK rationale)
# lives in this file's header block; the description below is the
# operator-facing summary (813 chars, inside the cap).
resource "aws_cloudwatch_metric_alarm" "telegram_drops" {
  alarm_name          = "tv-${var.environment}-telegram-drops"
  alarm_description   = "Telegram typed-event pages are being DROPPED: >=3 drops of tv_telegram_dropped_total within one 15-min window (Sum of CW-agent per-scrape deltas; reasons send_failed / noop_mode / coalesced_sample_capped aggregated - read the reason split on the app /metrics exporter). A broken bot token or Telegram-API failure silently kills EVERY typed-event page (REST-leg escalations, AUTH-GAP-05, scorecards) - this alarm is the SNS->Lambda backstop leg, which delivers while the app-side dispatcher is broken. NO recovered/OK page: the sparse metric's auto-OK only means no typed event fired lately, never bot recovery - verify by sending a test page. Triage: bot token in SSM, api.telegram.org reachability. Full residuals: telegram-drop-alarm.tf header. Runbook: .claude/rules/project/wave-3-error-codes.md (TELEGRAM-01)"
  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 3
  evaluation_periods  = 1
  metric_name         = "tv_telegram_dropped_total"
  namespace           = local.app_namespace
  # 900s aligned window: Sum of per-scrape deltas = drops in the window
  # (see COUNTER SHAPE + TUNING header blocks).
  period             = 900
  statistic          = "Sum"
  dimensions         = local.app_dimensions # { host = "tickvault-prod" }
  treat_missing_data = "notBreaching"
  # Always-armed: typed events (and therefore drops) can fire at any hour;
  # a drop is page-worthy whenever it happens.
  alarm_actions = local.app_alarm_actions
  # NO ok_actions (Rule-11 — see the header rationale): the sparse metric's
  # auto-OK ~15 min after drops age out would be a false "recovered" while
  # the bot may still be broken.
  ok_actions = []
}
