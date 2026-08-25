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
# 2026-07-17 (stage-3 dead-WS sweep): the 2 [host,feed] boundary-catchup
# declarations AND the tv_aggregator_seals_emitted_total +
# tv_aggregator_close_pct_nonzero_total main-list names are RETIRED — the
# tick aggregator (their writers' owner) is deleted. Main EMF list is now
# 17 names / no second declaration; the historical 27/29 figures below are
# retained as dated audit. See aws-budget.md COST NOTE 2026-07-17.
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
# window-gate entries. 2026-07-15 (Groww live-feed retirement):
# groww_ws_inactive + groww_stall_restart_storm ALSO retired — their
# gauge/counter producers (the Groww bridge + sidecar stall watchdog) were
# deleted; in-session process liveness is owned by the market-hours liveness
# alarm, re-pointed to tv_rest_1m_fire_heartbeat.
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
# 3b. Groww feed inactive — RETIRED 2026-07-15 (Groww live-feed retirement):
# `tv_groww_ws_active` was written ONLY by the deleted Groww bridge loop, so
# the alarm could never fire again (permanent missing-data;
# treat_missing_data=notBreaching made it silently dead, not stuck-FIRING).
# Process liveness in-session is owned by the market-hours liveness alarm,
# re-pointed to tv_rest_1m_fire_heartbeat in the same PR.
# ---------------------------------------------------------------------------

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
# FEED-level stall detection retired 2026-07-15 with the Groww live feed.
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
# 7/8/8b. Tick backpressure-chain alarms — RETIRED 2026-07-18 (stage-4
# dead-producer sweep). The three alarms (tv-${env}-spill-dropped,
# tv-${env}-dlq-ticks, tv-${env}-ticks-dropped) monitored the tick
# ring->spill->DLQ chain in tick_persistence.rs, which was DELETED in the
# stage-2 dead-WS sweep (2026-07-17) — the runtime is REST-only and nothing
# writes the ticks table anymore, so tv_spill_dropped_total /
# tv_dlq_ticks_total / tv_ticks_dropped_total have ZERO emit sites and the
# alarms (treat_missing_data = notBreaching) were permanently-dead monitors.
# The candle-side seal chain keeps its own pagers (seal-drop-alarm.tf +
# the AGGREGATOR-DROP-01 errcode alarm).
#
# 2026-08-14 CORRECTION — the tv_ticks_dropped_total half of the paragraph
# above is now STALE. It was TRUE when written: tick_persistence.rs had just
# been deleted in the stage-2 dead-WS sweep. The 2026-08-09 Dhan live-lane
# revival REBUILT that module, and it emits tv_ticks_dropped_total again today
# (tick_persistence.rs:784, verified in source). The retirement was correct at
# its date and the metric is live again at this one; both statements are true,
# which is exactly why a re-added alarm must re-verify the emit site rather
# than trust this comment. A replacement alarm lives in live-lane-alarms.tf.
# tv_dlq_ticks_total and tv_spill_dropped_total are UNCHANGED — still no emit
# sites, still correctly un-alarmed.
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# 9. Aggregator no-seals alarm — RETIRED 2026-07-15 (Groww live-feed
# retirement). The alarm's metric, tv_aggregator_seals_emitted_total, lost
# its LAST live producer with this PR: the Groww bridge's aggregator drain
# was deleted, and the Dhan broadcast instance has been publisher-less since
# the 2026-07-13 Dhan live-WS retirement — so the metric can never emit a
# datapoint again and the alarm (treat_missing_data = notBreaching) was a
# permanently-dead monitor that the window gate kept arming daily. The
# dormant seal_routing emit site + the EMF selector row survived that PR;
# BOTH are now RETIRED (2026-07-17, stage-3 dead-WS sweep — seal_routing
# deleted with the tick aggregator; the selector rows left the EMF list
# the same day).
# ---------------------------------------------------------------------------

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
# ---------------------------------------------------------------------------
# 2026-08-25 — THRESHOLD RAISED 75 -> 90, AND WHY THAT IS NOT A WEAKENING.
#
# Measured state on the box: the root volume is structurally stuck at 86%.
# Retention has never dropped a single partition (partition_archive_audit held
# 1,000 rows, every one outcome='s3_conflict', zero successes ever — now paged
# by tv-<env>-partition-archive-failed in loss-and-retention-alarms.tf), so the
# oldest ticks on a nominal 1-day hot window date to 2026-06-02.
#
# At a 75% threshold this alarm has therefore been in ALARM continuously, with
# no edge, for as long as the condition has existed. A PERMANENTLY LATCHED
# alarm is worse than no alarm: it cannot transition, so it can never page
# again, and every day it sits red teaches the operator that red is this
# alarm's normal colour. By the time 75% meant something the operator had
# already been trained to scroll past it. That is the same alert-fatigue
# failure `dhan-rest-only-noise-lock-2026-07-14.md` §2.3a describes, arriving
# from the opposite direction — not too many pagers, one pager that never stops.
#
# 90% restores the EDGE: the alarm is OK today at 86%, so its next transition
# is a real event. The 4 points of headroom that buys is deliberately thin, and
# it is not the early warning — 13b below is. Losing sensitivity between 75 and
# 90 is the price of getting a working transition back, and it is only
# acceptable BECAUSE a trend alarm now covers the range this one gave up.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "disk_used_high" {
  alarm_name          = "tv-${var.environment}-disk-used-high"
  alarm_description   = "Root volume > 90% full - the CEILING, not the early warning (tv-<env>-disk-fill-rate-high is the trend alarm that should fire days before this one). Raised 75->90 on 2026-08-25: the box was structurally stuck at 86%, so at 75 this alarm was permanently latched in ALARM, could never transition, and could never page again. Grow online (no downtime): scripts/aws-upgrade-instance.sh --ebs-size N (modify-volume; grown 30->50 on 2026-07-13, 100->200 on 2026-08-21), then on the box: sudo growpart /dev/nvme0n1 1 && sudo xfs_growfs / (or the next daily boot's cloud-init growpart/resizefs). gp3 grows online and can NEVER shrink. First check whether retention is actually running: SELECT outcome, count() FROM partition_archive_audit. See may31-inplace-upgrade-and-access.md §2.1."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  threshold           = 90
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
# 13b. Root volume FILL TREND — the early warning alarm 13 stopped being
#      (2026-08-25).
#
# Alarm 13 answers "are we nearly full?". It cannot answer "are we filling?",
# and on this box that is the question with lead time in it. The disk moved
# from comfortable to 86% with a projected mid-session fill date, and the only
# thing that reported the trajectory was somebody reading the number.
#
# WHAT IT MEASURES. RATE() of the same Metrics Insights series alarm 13 uses,
# scaled by 86,400 so the threshold reads in the operator's own units:
# PERCENTAGE POINTS PER DAY. Threshold 4 pts/day means "at this rate the
# remaining headroom is gone inside about a working week" — early enough to
# grow the volume in a maintenance window instead of mid-session.
#
# WHY A 6-HOUR PERIOD AND TWO DATAPOINTS, not the 5 minutes alarm 13 uses. The
# box runs ~9 hours a weekday, so intraday growth is roughly 2.7x the 24-hour
# average by construction. A 5-minute rate window would convert any ordinary
# busy hour into an alarming points-per-day figure and page constantly. Six
# hours spans most of a session and, across the overnight stop, most of a day;
# requiring TWO consecutive such windows (~12h of sustained fill) means a
# compaction burst or one heavy afternoon cannot trip it alone.
#
# NOT market-hours gated. Growth is measured across the overnight gap on
# purpose — that is what makes the 6-hour windows approximate a daily rate
# rather than a session rate — and the gate would discard exactly those
# windows.
#
# treat_missing_data = notBreaching: a stopped box publishes no disk samples,
# and a stopped box is not filling. `breaching` would page every night for the
# absence of a problem.
#
# HONEST RESIDUAL, stated rather than discovered at apply time: this is metric
# math layered on a Metrics Insights query. The Insights query returns a single
# time series (no GROUP BY), which is the documented precondition, but the
# combination is not exercised elsewhere in this repo. If CloudWatch rejects
# it, `terraform apply` fails LOUDLY at plan/apply — it cannot land as a
# silently-broken alarm, which is the failure mode that would actually matter.
# The fallback is to alarm the raw CWAgent metric with explicit device/fstype
# dimensions; alarm 13 uses Insights precisely to avoid pinning those, so that
# fallback trades one fragility for another and should only be taken if forced.
#
# A DROP IS NOT AN ALARM. A partition drop or a volume grow makes the rate
# strongly negative; GreaterThanThreshold ignores that, which is correct — the
# disk shrinking is the outcome we want.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "disk_fill_rate_high" {
  alarm_name          = "tv-${var.environment}-disk-fill-rate-high"
  alarm_description   = "Root volume is FILLING at more than 4 percentage points per day, sustained across two consecutive 6-hour windows (~12h). This is the early warning: at this rate the headroom below the 90% ceiling is gone inside about a working week, so grow the volume in a maintenance window rather than mid-session. FIRST check whether retention is running at all - the 2026-08-25 state was 1,000 archive attempts, all failed, zero partitions ever dropped, which is a fill trend with a fixable cause: SELECT outcome, count() FROM partition_archive_audit. If archival is healthy and the trend is real ingest growth, grow gp3 online (scripts/aws-upgrade-instance.sh --ebs-size N, then growpart + xfs_growfs) - it can never shrink, so go up in steps. Companion ceiling alarm: tv-<env>-disk-used-high."
  comparison_operator = "GreaterThanThreshold"
  threshold           = 4
  evaluation_periods  = 2
  datapoints_to_alarm = 2
  treat_missing_data  = "notBreaching"
  alarm_actions       = local.app_alarm_actions
  # NO ok_actions. The rate falling back below 4 pts/day means the disk is
  # filling more slowly, never that the space came back - and after a real
  # grow the operator already knows. An auto-OK would read as "resolved" while
  # the volume is still fuller than it was (Rule 11, no false recovery).
  ok_actions = []

  # WHY THIS ALARM DOES NOT USE METRICS INSIGHTS, unlike its sibling
  # `disk_used_high` (2026-08-25, decided on MEASURED data).
  #
  # It shipped in #1805 as a Metrics Insights alarm at period 21600 (6h) x 2
  # evaluations = 12h, and AWS rejected it on every apply:
  #
  #   ValidationError: MetricsInsights monitors cannot be checked across
  #   more than 3 hours
  #
  # That froze the whole apply lane for hours - terraform stops at the first
  # failing resource, so every later change sat on main undeployed.
  #
  # The first repair narrowed the window to 1h x 2 = 2h to fit the cap. That
  # applied, and it was WRONG. Real hourly rates on this volume, read from
  # CloudWatch over 48h, are violent: +230, +278, +181, +147 points/day on
  # ordinary consecutive hours (compaction and archival churn), against a
  # threshold of 4. Two of those in a row is an ALARM, so the "fix" converted
  # a never-applying alarm into one that pages several times a day.
  #
  # No legal Insights window can work here. The measured 24h drift - the REAL
  # signal - was +10.9 points/day while hourly NOISE reaches +280. Only a
  # window long enough to span the overnight stop averages the churn out, and
  # 12h is over the 3h Insights cap by construction.
  #
  # So this uses a plain metric block with explicit dimensions, which has no
  # such cap. That is exactly the fallback #1805's own header named and hoped
  # to avoid: "alarm the raw CWAgent metric with explicit device/fstype
  # dimensions ... should only be taken if forced". We are forced, and unlike
  # that author we could VERIFY the dimensions against the live account rather
  # than guess them - `aws cloudwatch list-metrics --namespace CWAgent
  # --metric-name disk_used_percent` returns exactly ONE series:
  # path=/, InstanceId=i-0c3fe906dad5492fc, device=nvme0n1p1, fstype=xfs.
  #
  # THE FRAGILITY THIS BUYS, stated plainly: device and fstype are pinned. An
  # instance recreate onto non-nvme storage, or a filesystem change, silently
  # sends this alarm to INSUFFICIENT_DATA - a dead monitor. That is accepted
  # HERE and only here because `disk_used_high` is the backstop and it is
  # dimension-agnostic: it still fires at 90% full whatever the device is
  # called. The fragile alarm is the early warning; the robust one is the
  # safety net. Re-run the list-metrics command above after any instance
  # recreate.
  metric_query {
    id          = "disk_pct"
    return_data = false

    metric {
      namespace   = "CWAgent"
      metric_name = "disk_used_percent"
      period      = 21600
      stat        = "Maximum"

      dimensions = {
        InstanceId = aws_instance.tv_app.id
        path       = "/"
        device     = "nvme0n1p1"
        fstype     = "xfs"
      }
    }
  }

  metric_query {
    id = "fill_rate_per_day"
    # RATE() is per SECOND; x86400 renders the threshold in points per day,
    # which is the unit the operator actually reasons in.
    expression  = "RATE(disk_pct) * 86400"
    label       = "root volume fill rate (percentage points per day)"
    return_data = true
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
# 16b. Groww sidecar stall-restart storm — RETIRED 2026-07-15 (Groww
# live-feed retirement): `tv_feed_sidecar_stall_restart_total` was
# incremented ONLY by the deleted sidecar stall watchdog
# (groww_sidecar_supervisor.rs), so the alarm could never fire again.
# The FEED-STALL-01 errcode filter + the feed-stall-restarts counter pager
# (feed-stall-restart-alarm.tf) were removed in the same PR.
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Post-close tick anomaly alarm — RETIRED 2026-07-18 (stage-4 dead-producer
# sweep). Its metric, tv_late_tick_after_boundary_total, lost its only emit
# site when the per-tick check in tick_processor.rs was deleted with the dead
# Dhan tick chain (2026-07-17, stage-2 sweep) — the metric can never emit a
# datapoint again, so the alarm was a permanently-dead monitor.
# ---------------------------------------------------------------------------

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
  description = "12 application-level alarms in THIS file (10 Prometheus-via-CW-agent + 1 disk-used + 1 mem-used Metrics-Insights; PR-C2 2026-07-13 retired 5 Dhan-lane alarms; order-update-ws-inactive RETIRED 2026-07-14 per dhan-rest-only-noise-lock-2026-07-14.md; tick-gap-instruments-silent RETIRED in PR-C3 2026-07-14; groww-ws-inactive + groww-stall-restart-storm RETIRED 2026-07-15 — their gauge/counter producers, the Groww bridge + sidecar stall watchdog, were deleted with the Groww live feed); 2 more silent-feed alarms live in silent-feed-alarms.tf (the Groww lag mirror also retired 2026-07-15). Cost note: the 2026-07-15 Groww live retirement removes 3 alarms + the feed-stall-restarts counter pager + 4 EMF series and adds 1 (tv_rest_1m_fire_heartbeat) — dated note in aws-budget.md; still well inside the $55 budget cap."
  value = [
    aws_cloudwatch_metric_alarm.disk_used_high.alarm_name,
    aws_cloudwatch_metric_alarm.mem_used_high.alarm_name,
    # groww_ws_inactive + groww_stall_restart_storm retired 2026-07-15
    # (Groww live-feed retirement).
    aws_cloudwatch_metric_alarm.questdb_disconnected.alarm_name,
    # tick_gap_instruments_silent retired in PR-C3 (2026-07-14).
    aws_cloudwatch_metric_alarm.token_remaining_low.alarm_name,
    # spill_dropped + dlq_ticks retired 2026-07-18 (stage-4 unit A —
    # dead-tick alarm chains; the ticks table has no writer anymore).
    # aggregator_no_seals retired 2026-07-15 (Groww live-feed retirement —
    # the seals metric lost its last live producer; see section 9 note).
    aws_cloudwatch_metric_alarm.orders_rejected.alarm_name,
    aws_cloudwatch_metric_alarm.clock_skew_high.alarm_name,
    aws_cloudwatch_metric_alarm.disk_watcher_respawn.alarm_name,
    # late_tick_after_boundary retired 2026-07-18 (stage-4 unit A).
  ]
}
