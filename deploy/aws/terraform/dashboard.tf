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

# =============================================================================
# CloudWatch Dashboard #2 — dual-feed scoreboard live trends (scoreboard PR-C)
# =============================================================================
# Free tier: 3 dashboards; this is dashboard #2 of 3 (slot verified free
# 2026-07-10, dual-feed scoreboard design §6; slot #3 taken 2026-07-14 by
# the orders dashboard below — still ₹0, all 3 free-tier slots used).
# LIVE TRENDS ONLY — coverage,
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
          title   = "Groww price delay, worst 1% (seconds — millisecond clock, measured at helper capture)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_groww_exchange_lag_p99_seconds"]]
          period  = 60
          stat    = "Maximum"
        }
      },

      # ----- Row 2 (PR-D): stall restarts | catch-up seals per feed -----
      {
        type   = "metric"
        x      = 0
        y      = 8
        width  = 12
        height = 6
        properties = {
          title  = "Feed helper restarts (stalled / never-streamed sockets killed + relaunched)"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_feed_sidecar_stall_restart_total"],
            [local.dash_namespace, "tv_feed_sidecar_never_streamed_restart_total"]
          ]
          period = 300
          stat   = "Sum"
        }
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
            aws_cloudwatch_metric_alarm.ws_pool_all_dead.arn,
            aws_cloudwatch_metric_alarm.feed_stall_restarts.arn,
            aws_cloudwatch_metric_alarm.boundary_catchup_storm_dhan.arn,
            aws_cloudwatch_metric_alarm.dhan_exchange_lag_p99_high.arn,
            aws_cloudwatch_metric_alarm.groww_exchange_lag_p99_high.arn,
            aws_cloudwatch_metric_alarm.realtime_guarantee_degraded.arn
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

# =============================================================================
# CloudWatch Dashboard #3 — DHAN order desk (order-side alerting, 2026-07-14)
# =============================================================================
# Free tier: 3 dashboards; this is dashboard #3 of 3 (the LAST free slot —
# still ₹0). EVERYTHING here is DORMANT today by design: the order engine is
# never instantiated while dry_run=true + dhan_enabled=false, so every widget
# is empty until Cluster A revives order flow. The text widget states that
# honestly (Rule 11 — an empty chart must never read as a broken dashboard).
#
# Row 1: orders placed per-mode (the decl-3 [host,mode] series) | orders
#        rejected | circuit-breaker state (annotations at 1=Open, 2=HalfOpen).
# Row 2: tv_daily_pnl (Minimum, annotation at -20,000) | order latency
#        last-sample (Maximum).
# Row 3: the 3 errcode filter metrics (dimensionless, Sum) — OMS-GAP-03 /
#        OMS-GAP-04 / RISK-GAP-01.
# =============================================================================
resource "aws_cloudwatch_dashboard" "orders" {
  dashboard_name = "tv-${var.environment}-orders"

  dashboard_body = jsonencode({
    widgets = [
      # ----- Row 0: headline honesty text -----
      {
        type   = "text"
        x      = 0
        y      = 0
        width  = 24
        height = 2
        properties = {
          markdown = "# 🔷 DHAN order desk (${var.environment})\nEverything here is OFF today by design: the order engine is behind the safety switch (paper-trading lock + Dhan feed off). Empty charts mean NO orders exist, not a broken dashboard. The first datapoint appears only when order flow is switched on."
        }
      },

      # ----- Row 1: order flow + circuit breaker -----
      {
        type   = "metric"
        x      = 0
        y      = 2
        width  = 8
        height = 6
        properties = {
          title  = "Orders placed per mode (paper vs live)"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_orders_placed_total", "host", "tickvault-prod", "mode", "paper"],
            [local.dash_namespace, "tv_orders_placed_total", "host", "tickvault-prod", "mode", "live"]
          ]
          period = 300
          stat   = "Sum"
        }
      },
      {
        type   = "metric"
        x      = 8
        y      = 2
        width  = 8
        height = 6
        properties = {
          title   = "Orders rejected (0 = none)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_orders_rejected_total", { stat = "Sum" }]]
          period  = 300
        }
      },
      {
        type   = "metric"
        x      = 16
        y      = 2
        width  = 8
        height = 6
        properties = {
          title   = "Circuit breaker state (0=Closed 1=Open 2=HalfOpen)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_circuit_breaker_state"]]
          period  = 60
          stat    = "Maximum"
          annotations = {
            horizontal = [
              { label = "Open (all orders blocked)", value = 1 },
              { label = "Half-open (one probe allowed)", value = 2 }
            ]
          }
        }
      },

      # ----- Row 2: P&L + latency -----
      {
        type   = "metric"
        x      = 0
        y      = 8
        width  = 12
        height = 6
        properties = {
          title   = "Daily P&L, rupees (realized + unrealized — halts trading at -20,000)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_daily_pnl"]]
          period  = 300
          stat    = "Minimum"
          annotations = {
            horizontal = [
              { label = "daily-loss halt threshold", value = -20000 }
            ]
          }
        }
      },
      {
        type   = "metric"
        x      = 12
        y      = 8
        width  = 12
        height = 6
        properties = {
          title   = "Order placement time, last sample (ms — alarm at 5,000)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_order_placement_last_ms"]]
          period  = 300
          stat    = "Maximum"
        }
      },

      # ----- Row 3: errcode pager activity (dimensionless filter metrics) -----
      {
        type   = "metric"
        x      = 0
        y      = 14
        width  = 8
        height = 6
        properties = {
          title   = "Circuit-breaker OPEN pages (OMS-GAP-03)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_errcode_oms_gap_03", { stat = "Sum" }]]
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
          title   = "Rate-limit denial pages (OMS-GAP-04)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_errcode_oms_gap_04", { stat = "Sum" }]]
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
          title   = "Risk-halt pages (RISK-GAP-01)"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_errcode_risk_gap_01", { stat = "Sum" }]]
          period  = 300
        }
      }
    ]
  })
}
