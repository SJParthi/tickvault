# =============================================================================
# Silent-feed degradation alarms — 2026-07-06 incident hardening.
# =============================================================================
# THE INCIDENT (2026-07-06): the Dhan feed degraded ALL day with FOUR
# independent signals and ZERO pages:
#   1. exchange->receive lag p99 46s / max 199s (no metric existed for it)
#   2. 29-67 of 776 instruments silent EVERY minute (old tick-gap alarm
#      threshold 100 never crossed — retuned to 40 PROVISIONAL in
#      app-alarms.tf #5; sits above the documented ~33 always-silent
#      healthy floor, round-3 correction 2026-07-08)
#   3. 125 SLO score crossings in the 0.94-0.97 band (in-app SLO-02 is
#      Telegram-suppressed/log-only since 2026-05-11; the CW
#      realtime-guarantee alarm only fires < 0.80 — a dead band this file's
#      degraded alarm closes)
#   4. BOUNDARY-01 catch-up sealing 9k-11.5k seals/10min (never exported to
#      CW at all — per-feed export added 2026-07-06 to cloudwatch-agent.json
#      + user-data.sh.tftpl)
#
# All 3 alarms here follow the house market-hours-gate pattern
# (audit-findings Rule 3): actions_enabled=false + appended to the window-gate
# Lambda ALARM_NAMES (market-hours-liveness-alarm.tf, 09:20-15:35 IST Mon-Fri).
# Edge-triggering is CloudWatch-native (SNS fires on OK->ALARM transition only
# — audit-findings Rule 4); treat_missing_data=notBreaching everywhere (the
# box is intentionally stopped nightly at 16:30 IST + weekends).
#
# Namespace / dimensions / SNS reuse the module-global locals declared in
# app-alarms.tf (local.app_namespace / local.app_dimensions /
# local.app_alarm_actions / local.app_alarm_ok).
#
# COST (dated 2026-07-06): +3 metric NAMES = 4 new CloudWatch custom-metric
# series (~$1.20/mo: 2x tv_boundary_catchup_total under [host,feed]
# (feed=dhan|groww), 1x tv_dhan_exchange_lag_p99_seconds, 1x
# tv_dhan_lag_samples_excluded_total) + 3 alarms (~$0.30/mo) ≈ $1.50/mo —
# inside the $35/mo pre-GST budget alarm ceiling. aws-budget.md carries the
# matching dated note.
# =============================================================================

# ---------------------------------------------------------------------------
# S1. Composite real-time guarantee score degraded (Medium) — sustained < 0.95
#
# Closes the exact 0.80-0.95 dead band of the 2026-07-06 incident: the in-app
# SLO-02 emission is Telegram-suppressed (log-only) since 2026-05-11, and the
# sibling < 0.80 alarm (app-alarms.tf #11, UNTOUCHED) is mathematically
# unreachable for feed degradation — tick_freshness alone bottoms out at
# 1 - 67/776 ≈ 0.914.
#
# Threshold 0.95 == SLO_WARN_THRESHOLD (crates/core/src/instrument/
# slo_score.rs); score == 0.95 is Healthy and correctly non-breaching under
# LessThanThreshold. statistic=Minimum: the publisher runs every 10s but the
# CW agent scrapes once per 60s -> 1 datapoint/period today (Min == Max);
# Minimum is future-proof and mirrors the sibling < 0.80 alarm.
#
# M-of-N 9-of-15 (round-2 correction 2026-07-07 — the first cut shipped
# 12-of-15 justified by "the incident's tick_freshness 0.914-0.963 satisfies
# it"; that claim was WRONG): with universe 776, tick_freshness breaches
# < 0.95 only at >= 39 silent instruments (1 - 38/776 ≈ 0.9510 samples
# Healthy), while the incident band was 29-67 silent EVERY minute — the band
# STRADDLES the threshold, and the CW agent point-samples the oscillating
# 10s-cadence gauge ONCE per 60s period (125 crossings that day). Any
# 15-min window with > 3 minutes sampled in the 29-38-silent sub-band could
# never accumulate 12 breaching datapoints — 12/15 could reproduce the
# exact zero-page miss on the marginal band (Rule-11 false-OK). 9/15 (60%)
# latches whenever >= 60% of sampled minutes breach, while a 2-3 min
# single-reconnect dip (<= 3 breaching points) still cannot reach 9. A
# strict 15/15 remains rejected (would likely never latch on an oscillating
# signal). Honest coverage split (round-3 correction 2026-07-08, review
# finding 4): the 26-38-silent sub-band is NOT count-detectable — it
# overlaps the documented ~33 always-silent healthy floor (main.rs D2 note,
# 2026-07-03), which is also why the tick-gap alarm was re-raised from 25
# to 40 PROVISIONAL; the lag-p99 alarm owns that marginal band — this
# alarm's UNIQUE coverage is >= 39-silent freshness degradation plus every
# OTHER-dimension partial degradation (WS pool fraction, QuestDB, token,
# spill, Phase 2) inside the 0.80-0.95 dead band.
# M=9 is threshold-adjacent and PROVISIONAL: observe one trading week and
# ratchet with a dated note if healthy-day windows approach 9 breaching
# minutes.
#
# Metric is ALREADY CW-exported (cloudwatch-agent.json emf allowlist + the
# log-metric-filter fallback in metrics-log-metric-filters.tf): ZERO Rust,
# ZERO allowlist change for this alarm. The publisher runs 24/7 with
# off-hours dimension dips, hence the market-hours action gate.
# ok_actions gives the falling-edge recovery note (Rule 4).
#
# Medium-tier VISUAL rendering is a flagged follow-up: the telegram-webhook
# Lambda handler.py stamps the same emoji on any ALARM today.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "realtime_guarantee_degraded" {
  alarm_name          = "tv-${var.environment}-realtime-guarantee-degraded"
  alarm_description   = "Composite real-time guarantee score degraded (Medium) — < 0.95 for >= 9 of the last 15 in-market minutes (M=9 PROVISIONAL, 2026-07-07: freshness-only breach needs >= 39 of 776 silent, so the incident's 29-38-silent minutes sample Healthy — a stricter latch could miss the marginal band; observe one trading week + ratchet with a dated note). At least one dimension (WS, QuestDB, tick freshness, token, spill, Phase 2) is persistently degraded but above the 0.80 sibling alarm's floor (the exact dead band of the 2026-07-06 all-day silent-feed incident). Actions gated to 09:20-15:35 IST Mon-Fri by the market-hours gate Lambda. See SLO-02 runbook."
  comparison_operator = "LessThanThreshold"
  evaluation_periods  = 15
  datapoints_to_alarm = 9
  metric_name         = "tv_realtime_guarantee_score"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Minimum"
  threshold           = 0.95
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  # Actions OFF by default; the market-hours gate Lambda flips them ON
  # 09:20-15:35 IST Mon-Fri (market-hours-liveness-alarm.tf).
  actions_enabled = false
  alarm_actions   = local.app_alarm_actions
  ok_actions      = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# S2. BOUNDARY-01 catch-up seal STORM on the Dhan feed
#
# The incident ran 9k-11.5k catch-up seals/10min all day — a stalled/lagging
# Dhan tick stream forcing the watermark catch-up sealer to do the
# aggregator's work — with zero CW visibility.
#
# PER-FEED DIMENSION IS RULE-11-MANDATORY: Groww's 60s catch-up lateness
# margin makes catch-up sealing its ROUTINE steady-state path for quiet SIDs
# (~767 SIDs x 21 TFs) — a host-only folded series would either mask a Dhan
# storm under the Groww baseline or page on healthy Groww behaviour. Hence
# the explicit { host, feed=dhan } dimensions map (NOT local.app_dimensions)
# + the second emf_processor metric_declaration with dimensions
# [["host","feed"]] in cloudwatch-agent.json / user-data.sh.tftpl.
#
# Rule 12 semantics: the CW agent ships prometheus counters as per-scrape
# DELTAS, so Sum over 300s IS increase(5m) — house precedent:
# tv_disk_watcher_respawn_total (app-alarms.tf #16, Sum/5m).
#
# 10-min sustained (2 x 5min): a bounded one-shot restart catch-up wave
# (<= ~25K seals) drains inside one 5-min period -> absorbed; the 08:30 IST
# boot wave sits outside the 09:20-15:35 gate window anyway. Incident rate
# ~4.5-5.75k/5min = 2.25-2.9x threshold -> pages at minute 10.
#
# HONESTY — threshold 2000 is PROVISIONAL: the healthy Dhan per-feed Sum(5m)
# floor is UNMEASURED (the metric was never exported before 2026-07-06); the
# incident rate gives 4-10x headroom over any plausible healthy floor.
# MANDATED FOLLOW-UP: observe the exported per-feed Sum(5m) distribution for
# one trading week and ratchet the threshold with a dated note if the healthy
# floor approaches 2000.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "boundary_catchup_storm_dhan" {
  alarm_name          = "tv-${var.environment}-boundary-catchup-storm-dhan"
  alarm_description   = "BOUNDARY-01 catch-up seal storm on feed=dhan — > 2000 catch-up-sealed candles per 5 min sustained for 10 min in-market (PROVISIONAL threshold, 2026-07-06: healthy floor unmeasured; observe one trading week + ratchet with a dated note). The Dhan tick stream is stalling/lagging enough that the watermark catch-up sealer is doing the aggregator's work (2026-07-06 incident: 9k-11.5k/10min all day). Actions gated to 09:20-15:35 IST Mon-Fri. See BOUNDARY-01 runbook (wave-6-error-codes.md)."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 2
  datapoints_to_alarm = 2
  metric_name         = "tv_boundary_catchup_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  threshold           = 2000
  treat_missing_data  = "notBreaching"
  # Explicit per-feed dimensions — this alarm watches the Dhan series ONLY
  # (the [host,feed] EMF declaration publishes dhan + groww separately).
  dimensions = {
    host = "tickvault-prod"
    feed = "dhan"
  }
  # Actions OFF by default; the market-hours gate Lambda flips them ON
  # 09:20-15:35 IST Mon-Fri (market-hours-liveness-alarm.tf).
  actions_enabled = false
  alarm_actions   = local.app_alarm_actions
  ok_actions      = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# S3. Dhan exchange->receive lag p99 high
#
# The incident's PRIMARY signal: exchange-timestamp -> receive-instant lag
# p99 46s / max 199s all day, with no metric measuring it
# (tv_wire_to_done_duration_ns is receive->done only). The
# tv_dhan_exchange_lag_p99_seconds gauge is published by the supervised
# feed-lag monitor (crates/core/src/pipeline/feed_lag_monitor.rs), spawned
# from BOTH boot arms (fast crash-recovery + start_dhan_lane — round-1 fix
# 2026-07-07; lane-only wiring left this alarm silently notBreaching for
# the whole session after any mid-market crash restart): trailing 60s
# window, recomputed every 10s, in-session only (Rule 3 — the regular
# [09:00,15:30) IST window plus the Muhurat [18:00,19:30) window when
# active; NOTE the action gate below stays 09:20-15:35, so Muhurat
# publication is gauge-visibility only, not paging), >= 50 samples
# or nothing published (Rule 11 — an empty window must never read as
# "perfect lag"; feed-dead is owned by the silent-instruments + WS alarms
# via notBreaching). Unlabeled, dhan-only NAME — sidesteps the host-only EMF
# dimension label-folding trap; a future Groww gauge gets its own name.
#
# QUANTIZATION HONESTY: Dhan LTT is u32 whole IST SECONDS (ticker.rs bytes
# 12-15 / quote.rs bytes 14-17) -> >= 1s measurement floor; healthy p99
# reads ~1-2s and can never read 0; sub-second wire lag is UNMEASURABLE for
# feed=dhan. Threshold 10s sits 10x above that floor.
#
# Strict 10-of-10 is safe HERE (unlike the flap-prone signals above): the
# metric is itself a trailing-60s p99 recomputed every 10s, so a one-burst
# transient decays out of the window within ~60s and cannot hold 10
# consecutive breaching minutes. The incident's all-day p99 46s pages at
# minute 10.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "dhan_exchange_lag_p99_high" {
  alarm_name          = "tv-${var.environment}-dhan-exchange-lag-p99-high"
  alarm_description   = "Dhan exchange->receive lag p99 > 10s for 10 consecutive in-market minutes (2026-07-06 incident signal: p99 46s / max 199s all day). Trailing-60s p99; boot-time WAL-replay rows excluded via the two-condition discriminator (>=60s receipt-capture dwell AND pre-boot capture instant — live rows delayed >60s in-pipeline by a consumer stall are KEPT, round-2 fix 2026-07-07); >= 50 samples required. NOTE: Dhan LTT is whole IST seconds -> >= 1s measurement floor (healthy p99 ~1-2s, never 0); the 10s threshold sits 10x above the floor. Actions gated to 09:20-15:35 IST Mon-Fri. See WS-GAP-06 runbook + feed_lag_monitor module docs."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 10
  datapoints_to_alarm = 10
  metric_name         = "tv_dhan_exchange_lag_p99_seconds"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Maximum"
  threshold           = 10
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  # Actions OFF by default; the market-hours gate Lambda flips them ON
  # 09:20-15:35 IST Mon-Fri (market-hours-liveness-alarm.tf).
  actions_enabled = false
  alarm_actions   = local.app_alarm_actions
  ok_actions      = local.app_alarm_ok
}

output "silent_feed_cloudwatch_alarms" {
  description = "3 silent-feed degradation alarms (2026-07-06 incident hardening): SLO 0.80-0.95 dead-band (9-of-15 min, PROVISIONAL), per-feed BOUNDARY-01 catch-up storm (dhan, PROVISIONAL 2000/5m x2), Dhan exchange->receive lag p99 > 10s x10min. All market-hours-gated via the window-gate Lambda; the retuned tick-gap alarm (threshold 100 -> 40 PROVISIONAL, 10-of-12) stays in app-alarms.tf."
  value = [
    aws_cloudwatch_metric_alarm.realtime_guarantee_degraded.alarm_name,
    aws_cloudwatch_metric_alarm.boundary_catchup_storm_dhan.alarm_name,
    aws_cloudwatch_metric_alarm.dhan_exchange_lag_p99_high.alarm_name,
  ]
}
