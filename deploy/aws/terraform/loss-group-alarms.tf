# =============================================================================
# Loss-group pagers — 2026-10-05 (audit H1, N3)
# =============================================================================
# AUTHORITY: dhan-rest-only-noise-lock-2026-07-14.md §2.10 (the operator tapped
# "Turn on" on the 2026-10-05 card "Turn on the six extra phone alarms (H1) and
# the N3 counters?"). Cost recorded in aws-budget.md "COST NOTE 2026-10-05".
#
# WHY THIS FILE EXISTS. Fifty-five counters each count a refusal, a drop or a
# failed write on the box, and none reached CloudWatch. Some log a coded line
# no filter reads, one logs at warn!, some log nothing. They are grouped by
# what the operator DOES (the §2.9 / loss-and-retention-alarms.tf precedent):
# fifty-five counters, six pages.
#
# ROUTE (seal-drop-alarm.tf house pattern): no EMF name is added. Every 60 s
# scrape already ships each series as a plain JSON event, with $.host and each
# Prometheus label as a same-named field, into /tickvault/<env>/metrics. One
# log metric filter per counter extracts the per-scrape delta into ONE derived
# metric per group. Each group's metric has a DISTINCT tv_loss_* name, never a
# raw counter name, so a later EMF selection of a raw counter cannot be
# double-counted under these alarms. Filters are free; the group holds about
# 60 of its 100.
#
# SLICES. A label clause narrows a counter to its loss values where some of
# its values are normal (each exclusion and its reason is in §2.10).
#
# Every counter is registered at 0 at boot for every label value its emit
# sites use (crates/app/src/main.rs, or beside the emit site where a seed runs
# before the first possible increment), so the agent's dropped first sample is
# the zero, not the incident. Ratchet:
# crates/app/tests/alarmed_counters_are_seeded_guard.rs
# (every_loss_group_filter_counter_is_registered_at_boot).
#
# Shared shape, all six:
#   treat_missing_data = notBreaching - the box is stopped outside its window
#     and these counters move only on a failure.
#   NO ok_actions - a delta returning to zero never brings a row back (Rule 11).
#   NOT market-hours gated - dials, audit writes and syncs run outside the
#     session too.
#   host dimension only.
#
# COUNTER SHAPE (house residual, seal-drop-alarm.tf): the agent turns COUNTER
# samples into per-scrape deltas, so Sum over a period = events in the period.
# If the field ever proved cumulative, Sum overcounts and pages too eagerly -
# loud, never silent.
# =============================================================================

locals {
  # Filter key -> group, source counter, optional label slice (an extra
  # `&& ...` clause inside the JSON pattern, or "" for every label value).
  loss_group_filters = {
    # ---- 1. market data refused, set aside or discarded --------------------
    "close-drain-discarded" = { group = "market_data_refused", counter = "tv_dhan_ws_close_drain_discarded_total", slice = "" }
    "feed-aux-refused"      = { group = "market_data_refused", counter = "tv_feed_aux_rows_refused_total", slice = "" }
    # Depth only: the tick tier writes blind (not loss), depth refuses.
    "depth-probe-blind"           = { group = "market_data_refused", counter = "tv_spill_free_probe_blind_total", slice = " && $.tier = \"depth\"" }
    "spill-replay-lines-rejected" = { group = "market_data_refused", counter = "tv_tick_spill_replay_lines_rejected_total", slice = "" }
    "candle-int-heal-refused"     = { group = "market_data_refused", counter = "tv_candle_int_self_heal_refused_total", slice = "" }
    "top-volume-rows-discarded"   = { group = "market_data_refused", counter = "tv_top_volume_rank_rows_discarded_total", slice = "" }
    "top-volume-append-failed"    = { group = "market_data_refused", counter = "tv_top_volume_rank_append_failed_total", slice = "" }

    # ---- 2. subscription coverage lost -------------------------------------
    # spawn_skipped only: gave_up_outstanding_sockets can happen on a flat open.
    "dial-incomplete"           = { group = "subscription_gap", counter = "tv_dhan_dial_incomplete_total", slice = " && $.stage = \"spawn_skipped\"" }
    "subscribe-dispatch-failed" = { group = "subscription_gap", counter = "tv_dhan_ws_subscribe_dispatch_failed_total", slice = "" }
    "topup-failed"              = { group = "subscription_gap", counter = "tv_dhan_ws_topup_failed_total", slice = "" }
    "swap-failed"               = { group = "subscription_gap", counter = "tv_dhan_ws_swap_failed_total", slice = "" }
    "swap-timeout"              = { group = "subscription_gap", counter = "tv_dhan_ws_swap_timeout_total", slice = "" }
    # ack_pending / not_held retry next minute; rotation_halted follows an 805.
    "rebalance-swaps-refused" = { group = "subscription_gap", counter = "tv_depth_rebalance_swaps_refused_total", slice = " && ($.reason = \"no_socket\" || $.reason = \"channel_full\" || $.reason = \"channel_closed\")" }
    # empty_selection only: partial_selection is normal at a 09:00 attach.
    "depth-universe-failed" = { group = "subscription_gap", counter = "tv_dhan_depth_universe_failed_total", slice = " && $.reason = \"empty_selection\"" }

    # ---- 3. audit rows lost (N3: socket-audit write failures) --------------
    "ws-audit-write-errors"       = { group = "audit_rows", counter = "tv_ws_event_audit_write_errors_total", slice = "" }
    "feed-gap-audit-write-errors" = { group = "audit_rows", counter = "tv_feed_gap_audit_write_errors_total", slice = "" }
    # live_feed_forward already pages through HOT-PATH-02.
    "ws-audit-dropped"               = { group = "audit_rows", counter = "tv_ws_event_audit_dropped_total", slice = " && $.reason != \"live_feed_forward\"" }
    "order-update-ws-audit-dropped"  = { group = "audit_rows", counter = "tv_order_update_ws_audit_dropped_total", slice = "" }
    "xverify-persist-errors"         = { group = "audit_rows", counter = "tv_dhan_feed_xverify_persist_errors_total", slice = "" }
    "xverify-audit-rows-discarded"   = { group = "audit_rows", counter = "tv_dhan_live_xverify_audit_rows_discarded_total", slice = "" }
    "tf-verify-audit-rows-discarded" = { group = "audit_rows", counter = "tv_tf_verify_audit_rows_discarded_total", slice = "" }
    "audit-spill-refused"            = { group = "audit_rows", counter = "tv_audit_spill_refused_rows_total", slice = "" }
    "audit-spill-quarantined"        = { group = "audit_rows", counter = "tv_audit_spill_quarantined_rows_total", slice = "" }
    "audit-spill-replay-failed"      = { group = "audit_rows", counter = "tv_audit_spill_replay_failed_total", slice = "" }
    "pnl-audit-persist-errors"       = { group = "audit_rows", counter = "tv_pnl_audit_persist_errors_total", slice = "" }
    "order-leg-pnl-persist-errors"   = { group = "audit_rows", counter = "tv_order_leg_pnl_persist_errors_total", slice = "" }
    "order-leg-pnl-dropped"          = { group = "audit_rows", counter = "tv_order_leg_pnl_dropped_total", slice = "" }
    "order-alert-dropped"            = { group = "audit_rows", counter = "tv_order_alert_dropped_total", slice = "" }

    # ---- 4. durability sync failing (N3: WAL and seal sync failures) -------
    "wal-fsync-errors"                 = { group = "durability_sync", counter = "tv_wal_fsync_errors_total", slice = "" }
    "seal-spill-sync-failed"           = { group = "durability_sync", counter = "tv_seal_spill_sync_failed_total", slice = "" }
    "seal-unwritten-mark-errors"       = { group = "durability_sync", counter = "tv_seal_unwritten_mark_errors_total", slice = "" }
    "deferred-depth-persist-failed"    = { group = "durability_sync", counter = "tv_wal_deferred_depth_persist_failed_total", slice = "" }
    "applied-watermark-persist-failed" = { group = "durability_sync", counter = "tv_wal_applied_watermark_persist_failed_total", slice = "" }

    # ---- 5. order path dropped (paper mode today) --------------------------
    "order-update-frames-dropped" = { group = "order_path", counter = "tv_order_update_frames_dropped_total", slice = "" }
    "order-update-hollow-decode"  = { group = "order_path", counter = "tv_order_update_hollow_decode_total", slice = "" }
    "order-update-lagged"         = { group = "order_path", counter = "tv_order_update_receiver_lagged_total", slice = "" }
    "mark-forward-dropped"        = { group = "order_path", counter = "tv_mark_forward_dropped_total", slice = "" }
    "mark-producer-lost"          = { group = "order_path", counter = "tv_order_runtime_mark_producer_lost_total", slice = "" }
    "unknown-segment-fills"       = { group = "order_path", counter = "tv_oms_unknown_segment_fills_refused_total", slice = "" }
    "risk-fill-rejected"          = { group = "order_path", counter = "tv_risk_fill_rejected_total", slice = "" }
    "oms-fill-price-rejected"     = { group = "order_path", counter = "tv_oms_fill_price_rejected_total", slice = "" }
    "oms-order-book-full"         = { group = "order_path", counter = "tv_oms_order_book_full_total", slice = "" }
    "order-runtime-sid-refused"   = { group = "order_path", counter = "tv_order_runtime_sid_refused_total", slice = "" }
    "oms-aliases-refused"         = { group = "order_path", counter = "tv_oms_order_aliases_refused_total", slice = "" }
    "oms-correlations-refused"    = { group = "order_path", counter = "tv_oms_correlations_refused_total", slice = "" }

    # ---- 6. a safety bound reached, or a monitor blind ---------------------
    "spot-price-store-refused" = { group = "bound_or_blind", counter = "tv_spot_price_store_refused_total", slice = "" }
    "prev-close-store-refused" = { group = "bound_or_blind", counter = "tv_prev_close_store_refused_total", slice = "" }
    # zero_lot_window is normal; non_monotonic already logs VOLUME-MONO-01.
    "volume-leaderboard-refused"    = { group = "bound_or_blind", counter = "tv_volume_leaderboard_refused_total", slice = " && ($.reason = \"capacity\" || $.reason = \"window_bar_missing\")" }
    "depth-view-dropped-refused"    = { group = "bound_or_blind", counter = "tv_depth_view_dropped_refused_total", slice = "" }
    "depth-view-held-today-refused" = { group = "bound_or_blind", counter = "tv_depth_view_held_today_refused_total", slice = "" }
    "wal-lag-tracker-refused"       = { group = "bound_or_blind", counter = "tv_wal_lag_tracker_refused_total", slice = "" }
    "spill-dir-health-check-failed" = { group = "bound_or_blind", counter = "tv_spill_dir_health_check_failed_total", slice = "" }
    "resource-monitor-probe-failed" = { group = "bound_or_blind", counter = "tv_resource_monitor_probe_failed_total", slice = "" }
    "oom-monitor-probe-failed"      = { group = "bound_or_blind", counter = "tv_oom_monitor_probe_failed_total", slice = "" }
    "ws-activity-watchdog-panicked" = { group = "bound_or_blind", counter = "tv_ws_activity_watchdog_panicked_total", slice = "" }
  }
}

resource "aws_cloudwatch_log_metric_filter" "loss_group" {
  for_each = local.loss_group_filters

  name = "tv-${var.environment}-loss-${each.key}"
  # Agent-created group, referenced by name (metrics-log-metric-filters.tf
  # precedent; adopted into terraform by log-retention.tf).
  log_group_name = "/tickvault/${var.environment}/metrics"
  pattern        = "{ $.${each.value.counter} = *${each.value.slice} }"
  metric_transformation {
    # DERIVED group name tv_loss_<group>_total (see header). The six alarms
    # below name the same six metrics literally; the seeding guard checks the
    # two agree (the_loss_group_alarms_read_the_metrics_the_filters_write).
    name      = "tv_loss_${each.value.group}_total"
    namespace = local.app_namespace
    value     = "$.${each.value.counter}"
    dimensions = {
      host = "$.host"
    }
    # No default_value: that knob emits datapoints for NON-matching events.
  }
}

# ---------------------------------------------------------------------------
# 1. MARKET DATA REFUSED
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "market_data_refused" {
  alarm_name          = "tv-${var.environment}-market-data-refused"
  alarm_description   = "RECEIVED MARKET DATA WAS REFUSED, SET ASIDE OR DISCARDED. Sums seven counters: frames drained from a stale socket at reconnect, open-interest/market-status rows refused, depth spill rows refused because the free-space probe was blind, spill replay lines QuestDB refused (kept in quarantine), candles refused on a table still on the old id type, and top-volume rows that failed to append or were discarded. DO: (1) grep /tickvault/<env>/app for STORAGE-GAP-01, STORAGE-GAP-03 and TICK-SPILL-01 lines; they name the table and the cause. (2) check QuestDB health and df -h /data. (3) re-ingest data/spill/quarantine/*.rejected-lines once the cause is fixed. Authority: dhan-rest-only-noise-lock-2026-07-14.md section 2.10."
  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  datapoints_to_alarm = 1
  metric_name         = "tv_loss_market_data_refused_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions
  treat_missing_data  = "notBreaching"
  alarm_actions       = local.app_alarm_actions
  ok_actions          = []
}

# ---------------------------------------------------------------------------
# 2. SUBSCRIPTION COVERAGE LOST
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "subscription_coverage_lost" {
  alarm_name          = "tv-${var.environment}-subscription-coverage-lost"
  alarm_description   = "SOME PLANNED CONTRACTS ARE GETTING NO DATA. Sums seven counters: planned sockets the dial never spawned, subscribe dispatches that stopped part-way, top-up subscribes that failed, depth swaps that failed or timed out, decided depth swaps that never reached a socket, and a depth selection that came back empty. DO: (1) grep /tickvault/<env>/app for WS-GAP-02 and WS-GAP-03 lines; they name the socket, endpoint and how many instruments are uncovered. (2) if a socket is short and no 805 was reported, restart the lane in a quiet window. Authority: dhan-rest-only-noise-lock-2026-07-14.md section 2.10."
  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  datapoints_to_alarm = 1
  metric_name         = "tv_loss_subscription_gap_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions
  treat_missing_data  = "notBreaching"
  alarm_actions       = local.app_alarm_actions
  ok_actions          = []
}

# ---------------------------------------------------------------------------
# 3. AUDIT ROWS LOST
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "audit_rows_lost" {
  alarm_name          = "tv-${var.environment}-audit-rows-lost"
  alarm_description   = "AUDIT HISTORY ROWS WERE LOST. Sums fourteen counters: socket-lifecycle and feed-gap audit rows that failed to write, audit rows dropped at a full channel, cross-verification and timeframe-check audit rows discarded, order and P&L audit rows refused, quarantined or not replayed by the disk spill, P&L and order-leg rows that failed to write or were dropped, and order-side events dropped at a full channel. DO: (1) grep /tickvault/<env>/app for AUDIT-WS-01, AUDIT-06, STORAGE-GAP-03 and ORDER-PNL-01 lines; they name the table. (2) check QuestDB health and ls data/audit-spill/quarantine. (3) record the window as an audit gap. Authority: dhan-rest-only-noise-lock-2026-07-14.md section 2.10."
  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  datapoints_to_alarm = 1
  metric_name         = "tv_loss_audit_rows_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions
  treat_missing_data  = "notBreaching"
  alarm_actions       = local.app_alarm_actions
  ok_actions          = []
}

# ---------------------------------------------------------------------------
# 4. DURABILITY SYNC FAILING — 2 of 3 periods
# ---------------------------------------------------------------------------
# A sync is retried on the next attempt, so one transient device error is not
# the page; a device that keeps failing is.
resource "aws_cloudwatch_metric_alarm" "durability_sync_failing" {
  alarm_name          = "tv-${var.environment}-durability-sync-failing"
  alarm_description   = "THE SERVER COULD NOT CONFIRM SAVED DATA TO DISK. Sums five counters: frame log syncs that failed, candle spill file syncs that failed, crash-marker writes that failed, and the deferred-depth marks and applied-watermark files that could not be saved. A power cut now could lose the newest data. DO: (1) check the disk: dmesg for I/O errors, df -h /data, the EBS volume status. (2) grep /tickvault/<env>/app for WS-SPILL-01 and AGGREGATOR-SEAL-01 lines. Authority: dhan-rest-only-noise-lock-2026-07-14.md section 2.10."
  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 3
  datapoints_to_alarm = 2
  metric_name         = "tv_loss_durability_sync_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions
  treat_missing_data  = "notBreaching"
  alarm_actions       = local.app_alarm_actions
  ok_actions          = []
}

# ---------------------------------------------------------------------------
# 5. ORDER PATH DROPPED
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "order_path_dropped" {
  alarm_name          = "tv-${var.environment}-order-path-dropped"
  alarm_description   = "AN ORDER UPDATE, FILL OR PRICE FOR THE ORDER SYSTEM WAS DROPPED OR REFUSED. Sums twelve counters: order-update frames dropped or decoded hollow, order updates skipped by a lagging receiver, prices dropped or their producer lost, fills refused (unknown segment, bad price, risk engine), and orders, aliases, correlations or instruments refused at a size limit. DO: (1) reconcile the named orders against the Dhan order book. (2) grep /tickvault/<env>/app for ORDER-EVT-02, OMS-GAP and RISK-GAP lines. Paper mode today. Authority: dhan-rest-only-noise-lock-2026-07-14.md section 2.10."
  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  datapoints_to_alarm = 1
  metric_name         = "tv_loss_order_path_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions
  treat_missing_data  = "notBreaching"
  alarm_actions       = local.app_alarm_actions
  ok_actions          = []
}

# ---------------------------------------------------------------------------
# 6. A SAFETY BOUND REACHED, OR A MONITOR BLIND
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "safety_bound_or_monitor_blind" {
  alarm_name          = "tv-${var.environment}-safety-bound-or-monitor-blind"
  alarm_description   = "A FIXED-SIZE TABLE IS FULL OR A HEALTH CHECK IS BLIND. Sums ten counters: spot-price, previous-close and volume-ranking entries refused at their caps (or a ranking window lost), depth-view tracking refused at its cap, the WAL lag tracker full, the spill-disk, memory and OOM probes failing, and the order-update activity watchdog panicking. DO: read the log line naming the table or probe in /tickvault/<env>/app. A cap hit means the universe outgrew it; a failing probe means that monitor is not watching. Authority: dhan-rest-only-noise-lock-2026-07-14.md section 2.10."
  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  datapoints_to_alarm = 1
  metric_name         = "tv_loss_bound_or_blind_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions
  treat_missing_data  = "notBreaching"
  alarm_actions       = local.app_alarm_actions
  ok_actions          = []
}
