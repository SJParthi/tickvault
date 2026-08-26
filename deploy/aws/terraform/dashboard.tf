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
            [local.dash_namespace, "tv_chain1m_persist_errors_total", { label = "Dhan option chain 1m", stat = "Sum" }]
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
          title   = "Spill disk free (bytes) — the zero-loss safety margin"
          region  = local.dash_region
          view    = "timeSeries"
          metrics = [[local.dash_namespace, "tv_spill_dir_free_bytes"]]
          period  = 300
          stat    = "Minimum"
        }
      },

      # ----- Row 9: the DHAN LIVE LANE (2026-08-15) -----
      #
      # The lane was switched on 2026-08-11 and its universe widened to ~4,565
      # instruments on 2026-08-12. Twenty-one of its series were added to the
      # CloudWatch agent's allowlist across three dated cost notes — and NOT
      # ONE of them was charted anywhere. We were paying to publish the whole
      # failure surface of the only live tick source and looking at none of it.
      #
      # That is the same defect the Row-7 comment above describes ("the
      # operator could look at an all-green page while the whole runtime was
      # dead"), recreated one subsystem later. It is also the more expensive
      # half: the REST legs at least page on failure, whereas every counter
      # below is visible-only — no alarm reads them — so a dashboard widget is
      # currently the ONLY way any of this reaches a human.
      #
      # Every name below was checked against the EMF allowlist in
      # user-data.sh.tftpl before charting, per the Row-7 rule: a widget with
      # no producer renders as "no data", which reads as "fine".
      {
        type   = "text"
        x      = 0
        y      = 56
        width  = 24
        height = 2
        properties = {
          markdown = "## The live market-data feed (Dhan WebSocket)\n**Every loss line below should sit flat at zero.** A rising line is ticks going missing — the top-left panel tells you whether the feed is even connected, and the loss panels tell you where they are being lost: at the socket, in the buffer, or on the way to the database. The silence panel answers a different question: *are we connected but hearing nothing?*"
        }
      },
      {
        type   = "metric"
        x      = 0
        y      = 58
        width  = 8
        height = 6
        properties = {
          # 2026-08-20: the second series was labelled "open sockets" and is
          # not one. `tv_dhan_feed_stack_connections` is set ONCE from the
          # PLAN, before a single dial, and never again — the repo's own EMF
          # note calls it "a BOOT-TIME CONSTANT that reports '5 depth-20'
          # whether or not a single byte ever arrives". Through the 2026-08-12
          # blackout (12 consecutive HTTP 400 dials, zero candles for 373
          # minutes) this chart read "open sockets: 5" the entire session.
          #
          # The metric is fine; the LABEL was the lie, and it was the one an
          # operator reads at a glance. Relabelled to what it is, and the gauge
          # that actually tracks live sockets is charted beside it — that pair
          # is the answer to "how many of the 16 are really up?", which until
          # now needed a log grep. `tv_dhan_ws_alive_connections` was already
          # alarmed and simply never charted.
          title  = "Feed alive? (1 = lane up) + planned vs live sockets"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_dhan_feed_stack_up", { label = "lane up (1 = yes)", stat = "Minimum" }],
            [local.dash_namespace, "tv_dhan_ws_alive_connections", { label = "sockets LIVE now", stat = "Minimum" }],
            [local.dash_namespace, "tv_dhan_feed_stack_connections", { label = "sockets PLANNED at boot (constant)", stat = "Maximum" }]
          ]
          period = 60
        }
      },
      {
        type   = "metric"
        x      = 8
        y      = 58
        width  = 8
        height = 6
        properties = {
          # These five are the SOCKET-side losses: a tick that dies here never
          # reached the durable floor at all, so nothing downstream can recover
          # it. `wal dropped` is the most serious line on this dashboard.
          title  = "Ticks lost AT THE SOCKET (flat zero = healthy)"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_dhan_ws_wal_dropped_total", { label = "never durably captured", stat = "Sum" }],
            [local.dash_namespace, "tv_dhan_ws_ring_full_total", { label = "buffer full (count)", stat = "Sum" }],
            [local.dash_namespace, "tv_dhan_ws_ring_bytes_full_total", { label = "buffer full (bytes)", stat = "Sum" }],
            [local.dash_namespace, "tv_dhan_ws_frame_refused_total", { label = "frame refused", stat = "Sum" }],
            [local.dash_namespace, "tv_dhan_ws_subscribe_failed_total", { label = "never subscribed", stat = "Sum" }]
          ]
          period = 300
        }
      },
      {
        type   = "metric"
        x      = 16
        y      = 58
        width  = 8
        height = 6
        properties = {
          # The DATABASE side. The 2026-08-11 allowlist widening instrumented
          # the socket and left this half blind for a day; splitting the two
          # panels is what makes "where are they being lost?" answerable at a
          # glance instead of by reading counter names.
          title  = "Ticks lost ON THE WAY TO THE DATABASE (flat zero = healthy)"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_ticks_dropped_total", { label = "discarded on flush failure", stat = "Sum" }],
            [local.dash_namespace, "tv_tick_persist_errors_total", { label = "write errors", stat = "Sum" }],
            [local.dash_namespace, "tv_tick_rows_refused_total", { label = "rows refused", stat = "Sum" }],
            [local.dash_namespace, "tv_ws_frame_spill_write_errors_total", { label = "durable-floor write errors", stat = "Sum" }],
            [local.dash_namespace, "tv_dhan_feed_seals_dropped_total", { label = "candles discarded", stat = "Sum" }],
            [local.dash_namespace, "tv_dhan_feed_seq_refused", { label = "sequence unrepresentable", stat = "Sum" }]
          ]
          period = 300
        }
      },
      {
        type   = "metric"
        x      = 0
        y      = 64
        width  = 8
        height = 6
        properties = {
          # Connected-but-silent is the failure mode with NO other evidence: a
          # subscribe that quietly did not take produces no payload to count,
          # no parse to fail and no error to log. Absence measured against a
          # seeded key is the only signal that exists, which is why it earns a
          # panel of its own rather than a line on a loss chart.
          title  = "Connected but hearing nothing (instrument counts)"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_dhan_feed_instruments_never_ticked", { label = "never ticked at all", stat = "Maximum" }],
            [local.dash_namespace, "tv_dhan_feed_instruments_silent", { label = "gone quiet", stat = "Maximum" }]
          ]
          period = 300
        }
      },
      {
        type   = "metric"
        x      = 16
        y      = 64
        width  = 8
        height = 6
        properties = {
          # The only ring signal that rises BEFORE the loss, not after it.
          #
          # Every other ring metric on this dashboard is a post-mortem count:
          # ring_full_total and frame_refused_total move once frames have
          # already been turned away. This is how long a frame WAITED before
          # the drain reached it, so it climbs while there is still headroom
          # left — which is the only window in which an operator can act.
          #
          # MAXIMUM, never Average. A mean hides the stall that matters: one
          # eight-second dwell inside a window of microsecond dwells averages
          # to approximately zero, and it is precisely that one frame that
          # says the drain stopped draining.
          #
          # Deliberately UNALARMED for now. The value has never been observed,
          # because until 2026-08-26 it was computed ~5,000 times a second and
          # thrown away; picking a threshold before there is a baseline invents
          # a number and then teaches the operator to ignore the alarm built on
          # it. Chart first, threshold when the chart has something to read.
          title  = "How far behind the drain is (worst frame wait, ms)"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_dhan_feed_ring_dwell_max_ms", { label = "worst ring wait", stat = "Maximum" }]
          ]
          period = 300
        }
      },
      {
        type   = "metric"
        x      = 0
        y      = 70
        width  = 8
        height = 6
        properties = {
          # A socket that keeps answering pings but stops delivering data.
          #
          # This failure defeats every other mechanism by construction. The
          # idle watchdog governs SILENCE, and a ponging socket is not silent.
          # The reconnect counters stay flat, because the defining property of
          # a deaf socket is that nothing about it is retrying. And the
          # LANE-level tick age reads about a second throughout, because
          # fifteen of the sixteen sockets are fine.
          #
          # Read it AGAINST the lane tick age, not alone: both low is healthy,
          # this one climbing while the lane stays flat is exactly one deaf
          # socket, and both climbing is the whole feed. That difference is the
          # diagnosis, which is why the two belong on the same screen.
          #
          # -1 means no connection has ticked yet — pre-open, or a lane that
          # has just started. Deliberately not 0, which would read as "every
          # socket ticked this instant" at the moment we know least.
          title  = "Deaf socket check: worst connection tick age (s), -1 = none yet"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_dhan_ws_worst_conn_tick_age_secs", { label = "worst socket", stat = "Maximum" }],
            [local.dash_namespace, "tv_dhan_feed_last_tick_age_secs", { label = "whole lane (for contrast)", stat = "Maximum" }]
          ]
          period = 300
        }
      },
      {
        type   = "metric"
        x      = 8
        y      = 70
        width  = 8
        height = 6
        properties = {
          # The other half of the ring. Read this WITH the dwell chart, never
          # alone — the pair is the diagnosis and neither number gives it:
          #
          #   both flat            -> healthy
          #   both climbing        -> the drain cannot keep up
          #   dwell flat, this up  -> large frames, not a slow drain
          #   dwell up, this flat  -> the drain is slow on small frames
          #
          # Until 2026-08-26 CloudWatch had tv_dhan_feed_ring_max_bytes (the
          # CAPACITY) and nothing for the occupancy — the denominator without
          # the numerator. `RingByteBudget::resident()` existed the whole time
          # with call sites only in its own unit tests.
          #
          # Percent rather than bytes because the two pools are sized 3:1, so
          # raw bytes are not comparable and the larger pool would dominate the
          # chart regardless of which one is in trouble.
          title  = "Ring fill (% of byte budget, worst pool)"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_dhan_feed_ring_resident_pct", { label = "worst pool", stat = "Maximum" }]
          ]
          period = 300
        }
      },
      {
        type   = "metric"
        x      = 8
        y      = 64
        width  = 8
        height = 6
        properties = {
          # Socket churn. A parked socket is the quiet one: it has stopped
          # retrying permanently and will never dial again, so a flat line here
          # after a spike is worse news than a rising one.
          #
          # `tv_dhan_feed_drain_respawn_total` is deliberately NOT charted: it
          # has ZERO emit sites and is not in the EMF allowlist, because the
          # drain is not respawned at all — if it dies the lane is over, and
          # the "lane up" gauge in the first panel falls to 0. Charting a
          # counter nothing writes would render as "no data", which reads as
          # "no restarts, all good" — the precise false-OK this row exists to
          # remove. The correct signal for a dead drain is already the leftmost
          # panel.
          title  = "Socket churn — dial failures / reconnects / parked forever"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_dhan_ws_dial_failed_total", { label = "dial failed", stat = "Sum" }],
            [local.dash_namespace, "tv_dhan_ws_reconnect_total", { label = "reconnects", stat = "Sum" }],
            [local.dash_namespace, "tv_dhan_ws_park_total", { label = "parked PERMANENTLY", stat = "Sum" }]
          ]
          period = 300
        }
      },
      {
        type   = "metric"
        x      = 16
        y      = 64
        width  = 8
        height = 6
        properties = {
          # The buffer budget is derived from the host's RAM at boot, so it is
          # no longer readable from the source. These two lines are how that
          # decision stays checkable: the ring should be ~2% of the host, and a
          # ring sitting at exactly the 256 MiB floor on a 32 GiB box means the
          # memory read failed and the lane is running with 16x less headroom
          # than intended — a fully healthy-looking degradation.
          title  = "Buffer budget vs host memory (sizing sanity check)"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_host_total_ram_bytes", { label = "host RAM measured", stat = "Maximum" }],
            [local.dash_namespace, "tv_dhan_feed_ring_max_bytes", { label = "buffer budget chosen", stat = "Maximum" }]
          ]
          period = 300
        }
      },
      {
        type   = "metric"
        x      = 0
        y      = 70
        width  = 24
        height = 6
        properties = {
          # Throughput. Non-vacuity for every panel above: all the loss charts
          # read flat-zero when the feed is working AND when it is delivering
          # nothing at all, and those two look identical. This is the line that
          # tells them apart.
          title  = "Feed throughput — frames in, ticks folded (this is the 'is it actually working' line)"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_dhan_feed_drain_frames_total", { label = "frames received", stat = "Sum" }],
            [local.dash_namespace, "tv_dhan_feed_ingest_ticks_total", { label = "ticks folded into candles", stat = "Sum" }],
            # Added 2026-08-21. Belongs on THIS widget rather than the loss
            # widget because it explains a flat line here: when the contract
            # universe fails to resolve, the ticks line stays low all session
            # for a reason that is not a feed fault at all — the lane simply
            # never subscribed the ~22,000 contracts it was authorized to.
            # Reading the two lines together separates "the feed is broken"
            # from "the feed is fine and carrying almost nothing".
            [local.dash_namespace, "tv_dhan_contract_universe_failed_total", { label = "contract universe defects (by reason, folded)", stat = "Sum" }],
            # Added 2026-08-18. The chain this widget exists to make legible
            # ran frames -> ticks and then STOPPED, while the loss widget
            # separately charted "candles discarded". So the operator could
            # see candles thrown away but never candles produced, and a
            # flat-zero discard line read identically whether the lane was
            # healthy or emitting nothing at all — the exact ambiguity the
            # comment above this widget was written to kill, left open one
            # stage further down.
            #
            # The gap has a dated cause: the old "Candle seals emitted" widget
            # was retired 2026-07-17 when its metric died with the tick
            # aggregator (correct at the time). The aggregator was REBUILT on
            # 2026-08-09 under a new metric name, and nothing restored it.
            [local.dash_namespace, "tv_dhan_feed_seals_emitted_total", { label = "candles sealed and sent", stat = "Sum" }],
            [local.dash_namespace, "tv_dhan_feed_ingest_refused_total", { label = "ticks refused", stat = "Sum" }]
          ]
          period = 300
        }
      },

      # ----- Row 12: the counters we PAY to ship and nobody could see -----
      # (2026-08-22 audit.) A sweep of the EMF allowlist against this file and
      # every alarm found 17 metrics that reach CloudWatch — and therefore
      # appear on the bill — while being charted nowhere and alarmed by
      # nothing. Nine of them count LOSS or FAILURE, which is the worst
      # possible thing to be paying for and not looking at: the loss is
      # measured, the measurement is discarded, and every surface stays green.
      #
      # DELIBERATELY charted rather than alarmed. `dhan-rest-only-noise-lock`
      # §2.3a already ruled on this trade for the live lane: each new pager
      # costs ~$0.10/mo, several of these are downstream symptoms of failures
      # that an existing alarm fires on first, and "a family of eleven pagers
      # for one subsystem trains an operator to ignore all of them". A chart
      # costs nothing, adds no page, and turns paid-for-and-invisible into
      # paid-for-and-visible.
      #
      # Both of this file's own checks were run per metric before charting:
      #   1. a producer exists in crates/*/src  (grep-verified 2026-08-22)
      #   2. the name is in the EMF allowlist   (that is how they were found)
      # So none of these renders "no data", which this file rightly calls
      # strictly worse than no widget at all.
      {
        type   = "text"
        x      = 0
        y      = 76
        width  = 24
        height = 2
        properties = {
          markdown = "## Quiet failures — measured, shipped, previously unwatched\nEvery line here should sit **flat at zero**. None of these raises an alarm on its own, by design: they are the second opinion you read when something else looks wrong, and the early warning you can spot before anything is wrong. A rising line means data was discarded, a write failed, or a boot step ran out of time."
        }
      },
      {
        type   = "metric"
        x      = 0
        y      = 78
        width  = 12
        height = 6
        properties = {
          title  = "Rows discarded or refused — anything above zero is data that did not land"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_ilp_rows_discarded_total", { label = "database rows discarded", stat = "Sum" }],
            [local.dash_namespace, "tv_depth_rows_dropped_total", { label = "depth rows dropped", stat = "Sum" }],
            [local.dash_namespace, "tv_rest_fetch_audit_rows_discarded_total", { label = "fetch-audit rows discarded", stat = "Sum" }],
            [local.dash_namespace, "tv_ws_frame_spill_drop_critical", { label = "raw frames lost before capture", stat = "Sum" }]
          ]
          period = 300
        }
      },
      {
        type   = "metric"
        x      = 12
        y      = 78
        width  = 12
        height = 6
        properties = {
          title  = "Writes and recovery that failed — the paths that keep the data safe"
          region = local.dash_region
          view   = "timeSeries"
          metrics = [
            [local.dash_namespace, "tv_depth_persist_errors_total", { label = "depth writes failed", stat = "Sum" }],
            [local.dash_namespace, "tv_rest_fetch_audit_persist_errors_total", { label = "fetch-audit writes failed", stat = "Sum" }],
            [local.dash_namespace, "tv_wal_replay_corrupted_segments_total", { label = "corrupted segments on replay", stat = "Sum" }],
            [local.dash_namespace, "tv_partition_archive_failed_total", { label = "archive to S3 failed", stat = "Sum" }],
            [local.dash_namespace, "tv_boot_deadline_exceeded_total", { label = "boot step ran out of time", stat = "Sum" }]
          ]
          period = 300
        }
      }
    ]
  })
}
