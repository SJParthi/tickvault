# App-level CloudWatch alarms — Z+ L2 VERIFY layer.
#
# The 5 alarms in alarms.tf cover infrastructure (EC2 status, CPU, EBS,
# network). The 20 alarms in THIS file (21 until the 2026-07-14 Dhan noise
# lock retired order_update_ws_inactive) cover application signals: WebSocket
# health, QuestDB connectivity, token lifecycle, tick freshness, order
# rejection, aggregator liveness, backpressure, clock drift, composite SLO
# score. 4 more silent-feed alarms live in silent-feed-alarms.tf
# (2026-07-06 incident hardening + scoreboard PR-C S4).
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
# emf_processor block — keeps custom-metric cost capped (29 selected
# series × ~$0.30 ≈ $8.70/mo absolute, $5.70/mo above the 10-free-metric
# tier — vs. ~₹4500/mo for an unfiltered 150-metric scrape; the 27-name
# MAIN EMF list is pinned by cloudwatch_app_alarms_wiring.rs, and the two
# [host,feed] boundary-catchup declarations bring the series count to 29).
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
#   - Current (2026-07-14, Dhan noise lock: order_update_ws_inactive
#     retired, ~-$0.10/mo): 20 app alarms
#     in THIS file + 4 in silent-feed-alarms.tf; 29 selected custom-metric
#     series (27 main EMF names + the 2 [host,feed] boundary-catchup
#     declarations). Overage now: alarms ≈ $1.90/mo + metrics (29 − 10
#     free) × $0.30 = $5.70/mo ⇒ ~$7.60/mo ≈ ₹650/mo total (matches the
#     app_cloudwatch_alarms output below + aws-budget.md's 2026-07-06
#     note). Operator MUST acknowledge before terraform apply.
#   - +3 alarms (order-side-alarms.tf, 2026-07-14): orders-placed-storm
#     (armed) + daily-loss-breach (armed, dormant-silent in dry-run) +
#     order-fill-lag-high (disarmed) ≈ +$0.30/mo, +1 derived metric
#     series (tv_orders_placed_delta_total) ≈ +$0.30/mo; the 2 new EMF
#     names (tv_daily_pnl, tv_order_fill_lag_seconds) are DORMANT ($0
#     until cluster A / Phase-1 emits). THIS file's alarm RESOURCE count
#     stays 21 (the 3 new alarms live standalone in order-side-alarms.tf;
#     the "twenty_three" wiring test counts METRIC NAMES across
#     app-alarms.tf + silent-feed-alarms.tf — a different axis, no
#     conflict). See aws-budget.md COST NOTE 2026-07-14.

locals {
  # All alarms publish to the same SNS topic. Single source of truth so
  # the operator can swap actions topic-wide in one place.
  app_alarm_actions = [aws_sns_topic.tv_alerts.arn]
  app_alarm_ok      = [aws_sns_topic.tv_alerts.arn]
  app_namespace     = "Tickvault/Prod"
  app_dimensions    = { host = "tickvault-prod" }
}

# ---------------------------------------------------------------------------
# RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
# retirement directive per websocket-connection-scope-lock.md "2026-07-13
# Amendment" §B): the alarms `ws_pool_all_dead` (tv_websocket_pool_all_dead)
# + `ws_failed_connections` (tv_websocket_failed_connections_count) watched
# the deleted main-feed pool watchdog's gauges — no emit site exists, so the
# alarms could never fire again (permanent missing-data). Removed with their
# window-gate entries. Groww feed liveness is owned by groww_ws_inactive +
# groww_stall_restart_storm + the market-hours liveness alarm (re-pointed to
# the Groww lag gauge in Phase A).
# ---------------------------------------------------------------------------
# ---------------------------------------------------------------------------
# 3. Order-update WebSocket down — RETIRED 2026-07-14 (operator Dhan noise
# lock, dhan-rest-only-noise-lock-2026-07-14.md): the order-update WS spawn
# itself is retired (no process opens the socket until live trading), and
# the alarm was already blind on dhan-off boots — tv_order_update_ws_active
# was written ONLY by the dead lane spawn sites (missing-data-silent both
# ways). Deleted together with order-update-reconnect-storm-alarm.tf.
# ---------------------------------------------------------------------------

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
# 5. RETIRED in PR-C3 (2026-07-14): tick-gap-instruments-silent
#
# The `tv-<env>-tick-gap-instruments-silent` alarm was DELETED with its
# gauge producer — the per-SID tick-gap detector retired per the operator's
# 2026-07-13 Q4-ii ruling (websocket-connection-scope-lock.md "2026-07-13
# Amendment" §B item 4: the detector was fed only by the retired Dhan WS
# lane, so `tv_tick_gap_instruments_silent` would never be written again —
# keeping the alarm would orphan a dead monitor). Per-SID silence
# visibility is now the scoreboard presence/coverage columns (15:45 IST);
# FEED-level stall detection is FEED-STALL-01 (feed-stall-restart-alarm.tf).
# The 2026-07-06/07-08 retune history (threshold 100 -> 40 PROVISIONAL,
# pre-open pin, ~33 always-silent floor) is retained in git history.
# ---------------------------------------------------------------------------


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
# REJECTION-CLASS SPLIT (C4, 2026-07-14 hostile review): there are TWO
# DISJOINT rejection classes. (a) Place-time API errors (DH-905/DH-906
# at the place_order Err arm) — these fire the OrderRejected Telegram +
# the `rejected` order_audit row AND (since the C4 fix) increment
# tv_orders_rejected_total, so they page this alarm. (b) WS-reported
# REJECTED transitions (process_order_update — the order-update WS is
# functional-dormant today) — these increment the counter/alarm but
# produce NO Telegram/audit row (the fire_alert at that transition is a
# Phase-1 follow-up). The alarm and the Telegram are NOT one signal chain.
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
  # 2026-07-14 cluster-C order-side: ok_actions STRIPPED. The rejected
  # count returning to 0 is not an all-clear (the rejected orders exist;
  # the rejection cause may persist) — the auto-OK paged a Rule-11 false
  # recovery on every episode aging out. The counter is now also
  # pre-registered at 0 in main.rs (first-sample-baseline lesson) so a
  # single-rejection session (place-time class — the counter emit at the
  # place_order Err arm, C4) actually pages — see
  # deploy/aws/terraform/order-side-alarms.tf +
  # crates/app/tests/order_side_paging_wiring_guard.rs.
  ok_actions = []
}

# ---------------------------------------------------------------------------
# RETIRED (PR-C2, 2026-07-13): `realtime_guarantee_critical`
# (tv_realtime_guarantee_score < 0.80) — the SLO evaluator/publisher was
# deleted per the operator PARK ruling (wave-3-d-error-codes.md banner), so
# the score is never published again. Removed with its window-gate entry;
# the market-hours liveness alarm was re-pointed to the Groww lag gauge in
# Phase A.
# ---------------------------------------------------------------------------
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
#   1. grow the LIVE volume online: scripts/aws-upgrade-instance.sh --ebs-size N
#      (aws ec2 modify-volume; terraform apply does NOT touch the live volume —
#      volume_size is in lifecycle.ignore_changes) + bump ebs_gp3_size_gb in
#      variables.tf so fresh-provision intent matches (done 30 -> 50 on
#      2026-07-13 when the fs hit 82%).
#   2. on the box (SSM): sudo growpart /dev/nvme0n1 1 && sudo xfs_growfs /
#      (or wait for the next daily boot — AL2023 cloud-init growpart/resizefs
#      run every boot and auto-expand the fs).
#   See docs/runbooks/may31-inplace-upgrade-and-access.md §2.1.
#
# Uses a CloudWatch Metrics Insights query so we do NOT have to pin the
# CWAgent disk dimensions (device/fstype vary); it selects by InstanceId +
# mount path only.
resource "aws_cloudwatch_metric_alarm" "disk_used_high" {
  alarm_name          = "tv-${var.environment}-disk-used-high"
  alarm_description   = "Root volume > 75% full. 90-day retention means a 3-month run never auto-evicts, so the disk only grows. Grow online (no downtime): scripts/aws-upgrade-instance.sh --ebs-size N (modify-volume; grown 30->50 on 2026-07-13), then on the box: sudo growpart /dev/nvme0n1 1 && sudo xfs_growfs / (or the next daily boot's cloud-init growpart/resizefs). See may31-inplace-upgrade-and-access.md §2.1."
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
# RETIRED (PR-C2, 2026-07-13): `ws_frame_dropped_no_wal`
# (tv_ws_frame_dropped_no_wal_total) + `ws_reconnect_gap_high`
# (tv_ws_reconnect_gap_seconds_total) — both counters were emitted only by
# the deleted main-feed `connection.rs` read loop. The surviving durable
# floor is the WAL writer's own WS-SPILL-01/02 codes + the ws_event_audit
# chain; order-update/Groww reconnects keep their own pagers.
# ---------------------------------------------------------------------------
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
# Auto-DETECT, not auto-spend: this alarm tells the operator WHEN the box is
# running hot, so they can decide to run scripts/aws-upgrade-instance.sh to a
# bigger type AFTER the dated-quote + 4-file lock flip — it never resizes
# anything itself. 2026-07-15 note (Quote 8 downsize): the box is now
# t4g.medium 4 GiB (was r8g.large 16 GiB) — this signal is MORE load-bearing
# post-downsize (§7 Rule 2 headroom is ~0.9–1.7 GB budgeted, Assumed until
# live-measured; t4g.large 8 GiB is the rip-cord). Mirrors disk_used_high:
# a CloudWatch Metrics Insights query so we do NOT pin CWAgent mem dimensions.
# CWAgent already publishes mem_used_percent (user-data.sh.tftpl metrics block).
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "mem_used_high" {
  alarm_name          = "tv-${var.environment}-mem-used-high"
  alarm_description   = "Host memory > 80% on t4g.medium (4 GiB — 2026-07-15 downsize lock). Capacity signal — time to consider an instance upgrade (t4g.large 8 GiB is the rip-cord). Run scripts/aws-upgrade-instance.sh --to <bigger-type> --ebs-size <GB> --qdb-mem <N>g AFTER the dated-quote + 4-file lock flip (daily-universe-scope-expansion-2026-05-27.md §7 Mechanical Rule 1). Auto-detect only — never auto-upgrades. See docs/runbooks/instance-upgrade.md."
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
  description = "14 application-level alarms in THIS file (12 Prometheus-via-CW-agent + 1 disk-used + 1 mem-used Metrics-Insights; PR-C2 2026-07-13 retired 5 Dhan-lane alarms — ws-pool-all-dead, ws-failed-connections, realtime-guarantee-critical, ws-frame-dropped-no-wal, ws-reconnect-gap-high — their emitters died with the Dhan live-WS lane; order-update-ws-inactive RETIRED 2026-07-14 per dhan-rest-only-noise-lock-2026-07-14.md; tick-gap-instruments-silent RETIRED in PR-C3 2026-07-14 — its gauge producer, the per-SID tick-gap detector, was deleted per operator Q4-ii 2026-07-13, so the gauge is never written again); 3 more silent-feed alarms live in silent-feed-alarms.tf (realtime-guarantee-degraded also retired PR-C2). Cost note: the PR-C2 retirement REMOVES 6 alarms + 5 selected custom-metric series, the 2026-07-14 noise lock a further alarm + the order-update gauge series, and PR-C3 one more alarm + the tick-gap gauge series from the pre-C2 bill (was ~$7.60/mo overage) — still well inside the $55 budget cap."
  value = [
    aws_cloudwatch_metric_alarm.disk_used_high.alarm_name,
    aws_cloudwatch_metric_alarm.mem_used_high.alarm_name,
    aws_cloudwatch_metric_alarm.groww_ws_inactive.alarm_name,
    aws_cloudwatch_metric_alarm.groww_stall_restart_storm.alarm_name,
    aws_cloudwatch_metric_alarm.questdb_disconnected.alarm_name,
    # tick_gap_instruments_silent retired in PR-C3 (2026-07-14).
    aws_cloudwatch_metric_alarm.token_remaining_low.alarm_name,
    aws_cloudwatch_metric_alarm.spill_dropped.alarm_name,
    aws_cloudwatch_metric_alarm.dlq_ticks.alarm_name,
    aws_cloudwatch_metric_alarm.aggregator_no_seals.alarm_name,
    aws_cloudwatch_metric_alarm.orders_rejected.alarm_name,
    aws_cloudwatch_metric_alarm.clock_skew_high.alarm_name,
    aws_cloudwatch_metric_alarm.disk_watcher_respawn.alarm_name,
    aws_cloudwatch_metric_alarm.late_tick_after_boundary.alarm_name,
  ]
}
