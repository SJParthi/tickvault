# App-level CloudWatch alarms — Z+ L2 VERIFY layer.
#
# The 5 alarms in alarms.tf cover infrastructure (EC2 status, CPU, EBS,
# network). These 12 alarms cover application signals: WebSocket health,
# QuestDB connectivity, token lifecycle, tick freshness, order rejection,
# aggregator liveness, backpressure, clock drift, composite SLO score.
#
# Charter authority: operator-charter-forever.md §C row "100% monitoring"
# + §F "Severity::Critical → Telegram". Without these the operator only
# learns about app failures by tailing /opt/tickvault/logs/errors.jsonl
# via SSM — which is reactive, not proactive.
#
# Data path:
#   tickvault Rust binary -> :9091/metrics (Prometheus exporter)
#                         -> CloudWatch agent prometheus scrape (60s interval)
#                         -> EMF processor filter (only the selected metrics)
#                         -> CloudWatch namespace "Tickvault/Prod"
#                         -> CloudWatch alarm
#                         -> SNS tv_alerts
#                         -> Telegram webhook Lambda (PR #781)
#                         -> Operator's phone
#
# Filter is configured in user-data.sh.tftpl::amazon-cloudwatch-agent.json
# emf_processor block — keeps custom-metric cost capped (24 selected
# metrics × ~$0.30 = $7.20/mo ≈ ₹612/mo absolute, $4.20/mo above the
# 10-free-metric tier — vs. ~₹4500/mo for an unfiltered 150-metric scrape;
# the 24-name count is pinned by cloudwatch_app_alarms_wiring.rs).
# 2026-07-06 groww feed-down alerting: +3 selected metrics
# (tv_groww_ws_active, tv_feed_last_tick_age_seconds,
# tv_feed_sidecar_stall_restart_total) ≈ +$0.90/mo, +2 alarms ≈ +$0.20/mo.
#
# Cost honesty:
#   - CloudWatch free tier: 10 alarms + 10 custom metrics + 5GB logs.
#   - Pre-PR (historical, original alarm PR):  6 alarms (alarms.tf=5,
#     telegram-webhook-lambda.tf=1). 0 custom metrics.
#   - Post-PR (historical): 18 alarms, 12 custom metrics.
#     Overage then: 8 alarms × $0.10 = $0.80/mo + 2 custom metrics × $0.30
#     = $0.60/mo ≈ ₹120/mo extra.
#   - Current (2026-07-06): 20 app alarms, 24 selected custom metrics.
#     Overage now: alarms ≈ $1.50/mo + metrics (24 − 10 free) × $0.30 =
#     $4.20/mo ⇒ ~$5.70/mo ≈ ₹485/mo total. Operator MUST acknowledge
#     before terraform apply.

locals {
  # All alarms publish to the same SNS topic. Single source of truth so
  # the operator can swap actions topic-wide in one place.
  app_alarm_actions = [aws_sns_topic.tv_alerts.arn]
  app_alarm_ok      = [aws_sns_topic.tv_alerts.arn]
  app_namespace     = "Tickvault/Prod"
  app_dimensions    = { host = "tickvault-prod" }
}

# ---------------------------------------------------------------------------
# 1. Main-feed WebSocket pool dead — every conn dropped, no live ticks
# treat_missing_data = notBreaching (was "breaching", 2026-06-02): the metric
# is emitted ONLY while the app runs, so on the scheduled weekday 16:30 IST
# stop / weekends / any deploy gap the metric goes missing and "breaching"
# held this alarm STUCK FIRING across the gap (operator saw it firing while the
# box was up + ticks flowing). App-death while the box is up is still caught by
# systemd Restart=always + the in-app tick-gap Telegram + the EC2 status alarm.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "ws_pool_all_dead" {
  alarm_name          = "tv-${var.environment}-ws-pool-all-dead"
  alarm_description   = "Main-feed WebSocket pool reports all conns dead — no live ticks reaching the pipeline. See disaster-recovery.md scenario 6."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 2
  metric_name         = "tv_websocket_pool_all_dead"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Maximum"
  threshold           = 0
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 2. WS pool partial degradation — some but not all conns failed
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "ws_failed_connections" {
  alarm_name          = "tv-${var.environment}-ws-failed-connections"
  alarm_description   = "One or more main-feed conns are in failed state. Pool may self-heal via watchdog; investigate if sustained."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 3
  metric_name         = "tv_websocket_failed_connections_count"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Maximum"
  threshold           = 0
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 3. Order-update WebSocket down — orders fly blind
# treat_missing_data = notBreaching (was "breaching", 2026-06-02): same
# stale-FIRING fix as ws_pool_all_dead — the metric is missing whenever the box
# is intentionally stopped (16:30 IST / weekends) or during a deploy gap, and
# "breaching" held this alarm stuck FIRING across that gap.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "order_update_ws_inactive" {
  alarm_name          = "tv-${var.environment}-order-update-ws-inactive"
  alarm_description   = "Order-update WebSocket is inactive. New orders will not receive fill confirmations via WS (postback fallback only)."
  comparison_operator = "LessThanThreshold"
  evaluation_periods  = 2
  metric_name         = "tv_order_update_ws_active"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Minimum"
  threshold           = 1
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 3b. Groww feed inactive (operator 2026-07-06 — Groww feed-down alerting).
# `tv_groww_ws_active` is the CONNECTED-level 0/1 gauge from the Groww
# bridge loop: 1 while the connected episode holds (socket connected +
# subscribed OR streaming, backed by FRESH sidecar status / tick evidence —
# a stale status file left by a killed or prior-day sidecar, or a replayed
# pre-disable tick backlog, can never read 1). Connected-level (not
# streaming-level) so the pre-open subscribed→first-tick window does not
# page every morning. REGISTRATION MATCHES THE ORDER-UPDATE PRECEDENT
# (2026-07-06 boot-grace fix): the gauge is registered only at the FIRST
# connected episode of the session — the Groww activation chain (CSV
# pull-until-success → sidecar launch incl. possible venv re-provision →
# SSM token → NATS connect → subscribe → notifier-slot fill) routinely
# exceeds any fixed N×60s grace on cold/slow boots, so an honest 0 from the
# first enabled wake produced a deterministic pre-open false ALARM/OK page
# pair; MISSING + notBreaching stays silent for a boot chain of ANY length.
# Once registered, 0/1 publishes every wake (and 0 on disabled wakes), so a
# mid-session outage, a FAILED disable→re-enable (sidecar auth reject), and
# a runtime disable all fire honestly. HONEST ENVELOPE: a sidecar DEAD AT
# BOOT (never connects at all) leaves the metric missing and this alarm
# silent — that class is paged by the sidecar supervisor's reject Telegrams
# + the FEED-STALL-01 watchdog, not by this alarm. A session where Groww is
# never enabled also stays missing, so a deliberate multi-day disable never
# pages daily.
# treat_missing_data = notBreaching: metric is missing whenever the box is
# intentionally stopped (16:30 IST / weekends), during a deploy gap, or for
# a Groww-disabled session — the same stuck-FIRING fix rationale as
# ws_pool_all_dead (2026-06-02).
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "groww_ws_inactive" {
  alarm_name          = "tv-${var.environment}-groww-ws-inactive"
  alarm_description   = "Groww feed lost its connection after being up this session. Groww prices are not flowing. If the feed was deliberately switched off, re-enable it from the feeds page; otherwise recovery is automatic — investigate if this stays firing. (A sidecar that never connected at boot is paged by the Groww reject alerts, not this alarm. A whole-process abort — e.g. a release-build panic — makes this metric go MISSING rather than 0, which stays silent here; that class is paged by the restart/boot Telegram chain.)"
  comparison_operator = "LessThanThreshold"
  evaluation_periods  = 2
  metric_name         = "tv_groww_ws_active"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Minimum"
  threshold           = 1
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 4. QuestDB disconnected — persistence backed up to rescue ring + spill
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "questdb_disconnected" {
  alarm_name          = "tv-${var.environment}-questdb-disconnected"
  alarm_description   = "QuestDB has been disconnected for > 30 seconds. Ticks buffer in the 100K rescue ring. See BOOT-01/BOOT-02 runbook."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "tv_questdb_disconnected_seconds"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Maximum"
  threshold           = 30
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 5. Many instruments silent — partial feed degradation
#
# RETUNED 2026-07-06 (silent-feed incident): Dhan degraded ALL day — 29-67 of
# 776 instruments silent EVERY minute — and the old threshold=100 never
# crossed, so zero pages. New tuning:
#   - threshold 40 (fires at >=41), PROVISIONAL. Round-3 correction
#     2026-07-08 (review finding 4): the first retune shipped 25, BELOW the
#     codebase's own documented healthy always-silent floor — main.rs's
#     2026-07-03 D2 note (live-verified) records "~33 illiquid/suspended
#     SIDs are ALWAYS silent" under the ~775-SID DailyUniverse (NT-15 boot
#     seeding makes every subscribed SID a tick-gap map key), and this gauge
#     is set from the SAME scan with NO always-silent exclusion — so 25
#     would have breached EVERY healthy in-session minute, latched within
#     ~10-12 min of the 09:20 gate-open, and paged daily (alert-fatigue
#     inversion of the zero-page incident). 40 clears the ~33 floor with
#     margin and aligns with the sibling SLO-degraded alarm (freshness
#     breaches <0.95 at >=39 silent of 776). HONEST COVERAGE LIMIT: the
#     incident's 29-40-silent minutes are indistinguishable from the
#     documented healthy floor by count alone — that marginal band is owned
#     by the SLO-degraded (>=39-silent freshness) + lag-p99 alarms; this
#     alarm pages on the sustained upper band (>40). PROVISIONAL + one
#     trading-week soak of the in-session gauge distribution mandated
#     (mirrors the boundary-storm 2000 soak); ratchet with a dated note.
#     ABSOLUTE, not %-of-universe: no universe-size denominator exists in CW
#     (tv_universe_size is kind-labeled and folds under the host-only EMF
#     dimensions), and the universe is envelope-locked [100, 1200], so
#     40 = 5.2% of today's ~776 / 3.3% at the 1200 cap. DATED REVISIT:
#     re-tune if the universe re-scopes toward the 1200 cap.
#   - M-of-N 10-of-12 at period=60/Maximum (1 datapoint per 60s agent scrape):
#     a value flapping 39/41/39 neither pages nor lets one clean scrape erase
#     9 minutes of evidence; a 1-3 min reconnect blip cannot reach 10 breaching
#     minutes.
#   - PRE-OPEN PIN (round-4 correction 2026-07-08, final-review findings
#     1/2/4): the producer (main.rs tick-gap scan) pins the gauge to 0 during
#     the NSE pre-open/auction window [09:00, 09:15) IST — the 09:08-09:15
#     matching/buffer freeze makes the boot-seeded ~775 SIDs legitimately
#     silent (values in the hundreds >> 40), and this alarm's 12-min lookback
#     (the LONGEST of any gated alarm) at the 09:20 gate-open would otherwise
#     hold ~7 guaranteed breaching pre-open datapoints (the gate Lambda's
#     forced-OK does NOT purge datapoints) -> only ~3 open-ramp minutes > 40
#     needed for a near-daily false page at ~09:21. Exact mirror of the SLO
#     tick_freshness pre-open pin; ratcheted by
#     test_tick_gap_silent_gauge_producer_pins_pre_open_to_zero. The mandated
#     soak week additionally checks the 09:20-09:25 transition band, not just
#     the steady-state distribution.
#   - MARKET-HOURS GATE (audit-findings Rule 3, MANDATORY): the gauge is set
#     only in-session (main.rs tick-gap scan gates on
#     is_within_trading_session_ist), so the LAST in-session value keeps being
#     re-scraped 15:30->16:30 IST — a stale post-close value must never page.
#     Actions OFF by default; the shared market-hours gate Lambda
#     (market-hours-liveness-alarm.tf) enables them 09:20-15:35 IST Mon-Fri.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "tick_gap_instruments_silent" {
  alarm_name          = "tv-${var.environment}-tick-gap-instruments-silent"
  alarm_description   = "> 40 instruments silent (tick-gap threshold, 30s default) sustained >=10 of 12 min in-market. Partial feed degradation — slow socket or Dhan segment outage. Threshold 40 is PROVISIONAL (2026-07-08: sits above the documented ~33 always-silent healthy floor — main.rs D2 note — with margin; observe one trading week of the in-session gauge distribution + ratchet with a dated note; the 29-40 marginal band is owned by the SLO-degraded + lag-p99 alarms). Actions gated to 09:20-15:35 IST Mon-Fri by the market-hours gate Lambda (the gauge is written in-session only, so its last value goes stale post-close). See WS-GAP-06 runbook."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 12
  datapoints_to_alarm = 10
  metric_name         = "tv_tick_gap_instruments_silent"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Maximum"
  threshold           = 40
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  # Actions OFF by default; the market-hours gate Lambda flips them ON
  # 09:20-15:35 IST Mon-Fri (market-hours-liveness-alarm.tf).
  actions_enabled = false
  alarm_actions   = local.app_alarm_actions
  ok_actions      = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 6. JWT token expiring within 4h — must force-renew before SEBI 24h cap
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "token_remaining_low" {
  alarm_name          = "tv-${var.environment}-token-remaining-low"
  alarm_description   = "Dhan JWT has < 4h remaining. Token manager should auto-refresh; alarm if it does not. See AUTH-GAP-03 runbook."
  comparison_operator = "LessThanThreshold"
  evaluation_periods  = 3
  metric_name         = "tv_token_remaining_seconds"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Minimum"
  threshold           = 14400 # 4 hours
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 7. Spill ring dropping ticks — backpressure breach, downstream slow
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "spill_dropped" {
  alarm_name          = "tv-${var.environment}-spill-dropped"
  alarm_description   = "Spill writer is dropping ticks — rescue ring + spill both saturated. DLQ NDJSON catches them. See STORAGE-GAP-03."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "tv_spill_dropped_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  threshold           = 0
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 8. DLQ catching ticks — last-resort sink in use
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "dlq_ticks" {
  alarm_name          = "tv-${var.environment}-dlq-ticks"
  alarm_description   = "Dead-letter queue is catching ticks — all upstream tiers (ring + spill) saturated. Investigate immediately."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "tv_dlq_ticks_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  threshold           = 0
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 8b. Tick PERMANENTLY dropped — the final zero-tick-loss breach
#
# `tv_ticks_dropped_total` increments ONLY when the rescue ring AND the disk
# spill AND the DLQ NDJSON all failed for a tick (tick_persistence.rs). This
# is strictly more severe than `tv_spill_dropped_total` / `tv_dlq_ticks_total`
# (which still recover the payload downstream): a non-zero value here means a
# tick was IRRECOVERABLY lost. It is the operator's #1 invariant breach, so it
# gets its own dedicated alarm even though the upstream tiers also alarm.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "ticks_dropped" {
  alarm_name          = "tv-${var.environment}-ticks-dropped"
  alarm_description   = "A tick was PERMANENTLY dropped — rescue ring + disk spill + DLQ NDJSON ALL failed. Irrecoverable zero-tick-loss breach (host OOM + disk full + dlq dir unwritable). Investigate immediately. MUST always be 0."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "tv_ticks_dropped_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  threshold           = 0
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 9. Aggregator producing zero seals during market hours
#
# MARKET-HOURS-GATED (2026-07-03, 5 AM false-SOS fix): zero seals is the
# CORRECT, by-design state whenever the market is closed but the app is
# running (early manual start, 15:30-16:30 IST post-close idle). The prior
# "operator learns to ignore off-hours fires" stance is exactly the
# pager-fatigue anti-pattern audit-findings Rule 3 (market-hours-aware)
# forbids. Actions are OFF by default; the shared market-hours gate Lambda
# (market-hours-liveness-alarm.tf) enables them 09:20-15:35 IST Mon-Fri
# only. In-market sensitivity is UNCHANGED (same metric/threshold/periods).
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "aggregator_no_seals" {
  alarm_name          = "tv-${var.environment}-aggregator-no-seals"
  alarm_description   = "Aggregator emitted zero seals in the last 5 minutes DURING MARKET HOURS. Actions gated to 09:20-15:35 IST Mon-Fri by the market-hours gate Lambda — off-hours zero-seal is by design and never pages."
  comparison_operator = "LessThanOrEqualToThreshold"
  evaluation_periods  = 1
  metric_name         = "tv_aggregator_seals_emitted_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  threshold           = 0
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  # Actions OFF by default; the market-hours gate Lambda flips them ON
  # 09:20-15:35 IST Mon-Fri (market-hours-liveness-alarm.tf).
  actions_enabled = false
  alarm_actions   = local.app_alarm_actions
  # No ok_actions — would page on every off-hour transition.
}

# ---------------------------------------------------------------------------
# 10. Order rejections — OMS or Dhan-side issue
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "orders_rejected" {
  alarm_name          = "tv-${var.environment}-orders-rejected"
  alarm_description   = "One or more orders rejected in the last 5 minutes. Could be DH-905 (bad input), DH-906 (order error), or risk-gate denial."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "tv_orders_rejected_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  threshold           = 0
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 11. Composite real-time guarantee score critical (< 0.80)
#
# MARKET-HOURS-GATED (2026-07-03, 5 AM false-SOS fix): the SLO score is
# LEGITIMATELY 0 outside market hours — its dimensions (tick freshness,
# aggregator health, ...) are market-gated in-app, so the metric is PRESENT
# with value 0.0 whenever the app runs off-hours. treat_missing_data does
# NOT help (the data is not missing). VERIFIED incident 2026-07-03 05:40
# IST: operator manually started the box pre-market and got an SOS Telegram
# ("2 datapoints 0.0 ... less than threshold (0.8)") for a healthy, idle
# system. Actions are OFF by default; the shared market-hours gate Lambda
# (market-hours-liveness-alarm.tf) enables them 09:20-15:35 IST Mon-Fri and
# resets state to OK on open so a stale off-hours ALARM never re-fires. A
# genuine in-market degradation (score < 0.80 for 2 min) pages exactly as
# before — in-market sensitivity is UNCHANGED.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "realtime_guarantee_critical" {
  alarm_name          = "tv-${var.environment}-realtime-guarantee-critical"
  alarm_description   = "Composite real-time guarantee score < 0.80 DURING MARKET HOURS — at least one dimension (WS, QuestDB, tick freshness, token, spill, Phase 2) is severely degraded. Actions gated to 09:20-15:35 IST Mon-Fri by the market-hours gate Lambda — the score is legitimately 0 off-hours (feeds idle by design) and never pages then. See SLO-02 runbook."
  comparison_operator = "LessThanThreshold"
  evaluation_periods  = 2
  metric_name         = "tv_realtime_guarantee_score"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Minimum"
  threshold           = 0.80
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  # Actions OFF by default; the market-hours gate Lambda flips them ON
  # 09:20-15:35 IST Mon-Fri (market-hours-liveness-alarm.tf).
  actions_enabled = false
  alarm_actions   = local.app_alarm_actions
  ok_actions      = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 12. Wall-clock skew > 1s — IST timestamp math at risk
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "clock_skew_high" {
  alarm_name          = "tv-${var.environment}-clock-skew-high"
  alarm_description   = "Wall-clock skew > 1s vs trusted source. IST timestamps may cross day boundaries. BOOT-03 fires at >2s. See BOOT-03 runbook."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 2
  metric_name         = "tv_clock_skew_seconds"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Maximum"
  threshold           = 1
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 13. Root volume filling — the "grow online when the alarm fires" trigger
#     (operator lock 2026-05-29: start 30 GB, grow on alarm).
#
# WHY THIS IS NEEDED: retention_days=90 (config/base.toml) means a ~90-day
# (3-month) data-pull NEVER ages a partition past the eviction window — so the
# disk only grows for the whole run, with zero auto-eviction. This alarm is the
# trip-wire. RESPONSE (no downtime, no data loss — gp3 grows online):
#   1. bump ebs_gp3_size_gb (e.g. 30 -> 50) in variables.tf + terraform apply
#   2. on the box (SSM): sudo growpart /dev/nvme0n1 1 && sudo xfs_growfs /
#   See docs/runbooks/may31-inplace-upgrade-and-access.md §2.1.
#
# Uses a CloudWatch Metrics Insights query so we do NOT have to pin the
# CWAgent disk dimensions (device/fstype vary); it selects by InstanceId +
# mount path only.
resource "aws_cloudwatch_metric_alarm" "disk_used_high" {
  alarm_name          = "tv-${var.environment}-disk-used-high"
  alarm_description   = "Root volume > 75% full. 90-day retention means a 3-month run never auto-evicts, so the disk only grows. Grow online (no downtime): bump ebs_gp3_size_gb 30->50 + apply, then on the box: sudo growpart /dev/nvme0n1 1 && sudo xfs_growfs /. See may31-inplace-upgrade-and-access.md §2.1."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  threshold           = 75
  treat_missing_data  = "notBreaching"
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok

  metric_query {
    id          = "disk_used"
    period      = 300
    return_data = true
    expression  = "SELECT MAX(disk_used_percent) FROM \"CWAgent\" WHERE InstanceId = '${aws_instance.tv_app.id}' AND path = '/'"
  }
}

# ---------------------------------------------------------------------------
# 14. WS frame dropped with NO WAL — the hard zero-tick-loss breach (G4)
# `tv_ws_frame_dropped_no_wal_total` increments ONLY when the live frame
# channel is full AND the WAL spill is not attached — the frame is genuinely
# lost (not just buffered). Any non-zero value is an irrecoverable tick loss.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "ws_frame_dropped_no_wal" {
  alarm_name          = "tv-${var.environment}-ws-frame-dropped-no-wal"
  alarm_description   = "WS live frame LOST — channel full AND no WAL attached. Irrecoverable tick loss. Investigate consumer liveness + re-enable WAL spill immediately."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "tv_ws_frame_dropped_no_wal_total"
  namespace           = local.app_namespace
  period              = 60
  statistic           = "Sum"
  threshold           = 0
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 15. WS reconnect-gap rate too high (G1) — sustained/excessive reconnect churn
# `tv_ws_reconnect_gap_seconds_total` accumulates the measured down-time of
# every reconnect. A rate-alarm (Sum over 5m) flags excessive churn WITHOUT
# paging on each routine 5-10s reconnect. Threshold 60s of cumulative gap per
# 5m window = the feed spent >20% of the window reconnecting.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "ws_reconnect_gap_high" {
  alarm_name          = "tv-${var.environment}-ws-reconnect-gap-high"
  alarm_description   = "WS reconnect churn high — cumulative reconnect-gap > 60s in 5m. Sub-30s reconnects drop ticks invisibly (no Dhan sequence number); investigate network/Dhan stability."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "tv_ws_reconnect_gap_seconds_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  threshold           = 60
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 16. Disk-health watcher respawn churn (G3) — the watcher that guards the
# "disk full + QuestDB down" gap died and was respawned by its supervisor.
# `tv_disk_watcher_respawn_total` increments once per watcher death. A
# rate-alarm (Sum over 5m) pages only on a FLAPPING watcher (a real bug),
# not on a benign one-off respawn at shutdown.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "disk_watcher_respawn" {
  alarm_name          = "tv-${var.environment}-disk-watcher-respawn"
  alarm_description   = "Spill disk-health watcher is flapping — respawned >0 times in 5m. Disk-free monitoring (the disk-full + QuestDB-down early warning) keeps running via the supervisor, but a repeating respawn means a real bug; inspect the DISK-WATCHER-01 panic backtrace."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "tv_disk_watcher_respawn_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  threshold           = 0
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 16b. Groww sidecar stall-restart storm (operator 2026-07-06 — Groww
# feed-down alerting). `tv_feed_sidecar_stall_restart_total` increments once
# per FEED-STALL-01 stall-kill (feed alive but silent across the whole
# subscribed universe during market hours). Per the FEED-STALL-01 runbook a
# SINGLE restart that recovers is a healthy self-heal (no page); a FLAPPING
# socket (reconnect→re-drop, the in-process storm escalation is >5 restarts
# per 300s) means the provider keeps closing the socket — operator must
# check the credential / entitlement.
# Shape honesty (corrected 2026-07-06): although the in-process metric is a
# cumulative session-scoped counter, the CloudWatch agent's Prometheus/EMF
# pipeline DELTA-CONVERTS counter-type metrics — every datapoint that
# reaches CloudWatch is the increase since the previous 60s scrape (the
# first sample is dropped), NEVER the cumulative count. A Maximum-statistic
# threshold of 3 could therefore never fire on the documented "3+ stall
# restarts spread across the session" condition (each datapoint is ~1), and
# a sustained storm throttled by the in-process backoff ladder to >=60s
# between kills would read 0-1 per scrape and un-fire mid-storm. Sum over a
# 1-hour window counts the restarts in that window under delta semantics:
# Sum >= 3 / 3600s fires on 3+ stall restarts within an hour (a single
# self-healing restart stays silent per the FEED-STALL-01 runbook) and
# returns to OK an hour after the storm genuinely ends.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "groww_stall_restart_storm" {
  alarm_name          = "tv-${var.environment}-groww-stall-restart-storm"
  alarm_description   = "Groww feed keeps stalling — 3+ silent-feed restarts within the last hour (flapping socket = provider-side reject). The system keeps reconnecting automatically; check the Groww credential/entitlement if this fires. See FEED-STALL-01."
  comparison_operator = "GreaterThanOrEqualToThreshold"
  evaluation_periods  = 1
  metric_name         = "tv_feed_sidecar_stall_restart_total"
  namespace           = local.app_namespace
  period              = 3600
  statistic           = "Sum"
  threshold           = 3
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# Post-close tick anomaly — Dhan stamped a tick at/after 15:30:00 IST.
# Hot-path-safe equivalent of the retired LastTickAfterBoundary Telegram
# variant (2026-06-12): the per-tick check + tv_late_tick_after_boundary_total
# counter live in tick_processor.rs; this alarm pages on the counter instead of
# threading a notifier into the hot path. SHOULD be zero — Dhan's session ends
# 15:30:00.000 exclusive. The box restarts daily (08:30 IST) so the counter is
# fresh each session; Maximum > 0 = a post-close tick happened today. Low
# priority / data-quality (correlate Dhan-side ingestion lag or local clock skew).
# treat_missing_data = notBreaching: metric is absent when the box is stopped
# (16:30 IST / weekends) — must not hold the alarm FIRING across that gap.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "late_tick_after_boundary" {
  alarm_name          = "tv-${var.environment}-late-tick-after-boundary"
  alarm_description   = "Dhan stamped a tick at/after 15:30:00 IST (post-market close). Informational data-quality signal — should be zero; correlate Dhan-side ingestion lag or local clock skew."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "tv_late_tick_after_boundary_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Maximum"
  threshold           = 0
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 17. Host memory > 80% — the "time to upgrade" capacity signal.
# Auto-DETECT, not auto-spend: this alarm tells the operator WHEN the r8g.large
# 16 GiB box is running hot (e.g. both feeds at ~2K SIDs), so they can decide to
# run scripts/aws-upgrade-instance.sh to a bigger type AFTER the dated-quote +
# 4-file lock flip — it never resizes anything itself. Mirrors disk_used_high:
# a CloudWatch Metrics Insights query so we do NOT pin CWAgent mem dimensions.
# CWAgent already publishes mem_used_percent (user-data.sh.tftpl metrics block).
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "mem_used_high" {
  alarm_name          = "tv-${var.environment}-mem-used-high"
  alarm_description   = "Host memory > 80% on r8g.large (16 GiB). Capacity signal — time to consider an instance upgrade. Run scripts/aws-upgrade-instance.sh --to <bigger-type> --ebs-size <GB> --qdb-mem <N>g AFTER the dated-quote + 4-file lock flip (daily-universe-scope-expansion-2026-05-27.md §7 Mechanical Rule 1). Auto-detect only — never auto-upgrades. See docs/runbooks/instance-upgrade.md."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 3
  threshold           = 80
  treat_missing_data  = "notBreaching"
  alarm_actions       = local.app_alarm_actions
  ok_actions          = local.app_alarm_ok

  metric_query {
    id          = "mem_used"
    period      = 300
    return_data = true
    expression  = "SELECT MAX(mem_used_percent) FROM \"CWAgent\" WHERE InstanceId = '${aws_instance.tv_app.id}'"
  }
}

# ---------------------------------------------------------------------------
# Output — operator-facing reminder + alarm list
# ---------------------------------------------------------------------------

output "app_cloudwatch_alarms" {
  description = "21 application-level alarms in THIS file (19 Prometheus-via-CW-agent incl. #1437's 2 groww alarms + 1 disk-used + 1 mem-used Metrics-Insights); 3 more silent-feed alarms live in silent-feed-alarms.tf (2026-07-06 incident hardening). tick-gap-instruments-silent was RETUNED 2026-07-06 (threshold 100 -> 40 PROVISIONAL, 10-of-12 min, market-hours-gated). Cost note (2026-07-06 groww feed-down alerting adds 2 alarms + 3 selected metrics; silent-feed hardening adds 3 alarms + 4 selected series): overage above the 10 free-tier alarms ≈ $1.80/mo + 28 selected custom-metric series ≈ $5.40/mo overage above the 10 free-tier metrics (≈ $8.40/mo absolute at ~$0.30 each; EMF name count pinned by cloudwatch_app_alarms_wiring.rs) ≈ $7.20/mo ≈ ₹610/mo total — well inside the $55 budget cap."
  value = [
    aws_cloudwatch_metric_alarm.disk_used_high.alarm_name,
    aws_cloudwatch_metric_alarm.mem_used_high.alarm_name,
    aws_cloudwatch_metric_alarm.ws_pool_all_dead.alarm_name,
    aws_cloudwatch_metric_alarm.ws_failed_connections.alarm_name,
    aws_cloudwatch_metric_alarm.order_update_ws_inactive.alarm_name,
    aws_cloudwatch_metric_alarm.groww_ws_inactive.alarm_name,
    aws_cloudwatch_metric_alarm.groww_stall_restart_storm.alarm_name,
    aws_cloudwatch_metric_alarm.questdb_disconnected.alarm_name,
    aws_cloudwatch_metric_alarm.tick_gap_instruments_silent.alarm_name,
    aws_cloudwatch_metric_alarm.token_remaining_low.alarm_name,
    aws_cloudwatch_metric_alarm.spill_dropped.alarm_name,
    aws_cloudwatch_metric_alarm.dlq_ticks.alarm_name,
    aws_cloudwatch_metric_alarm.aggregator_no_seals.alarm_name,
    aws_cloudwatch_metric_alarm.orders_rejected.alarm_name,
    aws_cloudwatch_metric_alarm.realtime_guarantee_critical.alarm_name,
    aws_cloudwatch_metric_alarm.clock_skew_high.alarm_name,
    aws_cloudwatch_metric_alarm.ws_frame_dropped_no_wal.alarm_name,
    aws_cloudwatch_metric_alarm.ws_reconnect_gap_high.alarm_name,
    aws_cloudwatch_metric_alarm.disk_watcher_respawn.alarm_name,
    aws_cloudwatch_metric_alarm.late_tick_after_boundary.alarm_name,
  ]
}
