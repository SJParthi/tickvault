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
          # PR-C3 (2026-07-14): the per-SID tick-gap silent-count panel was
          # replaced — its gauge producer (the tick-gap detector) was deleted
          # with the Dhan WS lane (operator Q4-ii 2026-07-13). The FEED-level
          # last-tick age is the surviving stall signal (FEED-STALL-01).
          title   = "Feed last-tick age (s — feed-level stall signal)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_feed_last_tick_age_seconds"]]
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
            # tv_order_update_ws_active panel row REMOVED 2026-07-14 (Dhan
            # noise lock fix round M4): the order-update WS spawn is retired,
            # so the gauge has zero reachable writers — a dead-metric panel
            # would render as missing data and mislead triage.
            [local.dash_namespace, "tv_websocket_pool_all_dead", { label = "pool all dead (1=bad)" }],
            [local.dash_namespace, "tv_websocket_failed_connections_count", { label = "failed conns" }]
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
            aws_cloudwatch_metric_alarm.questdb_disconnected.arn,
            aws_cloudwatch_metric_alarm.token_remaining_low.arn,
            # tick_gap_instruments_silent retired in PR-C3 (2026-07-14).
            aws_cloudwatch_metric_alarm.spill_dropped.arn,
            aws_cloudwatch_metric_alarm.dlq_ticks.arn,
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
# CloudWatch Dashboard #2 — dual-feed scoreboard live trends (scoreboard PR-C)
# =============================================================================
# Free tier: 3 dashboards; this is dashboard #2 of 3 (slot verified free
# 2026-07-10, dual-feed scoreboard design §6). LIVE TRENDS ONLY — coverage,
# blame and the daily verdict live in the 3:45 PM IST Telegram scorecard +
# the QuestDB scoreboard tables (feed_scoreboard_daily / feed_episode_audit);
# no fake CW series is charted for QuestDB-side data.
#
# Row 1 (PR-C scope): Dhan vs Groww exchange-lag p99 side by side — the
# Groww gauge is the ONE new EMF series of PR-C (+$0.30/mo, 27-name
# allowlist). Resolution asymmetry is stated on the widget title: Dhan's
# whole-second price clock gives it a >=1s floor; Groww is millisecond-
# precise but measured one hop downstream (sidecar capture instant).
# Rows 2-4 (scoreboard PR-D): stall counters + catch-up seals + WS health +
# a feed-focused alarm strip + the honesty text widget — EXISTING metrics
# and alarms only, ZERO new EMF series (design §6: total scoreboard cost
# stays the single PR-C gauge).
# =============================================================================
resource "aws_cloudwatch_dashboard" "scoreboard" {
  dashboard_name = "tv-${var.environment}-scoreboard"

  dashboard_body = jsonencode({
    widgets = [
      {
        type   = "text"
        x      = 0
        y      = 0
        width  = 24
        height = 2
        properties = {
          markdown = "# tickvault — dual-feed scoreboard (${var.environment})\nLive Dhan-vs-Groww feed trends. Coverage, blame and the daily verdict live in the 3:45 PM IST Telegram scorecard + the QuestDB scoreboard tables — this page is live trends only."
        }
      },
      {
        type   = "metric"
        x      = 0
        y      = 2
        width  = 12
        height = 6
        properties = {
          title   = "Dhan price delay, worst 1% (seconds — whole-second clock, floor ~1s)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_dhan_exchange_lag_p99_seconds"]]
          period  = 60
          stat    = "Maximum"
        }
      },
      {
        type   = "metric"
        x      = 12
        y      = 2
        width  = 12
        height = 6
        properties = {
          title   = "REST 1m fire heartbeat over time (official minute-candle pulls alive)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_rest_1m_fire_heartbeat"]]
          period  = 60
          stat    = "Maximum"
        }
      },

      # ----- Row 2 (PR-D): catch-up seals per feed -----
      # ("Feed helper restarts" widget retired 2026-07-15 — the stall-restart
      # counters died with the Groww live feed's stall watchdog.)
      },
      {
        type   = "metric"
        x      = 12
        y      = 8
        width  = 12
        height = 6
        properties = {
          title  = "Late catch-up candle seals per feed (delivery backlog signal)"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_boundary_catchup_total", "host", "tickvault-prod", "feed", "dhan"],
            [local.dash_namespace, "tv_boundary_catchup_total", "host", "tickvault-prod", "feed", "groww"]
          ]
          period = 300
          stat   = "Sum"
        }
      },

      # ----- Row 3 (PR-D): WS health | feed-focused alarm strip -----
      {
        type   = "metric"
        x      = 0
        y      = 14
        width  = 12
        height = 6
        properties = {
          title   = "Dhan WebSocket connections active (0 = feed dead)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_websocket_connections_active"]]
          period  = 60
          stat    = "Minimum"
        }
      },
      {
        type   = "alarm"
        x      = 12
        y      = 14
        width  = 12
        height = 6
        properties = {
          title = "Feed alarm status (red = firing -> already paged)"
          alarms = [
            # feed_stall_restarts + groww_exchange_lag_p99_high retired
            # 2026-07-15 with the Groww live feed.
            aws_cloudwatch_metric_alarm.boundary_catchup_storm_dhan.arn,
            aws_cloudwatch_metric_alarm.dhan_exchange_lag_p99_high.arn
          ]
        }
      },

      # ----- Row 4 (PR-D): honesty text — no fake CW series for QuestDB data -----
      {
        type   = "text"
        x      = 0
        y      = 20
        width  = 24
        height = 2
        properties = {
          markdown = "**Coverage, blame and the daily verdict** live in the 3:45 PM IST Telegram scorecard + the QuestDB tables (`feed_scoreboard_daily` / `feed_coverage_daily` / `feed_episode_audit`) — per-instrument coverage is QuestDB-side data and is deliberately NOT re-published as CloudWatch series (zero extra EMF cost). This page is live trends only."
        }
      }
    ]
  })
}
