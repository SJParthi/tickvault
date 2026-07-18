# =============================================================================
# CloudWatch Dashboard — single operator visual page
# =============================================================================
# One pane the operator opens in the AWS Console (from any browser / phone /
# IntelliJ) to watch the whole system: app health, market-data flow, resource
# usage, and the live alarm states. No local Macbook / Grafana needed — this
# IS the visualization layer for the CloudWatch-only runtime.
#
# Free tier: 3 dashboards. This is dashboard #1.
#
# Metric source: the CloudWatch agent scrapes the app's :9091 /metrics endpoint
# every 60s and publishes the allowlisted tv_* metrics under the
# "Tickvault/Prod" namespace (see user-data.sh.tftpl prometheus.yaml). EC2
# CPU comes from AWS/EC2; disk from the CWAgent namespace. Only metrics that
# are actually scraped are charted here — no empty widgets.
#
# Open it: AWS Console -> CloudWatch -> Dashboards -> tv-<env>-operator
# Or via outputs.tf `dashboard_url`.
# =============================================================================

locals {
  dash_namespace = "Tickvault/Prod"
  dash_region    = var.aws_region
}

resource "aws_cloudwatch_dashboard" "operator" {
  dashboard_name = "tv-${var.environment}-operator"

  dashboard_body = jsonencode({
    widgets = [
      # ----- Row 0: headline text -----
      {
        type   = "text"
        x      = 0
        y      = 0
        width  = 24
        height = 2
        properties = {
          markdown = "# tickvault — operator dashboard (${var.environment})\nLive view of app health, market-data flow, and resource usage. Metrics scraped from the app every 60s. Alarms at the bottom turn red on breach and page Telegram/Email/SMS."
        }
      },

      # ----- Row 1: the single most important signal -----
      {
        type   = "metric"
        x      = 0
        y      = 2
        width  = 8
        height = 6
        properties = {
          # 2026-07-15 (Groww live retirement): was the Groww lag p99 gauge —
          # its only sample producer (the Groww bridge) is deleted; the REST
          # 1m fire heartbeat is the liveness signal (1 = per-minute legs
          # firing; MISSING in-session = wedged/dead — the liveness alarm).
          title   = "REST 1m fire heartbeat (1 = per-minute candle pulls firing)"
          region  = local.dash_region
          view    = "gauge"
          metrics = [[local.dash_namespace, "tv_rest_1m_fire_heartbeat"]]
          yAxis   = { left = { min = 0, max = 1 } }
          period  = 60
          stat    = "Average"
        }
      },
      {
        type   = "metric"
        x      = 8
        y      = 2
        width  = 8
        height = 6
        properties = {
          title   = "Token remaining (seconds until JWT expiry)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_token_remaining_seconds"]]
          period  = 60
          stat    = "Minimum"
        }
      },
      {
        type   = "metric"
        x      = 16
        y      = 2
        width  = 8
        height = 6
        properties = {
          title   = "QuestDB disconnected (seconds — 0 = healthy)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_questdb_disconnected_seconds"]]
          period  = 60
          stat    = "Maximum"
        }
      },

      # ----- Row 2: market-data flow + WebSocket health -----
      # ("Candle seals emitted" widget retired 2026-07-17 — stage-3 dead-WS
      # sweep: tv_aggregator_seals_emitted_total's emit site (the seal
      # routing fan-in) was deleted with the tick aggregator; the series can
      # never publish again. Seal-chain liveness lives in the seal-writer
      # drain counters + tv_rest_candle_fold_heartbeat_total.)
      # ("Feed last-tick age" widget retired 2026-07-15 — its sole producer,
      # the Groww bridge liveness stamp, was deleted with the Groww live feed;
      # the series can never publish again.)
      # ("WebSocket health" widget retired 2026-07-17 — dashboard tidy:
      # tv_websocket_pool_all_dead + tv_websocket_failed_connections_count
      # have ZERO producers in crates/*/src after the live-WS retirements
      # (Dhan 2026-07-13, Groww 2026-07-15); a dead-metric panel would
      # render as missing data and mislead triage.)

      # ----- Row 3: data-integrity / loss signals -----
      # ("Tick spill dropped" + "DLQ ticks" widgets retired 2026-07-17 —
      # dashboard tidy: tv_spill_dropped_total + tv_dlq_ticks_total have
      # ZERO producers after the stage-2 dead-WS sweep (2026-07-17) deleted
      # the dead tick-writer chain; both series can never publish again.)
      {
        type   = "metric"
        x      = 16
        y      = 14
        width  = 8
        height = 6
        properties = {
          title   = "Clock skew (seconds vs trusted source)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_clock_skew_seconds"]]
          period  = 60
          stat    = "Maximum"
        }
      },

      # ----- Row 4: host resources -----
      {
        type   = "metric"
        x      = 0
        y      = 20
        width  = 12
        height = 6
        properties = {
          title   = "EC2 CPU utilization (%)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [["AWS/EC2", "CPUUtilization", "InstanceId", aws_instance.tv_app.id]]
          period  = 60
          stat    = "Average"
        }
      },
      {
        type   = "metric"
        x      = 12
        y      = 20
        width  = 12
        height = 6
        properties = {
          title   = "Root disk used (%)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[{ expression = "SELECT MAX(disk_used_percent) FROM \"CWAgent\" WHERE InstanceId = '${aws_instance.tv_app.id}' AND path = '/'", label = "disk used %", id = "diskpct" }]]
          period  = 300
        }
      },

      # ----- Row 5: live alarm status strip -----
      {
        type   = "alarm"
        x      = 0
        y      = 26
        width  = 24
        height = 4
        properties = {
          title = "Live alarm status (red = firing -> Telegram/Email/SMS already paged)"
          alarms = [
            aws_cloudwatch_metric_alarm.questdb_disconnected.arn,
            aws_cloudwatch_metric_alarm.token_remaining_low.arn,
            # tick_gap_instruments_silent retired in PR-C3 (2026-07-14).
            # spill_dropped + dlq_ticks retired 2026-07-18 (stage-4 unit A).
            aws_cloudwatch_metric_alarm.clock_skew_high.arn,
            aws_cloudwatch_metric_alarm.high_cpu.arn,
            aws_cloudwatch_metric_alarm.disk_used_high.arn
          ]
        }
      },

      # ----- Row 6: order-side (cluster C, 2026-07-14) -----
      # tv_orders_placed_delta_total = the DERIVED metrics-log-filter
      # series from order-side-alarms.tf (dense from boot via the main.rs
      # pre-registrations); tv_orders_rejected_total is EMF-published.
      # ₹0 widget — appended to the EXISTING dashboard, free-tier slot 3
      # deliberately NOT consumed.
      {
        type   = "metric"
        x      = 0
        y      = 30
        width  = 12
        height = 6
        properties = {
          title  = "Orders (paper mode until Phase-1) — placed vs rejected, 5m sums"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            ["Tickvault/Prod", "tv_orders_placed_delta_total", "host", "tickvault-prod", { stat = "Sum" }],
            ["Tickvault/Prod", "tv_orders_rejected_total", "host", "tickvault-prod", { stat = "Sum" }]
          ]
          period = 300
        }
      }
    ]
  })
}

# =============================================================================
# CloudWatch Dashboard #2 (tv-<env>-scoreboard) RETIRED 2026-07-17 — dashboard
# tidy. The dual-feed scoreboard dashboard's live-trend widgets charted the
# Dhan/Groww exchange-lag gauges (producers deleted with the dead Dhan-lag
# publisher chain, 2026-07-17), the stall/WS series (producers deleted with
# the live-feed retirements 2026-07-13/2026-07-15), and a feed-focused alarm
# strip over retired alarms. Coverage, blame and the daily verdict continue to
# live in the 3:45 PM IST Telegram scorecard + the QuestDB scoreboard tables
# (feed_scoreboard_daily / feed_coverage_daily / feed_episode_audit) — those
# are unaffected. Frees dashboard slot #2 of the 3-slot CloudWatch free tier.
# =============================================================================
