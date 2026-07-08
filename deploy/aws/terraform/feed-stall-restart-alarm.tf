# =============================================================================
# Groww sidecar stall-restart pager (counts EVERY restart) — 2026-07-06 (r3)
# =============================================================================
# THE GAP THIS CLOSES (round-3 review, 2026-07-06): the errcode-feed-stall-01
# log-filter alarm (error-code-alarms.tf) can only see ERROR-level
# FEED-STALL-01 lines — and in groww_sidecar_supervisor.rs the PER-RESTART
# emission is warn! (invisible to the ERROR-only errors.jsonl sink); only the
# sidecar's own STORM escalation (>5 restarts within a 300s anchored-reset
# window, i.e. the 6th+ rapid restart) is error!. So the errcode alarm's real
# paging floor is a <=~60s flap cycle (span math, round-13: 6 restarts span 5
# gaps <= 300s). Flap cycles between ~60s and ~5 min (e.g. the
# ~90s stall-flap analog of the 2026-07-06 order-update incident cadence)
# produced ZERO ERROR lines and therefore ZERO pages, while the docs claimed
# ">=3 stall-restarts in 15 min pages".
#
# THE FIX: alarm on the counter instead. tv_feed_sidecar_stall_restart_total
# increments exactly ONCE per stall-restart (groww_sidecar_supervisor.rs,
# the restart branch) — warn!- and error!-level alike — so this pager sees
# EVERY restart. Same route as the order-update reconnect-storm alarm: the
# counter is NOT in the ratcheted 21-name EMF metric_selectors allowlist, but
# every 60s scrape still ships it as a plain-JSON event (with $.host) into
# /tickvault/<env>/metrics; a log metric filter extracts the per-scrape delta.
#
# COUNTER SHAPE: identical model + identical honest residual as
# order-update-reconnect-storm-alarm.tf — the CW agent's prometheus pipeline
# converts COUNTER samples to PER-SCRAPE DELTAS, so `Sum` over the window =
# restarts in the window. Not live-verified from this sandbox; if the field
# ever proved CUMULATIVE, Sum overcounts and pages too eagerly (fail-loud,
# never a silent miss) and this alarm must be reworked to DIFF(Maximum).
#
# FIRST-SAMPLE BASELINE (round-5 review fix, 2026-07-06 — supersedes the
# VOID round-4 supervisor-spawn registration): the delta pipeline DROPS each
# counter series' first observed sample as its baseline. The restart counter
# is therefore PRE-REGISTERED at 0 in main.rs immediately AFTER the metrics
# recorder installs (boot Step 2, right next to prewarm_dispatcher_counters —
# ratcheted by test_stall_restart_counter_is_preregistered_after_recorder_install,
# a source-order scan of main.rs) so the dropped first sample is the harmless
# 0 baseline. Without a post-install registration the series was BORN at the
# first restart (value 1) and the dropped sample WAS restart #1 — the first
# stall episode of every app session (the box restarts daily) ran at an
# effective threshold of 4, not the documented 3, and "sees every restart"
# was false. Round-4 had placed the registration at the TOP of the sidecar
# supervisor task — but that task is spawned BEFORE the recorder install, so
# its handle resolved to the no-op recorder and registered NOTHING (the
# order_update_connection.rs task-start analogy does not transfer: that task
# starts post-auth, long after the install). The post-install registration
# makes the series DENSE while the app runs (a 0-delta event per 60s scrape)
# — harmless to Sum, and the alarm no longer depends on metric sparseness.
# Honest residual: a stall-restart inside the pre-install boot window
# (supervisor spawn → Step 2) increments a no-op handle and is uncounted —
# physically implausible (needs sidecar launch + a recorded tick + >30s feed
# silence within the boot prefix).
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
# TUNING (honest, aligned-window semantics — round-10 correction of the
# straddle example): Sum >= 3 per ONE 900s period. CloudWatch evaluates
# aligned tumbling 15-min windows, not a sliding window: a burst that STOPS
# after straddling a window boundary at 2+1 pages NEVER (the earlier "pages
# one window later" example was wrong — neither aligned window ever reaches
# 3). The honest miss set is up to 4 restarts inside a SLIDING 15-min span
# (2/2 across the boundary) — without a page; only a flap that KEEPS GOING
# faster than the stated floor is guaranteed to land >=3 inside some aligned
# window and page. Detection floor (span math, round-12 — the earlier
# "~5 min" figure used average-rate math, 900/3 = 300s, where the per-window
# bound needs SPAN math: 3 restarts span 2 gaps, so 3 fit inside one 900s
# window only when the cycle is <= ~450s): three bands — cycles FASTER than
# ~5 min page promptly (every aligned window holds >=3); cycles in the
# ~5-7.5 min band page EVENTUALLY (a sustained flap's phase drifts across
# window boundaries until some window holds 3); only cycles SLOWER than
# ~7.5 min (>450s — 2 gaps no longer fit in 900s) can NEVER fit 3 inside one
# aligned window — the true never-page floor, stated residual; a single
# self-heal restart never pages (the runbook's own operator-action bound).
# Fast flaps (<=~60s cycle — span math, 6 restarts span 5 gaps <= the 300s
# window; the Rust detector's window is ANCHORED-reset, not sliding, so a
# burst straddling the anchor can defer the escalation by up to ~one extra
# 300s window) additionally trip the errcode-feed-stall-01 storm-escalation
# tripwire.
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
    # Deliberately no default_value (that knob emits datapoints for
    # NON-matching events — never wanted). Note: since the round-5
    # post-recorder-install registration in main.rs (boot Step 2) the
    # counter is DENSE while the app runs (a 0-delta event per 60s scrape),
    # so this metric bills during uptime hours; the 0s sum harmlessly into
    # the alarm window.
  }
}

resource "aws_cloudwatch_metric_alarm" "feed_stall_restarts" {
  alarm_name          = "tv-${var.environment}-feed-stall-restarts"
  alarm_description   = "FEED-STALL-01 flap: >=3 Groww sidecar stall-restarts within one 15-min window (Sum of the agent's per-scrape deltas of tv_feed_sidecar_stall_restart_total - the counter increments once per restart, warn!- and error!-level alike, and is pre-registered at 0 at boot right after the metrics recorder installs so the dropped first delta sample is the 0 baseline, not restart #1 - THIS pager sees every restart incl. the session's first; the errcode-feed-stall-01 alarm sees only the sidecar's own >5-per-5-min STORM escalation ERROR lines). A single self-heal restart never pages. Honest floor (span math): 3 restarts span 2 gaps, so cycles faster than ~5 min page promptly, the ~5-7.5 min band pages eventually via phase drift of a sustained flap, and only cycles slower than ~7.5 min (>450s - 2 gaps no longer fit one aligned 900s window) never page - stated residual. The provider keeps closing the socket - check credential/entitlement. Runbook: .claude/rules/project/feed-stall-watchdog-error-codes.md"
  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 3
  evaluation_periods  = 1
  metric_name         = "tv_feed_sidecar_stall_restart_total"
  namespace           = local.app_namespace
  # 900s aligned window: Sum of per-scrape deltas = restarts in the window
  # (see COUNTER SHAPE + TUNING header blocks; a burst that stops after
  # straddling the boundary at 2+1 never pages — up to 4 restarts in a
  # sliding 15-min span can go unpaged; only a flap sustaining faster than
  # the stated floor always pages — round-10).
  period             = 900
  statistic          = "Sum"
  dimensions         = local.app_dimensions # { host = "tickvault-prod" }
  treat_missing_data = "notBreaching"
  # Always-armed (no market-hours gate): the Rust watchdog only restarts when
  # should_restart_on_stall() sees market_open=true, so the counter cannot
  # increment outside 09:00-15:30 IST - there is no off-hours churn to gate.
  alarm_actions = local.app_alarm_actions
  # Expect ONE one-time green OK page the apply evening: new-alarm
  # INSUFFICIENT_DATA -> OK creation settling, not a recovery (round-8;
  # full rationale in error-code-alarms.tf's ok_actions comment).
  ok_actions = local.app_alarm_ok
}
