# =============================================================================
# Data-at-risk pagers — 2026-10-04
# =============================================================================
# AUTHORITY: dhan-rest-only-noise-lock-2026-07-14.md §2.9 (the operator tapped
# "Page me" on the 2026-10-04 data-at-risk decision card). Cost recorded in
# aws-budget.md "COST NOTE 2026-10-04".
#
# WHY THIS FILE EXISTS. Ten counters were counted on the box and published
# nowhere: none was in the EMF selector, so a failed cloud upload, a dropped
# order update or a dropped log line was visible only to someone reading
# /metrics on the host. They now ship (deploy/aws/cloudwatch-agent.json) and
# page here. Grouped by what the operator DOES, not by counter, on the
# loss-and-retention-alarms.tf precedent: ten counters, five pages.
#
# Every counter is registered at 0 at boot in crates/app/src/main.rs, so the
# CloudWatch agent's dropped first sample is the zero, not the incident
# (ratchet: crates/app/tests/alarmed_counters_are_seeded_guard.rs).
#
# Shared shape, all five:
#   treat_missing_data = notBreaching — the box is stopped outside the trading
#     window and these counters move only on a failure, so no-data is normal.
#   NO ok_actions — a delta returning to zero never means the files reached
#     S3, the update arrived or the lines came back (Rule 11).
#   NOT market-hours gated — uploads and prunes run mostly outside 09:00-15:40.
#   host dimension only — EMF folds every label (set, dir, sink, feed, reason)
#     into {host} by summing; every label value of every counter here is a
#     failure, so the sum counts nothing successful.
#
# COUNTER SHAPE (house residual, seal-drop-alarm.tf): the agent turns COUNTER
# samples into per-scrape deltas, so Sum over a period = events in the period.
# `tv_log_lines_dropped_total` is set with .absolute() from a cumulative
# count; it is still exposed as a counter, so the same delta rule applies.
# =============================================================================

# ---------------------------------------------------------------------------
# 1. CLOUD BACKUP IS FAILING — raw frames and cold files are not reaching S3
# ---------------------------------------------------------------------------
# The three counters, emit sites in crates/storage/src/raw_frame_upload.rs:
#   tv_raw_frame_upload_failed_total        WAL segment upload ended unmarked
#   tv_cold_file_upload_failed_total{set}   seal spill / quarantine upload failed
#   tv_raw_upload_marker_write_failed_total upload verified, marker write failed
# Nothing is deleted without a verified copy, so this is disk-fill risk, not
# loss. 2 of 3 periods because one failed upload is retried on the next pass
# (every 2 minutes outside the session); a failure that lasts half an hour is
# the page.
resource "aws_cloudwatch_metric_alarm" "cold_backup_failing" {
  alarm_name        = "tv-${var.environment}-cold-backup-failing"
  alarm_description = "CAPTURED MARKET DATA IS NOT REACHING CLOUD STORAGE. Sums three counters: raw frame segment uploads that failed, seal-spill and quarantine file uploads that failed, and verified uploads whose local marker could not be written. Nothing is deleted without a verified copy, so the files stay on the server and the disk fills instead. DO: (1) check S3 reachability and the instance role (aws s3 ls s3://tv-prod-cold/raw-frames/ from the box). (2) grep /tickvault/<env>/app for STORAGE-GAP-04 lines; they name the set and the error. (3) df -h /data - the disk alarms are the next thing to fire if this persists. Runbook: docs/error-runbooks/wave-2-error-codes.md"

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 3
  datapoints_to_alarm = 2
  treat_missing_data  = "notBreaching"
  alarm_actions       = local.app_alarm_actions
  ok_actions          = []

  metric_query {
    id          = "cold_backup_failures"
    expression  = "FILL(m1,0)+FILL(m2,0)+FILL(m3,0)"
    label       = "cloud backup failures (segments, files, markers)"
    return_data = true
  }

  metric_query {
    id          = "m1"
    return_data = false
    metric {
      metric_name = "tv_raw_frame_upload_failed_total"
      namespace   = local.app_namespace
      period      = 900
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }

  metric_query {
    id          = "m2"
    return_data = false
    metric {
      metric_name = "tv_cold_file_upload_failed_total"
      namespace   = local.app_namespace
      period      = 900
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }

  metric_query {
    id          = "m3"
    return_data = false
    metric {
      metric_name = "tv_raw_upload_marker_write_failed_total"
      namespace   = local.app_namespace
      period      = 900
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }
}

# ---------------------------------------------------------------------------
# 2. FILES DUE TO BE CLEARED ARE KEPT — no verified cloud copy
# ---------------------------------------------------------------------------
# The three counters:
#   tv_wal_prune_refused_not_uploaded_total{dir}  ws_frame_spill.rs, byte-ceiling pass
#   tv_seal_spill_prune_refused_not_uploaded_total seal_spill.rs, age pass
#   tv_quarantine_prune_refused_not_uploaded_total tick_persistence.rs, quarantine budget
# Each means a prune wanted to free space and could not, because the file has
# no verified copy. The WAL leg counts only the BYTE-ceiling pass (the age
# pass keeping an unuploaded segment is normal and logged at info), so a
# non-zero value means the disk is already over its ceiling. One period.
resource "aws_cloudwatch_metric_alarm" "disk_kept_not_backed_up" {
  alarm_name        = "tv-${var.environment}-disk-kept-not-backed-up"
  alarm_description = "OLD MARKET DATA FILES ARE DUE TO BE CLEARED BUT HAVE NO CLOUD COPY, SO THEY ARE KEPT. Sums three counters: WAL segments the byte-ceiling prune refused, seal-spill files the age prune refused, and quarantined files the quarantine budget refused - each because no verified S3 copy is recorded. The data is safe; the disk is not. DO: (1) check whether tv-<env>-cold-backup-failing is also in ALARM - that is the usual cause. (2) grep /tickvault/<env>/app for STORAGE-GAP-04 and WS-SPILL-01 lines; they name the directory and the byte count. (3) df -h /data. Fix the upload, never delete an unuploaded file by hand. Runbook: docs/error-runbooks/wave-2-error-codes.md"

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  datapoints_to_alarm = 1
  treat_missing_data  = "notBreaching"
  alarm_actions       = local.app_alarm_actions
  ok_actions          = []

  metric_query {
    id          = "prune_refused_not_uploaded"
    expression  = "FILL(m1,0)+FILL(m2,0)+FILL(m3,0)"
    label       = "files kept because no cloud copy exists"
    return_data = true
  }

  metric_query {
    id          = "m1"
    return_data = false
    metric {
      metric_name = "tv_wal_prune_refused_not_uploaded_total"
      namespace   = local.app_namespace
      period      = 900
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }

  metric_query {
    id          = "m2"
    return_data = false
    metric {
      metric_name = "tv_seal_spill_prune_refused_not_uploaded_total"
      namespace   = local.app_namespace
      period      = 900
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }

  metric_query {
    id          = "m3"
    return_data = false
    metric {
      metric_name = "tv_quarantine_prune_refused_not_uploaded_total"
      namespace   = local.app_namespace
      period      = 900
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }
}

# ---------------------------------------------------------------------------
# 3. AN ORDER UPDATE REACHED NOTHING
# ---------------------------------------------------------------------------
# crates/core/src/websocket/order_update_connection.rs: the broadcast send
# fails only when no receiver is subscribed, i.e. the consumer task died or the
# channel was torn down. The broker does not replay order updates, so each one
# is gone. Paper mode today; a breach now is the rehearsal failing.
resource "aws_cloudwatch_metric_alarm" "order_update_dropped" {
  alarm_name          = "tv-${var.environment}-order-update-dropped"
  alarm_description   = "AN ORDER UPDATE FROM THE BROKER REACHED NOTHING IN THE APP. The order-update socket received an update and no consumer was subscribed to take it - the consumer task crashed or its channel was torn down. Dhan does not replay order updates, so that update is missing from the order book and the audit tables. DO: (1) grep /tickvault/<env>/app for 'order update broadcast dropped' - each line names the order number. (2) look for a panic or respawn of the order-push consumer just before. (3) reconcile the named orders against the Dhan order book by hand. Order push is receive-only PAPER today."
  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  datapoints_to_alarm = 1
  metric_name         = "tv_order_update_broadcast_drops_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions
  treat_missing_data  = "notBreaching"
  alarm_actions       = local.app_alarm_actions
  ok_actions          = []
}

# ---------------------------------------------------------------------------
# 4. LOG LINES WERE DROPPED
# ---------------------------------------------------------------------------
# crates/app/src/observability.rs: each non-blocking log sink (app_log,
# errors_jsonl, the category files) drops a line when its 128,000-line buffer
# is full, and publishes the cumulative drop count every 10 s. A dropped
# ERROR line is a coded page that never reached the log filter, which is why
# this is a page and not a chart.
#
# 4a. THE SHIPPED-SINK SLICE (2026-10-10, noise lock §2.9-i, stage 1 of 3).
# Only two log files reach CloudWatch (cloudwatch-agent.json collect_list:
# machine/app.2* and machine/errors.jsonl.2*), so only a drop on THOSE sinks
# can hide a coded ERROR from its log filter. This filter slices the raw
# per-sink series in the metrics log down to sink = app_log or errors_jsonl
# and writes a DISTINCT derived name, on the seal-drop-alarm.tf naming rule:
# the raw name is still EMF-selected, so reusing it would count twice.
# Stage 1 adds the filter ONLY; the alarm below is unchanged and still reads
# the raw all-sink sum. It switches (stage 2) only after a measured match:
# SampleCount of the derived metric > 0 while the box runs, since both sliced
# labels are seeded at 0 at boot and publish a zero every scrape.
# Lockstep: crates/app/tests/log_drop_alarm_shipped_sinks_guard.rs.
resource "aws_cloudwatch_log_metric_filter" "log_lines_dropped_shipped" {
  name = "tv-${var.environment}-log-lines-dropped-shipped"
  # Agent-created group, referenced by name (metrics-log-metric-filters.tf
  # precedent; adopted into terraform by log-retention.tf).
  log_group_name = "/tickvault/${var.environment}/metrics"
  pattern        = "{ $.tv_log_lines_dropped_total = * && ($.sink = \"app_log\" || $.sink = \"errors_jsonl\") }"
  metric_transformation {
    name      = "tv_log_lines_dropped_shipped_total" # DERIVED sink-slice name, never the raw one
    namespace = local.app_namespace
    value     = "$.tv_log_lines_dropped_total"
    dimensions = {
      host = "$.host"
    }
    # No default_value: that knob emits datapoints for NON-matching events,
    # and AWS refuses it together with dimensions.
  }
}

resource "aws_cloudwatch_metric_alarm" "log_lines_dropped" {
  alarm_name          = "tv-${var.environment}-log-lines-dropped"
  alarm_description   = "LOG LINES WERE DROPPED. A log writer fell behind and its 128,000-line buffer filled, so lines were discarded - including, possibly, coded ERROR lines whose own alarms then never fired. DO: (1) treat the window as a blind spot: other alarms may have missed events in it. (2) look for the burst that caused it - the app logs one line per sink per 10 s while drops continue, naming the sink. (3) check disk write latency and df -h /data; a slow or full log volume is the usual cause."
  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  datapoints_to_alarm = 1
  metric_name         = "tv_log_lines_dropped_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions
  treat_missing_data  = "notBreaching"
  alarm_actions       = local.app_alarm_actions
  ok_actions          = []
}

# ---------------------------------------------------------------------------
# 5. THE LIVE FEED THREAD WROTE TO DISK ITSELF
# ---------------------------------------------------------------------------
# tick_persistence.rs / depth_persistence.rs: a rescue batch the frame drain
# could not hand to the rescue thread (queue full or thread gone) and could not
# park (park full, 8 batches / 128 MiB) is written inline on the drain. Nothing
# is dropped, but the drain is the only thing emptying the socket while it
# waits, and Dhan skips a slow consumer forward with no sequence number.
resource "aws_cloudwatch_metric_alarm" "feed_thread_wrote_to_disk" {
  alarm_name        = "tv-${var.environment}-feed-thread-wrote-to-disk"
  alarm_description = "THE LIVE PRICE THREAD HAD TO WRITE TO DISK ITSELF. A tick or depth rescue batch could not be handed to the rescue thread (queue full or thread gone) or parked, so the frame drain wrote it inline. Nothing in the batch is dropped, but the drain is the only thing emptying the socket while it waits, and Dhan skips a slow reader forward without telling us. DO: (1) check disk write latency and df -h /data. (2) grep /tickvault/<env>/app for the rescue fallback lines; reason=thread_gone means the rescue thread died and needs a restart. (3) check whether tv-<env>-errcode-hot-path-stall-01 fired in the same window."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  datapoints_to_alarm = 1
  treat_missing_data  = "notBreaching"
  alarm_actions       = local.app_alarm_actions
  ok_actions          = []

  metric_query {
    id          = "inline_fallbacks"
    expression  = "FILL(m1,0)+FILL(m2,0)"
    label       = "rescue batches written inline on the frame drain"
    return_data = true
  }

  metric_query {
    id          = "m1"
    return_data = false
    metric {
      metric_name = "tv_tick_rescue_inline_fallback_total"
      namespace   = local.app_namespace
      period      = 300
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }

  metric_query {
    id          = "m2"
    return_data = false
    metric {
      metric_name = "tv_depth_rescue_inline_fallback_total"
      namespace   = local.app_namespace
      period      = 300
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }
}
