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
      },

      # ----- Row 7: the LIVE runtime (2026-08-11, observability audit r2) -----
      # The dashboard charted 8 series and NONE of them covered the four
      # per-minute REST legs, the seal writer, the RAM store or the cadence
      # scheduler — i.e. everything that actually runs. The operator could
      # look at an all-green page while the whole runtime was dead.
      #
      # EVERY metric below was verified twice before being charted, because a
      # widget with no producer renders as "no data", which an operator reads
      # as "fine" — strictly worse than no widget:
      #   1. a producer exists in crates/*/src  (grep-verified 2026-08-11)
      #   2. the name is in the CloudWatch agent EMF allowlist
      #      (user-data.sh.tftpl + cloudwatch-agent.json) — without this the
      #      series never reaches the Tickvault/Prod namespace at all.
      # Metrics failing check 2 are deliberately NOT charted here; widening
      # the allowlist is a separate owner's file.
      {
        type   = "text"
        x      = 0
        y      = 36
        width  = 24
        height = 2
        properties = {
          markdown = "## The live runtime — per-minute pulls, storage, memory\nThese are the parts that actually run today. **Persist-error lines should sit flat at zero.** A rising line means candles are being fetched but not saved. If the fire heartbeat at the top is missing during market hours, nothing is being pulled at all."
        }
      },
      {
        type   = "metric"
        x      = 0
        y      = 38
        width  = 12
        height = 6
        properties = {
          title  = "REST 1m legs — persist errors (flat zero = healthy)"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_spot1m_persist_errors_total", { label = "Dhan spot 1m", stat = "Sum" }],
            [local.dash_namespace, "tv_chain1m_persist_errors_total", { label = "Dhan option chain 1m", stat = "Sum" }],
            [local.dash_namespace, "tv_groww_spot1m_persist_errors_total", { label = "Groww spot 1m", stat = "Sum" }],
            [local.dash_namespace, "tv_groww_chain1m_persist_errors_total", { label = "Groww option chain 1m", stat = "Sum" }]
          ]
          period = 300
        }
      },
      {
        type   = "metric"
        x      = 12
        y      = 38
        width  = 12
        height = 6
        properties = {
          title  = "Restarts — cadence scheduler / disk watcher / order push"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_cadence_runner_respawn_total", { label = "cadence scheduler", stat = "Sum" }],
            [local.dash_namespace, "tv_disk_watcher_respawn_total", { label = "disk watcher", stat = "Sum" }],
            [local.dash_namespace, "tv_dhan_order_push_respawn_total", { label = "order push", stat = "Sum" }]
          ]
          period = 300
        }
      },
      {
        type   = "metric"
        x      = 0
        y      = 44
        width  = 8
        height = 6
        properties = {
          title  = "Cadence pulls skipped / denied / exhausted"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_cadence_boundary_skipped_total", { label = "minute skipped", stat = "Sum" }],
            [local.dash_namespace, "tv_cadence_gate_denials_total", { label = "gate denied", stat = "Sum" }],
            [local.dash_namespace, "tv_cadence_ladder_exhausted_total", { label = "retries exhausted", stat = "Sum" }]
          ]
          period = 300
        }
      },
      {
        type   = "metric"
        x      = 8
        y      = 44
        width  = 8
        height = 6
        properties = {
          title  = "In-memory store — dropped / errors (flat zero = healthy)"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_ram_store_dropped_total", { label = "dropped", stat = "Sum" }],
            [local.dash_namespace, "tv_ram_store_errors_total", { label = "errors", stat = "Sum" }]
          ]
          period = 300
        }
      },
      {
        type   = "metric"
        x      = 16
        y      = 44
        width  = 8
        height = 6
        properties = {
          title  = "Database write health — WAL suspended tables / reconnects"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_questdb_wal_suspended_tables", { label = "WAL suspended tables", stat = "Maximum" }],
            [local.dash_namespace, "tv_questdb_reconnects_total", { label = "reconnects", stat = "Sum" }]
          ]
          period = 300
        }
      },

      # ----- Row 8: host memory + spill headroom -----
      # The 32 GiB sizing flagged an UNMEASURED memory risk (rule file
      # daily-universe §7 Rule 2 NEW FLAG: "the first live session at scale
      # is the measured gate — read tv_process_rss_bytes"). It was not
      # charted anywhere. Now it is.
      {
        type   = "metric"
        x      = 0
        y      = 50
        width  = 12
        height = 6
        properties = {
          title  = "App memory used (bytes) + OOM kills"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_process_rss_bytes", { label = "app memory used", stat = "Maximum" }],
            [local.dash_namespace, "tv_oom_kills_total", { label = "OOM kills", stat = "Sum" }]
          ]
          period = 300
        }
      },
      {
        type   = "metric"
        x      = 12
        y      = 50
        width  = 12
        height = 6
        properties = {
          title  = "Spill disk free (bytes) — the zero-loss safety margin"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [[local.dash_namespace, "tv_spill_dir_free_bytes"]]
          period  = 300
          stat    = "Minimum"
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
