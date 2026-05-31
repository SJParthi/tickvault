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
          title   = "Real-time guarantee score (1.0 = all healthy)"
          region  = local.dash_region
          view    = "gauge"
          metrics = [[local.dash_namespace, "tv_realtime_guarantee_score"]]
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
      {
        type   = "metric"
        x      = 0
        y      = 8
        width  = 8
        height = 6
        properties = {
          title   = "Candle seals emitted (data flowing during market hours)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_aggregator_seals_emitted_total", { stat = "Sum" }]]
          period  = 300
        }
      },
      {
        type   = "metric"
        x      = 8
        y      = 8
        width  = 8
        height = 6
        properties = {
          title   = "Instruments silent (tick-gap — 0 = all streaming)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_tick_gap_instruments_silent"]]
          period  = 60
          stat    = "Maximum"
        }
      },
      {
        type   = "metric"
        x      = 16
        y      = 8
        width  = 8
        height = 6
        properties = {
          title  = "WebSocket health"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_websocket_pool_all_dead", { label = "pool all dead (1=bad)" }],
            [local.dash_namespace, "tv_websocket_failed_connections_count", { label = "failed conns" }],
            [local.dash_namespace, "tv_order_update_ws_active", { label = "order-update WS active (1=good)" }]
          ]
          period = 60
          stat   = "Maximum"
        }
      },

      # ----- Row 3: data-integrity / loss signals -----
      {
        type   = "metric"
        x      = 0
        y      = 14
        width  = 8
        height = 6
        properties = {
          title   = "Tick spill dropped (0 = no loss)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_spill_dropped_total", { stat = "Sum" }]]
          period  = 300
        }
      },
      {
        type   = "metric"
        x      = 8
        y      = 14
        width  = 8
        height = 6
        properties = {
          title   = "DLQ ticks (recoverable overflow — 0 = none)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_dlq_ticks_total", { stat = "Sum" }]]
          period  = 300
        }
      },
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
            aws_cloudwatch_metric_alarm.ws_pool_all_dead.arn,
            aws_cloudwatch_metric_alarm.questdb_disconnected.arn,
            aws_cloudwatch_metric_alarm.token_remaining_low.arn,
            aws_cloudwatch_metric_alarm.tick_gap_instruments_silent.arn,
            aws_cloudwatch_metric_alarm.realtime_guarantee_critical.arn,
            aws_cloudwatch_metric_alarm.spill_dropped.arn,
            aws_cloudwatch_metric_alarm.dlq_ticks.arn,
            aws_cloudwatch_metric_alarm.clock_skew_high.arn,
            aws_cloudwatch_metric_alarm.high_cpu.arn,
            aws_cloudwatch_metric_alarm.disk_used_high.arn
          ]
        }
      }
    ]
  })
}
