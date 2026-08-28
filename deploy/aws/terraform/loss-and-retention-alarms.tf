# =============================================================================
# Retention + data-loss counter pagers — 2026-08-25
# =============================================================================
# WHY THIS FILE EXISTS. Every metric alarmed here was ALREADY being scraped,
# already EMF-selected in `user-data.sh.tftpl`, already billed, and consumed by
# NOTHING — not an alarm, in most cases not even a dashboard widget. That is
# the shape `dhan-rest-only-noise-lock-2026-07-14.md` §2.3a names as worse than
# no counter at all: the loss is measured, the measurement is discarded, and
# every console stays green. `loss_counter_visibility_guard.rs` enforces
# REACHABILITY (shipped or logged); it explicitly does not enforce PAGING, and
# says so. This file is the paging half for the counters that earned it.
#
# NEW FILE, not additions to app-alarms.tf, deliberately: that file and
# silent-feed-alarms.tf are parsed by
# `crates/common/tests/cloudwatch_app_alarms_wiring.rs` for `metric_name`
# literals, and every name found there must ALSO appear in the EMF selector
# inside `user-data.sh.tftpl`. That template renders at exactly its
# 15,872-byte budget with ZERO free bytes (measured 2026-08-22, §2.3d-ii), so
# a file whose alarms are coupled to that budget is the wrong place to grow.
# Everything alarmed here is already in the selector, so nothing about that
# budget moves — but keeping the resources out of the coupled files keeps it
# that way by construction.
#
# COST (arithmetic, not an estimate). 3 alarms x $0.10/alarm-month = $0.30/mo
# on the house convention that a metric-math alarm bills as one alarm (the
# convention `order-side-alarms.tf` records for its own five-metric composite).
# AWS may instead bill a metric-math alarm per REFERENCED metric, which is the
# honest worst case: (1 + 5 + 3) = 9 alarm-metrics x $0.10 = $0.90/mo. ZERO
# new custom metrics and ZERO new EMF names either way — all nine series are
# already published. Against the $130 kill-ceiling whose 90% line is $117.
# =============================================================================

# ---------------------------------------------------------------------------
# 1. RETENTION IS NOT RUNNING — the archive step failed
# ---------------------------------------------------------------------------
# THE INCIDENT THIS CLOSES (live evidence, 2026-08-25). `partition_archive_audit`
# holds 1,000 rows and EVERY ONE of them is `outcome='s3_conflict'`. There has
# never been a success. Retention has therefore never dropped a partition: the
# oldest tick data on a nominal 1-day hot window is 2026-06-02, the root volume
# sits at 86%, and the projected fill date is mid-session 2026-08-27.
#
# One thousand failures reached nobody, because `tv_partition_archive_failed_total`
# was EMF-selected and alarmed by nothing. This is the single most consequential
# unwatched counter on the box: archive->verify->drop is the ONLY mechanism that
# frees disk (`partition_archive.rs` header, step 3), so its silent failure is
# the direct cause of the disk-fill trajectory that alarm 13b below now tracks.
#
# NOT MARKET-HOURS GATED, and that is load-bearing rather than an omission. The
# archive pass runs POST-MARKET — `main.rs` sleeps MARKET_CLOSE_DRAIN_BUFFER_SECS
# past the close and then calls `archive_and_drop_old_partitions`, and the
# disk-pressure loop (`disk_pressure_boot.rs`) can fire a pass at any hour. The
# market-hours gate CLOSES at 15:35 IST. Gating this alarm would therefore blind
# it to the run it exists to watch — the daily one.
#
# treat_missing_data = notBreaching: the box is stopped outside 08:30-17:30 IST
# weekdays and the counter only moves when a pass runs, so no-data is the normal
# state for most of the day and all weekend. `breaching` would page nightly. This
# alarm reports a FAILED ARCHIVE, never silence; a dead app is the boot-heartbeat
# and market-hours-liveness alarms' job.
#
# Threshold 1 on a 900s Sum of per-scrape deltas. There is no benign failure
# rate: every failure is a partition that stayed on disk. The daily post-close
# pass makes this an edge (ALARM after the run, back to OK when the deltas stop),
# not a latch — the anti-pattern alarm 13 in app-alarms.tf was just repaired for.
#
# COUNTER SHAPE (house residual, seal-drop-alarm.tf verbatim): the agent's
# prometheus pipeline converts COUNTER samples to per-scrape DELTAS, so Sum over
# the window = failures in the window. If that ever proved CUMULATIVE, Sum
# overcounts and this pages too eagerly — fail-loud, never a silent miss.
#
# LABEL FOLD IS SAFE HERE. EMF folds the `stage` label into `{host}` by summing.
# Every stage this counter carries is a failure stage (`record_failure` is its
# only caller — export / upload / verify / drop / s3_conflict), so nothing
# successful is folded in. That is NOT true of every counter — see the two
# EXCLUDED counters documented at the bottom of this file.
resource "aws_cloudwatch_metric_alarm" "partition_archive_failed" {
  alarm_name        = "tv-${var.environment}-partition-archive-failed"
  alarm_description = "RETENTION IS NOT FREEING DISK. The archive->verify->drop pass failed - the partition stayed on disk and nothing else reclaims space. Live state 2026-08-25: partition_archive_audit held 1,000 rows, ALL outcome='s3_conflict', zero successes ever; oldest ticks 2026-06-02 on a 1-day hot window; root volume 86%. DO: (1) SELECT outcome, count() FROM partition_archive_audit WHERE ts > dateadd('d',-1,now()) - the outcome names the stage. (2) s3_conflict means the object already exists with different content: check the cold bucket key and whether a prior half-run left a partial object. (3) export/upload/verify failures point at S3 creds, bucket policy or the ILP client. (4) Nothing is dropped without a verified S3 copy, so a failure is safe-but-stuck, never data loss. Runbook: docs/runbooks/may31-inplace-upgrade-and-access.md"

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  datapoints_to_alarm = 1
  metric_name         = "tv_partition_archive_failed_total"
  namespace           = local.app_namespace
  period              = 900
  statistic           = "Sum"
  dimensions          = local.app_dimensions
  treat_missing_data  = "notBreaching"
  alarm_actions       = local.app_alarm_actions
  # NO ok_actions. Deltas returning to zero means the pass ENDED, never that it
  # succeeded - the partitions it failed to archive are still on the disk. An OK
  # page here would read as "retention is working again" (Rule 11, no false
  # recovery). The real recovery signal is a success row in
  # partition_archive_audit and disk_used_percent falling.
  ok_actions = []
}

# ---------------------------------------------------------------------------
# 2. MARKET-DATA ROWS DID NOT REACH QUESTDB — one composite over FIVE counters
# ---------------------------------------------------------------------------
# WHY ONE ALARM AND NOT FIVE. These five counters are five MECHANISMS for one
# CONDITION: a tick or depth row that the pipeline produced did not land in the
# table. The operator's first move is identical in every case - look at QuestDB
# (ILP flush latency, WAL-suspended tables, disk) and then read the coded lines
# to see which mechanism and which stage. Five separate pagers would deliver
# five different names for one incident, and per
# dhan-rest-only-noise-lock-2026-07-14.md §2.3a "a family of eleven pagers for
# one subsystem trains an operator to ignore all of them". The precedent is
# `order_audit_chain_loss` in order-side-alarms.tf, which makes the same trade
# for the same reason.
#
# THE FIVE, with their emit sites verified in source 2026-08-25 (this repo
# forbids alarms on metrics nothing emits - a filter that can never match is a
# permanently-green dead monitor):
#   tv_tick_persist_errors_total    -> crates/storage/src/tick_persistence.rs
#   tv_tick_rows_refused_total      -> crates/storage/src/tick_persistence.rs
#   tv_depth_rows_dropped_total     -> crates/storage/src/depth_persistence.rs
#   tv_depth_persist_errors_total   -> crates/storage/src/depth_persistence.rs
#   tv_ilp_rows_discarded_total     -> crates/storage/src/ilp_overflow.rs
# All five are already in the EMF selector, so this adds no new metric name and
# no new EMF cost - it consumes five series we were already paying to ship.
#
# WHAT THIS IS NOT. `tv_ticks_dropped_total` and the spill tier
# (`tv_ticks_spilled_total`, `tv_tick_spill_replay_failed_total`) are alarmed
# separately by live-lane-alarms.tf under §2.3a/§2.3c. Those watch the RESCUE
# chain: a flush failed and the rows went to disk, recoverable. These five watch
# the PERSISTENCE result: rows refused, discarded, or errored on the way to the
# table. Depth in particular has no spill tier at all - `depth_persistence.rs`
# discards the buffer so one rejected row cannot wedge the session, and until
# now that discard was invisible.
#
# ALWAYS ARMED (no market-hours gate), on the seal-drop-alarm.tf reasoning: a
# discarded row is a permanently missing row at any hour, and the 17:30 shutdown
# flush is exactly when a persist failure is most likely and least excusable.
# The spill alarms joined the gate because a shutdown SPILL is deferred recovery;
# a shutdown DISCARD is loss.
resource "aws_cloudwatch_metric_alarm" "market_data_persistence_loss" {
  alarm_name        = "tv-${var.environment}-market-data-persistence-loss"
  alarm_description = "TICK or DEPTH ROWS DID NOT REACH QUESTDB. Sums five counters: tick persist errors, tick rows refused, depth rows dropped, depth persist errors, ILP pending rows discarded. These are the PERSISTENCE result, not the rescue chain - the spill/replay tier has its own alarms and those rows are recoverable; these are not. Depth has no spill tier at all: the writer discards its buffer so one rejected row cannot wedge the session, and those levels are gone from the table. DO: (1) check QuestDB first - ILP flush latency, tv_questdb_wal_suspended_tables, df -h /data. (2) grep /tickvault/<env>/app for HOT-PATH-02 lines; they name the feed, the stage and the row count. (3) the raw frames remain in the write-ahead log, so a bounded window may be replayable by hand. Runbook: docs/error-runbooks/wave-1-error-codes.md"

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  # M-of-N since 2026-08-28. Was evaluation_periods = 1, which re-entered
  # ALARM on every window with a non-zero delta: a flapping condition paged the
  # operator once per period all session (five alarms on a ~7-minute loop on
  # 2026-08-28). With 1-of-3 the FIRST breach still pages immediately - so
  # detection is not delayed by a single second - but the alarm stays in ALARM
  # through interleaved zero windows instead of resolving and re-firing.
  evaluation_periods  = 3
  datapoints_to_alarm = 1
  # notBreaching: the box is stopped outside 08:30-17:30 IST weekdays and these
  # counters only move under live traffic, so no-data is the normal overnight
  # state and `breaching` would page every night. This alarm reports LOST ROWS,
  # never silence.
  treat_missing_data = "notBreaching"
  alarm_actions      = local.app_alarm_actions
  # NO ok_actions. Cumulative loss counters: a delta returning to zero means no
  # ADDITIONAL rows were lost, never that the lost ones came back (Rule 11).
  ok_actions = []

  # Metric math SUM, with each leg's own metric NOT returned so only the total
  # drives the alarm state. FILL(m,0) on every leg is load-bearing: an
  # expression over five series evaluates to no-data if ANY leg is missing, and
  # a counter that legitimately did not increment in the window has no sample -
  # without FILL, four healthy legs would silence the fifth.
  metric_query {
    id          = "persistence_loss_total"
    expression  = "FILL(m1,0)+FILL(m2,0)+FILL(m3,0)+FILL(m4,0)+FILL(m5,0)"
    label       = "tick/depth rows lost at persistence (all five mechanisms)"
    return_data = true
  }

  metric_query {
    id          = "m1"
    return_data = false
    metric {
      metric_name = "tv_tick_persist_errors_total"
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
      metric_name = "tv_tick_rows_refused_total"
      namespace   = local.app_namespace
      period      = 300
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }

  metric_query {
    id          = "m3"
    return_data = false
    metric {
      metric_name = "tv_depth_rows_dropped_total"
      namespace   = local.app_namespace
      period      = 300
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }

  metric_query {
    id          = "m4"
    return_data = false
    metric {
      metric_name = "tv_depth_persist_errors_total"
      namespace   = local.app_namespace
      period      = 300
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }

  metric_query {
    id          = "m5"
    return_data = false
    metric {
      metric_name = "tv_ilp_rows_discarded_total"
      namespace   = local.app_namespace
      period      = 300
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }
}

# ---------------------------------------------------------------------------
# 3. THE DURABLE FLOOR WAS BREACHED — one composite over THREE counters
# ---------------------------------------------------------------------------
# WHY THIS IS A SEPARATE ALARM FROM #2, and not six metrics in one. Grouping is
# by what the operator DOES, not by how many counters exist. Alarm #2 says "the
# database did not take a row" - the raw frame is still in the write-ahead log
# and a bounded window may be replayable. THIS alarm says the WRITE-AHEAD LOG
# ITSELF failed, which is the tier every other recovery path assumes is intact.
# Same distinction the noise lock draws in §2.3a between
# `tv_ticks_dropped_total` (loss AFTER the WAL, recoverable in principle) and
# `tv_dhan_ws_wal_dropped_total` (loss BEFORE it, gone). Folding these three
# into #2 would hand the operator one page whose triage forks in two directions
# on the first step.
#
# THE THREE, emit sites verified in source 2026-08-25:
#   tv_ws_frame_spill_write_errors_total   -> capture-at-receipt WAL append failed
#   tv_ws_frame_spill_drop_critical        -> a raw frame was dropped at the WAL
#   tv_wal_replay_corrupted_segments_total -> a WAL segment could not be read back
# The first two are write-side; the third is read-side and fires at boot replay.
# They belong together because all three mean the same thing to a human: the
# durable floor cannot be trusted for that window, and there is no lower tier.
#
# ALWAYS ARMED. A WAL breach is unrecoverable at any hour by definition, and the
# boot-time replay leg fires at 08:30 - before the market-hours gate opens at
# 09:20, so gating would blind exactly the corrupted-segment case.
resource "aws_cloudwatch_metric_alarm" "durable_floor_breach" {
  alarm_name        = "tv-${var.environment}-durable-floor-breach"
  alarm_description = "THE WRITE-AHEAD LOG FAILED - the tier every other recovery path assumes is intact. Sums three counters: WAL append write errors, raw frames dropped at the WAL, and WAL segments that could not be read back at replay. This is loss BEFORE the durable floor, so unlike a persistence failure there is no lower tier and nothing to re-ingest: the bytes were never written, or were written and are unreadable. DO: (1) df -h /data and ls -la data/spill/ - a full or unwritable spill dir is the usual cause. (2) grep /tickvault/<env>/app for WS-SPILL-01 / WS-SPILL-02 lines. (3) corrupted segments at boot mean a prior session died mid-append; record the window, because those frames are not recoverable. Runbook: docs/error-runbooks/ws-frame-spill-error-codes.md"

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  datapoints_to_alarm = 1
  # notBreaching for the same reason as #2: the box is off outside the trading
  # window, so no-data is normal. This alarm reports a BREACH, never silence.
  treat_missing_data = "notBreaching"
  alarm_actions      = local.app_alarm_actions
  # NO ok_actions - the frames are gone; nothing about a zero delta restores
  # them (Rule 11).
  ok_actions = []

  metric_query {
    id          = "durable_floor_total"
    expression  = "FILL(m1,0)+FILL(m2,0)+FILL(m3,0)"
    label       = "write-ahead log breaches (write, drop, replay)"
    return_data = true
  }

  metric_query {
    id          = "m1"
    return_data = false
    metric {
      metric_name = "tv_ws_frame_spill_write_errors_total"
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
      metric_name = "tv_ws_frame_spill_drop_critical"
      namespace   = local.app_namespace
      period      = 300
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }

  metric_query {
    id          = "m3"
    return_data = false
    metric {
      metric_name = "tv_wal_replay_corrupted_segments_total"
      namespace   = local.app_namespace
      period      = 300
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }
}

# =============================================================================
# THE TWO COUNTERS DELIBERATELY *NOT* ALARMED HERE (2026-08-25)
# =============================================================================
# Both were on the list this file was opened to close. Neither is an oversight,
# and both reasons are the kind that only show up when you read the emit site
# rather than the metric name.
#
# tv_dhan_feed_ingest_refused_total — WOULD PAGE EVERY MORNING.
#   The counter carries a `reason` label with four values, and one of them is
#   `out_of_session`: the DESIGNED refusal for a tick that arrives outside the
#   fold window. The EMF processor folds labels into {host} by SUMMING (the
#   §2.3b finding), so the CloudWatch series adds the by-design pre-open
#   refusals to the three real ones. A threshold-1 alarm on the folded series
#   would fire on normal pre-open traffic every trading day - the exact
#   false-alarm class the silence gate was fixed for.
#   IT IS ALREADY ROUTED, correctly and with the exclusion built in: the drain's
#   30-second timer arm in `dhan_feed_stack.rs` reports the per-window DELTAS of
#   the other three reasons as an AGGREGATOR-DROP-01 `error!`, and deliberately
#   omits `out_of_session` at the emit site. That code has a metric-filter alarm
#   in error-code-alarms.tf. The right surface for this counter already exists;
#   a metric-side alarm would be strictly worse than the one we have.
#
# tv_dhan_feed_seals_dropped_total — ALREADY HAS TWO PAGERS.
#   Every path that increments it calls `seal_loss_alarm::record_lost_seal`
#   first, which emits AGGREGATOR-DROP-01 (verified in source: both the
#   NoDurableTier and BothDiskTiersFailed arms in `dhan_feed_stack.rs`). The log
#   is throttled to powers of two, but the FIRST loss always logs, so the
#   errcode alarm fires on drop #1. The condition also carries the counter-side
#   `tv-<env>-seal-writer-dropped` alarm in seal-drop-alarm.tf. A third pager
#   for one condition is the eleven-pagers-for-one-subsystem trap named above,
#   and it would cost $0.10/mo to make the operator trust the family less.
#
# Both stay charted and both stay reachable; what they do not get is a third or
# fourth name for a condition that is already paged. Revisit either only with
# evidence that the existing route missed a real episode.
# =============================================================================

# ---------------------------------------------------------------------------
# WAL-suspension monitoring is BLIND (added 2026-08-25).
#
# `tv_questdb_wal_suspended_tables` normally reports 0..n. When the
# wal_tables() probe fails for WAL_PROBE_BLIND_AFTER_FAILURES consecutive
# polls the watcher sets it to -1 instead of letting the last reading stand.
#
# That distinction is the whole point. A failed probe used to leave the gauge
# holding its previous value and log at `debug!` — so a stale `0` read as "no
# tables are suspended" while the watcher could no longer see anything. On
# 2026-08-25 fourteen tables stopped applying rows during a disk-full episode
# and the operator found out by asking why an order was missing.
#
# No new metric name: the gauge is already EMF-selected, so this costs one
# alarm (~$0.10/mo) and no user-data byte.
#
# `treat_missing_data = "notBreaching"`: the box is stopped outside market
# hours and publishes nothing then. A dark lane is owned by
# tv-<env>-dhan-no-ticks-flowing; this alarm reports a monitor that is running
# and cannot see, which is a different condition.
#
# No ok_actions: the sentinel clears the moment one probe succeeds, and a
# "recovered" page for a monitor that was briefly blind is noise. The recovery
# signal is the info! line the watcher emits on its first good poll.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "wal_suspension_probe_blind" {
  alarm_name          = "tv-${var.environment}-wal-suspension-probe-blind"
  alarm_description   = "WAL-SUSPENSION MONITORING IS BLIND. The per-table wal_tables() probe has failed for several consecutive polls, so the suspended-tables gauge is reporting UNKNOWN (-1) rather than a stale reading. Tables may be WAL-suspended right now with nothing able to say so - ILP keeps ACKing rows while they stop becoming visible. DO: (1) df -h /data first, a full volume causes both this and real suspensions; (2) check QuestDB answers http://127.0.0.1:9000/exec; (3) once it answers, the gauge returns to a live count on the next poll and the WAL-SUSPEND-01 filter resumes paging on real suspensions. Runbook: docs/error-runbooks/wal-suspension-error-codes.md"
  comparison_operator = "LessThanThreshold"
  evaluation_periods  = 1
  metric_name         = "tv_questdb_wal_suspended_tables"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Minimum"
  threshold           = 0
  treat_missing_data  = "notBreaching"
  dimensions          = local.app_dimensions
  alarm_actions       = local.app_alarm_actions
  ok_actions          = []
}
