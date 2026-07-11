# =============================================================================
# API bearer-auth 401-burst pager (GAP-SEC-01 companion) — 2026-07-10
# =============================================================================
# THE GAP THIS CLOSES (wave-2 #2, 2026-07-10): the bearer-auth middleware
# (crates/api/src/middleware.rs::require_bearer_auth) rejects with 401 on
# three arms (invalid token / malformed Authorization header / missing
# header), each logging a warn! with sanitized path + peer (BUG-3,
# 2026-07-05) — but warn! lines are invisible to the ERROR-only errors.jsonl
# sink and no counter existed, so a credential brute-force sweep against the
# Tailscale-funnelled port 3001 paged NOBODY. The middleware now increments
# the SINGLE UNLABELED counter tv_api_auth_failed_total once per rejection
# (all 3 arms; deliberately NO per-401 warn/error beyond the pre-existing
# BUG-3 warns — log-amplification defence, same reasoning as the #1458 429
# handling).
#
# ROUTE (the feed-stall-restart-alarm.tf house pattern, FULL-extraction
# variant): the counter is NOT in the CW agent's ratcheted EMF
# metric_selectors allowlist. Every 60s scrape still ships the series as a
# plain-JSON event (with $.host) into /tickvault/<env>/metrics; a log metric
# filter extracts the per-scrape delta. NO allowlist edit, NO Rust rename.
# The metric_transformation REUSES the raw counter name (full extraction of
# an unlabeled single-series counter — the feed-stall precedent; a DISTINCT
# derived name is only needed for label SLICES like seal-drop's
# kind="dropped").
#
# COUNTER SHAPE: identical model + identical honest residual as
# feed-stall-restart-alarm.tf — the CW agent's prometheus pipeline converts
# COUNTER samples to PER-SCRAPE DELTAS, so `Sum` over the window = 401s in
# the window. Not live-verified from this sandbox; if the field ever proved
# CUMULATIVE, Sum overcounts and pages too eagerly (fail-loud, never a
# silent miss) and this alarm must be reworked to DIFF(Maximum).
#
# FIRST-SAMPLE BASELINE (the feed-stall round-5 lesson, applied at design
# time): the delta pipeline DROPS each counter series' first observed sample
# as its baseline. tv_api_auth_failed_total increments ONLY on a real 401,
# so without pre-registration the series would be BORN at the first
# rejection and the dropped baseline sample would eat part of the session's
# first burst — silently raising this alarm's effective threshold. The
# series is therefore PRE-REGISTERED at 0 at ROUTER CONSTRUCTION
# (crates/api/src/lib.rs::build_router_with_auth — the single choke point
# both boot paths call, provably BEFORE the first possible 401 and AFTER
# the main.rs Step-3 recorder install; ratcheted by
# crates/app/tests/auth_failed_alarm_wiring_guard.rs — api-site pin + a
# read-only main.rs source-order scan), so the dropped first sample is the
# harmless 0 baseline and the series is DENSE while the app runs (a
# 0-delta event per 60s scrape). Honest residual: none for ordering — the
# server cannot serve (and thus reject) a request before its router
# exists, and the router is built after the recorder installs.
#
# TUNING (funnel-exposure justification): Sum >= 25 per ONE aligned 300s
# window. A fat-fingered token paste on the /feeds page produces one 401 per
# manual toggle click — even frantic operator retrying stays in single
# digits per 5 minutes and NEVER pages. A brute-force credential sweep needs
# sustained request volume (hundreds+/min against the public funnel), which
# lands >= 25 in some aligned window within minutes. Aligned-window honest
# residual (the feed-stall round-10 semantics): a burst split e.g. 24+24
# across a window boundary never breaches either aligned window — only a
# SUSTAINED sweep is guaranteed to page; a stopped sub-threshold probe is
# absorbed (correct: the warn! lines in /tickvault/<env>/app remain the
# forensic record). eval 1 / dta 1: dense 0-deltas keep the metric
# non-breaching when idle, so there is no flap concern.
#
# ALWAYS-ARMED (no market-hours gate): the funnel is public 24/7 and a
# credential sweep at 03:00 IST is exactly as meaningful as one at noon.
# Dense 0-deltas keep the metric non-breaching off-hours; when the box is
# stopped (16:30-08:30 IST schedule) the metric goes missing ->
# notBreaching, never a false page.
#
# Future EMF-allowlisting: if tv_api_auth_failed_total is ever added to the
# EMF metric_selectors allowlist, REMOVE this log metric filter in the same
# PR (or switch the alarm to a deduplicated source): both pipelines would
# publish the same per-scrape delta into the same metric identity, and under
# Sum every 401 would count TWICE — halving the effective threshold
# (over-pages; fail-loud, never a silent miss).
# =============================================================================

resource "aws_cloudwatch_log_metric_filter" "api_auth_failed_fallback" {
  name = "tv-${var.environment}-api-auth-failed-fallback"
  # Agent-created group, referenced by name (house precedent:
  # metrics-log-metric-filters.tf; adopted into terraform by log-retention.tf).
  log_group_name = "/tickvault/${var.environment}/metrics"
  pattern        = "{ $.tv_api_auth_failed_total = * }"
  metric_transformation {
    name      = "tv_api_auth_failed_total" # full extraction — raw name reused (see header)
    namespace = "Tickvault/Prod"
    value     = "$.tv_api_auth_failed_total"
    dimensions = {
      host = "$.host" # /metrics events DO carry host (prometheus scrape label)
    }
    # Deliberately no default_value (that knob emits datapoints for
    # NON-matching events — never wanted). The series is DENSE while the
    # app runs (0-delta per 60s scrape) thanks to the router-build
    # pre-registration — see FIRST-SAMPLE BASELINE header.
  }
}

resource "aws_cloudwatch_metric_alarm" "api_auth_failed" {
  alarm_name          = "tv-${var.environment}-api-auth-failed"
  alarm_description   = "GAP-SEC-01 401-burst pager: >= 25 bearer-auth rejections within one 5-min window on the public API surface (Sum of the agent's per-scrape deltas of tv_api_auth_failed_total - the counter increments once per 401 across all 3 rejection arms of require_bearer_auth and is pre-registered at 0 at router construction (post-recorder-install), so this pager sees every 401 incl. the session's first). A fat-fingered token paste (single-digit 401s per 5 min) never pages; a sustained credential brute-force sweep against the Tailscale funnel does. Triage: read the GAP-SEC-01 warn! lines in /tickvault/<env>/app for path + sanitized peer; verify the bearer token in SSM /tickvault/<env>/api/bearer-token was not rotated; a persistent sweep from one peer means the funnel is being probed - consider rotating the token. Runbook: .claude/rules/project/gap-enforcement.md (GAP-SEC-01)"
  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 25
  evaluation_periods  = 1
  metric_name         = "tv_api_auth_failed_total"
  namespace           = local.app_namespace
  # 300s aligned window: Sum of per-scrape deltas = 401s in the window; the
  # 25/5min threshold separates fat-finger (single digits) from brute-force
  # (hundreds+/min) with wide margins — see TUNING header.
  period             = 300
  statistic          = "Sum"
  dimensions         = local.app_dimensions # { host = "tickvault-prod" }
  treat_missing_data = "notBreaching"
  # Always-armed (no market-hours gate): the public funnel is probeable
  # 24/7 — see ALWAYS-ARMED header block.
  alarm_actions = local.app_alarm_actions
  # Expect ONE one-time green OK page the apply evening: new-alarm
  # INSUFFICIENT_DATA -> OK creation settling, not a recovery (house
  # precedent; full rationale in error-code-alarms.tf's ok_actions comment).
  ok_actions = local.app_alarm_ok
}
