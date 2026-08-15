# ===========================================================================
# Dhan LIVE-LANE integrity alarms (2026-08-14)
# ===========================================================================
#
# Authority: `.claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md`
# §2.3 — the dated operator quote that admits the LIVE-LANE family into the
# allowed Dhan alert set. That file's §3 makes any new Dhan-scoped page a
# REJECT without such a row, and the row was recorded BEFORE this file.
#
# ---------------------------------------------------------------------------
# WHY THIS FILE EXISTS
# ---------------------------------------------------------------------------
# The Dhan 16-socket live lane was revived on 2026-08-09 and, until today,
# carried **zero CloudWatch alarms and zero Telegram events**: a grep for
# `NotificationEvent` across the whole feed stack returns empty, and no
# `tv_dhan_*` metric appeared in any alarm in this directory. Every one of its
# failure modes — the lane not coming up, a socket parking permanently, ticks
# being dropped under backpressure — was discoverable only by opening the
# operator console, which (until the same day) reported a hardcoded constant.
#
# All three metrics below were ALREADY being published to CloudWatch by the
# EMF selector in `cloudwatch-agent.json` / `user-data.sh.tftpl`. We were
# paying to ship them and watching none of them. This file adds no new metric
# and no new EMF name — deliberately, because the rendered user-data template
# runs close to EC2's 16,384-byte cap and every added name eats that headroom.
#
# ---------------------------------------------------------------------------
# EMIT SITES VERIFIED 2026-08-14 (no dead monitors)
# ---------------------------------------------------------------------------
# This repo forbids alarms on metrics nothing emits — a filter that can never
# match reads as a permanently-green alarm forever. Each was checked in source:
#
#   tv_dhan_feed_stack_up   -> crates/app/src/dhan_feed_stack.rs (FEED_STACK_UP_GAUGE)
#   tv_dhan_ws_park_total   -> crates/core/src/websocket/pool_supervisor.rs (PARK_METRIC)
#   tv_ticks_dropped_total  -> crates/storage/src/tick_persistence.rs:784
#
# NOTE on the third: `app-alarms.tf` section 7/8/8b carries a 2026-07-18
# comment stating tv_ticks_dropped_total has "ZERO emit sites". That comment
# was TRUE when written — `tick_persistence.rs` had been deleted in the
# stage-2 dead-WS sweep — and is STALE now: the 2026-08-09 lane revival
# rebuilt that module, and it emits today. The old alarm was correctly retired
# then; this one is correctly added now. Both facts are true at their dates,
# which is exactly why the emit site is re-verified here rather than inherited
# from a comment.
# ===========================================================================

# ---------------------------------------------------------------------------
# 1. The lane is DOWN
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "dhan_live_lane_down" {
  alarm_name        = "tv-${var.environment}-dhan-live-lane-down"
  alarm_description = "The Dhan live market-data lane is NOT carrying data. tv_dhan_feed_stack_up is a gauge set to 1 only when sockets have been dialed AND the tick fold is consuming the ring, and cleared on EVERY exit path (normal, error, and nothing-dialed alike — before 2026-08-14 it was cleared only on the error path, so a lane whose sockets had all died reported itself healthy forever). Breaching means live ticks are not flowing: no candles from the live feed, and the 15:31 cross-verification will have nothing on its live side. Triage: journalctl -u tickvault for the lane bring-up refusals (WAL missing, rest_candle_fold conflict, empty universe, token not registered), then the operator console feeds panel."

  comparison_operator = "LessThanThreshold"
  threshold           = 1
  evaluation_periods  = 2
  metric_name         = "tv_dhan_feed_stack_up"
  namespace           = local.app_namespace
  # 300s x 2: the lane legitimately reports 0 during the seconds between boot
  # and first dial, so a single period would page on every restart.
  period    = 300
  statistic = "Maximum"
  # Maximum, not Average: this is a 0/1 gauge and any scrape showing 1 in the
  # window means the lane was up. Average would drag a healthy lane below the
  # threshold purely from the scrape that caught it mid-restart.
  dimensions = local.app_dimensions

  # notBreaching, NOT breaching: the box is stopped outside 08:30-17:30 IST by
  # design, and a stopped box publishes nothing. treat_missing_data=breaching
  # would page every single evening at shutdown — the fastest possible way to
  # train an operator to ignore this alarm.
  treat_missing_data = "notBreaching"

  alarm_actions = local.app_alarm_actions
  # The lane coming back up IS meaningful and self-explanatory, so the
  # recovery page is wanted here (unlike the permanent-loss alarms below).
  ok_actions = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# WHY THE TWO COUNTER ALARMS BELOW WATCH `{host}` ON A LABELLED METRIC
# ---------------------------------------------------------------------------
# Both counters carry Prometheus labels — `tv_dhan_ws_park_total` has
# `endpoint` + `reason` (pool_supervisor.rs), `tv_ticks_dropped_total` has
# `feed` (tick_persistence.rs) — yet both alarms declare `dimensions =
# local.app_dimensions` (`{host}`). That looks like a dimension mismatch and
# is not one.
#
# The CloudWatch agent's EMF processor publishes each selected metric under
# the dimension sets named in its `metric_declaration`, and this deployment
# declares exactly one: `"dimensions": [["host"]]`
# (deploy/aws/cloudwatch-agent.json + user-data.sh.tftpl). Extra Prometheus
# labels ride along as non-dimension EMF fields, so a labelled metric is
# FOLDED to `{host}` — summed across its label values — before it reaches
# CloudWatch. `{host}` is therefore its real and only dimension set.
#
# In-repo proof rather than assertion: when the 2026-07-06 silent-feed work
# needed a per-feed dimension it had to ADD A SECOND declaration
# (`[["host","feed"]]`), stating the reason verbatim — "host-only folding
# would mask a Dhan storm under the Groww baseline". That declaration was
# later retired; one `[["host"]]` declaration remains. Folding is exactly
# what these two alarms want: any socket parking, any tick dropped, on any
# label, must page.
#
# RECORDED BECAUSE IT WAS GOT WRONG HERE FIRST (2026-08-14, same day): a
# round-2 edit to this file asserted the opposite as fact — "No datapoint is
# ever published under {host} alone" — and shipped two log metric filters
# deriving host-only series to work around a problem that does not exist.
# That cost two duplicate paid series and broke the house rule in
# seal-drop-alarm.tf ("re-point this alarm at the EMF-published metric and
# remove the filter... to avoid paying for both series"), since both metrics
# are already in the EMF allowlist. The filters were deleted and the alarms
# restored to the raw EMF metric names.
#
# The lesson worth keeping is not about dimensions: a claim about how a
# remote system behaves was written into a comment as established fact
# without being checked against the config file two directories away that
# answers it.
#
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# 2. A socket has PARKED PERMANENTLY
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "dhan_socket_parked" {
  alarm_name        = "tv-${var.environment}-dhan-socket-parked"
  alarm_description = "A Dhan live-feed socket has PARKED PERMANENTLY and will not dial again this session. Parking is deliberate and correct — a 805 (too many connections) kills the OLDEST socket rather than rejecting the new one, so re-dialing destroys a healthy pool member, and a credential/entitlement rejection (806/808/810) just repeats forever. What is NOT correct is doing it silently, which is what happened until 2026-08-14: the counter incremented and nothing logged, so a socket could vanish from a 16-socket pool with no signal. The pool is now carrying fewer connections than planned and only a process restart restores it. Triage: the coded WS-GAP-03 line in /tickvault/<env>/app names the endpoint, connection index and park reason. PoolOverflow means another process is holding Dhan connections on this account; Fatal means credentials or entitlement need a human."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  metric_name         = "tv_dhan_ws_park_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions
  treat_missing_data  = "notBreaching"

  alarm_actions = local.app_alarm_actions
  # NO ok_actions. The counter falling back to zero deltas does NOT mean the
  # socket came back — it means no ADDITIONAL socket parked. The parked one is
  # still parked until the process restarts, so an OK page here would be a
  # false recovery.
  ok_actions = []
}

# ---------------------------------------------------------------------------
# 3. Ticks are being DROPPED
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "ticks_dropped" {
  alarm_name        = "tv-${var.environment}-ticks-dropped"
  alarm_description = "Live ticks were DROPPED before reaching QuestDB. This repo's own EMF notes call tv_ticks_dropped_total the single largest tick-loss window, and until 2026-08-14 it was published to CloudWatch with no alarm consuming it — paid for and unwatched. A drop here is real data loss: the frame is in the write-ahead log, but nothing re-folds from the WAL, so the row never appears in the ticks table. The usual cause is backpressure, not a bug: a QuestDB stall blocks the drain, the ring fills, and the sink drops the NEWEST frames. Triage: check QuestDB health and ILP flush latency first, then the ring-full counters (tv_dhan_ws_ring_full_total / tv_dhan_ws_ring_bytes_full_total) to confirm the ring was the choke point."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  metric_name         = "tv_ticks_dropped_total"
  namespace           = local.app_namespace
  # Threshold 1 / eval 1: a dropped tick is unrecoverable. There is no
  # "acceptable" number of them, and waiting for a second window to confirm
  # only delays the page while more are lost.
  period             = 300
  statistic          = "Sum"
  dimensions         = local.app_dimensions
  treat_missing_data = "notBreaching"

  alarm_actions = local.app_alarm_actions
  # NO ok_actions: the loss is permanent. Deltas returning to zero can never
  # mean the dropped ticks came back.
  ok_actions = []
}

# ---------------------------------------------------------------------------
# 4. The DURABLE FLOOR was breached (2026-08-15)
# ---------------------------------------------------------------------------
# This is the most serious counter in the lane, and until now it was the one
# nobody watched.
#
# Alarm 3 above watches `tv_ticks_dropped_total` — a loss BETWEEN the
# write-ahead log and QuestDB. The bytes still exist on disk there. This alarm
# watches a loss BEFORE the log: `WalRingSink` increments it when the
# capture-at-receipt guarantee — the property this entire architecture is
# built on — did not hold. The frame was never written anywhere.
#
# So the pair that shipped 2026-08-14 alarmed the RECOVERABLE half of the loss
# chain and left the UNRECOVERABLE half silent. There is no second signal for
# this one: the frame is simply gone, with no payload to count downstream and
# no error raised later. If this counter moves and nobody is told, the loss is
# both total and invisible.
#
# Authority: dhan-rest-only-noise-lock-2026-07-14.md §2.3a (operator quote
# 2026-08-15), which also WITHDRAWS the §2.3 drain-respawn row — that metric
# has zero emit sites because the drain is not respawned at all, so building
# it would have created a permanently-green dead monitor.
resource "aws_cloudwatch_metric_alarm" "dhan_wal_dropped" {
  alarm_name        = "tv-${var.environment}-dhan-wal-dropped"
  alarm_description = "A live frame was NEVER DURABLY CAPTURED. tv_dhan_ws_wal_dropped_total counts frames that failed the capture-at-receipt write-ahead log — the durable floor that every zero-loss claim in this system rests on. This is strictly worse than the ticks-dropped alarm: there, the frame is on disk and the loss is between the log and the database; here the bytes were never written at all, so no replay, no backfill and no cross-verification can recover them. There is no second signal for this condition anywhere in the system. Triage in this order: (1) disk — a full or read-only data volume is the most common cause, check tv_spill_dir_free_bytes and the WS-SPILL-01 alarm which may have fired first; (2) the WAL writer thread — WS-SPILL-02 indicates the spill channel was full at the append instant; (3) the coded lines in /tickvault/<env>/app naming the endpoint. Frames lost during the outage do NOT come back when the disk recovers."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  metric_name         = "tv_dhan_ws_wal_dropped_total"
  namespace           = local.app_namespace
  # Threshold 1 / eval 1, for the same reason as alarm 3 and more so: there is
  # no acceptable number of frames that missed the durable floor, and waiting a
  # second window to confirm only loses more of them.
  period    = 300
  statistic = "Sum"
  # `{host}` deliberately — see the dimension-folding note above alarm 2. The
  # EMF processor folds this metric's labels to the single declared dimension
  # set, and folding is what this alarm wants: a drop on ANY endpoint pages.
  dimensions         = local.app_dimensions
  treat_missing_data = "notBreaching"

  alarm_actions = local.app_alarm_actions
  # NO ok_actions. Deltas returning to zero mean no ADDITIONAL frames were
  # lost — never that the lost ones came back. An OK page here would be a false
  # recovery of data that does not exist.
  ok_actions = []
}
