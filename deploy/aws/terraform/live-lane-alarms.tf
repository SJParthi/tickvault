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

  # breaching, NOT notBreaching — safe ONLY because this alarm is now in the
  # market-hours gate's ALARM_NAMES list (market-hours-liveness-alarm.tf), so
  # its ACTIONS are disabled outside 09:20 IST -> close.
  #
  # Why this had to change: a gauge alarm on LessThanThreshold can only fire on
  # a datapoint that exists. A CRASHED app publishes nothing at all, so the
  # missing data was read as healthy and the lane-down alarm was blind to the
  # exact total-failure case it exists for. Live proof, 2026-08-18: the alarm's
  # own state reason read "no datapoints were received for 2 periods and 2
  # missing datapoints were treated as [NonBreaching]" while it sat in OK.
  #
  # The previous comment here was RIGHT that a bare flip to breaching would
  # page every evening at shutdown — "the fastest possible way to train an
  # operator to ignore this alarm". That is why the gate membership is part of
  # the same change and not a follow-up: without it this line is a regression.
  #
  # HONEST RESIDUAL: the gate opens at 09:20 IST but the persistence window
  # starts at 09:00, so a crash in [09:00, 09:20) still pages only via the
  # boot-heartbeat alarm's handover, not this one. Narrowing that seam means
  # moving the gate open time, which arms other alarms whose signals are not
  # valid before 09:20 — deliberately not done here.
  treat_missing_data = "breaching"

  # ADDED 2026-08-21, and it closes a latent defect rather than tidying style.
  #
  # This alarm is `breaching` and relies on the market-hours gate Lambda to
  # disarm it overnight — but it never declared a default, and terraform's
  # default for `actions_enabled` is TRUE. So every `terraform apply` shipped
  # it ARMED, against a metric whose absence breaches, on a box that is
  # deliberately stopped outside 08:30-17:30 IST. An apply at any hour outside
  # the session — and terraform-apply runs path-filtered on push and via the
  # post-merge catch-up dispatcher, so that is most hours — armed a breaching
  # alarm against a stopped box until the gate's next close run at 15:35 IST.
  #
  # Found by the generalised `breaching_alarms_are_gated_guard`, which was
  # widened in the same change to derive the breaching set from this file
  # instead of naming one alarm. The old guard checked gate MEMBERSHIP only,
  # which this alarm had, and passed.
  actions_enabled = false
  alarm_actions   = local.app_alarm_actions
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
  alarm_description = "Live ticks were PERMANENTLY lost before reaching QuestDB. This fires on dropped MINUS spilled, so it is silent when the rescue tier caught the rows and loud only when it did not. The usual cause is backpressure, not a bug: a QuestDB stall blocks the drain, the ring fills, and the sink drops the newest frames; the rescue path normally saves them to the tick spill tier, where they are re-ingestable by the curl command the HOT-PATH-02 log line prints. A non-zero value here means that rescue also failed - disk full, spill cap reached, or no permission. Triage: (1) check disk free and tv_ticks_spilled_total, which is charted beside this; (2) check QuestDB health and ILP flush latency; (3) tv_dhan_ws_ring_full_total confirms whether the ring was the choke point. Rows counted here are gone: nothing re-folds live-feed frames from the write-ahead log."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  # Eval 1 with datapoints_to_alarm 1: permanent loss must page on the first
  # window. This alarm does NOT get the M-of-N treatment its siblings do,
  # because it is already rare by construction - it fires only when the rescue
  # tier failed too.
  evaluation_periods  = 1
  datapoints_to_alarm = 1
  treat_missing_data  = "notBreaching"

  alarm_actions = local.app_alarm_actions
  # NO ok_actions: the loss is permanent. Deltas returning to zero can never
  # mean the dropped ticks came back.
  ok_actions = []

  # METRIC MATH, since 2026-08-28. This alarmed on `tv_ticks_dropped_total`
  # alone, and the live session of 2026-08-28 showed what that costs: dropped
  # and spilled were EQUAL to the row - 308,818 each - which is the documented
  # arithmetic proof that every dropped tick was rescued to disk and nothing
  # was permanently lost. The operator was paged anyway, repeatedly, and the
  # alarm text told him the rows were gone.
  #
  # The discriminator existed as data the whole time. `discard_pending` and
  # `rescue` increment BOTH names on the rescue-succeeded arm and only
  # `dropped` when the rescue itself fails, so `dropped - spilled` IS the
  # count of permanently lost rows. Nothing anywhere computed it: a scan of
  # the terraform found `tv_ticks_spilled_total` in one alarm and two
  # comments, and no expression subtracting the two.
  #
  # FILL(m,0) on both legs is load-bearing: an expression over two series
  # evaluates to no-data if EITHER is missing, and a counter that legitimately
  # did not increment in a window has no sample. Without FILL, a window where
  # nothing was rescued would silence the very loss this alarm exists for.
  metric_query {
    id          = "permanently_lost_ticks"
    expression  = "FILL(m_dropped,0)-FILL(m_spilled,0)"
    label       = "ticks dropped AND not rescued (permanent loss)"
    return_data = true
  }

  metric_query {
    id          = "m_dropped"
    return_data = false
    metric {
      metric_name = "tv_ticks_dropped_total"
      namespace   = local.app_namespace
      period      = 300
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }

  metric_query {
    id          = "m_spilled"
    return_data = false
    metric {
      metric_name = "tv_ticks_spilled_total"
      namespace   = local.app_namespace
      period      = 300
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }
}

# ---------------------------------------------------------------------------
# 4. The ring REFUSED frames (2026-08-14 — audit finding)
# ---------------------------------------------------------------------------
# Added after an adversarial tick-loss sweep found that the ring's own refusal
# counters were EMF-shipped and alarmed by nothing. That is the same
# paid-for-and-unwatched shape alarm 3 above was created to end, and it matters
# more here: a ring refusal is not backpressure that later drains, it is the
# frame being discarded. The sink-side doc-comment used to call this "NOT
# capture loss — replay recovers it"; that is FALSE as shipped, because boot
# replay drops every live-feed frame (there is no re-fold path). The drain-side
# log says so honestly. Until a WAL re-fold exists, a ring refusal is permanent
# loss and must page.
#
# Both counters are summed across their labels: `tv_dhan_ws_ring_full_total`
# (slot exhaustion, 65,536 frames) and `tv_dhan_ws_ring_bytes_full_total`
# (byte budget, 256 MiB). They are separate alarms because they have different
# causes and different fixes — slots mean the fold is behind, bytes mean a few
# large frames (depth-200 is 512 KiB, so 512 of them exhaust the whole budget
# and starve the main feed).
resource "aws_cloudwatch_metric_alarm" "ws_ring_full" {
  alarm_name        = "tv-${var.environment}-ws-ring-full"
  alarm_description = "The live-feed frame ring REFUSED frames because its 65,536 slots were full. Those frames are gone: boot replay deliberately drops live-feed WAL records, so nothing re-folds them into the ticks table. The cause is almost always downstream — a QuestDB stall or a slow ILP flush blocks the drain, so the ring backs up and the sink refuses the newest frames. Triage: check QuestDB health and flush latency first, then tv_ticks_dropped_total to see whether rows were also lost at the writer."

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
  metric_name         = "tv_dhan_ws_ring_full_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions
  treat_missing_data  = "notBreaching"

  alarm_actions = local.app_alarm_actions
  # NO ok_actions — a refused frame never comes back.
  ok_actions = []
}

resource "aws_cloudwatch_metric_alarm" "ws_ring_bytes_full" {
  alarm_name        = "tv-${var.environment}-ws-ring-bytes-full"
  alarm_description = "The live-feed frame ring REFUSED frames because its 256 MiB byte budget was exhausted, even though slots remained. This is the LARGE-FRAME shape rather than the slow-drain shape: a depth-200 frame can be 512 KiB, so roughly 512 of them consume the entire budget and every main-feed frame is then refused behind them. The frames are permanently lost — boot replay drops live-feed WAL records. Triage: confirm whether depth sockets are attached, then check whether the drain is also stalled (tv_dhan_ws_ring_full_total)."

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
  metric_name         = "tv_dhan_ws_ring_bytes_full_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions
  treat_missing_data  = "notBreaching"

  alarm_actions = local.app_alarm_actions
  ok_actions    = []
}

# ---------------------------------------------------------------------------
# 5. Master sourcing silently collapsed the universe (2026-08-14 — audit)
# ---------------------------------------------------------------------------
# The worst signal in the lane, because it looks exactly like health. When the
# resolved-master artifact is missing or unparseable, the lane falls back to the
# 4 hardcoded index SIDs while the config asks for the full resolved set — 4,565
# instruments on 2026-08-12, i.e. a 99.9% collapse. Every other gauge reads
# normal: the lane is up, ticks flow, and the gap detector reports zero
# never-ticked instruments because it only seeds what was actually subscribed.
# Before this alarm the sole evidence was one uncoded error line.
resource "aws_cloudwatch_metric_alarm" "live_universe_fallback" {
  alarm_name        = "tv-${var.environment}-live-universe-fallback"
  alarm_description = "The live lane fell back to the 4 hardcoded index instruments while the config requested the master-sourced universe. This is a ~99.9% collapse of the subscribed set that looks HEALTHY on every other signal — the lane is up, ticks flow, and never-ticked reads zero because only the subscribed instruments are seeded. Cause is the day's resolved-mapping artifact being missing or unparseable. Triage: check that the daily universe rider ran and wrote today's artifact, then restart the app; the universe is resolved once at boot and does not re-resolve mid-session."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  metric_name         = "tv_dhan_live_universe_fallback_total"
  namespace           = local.app_namespace
  # The universe is resolved ONCE per boot, so this fires at most once per
  # restart. A 300s window with threshold 1 catches that single increment.
  period             = 300
  statistic          = "Sum"
  dimensions         = local.app_dimensions
  treat_missing_data = "notBreaching"

  alarm_actions = local.app_alarm_actions
  # NO ok_actions: the counter is cumulative and the session is already running
  # on the wrong universe. Only a restart fixes it, and that is a new session.
  ok_actions = []
}

# ---------------------------------------------------------------------------
# 6. PARTIAL socket loss (2026-08-14 — audit)
# ---------------------------------------------------------------------------
# Alarm 1 above (lane down) can only see TOTAL failure: tv_dhan_feed_stack_up
# clears when the frame ring closes, and the ring closes only when EVERY sender
# is dropped. The planned-connections gauge is a boot-time constant. So four of
# five main-feed sockets could park and both signals would still read healthy
# while roughly 80% of the subscribed universe went dark.
#
# tv_dhan_ws_alive_connections is the state those two could not express: it is
# incremented before each supervisor task starts and decremented when that task
# returns, so it answers "how many sockets exist right now". The park counter
# fires on the transition, but a counter cannot be queried for current state at
# 09:30 — a delta that already scrolled past is not a health signal.
#
# Threshold is deliberately "fewer than 1" rather than "fewer than planned":
# the planned count varies with the resolved universe (4 index SIDs open one
# socket; 4,565 open five), so a fixed comparison would page every day the
# master sourcing legitimately changed shape. Partial loss above zero is caught
# by the park alarm; this catches the all-sockets-gone case that the ring-close
# gauge misses when a sender is still held somewhere.
resource "aws_cloudwatch_metric_alarm" "ws_no_alive_connections" {
  alarm_name        = "tv-${var.environment}-ws-no-alive-connections"
  alarm_description = "Every Dhan live-feed socket is gone, while the lane may still report itself up. The lane-up gauge only clears when the frame ring closes, which requires every sender to be dropped, so a lane holding a sender with zero live sockets reads healthy and produces nothing. Triage: journalctl -u tickvault for the park reasons (WS-GAP-03) — a 805/804/806/808/810 disconnect parks a socket permanently and nothing re-dials it, so the fix is a restart once the underlying cause (token, entitlement, subscription) is addressed."

  comparison_operator = "LessThanThreshold"
  threshold           = 1
  evaluation_periods  = 2
  metric_name         = "tv_dhan_ws_alive_connections"
  namespace           = local.app_namespace
  # 2 periods: a rolling restart legitimately passes through zero for a moment.
  # Two consecutive 5-minute windows at zero is not a restart, it is an outage.
  period     = 300
  statistic  = "Minimum"
  dimensions = local.app_dimensions
  # notBreaching, NOT breaching: outside market hours the lane is deliberately
  # down and the gauge is legitimately absent. Treating missing as breaching
  # would page every evening.
  treat_missing_data = "notBreaching"

  alarm_actions = local.app_alarm_actions
  # An OK here is a genuine recovery — sockets came back — so it is worth
  # sending, unlike the loss alarms above where recovery is impossible.
  ok_actions = local.app_alarm_actions
}

# ---------------------------------------------------------------------------
# 7. Captured frames were NOT recovered at boot (2026-08-14)
# ---------------------------------------------------------------------------
# The write-ahead log is the durability floor the entire capture design rests
# on: every frame is written to it BEFORE it is parsed. Until 2026-08-14 that
# log was write-ONLY — on boot the staged live-feed frames were counted,
# logged, and discarded — so a session that died mid-market lost every frame
# captured since its last flush, and no alarm existed because "loss at boot"
# had no metric anyone watched.
#
# The re-fold now recovers them, which makes THIS counter meaningful: it moves
# only when recovery did NOT happen — the feature is disabled, or the rows were
# built and the ILP flush failed. Either way the ticks are on disk and not in
# the database, which is precisely the state that needs a human.
resource "aws_cloudwatch_metric_alarm" "wal_frames_not_recovered" {
  alarm_name        = "tv-${var.environment}-wal-frames-not-recovered"
  alarm_description = "Live-feed frames captured by a previous session were replayed from the write-ahead log and NOT recovered into the database. The raw frames are preserved in the WAL archive, so this is recoverable — but not automatically, and not after the segments are archived. Causes, in order of likelihood: [dhan_wal_replay] enabled is false; or the re-fold built the rows and the QuestDB flush failed. Triage: journalctl -u tickvault for the STAGE-C.2b line, which names which of the two it was and how many frames were involved."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  metric_name         = "tv_ws_frame_wal_reinjected_dropped_total"
  namespace           = local.app_namespace
  # Boot-time only, so this fires at most once per restart. Threshold 1 with a
  # single period catches that lone increment.
  period             = 300
  statistic          = "Sum"
  dimensions         = local.app_dimensions
  treat_missing_data = "notBreaching"

  alarm_actions = local.app_alarm_actions
  # NO ok_actions: the counter is cumulative and the window is already past.
  # Only a deliberate manual recovery changes the outcome, and that is not an
  # event this alarm can observe.
  ok_actions = []
}

# ---------------------------------------------------------------------------
# 9. The DURABLE FLOOR was breached (2026-08-15)
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

# ---------------------------------------------------------------------------
# 10. TICKS LOST AT THE SPILL WRITER — the loss counter of LAST RESORT
#     (2026-08-19, operator Quote 17: "no ticks loss ... needs to be
#     fuckign preicse")
# ---------------------------------------------------------------------------
# Alarm 3 watches loss BETWEEN the write-ahead log and QuestDB (bytes still on
# disk). Alarm 9 watches loss BEFORE the log at the ring sink. THIS one watches
# the last remaining unwatched arm of the same durable floor: `ws_frame_spill`
# itself shedding a frame because the spill CHANNEL was full, or because the
# spill WRITER TASK was gone. Both arms are in
# `crates/storage/src/ws_frame_spill.rs` (`SpillDropCounters`) and both mean
# the same thing: the bytes were never written anywhere. No replay, no
# backfill and no cross-verification can recover them.
#
# WHY ONE ALARM AND NOT TWO — `tv_ws_frame_spill_drop_critical` IS DELIBERATELY
# NOT SEPARATELY ALARMED. Verified in source 2026-08-19: `SpillDropCounters`
# increments `tv_ws_frame_spill_drop_critical{ws_type}` on BOTH drop arms, and
# `tv_ticks_lost_total{source,ws_type}` on the SAME two arms
# (source="spill_drop_critical" for the channel-Full arm, source="spill_writer_dead"
# for the Disconnected arm). They are the identical event counted twice, and
# `ws_frame_spill.rs` is the ONLY production emitter of either. A second alarm
# would therefore page twice for one frame — the family-of-pagers-for-one-
# condition pattern that dhan-rest-only-noise-lock-2026-07-14.md §2.3a argues
# against, and which trains an operator to ignore both. `tv_ticks_lost_total`
# is the one kept because it is the workspace's explicit tick-loss SLA name and
# it carries the `source` label that says WHICH arm shed the frame. It stays on
# the dashboard, and if a future emit site ever increments
# `tv_ws_frame_spill_drop_critical` WITHOUT `tv_ticks_lost_total`, this comment
# is the record that the coverage assumption must be re-checked.
#
# EMIT SITE VERIFIED 2026-08-19: crates/storage/src/ws_frame_spill.rs — the
# handles are pre-registered with `increment(0)` at construction, so the first
# real increment is never swallowed as the agent's missing baseline sample.
resource "aws_cloudwatch_metric_alarm" "ticks_lost_spill" {
  alarm_name = "tv-${var.environment}-ticks-lost-spill"
  # NOTE: AWS caps alarm_description at 1024 characters. The long-form
  # reasoning that used to live in this string is above; the description
  # itself is the pager text and must stay under the cap. A terraform
  # validate failure on 2026-08-19 is why this is stated here.
  alarm_description = "Ticks LOST at the spill writer - unrecoverable. tv_ticks_lost_total counts frames the capture-at-receipt spill shed before reaching disk (source=spill_drop_critical: channel full; source=spill_writer_dead: writer task dead). No payload remains to replay and no backfill covers them. DO: (1) check disk - tv_spill_dir_free_bytes and the WS-SPILL-01 alarm; a full or read-only volume is the usual cause and usually fires first. (2) WS-SPILL-02 in /tickvault/<env>/app means the channel was full at append - check ILP flush latency and QuestDB health. (3) The coded WS-SPILL lines name ws_type (live feed vs order-update). Frames lost during the window do NOT come back when the cause clears."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  metric_name         = "tv_ticks_lost_total"
  namespace           = local.app_namespace
  # Threshold 1 / eval 1, identical to alarms 3 and 9 and for the same reason:
  # there is no acceptable number of frames that missed the durable floor, and
  # spending a second window to confirm only loses more of them.
  period    = 300
  statistic = "Sum"
  # `{host}` — the EMF processor folds this metric's source/ws_type labels into
  # the single declared dimension set. Folding is what this alarm wants: a loss
  # on ANY arm, on ANY socket type, pages. The labels survive in the coded log
  # lines, which is where triage reads them.
  dimensions = local.app_dimensions
  # notBreaching: the box is stopped outside 08:30-17:30 IST weekdays, so
  # no-data is the NORMAL overnight state and `breaching` would page every
  # night. This alarm reports LOSS, never silence — a dead app is the
  # boot-heartbeat and market-hours-liveness alarms' job, and those two are the
  # only ones in this repo that may use `breaching`.
  treat_missing_data = "notBreaching"

  alarm_actions = local.app_alarm_actions
  # NO ok_actions. A delta returning to zero means no ADDITIONAL frames were
  # lost — never that the lost ones came back. An OK page here would be a false
  # recovery of data that does not exist (Rule 11).
  ok_actions = []
}

# ---------------------------------------------------------------------------
# 11. NO TICKS ARE FLOWING (2026-08-21)
# ---------------------------------------------------------------------------
# Every alarm above answers "did something BREAK". None answers "is the feed
# PRODUCING anything", and those are different questions.
#
# A lane that dials, connects, subscribes and delivers nothing reports fully
# green: the lane-up gauge reads 1, the connection gauge reads healthy, and
# every loss counter reads zero, because nothing was lost — nothing arrived.
# That is exactly what the 2026-08-12 session looked like (compared: 0,
# missing_live: 373, 12 dial failures), and it was found by reading a
# cross-verify log line rather than by being told.
#
# Authorization + binding constraints: the dated §2.3b row in
# .claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md, with the
# same-day implementation-correction note recorded beneath it.
#
# WHY A GAUGE, AND NOT THE TICK COUNTER (corrected 2026-08-21, same day)
#
# The first version of this alarm read `tv_dhan_feed_ingest_ticks_total` with
# `Sum < 1`, and its own header conceded the residual that killed it: the CW
# agent's prometheus pipeline is BELIEVED to publish per-scrape deltas, but
# that has never been verified from this sandbox, and "if the field ever
# proved CUMULATIVE, Sum would be large and this alarm would go BLIND".
#
# Blind is the whole problem. Under cumulative values `Sum` over 300s is
# roughly five times the running session total, so `< 1` stops being true the
# instant the first tick of the morning lands — and the alarm written to prove
# ticks are flowing would report health for the rest of the day no matter what
# the feed did. A signal whose correctness rests on an unverified pipeline
# detail is not a signal; it is a coin flip, and this is the one alarm in the
# lane that must not be one.
#
# `tv_dhan_feed_last_tick_age_secs` removes the question instead of answering
# it. A GAUGE is published verbatim by both pipelines — there is no delta to
# compute for a value that is free to go down — so this alarm means exactly the
# same thing whichever reading is true. The counter stays as-is for the
# dashboard, where either reading is legible to a human.
#
# The gauge is also a STRICTLY better liveness definition than the counter was.
# It is stamped in `flush_and_record` only when a flush actually PERSISTED
# rows, so a QuestDB outage decays it while the socket is busy — correct,
# because during one the feed is not delivering. And it is published every 30s
# from the moment the drain starts, so the series is DENSE rather than being
# born on the first frame: the counter's handles are built lazily inside the
# frame arm, which is why the counter version needed `breaching` to catch a
# lane that never received anything at all.
#
# WHY breaching AND THE GATE, AS ONE THING: a dead app publishes no datapoint
# at all, so `breaching` is what makes process death visible here. But a bare
# flip to breaching pages every evening at 17:30 and all weekend — the fastest
# possible way to train an operator to ignore an alarm. Membership in the
# market-hours gate's ALARM_NAMES list is what makes it safe, and the alarm
# NAME is unchanged by this correction precisely so that membership survives.
resource "aws_cloudwatch_metric_alarm" "dhan_no_ticks_flowing" {
  alarm_name        = "tv-${var.environment}-dhan-no-ticks-flowing"
  alarm_description = "The Dhan live lane has not PERSISTED a tick for ~10 minutes DURING MARKET HOURS. This is the 'is it actually working' signal: sockets can be connected, the lane-up gauge can read 1, every loss counter can read zero, and still no market data reaches QuestDB - that is what the 2026-08-12 session looked like. Before the session's first tick the gauge reports the drain's own uptime, so a lane that dials and never receives anything pages instead of reassuring. Missing data breaches deliberately: a dead app publishes nothing at all. Triage: (1) tv_dhan_ws_alive_connections and tv_dhan_ws_dial_failed_total - are the sockets up. (2) tv_dhan_feed_drain_frames_total and tv_dhan_feed_ingest_ticks_total - if frames arrive but ticks do not, read tv_dhan_feed_ingest_refused_total whose reason label names why. (3) Is QuestDB accepting writes - this gauge is stamped on a flush that PERSISTED rows, so an ILP outage decays it while the socket is busy. (4) journalctl -u tickvault for WS-GAP-03 and the subscribe lines."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  # 300s of silence. The gauge is refreshed every 30s, so it crosses this
  # threshold five minutes after the last persisted tick and keeps climbing.
  threshold = 300
  # 2 x 300s. One aligned window can legitimately be quiet at the session
  # edges - the gate opens 09:20, five minutes after the 09:15 open, and closes
  # 15:35, five minutes after the 15:30 close. Ten consecutive minutes without
  # a persisted tick INSIDE the session is not an edge effect.
  evaluation_periods = 2
  metric_name        = "tv_dhan_feed_last_tick_age_secs"
  namespace          = local.app_namespace
  period             = 300
  # Maximum, not Average: a window that contains one fresh scrape and four
  # stale ones is still a window in which the feed was silent, and averaging
  # would let a single late tick erase four minutes of nothing.
  statistic = "Maximum"
  # `{host}` - the metric is unlabelled and the EMF processor declares exactly
  # one dimension set. Same folding note as alarm 2.
  dimensions = local.app_dimensions

  treat_missing_data = "breaching"

  # Actions OFF by default; the market-hours gate Lambda flips them ON
  # 09:20-15:35 IST Mon-Fri. WITHOUT THIS LINE THIS ALARM PAGES EVERY EVENING.
  actions_enabled = false
  alarm_actions   = local.app_alarm_actions
  # Ticks resuming IS a real, self-explanatory recovery - unlike the loss
  # alarms above, where a delta returning to zero can never mean the lost data
  # came back.
  ok_actions = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 12. THE CONTRACT UNIVERSE DID NOT RESOLVE (2026-08-21)
# ---------------------------------------------------------------------------
# `dhan_contract_universe.rs` carried ZERO metrics calls until today. Its
# failures — an unreadable artifact, an unreadable symbol map, no ladder built
# because no underlying priced, an ATM window silently shrunk below the
# authorized 25 — were `error!` lines and struct fields that nothing consumed.
# The 2026-08-20 incident is the shape: `atm_window_reason = "no_ladders"` was
# recorded, printed, and ignored, and the session ran without a single stock
# option.
#
# Authorization: the dated §2.3b row in
# .claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md.
#
# Every `reason` label value is a defect, deliberately: the EMF processor folds
# labels to {host} by summing, so a name carrying successes too would fire on a
# healthy day. Emitted ONCE per session at the terminal verdict, never per
# retry — `no_ladders` before 09:16 is normal (no tick has landed yet) and a
# per-attempt emit would page every trading morning.
resource "aws_cloudwatch_metric_alarm" "dhan_contract_universe_failed" {
  alarm_name        = "tv-${var.environment}-dhan-contract-universe-failed"
  alarm_description = "The Dhan contract universe did not resolve cleanly for today's session. reason=no_contracts: nothing was selected at all, so the main feed carries its spot universe only - no futures, no option contracts. reason=artifact_unreadable or symbol_map_unreadable: the daily rider's output is missing or malformed. reason=no_ladders: the master lists stock options and NOT ONE underlying had a live spot price, so at-the-money could not be located and every stock option is absent - the 2026-08-20 shape. reason=no_room: the connection envelope could not fit even the at-the-money strike. reason=window_shrunk: the ATM window was narrowed below the authorized 25 strikes per side. Fires ONCE per session at the terminal verdict. Triage: journalctl -u tickvault for the 'contract universe resolved' line, which names the counts, the window, the reason and now underlyings_total; then check the daily rider wrote today's contract artifact and mapping files."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  # Once-per-session emit, so one 300s window at threshold 1 catches the lone
  # increment. A second period would only delay a page for a condition that
  # cannot repeat.
  evaluation_periods = 1
  metric_name        = "tv_dhan_contract_universe_failed_total"
  namespace          = local.app_namespace
  period             = 300
  statistic          = "Sum"
  # `{host}` - the reason label folds, and folding is what this alarm wants:
  # any defect on any reason pages. The label survives in the log line, which
  # is where triage reads it.
  dimensions = local.app_dimensions
  # notBreaching, NOT breaching: the box is stopped overnight, so no-data is
  # the normal off-hours state. This alarm reports a DEFECT, never silence -
  # the dark-lane case belongs to alarms 1 and 11. No market-hours gate is
  # needed for the same reason, which is why this one is NOT in the gate list.
  treat_missing_data = "notBreaching"

  alarm_actions = local.app_alarm_actions
  # NO ok_actions. The universe is resolved ONCE per session. The counter
  # falling back to zero deltas means no ADDITIONAL defect - never that this
  # session's universe was repaired. Only a restart changes it, and that is a
  # new session.
  ok_actions = []
}

# ---------------------------------------------------------------------------
# 12. The SPILL TIER is failing (2026-08-21)
# ---------------------------------------------------------------------------
# Authority: dhan-rest-only-noise-lock-2026-07-14.md §2.3c (operator quote
# 2026-08-21, "Go ahead wirh your recommendation. Dude okay").
#
# The two alarms above watch the WAL tier — a frame lost BEFORE the log, and a
# frame the boot re-fold could not recover. Neither can see the SPILL tier,
# which is a different failure with a different remedy.
#
# When an ILP flush fails, `tick_persistence` writes the buffer to
# data/spill/ticks/ as line protocol instead of discarding it, and
# `tick_spill_replay` posts it back and truncates on success. That chain has
# two ways to end in real loss, and until now neither reached the operator:
#
#   1. Flushes keep failing and the spill directory grows. Past
#      TICK_SPILL_MAX_BYTES (512 MiB) the writer stops rescuing and drops.
#   2. The drain cannot put the rescued bytes back. The rows sit on disk,
#      valid and queryable by nobody, until (1) catches up with them.
#
# `tv_ticks_dropped_total` (alarm 3) already fires on a failed flush — both
# counters increment on that path deliberately, so the rescue can never make a
# real loss quieter. What THESE add is the distinction between "a flush failed"
# and "and it was rescued to disk", which is the difference between loss and
# deferred recovery, and between "the drain is working" and "the countdown to
# the cap has started".
#
# The SUCCESS counter (tv_tick_spill_replayed_bytes_total) has NO alarm --
# paging when recovery WORKS is the false-OK's mirror image -- and as of
# 2026-08-21 it does not reach CloudWatch at all. It was to ship unalarmed so a
# chart of successful recoveries could sit beside these two, but with it in the
# selector the rendered EC2 user-data came out past AWS's hard 16,384-byte cap.
# Given a real byte budget the ALARMED names win. The counter is still emitted
# and still readable on the box's :9091/metrics; it is simply not chartable
# off-box until the selector has room. deploy/aws/EMF-METRIC-SELECTOR-NOTES.md
# carries the wider record of that rationing.
resource "aws_cloudwatch_metric_alarm" "tick_spill_replay_failing" {
  alarm_name        = "tv-${var.environment}-tick-spill-replay-failing"
  alarm_description = "The automatic drain could not return rescued ticks to the database. The rows are NOT lost — they are on the box as valid line protocol in data/spill/ticks/ — but they are not queryable, and the spill directory is now growing toward its 512 MiB cap, past which the writer stops rescuing and starts dropping. Causes, in order of likelihood: QuestDB is down or refusing writes; or the disk is full so the drained file cannot be emptied. Triage: journalctl -u tickvault for TICK-FLUSH-01 and for the drain's round summary, then ls -la data/spill/ticks/ — a non-empty .ilp file is unrecovered ticks. Manual recovery, unchanged and always available: curl --data-binary @<file> http://<questdb>:9000/write"

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  metric_name         = "tv_tick_spill_replay_failed_total"
  namespace           = local.app_namespace
  # The drain runs every 5 minutes, so one period matches one round.
  #
  # notBreaching: the box is stopped overnight and publishes nothing, which is
  # health, not a failing drain. The dark-lane case belongs to
  # dhan_no_ticks_flowing, which treats missing data as breaching and is gated.
  period             = 300
  statistic          = "Sum"
  dimensions         = local.app_dimensions
  treat_missing_data = "notBreaching"

  alarm_actions = local.app_alarm_actions
  # The counter is cumulative and only a successful round changes the outcome.
  # A round succeeding does not un-happen the failure that preceded it.
  ok_actions = []
}

resource "aws_cloudwatch_metric_alarm" "ticks_spilling" {
  alarm_name        = "tv-${var.environment}-ticks-spilling"
  alarm_description = "A QuestDB tick flush failed and the buffer was rescued to disk instead of being discarded. No ticks are lost yet — this is the rescue tier working — but a flush failing at all means QuestDB write latency is degrading under the live load, and sustained spilling ends at the 512 MiB cap where the writer starts dropping for real. Triage: check QuestDB health and disk pressure first (make doctor), then watch whether tv-<env>-tick-spill-replay-failing follows: spilling that drains is deferred recovery, spilling that does not drain is a countdown."

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
  metric_name         = "tv_ticks_spilled_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions
  treat_missing_data  = "notBreaching"

  alarm_actions = local.app_alarm_actions
  ok_actions    = []
}

# ---------------------------------------------------------------------------
# 13. DEPTH STEERING STOPPED (2026-08-26)
# ---------------------------------------------------------------------------
# Both depth pools re-aim once a minute: depth-200 follows the at-the-money
# strike, and depth-20 now follows its index windows and the day's movers. Five
# counters describe how much MOVED. Not one of them answered the question an
# operator actually has at 14:00 — "is depth steering still running at all?" —
# and until today none of the five reached CloudWatch either.
#
# That question has several causes and exactly one symptom. The task panicked;
# the loop is wedged on a QuestDB query; every command channel closed; the
# stack never spawned it. In all four the sockets sit on whatever they held at
# 09:1x for the rest of the session, every other alarm stays green, and the
# dashboards show a feed delivering ticks normally — because it is. Depth is
# simply aimed at yesterday's strikes.
#
# WHY A GAUGE, NOT ANOTHER COUNTER. A counter that stops incrementing is
# indistinguishable from a quiet market, and this repository has already paid
# to learn the second half: the CloudWatch agent's prometheus pipeline is
# ambiguous about whether a `_total` arrives as a delta or a running
# cumulative, so an alarm written for the wrong reading is silently blind. A
# gauge is published verbatim either way — see alarm 11's header, which
# corrects exactly this mistake on the tick-flow signal.
#
# WHY IT IS PUBLISHED BY A SEPARATE TASK. The rebalance loop stamps a shared
# timestamp; a small ticker publishes `now - stamp` every 30 seconds. Had the
# loop published its own gauge, a wedged loop would freeze it at zero, and a
# frozen zero reads as perfectly healthy. Now a stall makes the number GROW,
# and a total task death stops the series entirely — which `breaching` catches.
# Both failure shapes are visible; neither was before.
#
# WHY THE GATE. `breaching` is what makes process death visible, but a bare
# flip to breaching pages every evening at 17:30 and all weekend. Membership in
# the market-hours gate's ALARM_NAMES list is what makes it safe.
resource "aws_cloudwatch_metric_alarm" "depth_steering_stalled" {
  alarm_name          = "tv-${var.environment}-depth-steering-stalled"
  alarm_description   = <<-EOT
    Depth has stopped re-aiming. The 20-level and 200-level connections are
    still up and still delivering, but they are pointed at whatever strikes
    they held when steering stopped — so the deepest book we collect is for
    contracts the money may have left. Nothing else will report this: every
    other depth signal describes what moved, and nothing moving is exactly
    the failure.
  EOT
  comparison_operator = "GreaterThanOrEqualToThreshold"
  # 180s against a 60s loop. Two missed minutes is a slow query or a busy
  # host; three is a stall.
  threshold          = 180
  evaluation_periods = 2
  metric_name        = "tv_depth_rebalance_age_secs"
  namespace          = local.app_namespace
  period             = 300
  # Maximum, not Average: a window holding one fresh reading and four stale
  # ones is still a window in which steering was stopped, and averaging would
  # let a single recovered minute erase four dead ones.
  statistic  = "Maximum"
  dimensions = local.app_dimensions

  treat_missing_data = "breaching"

  # Actions OFF by default; the market-hours gate Lambda flips them ON.
  # WITHOUT THIS LINE THIS ALARM PAGES EVERY EVENING.
  actions_enabled = false
  alarm_actions   = local.app_alarm_actions
  # Steering resuming is a real, self-explanatory recovery — the sockets
  # re-aim on the next minute and the age falls back to near zero.
  ok_actions = local.app_alarm_ok
}

# 14. ONE SOCKET WENT DEAF (2026-08-26) — operator-approved
# ---------------------------------------------------------------------------
# A socket that keeps answering pings but stops delivering data is invisible to
# every other mechanism, and each miss is STRUCTURAL rather than an oversight:
#
#   * the idle watchdog governs SILENCE, and a ponging socket is not silent;
#   * the reconnect family stays flat — the defining property of a deaf socket
#     is that nothing about it is retrying, which is why "alarm the reconnect
#     counters", the recommendation on record, cannot catch it;
#   * alarm 11 above (dhan_no_ticks_flowing) reads the LANE. With fifteen of
#     sixteen sockets delivering, the lane's last tick is always ~1 s old
#     however dead the sixteenth is;
#   * tv_dhan_ws_alive_connections counts sockets DIALED, not DELIVERING.
#
# The gauge reports the WORST connection that has ever delivered, so a single
# deaf socket moves it while alarm 11 stays flat — and that DIFFERENCE is the
# diagnosis.
#
# THRESHOLD 600, NOT INVENTED: alarm 11 pages at 300 s × 2 periods = ten
# minutes of lane silence. This is the same question scoped to one socket, so
# it uses the same ten minutes, expressed as one 600 s crossing rather than two
# 300 s ones because this gauge is a per-socket age that only climbs.
#
# notBreaching, NOT breaching, and that differs from alarms 11 and 13 on
# purpose. Those flip to breaching because a DEAD APP publishes nothing and
# absence must page. That case is already covered by them — adding a third
# alarm that pages on the same absence buys nothing and triples the noise of
# one outage. This one answers a narrower question that only has meaning while
# the lane is alive: "of the sockets that ARE running, is one of them deaf?"
#
# GATED ANYWAY. Without the gate this pages every single trading day at ~15:40:
# after the 15:30 close every socket legitimately stops delivering, the gauge
# climbs past 600 within ten minutes, and the alarm fires on a market that is
# simply shut. That is the fastest way to train an operator to ignore it.
resource "aws_cloudwatch_metric_alarm" "dhan_worst_socket_deaf" {
  alarm_name        = "tv-${var.environment}-dhan-worst-socket-deaf"
  alarm_description = "ONE Dhan socket has delivered nothing for ~10 minutes DURING MARKET HOURS while the lane as a whole looks healthy. This is the deaf-socket case: a connection that keeps answering pings but stops sending data never trips the idle watchdog (it is not silent), never reconnects (nothing is retrying), and does not move the lane-level tick-age gauge (the other fifteen sockets are fine). Read this AGAINST tv_dhan_feed_last_tick_age_secs on the live-lane dashboard row: this one climbing ALONE is a single deaf socket; both climbing is the whole feed and alarm 11 will have fired too. Triage: (1) tv_dhan_ws_worst_conn_tick_age_secs_by_pool and the per-connection detail on /metrics name WHICH socket. (2) tv_dhan_ws_alive_connections - is it still dialed. (3) tv_dhan_feed_instruments_never_ticked - a subscribe that silently did not take looks the same from here. (4) journalctl -u tickvault for WS-GAP-03 and the subscribe batches. A value of -1 means no connection has ticked yet and is NOT a fault."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 600
  evaluation_periods  = 1
  metric_name         = "tv_dhan_ws_worst_conn_tick_age_secs"
  namespace           = local.app_namespace
  period              = 300
  # Maximum, never Average: a window holding one fresh scrape and four stale
  # ones is still a window in which that socket was silent, and averaging would
  # let a single late frame erase four minutes of nothing.
  statistic = "Maximum"
  # `{host}` — the metric is unlabelled and the EMF processor declares exactly
  # one dimension set. Per-socket attribution lives on /metrics deliberately:
  # sixteen CloudWatch dimensions would cost ~$4.80/mo to answer a yes/no
  # question this one series answers.
  dimensions = local.app_dimensions

  # A stopped box publishes nothing, and that is health rather than a deaf
  # socket. Absence-must-page is alarms 11 and 13's job, not this one's.
  treat_missing_data = "notBreaching"

  # Actions OFF by default; the market-hours gate Lambda flips them ON
  # 09:20-15:35 IST Mon-Fri. WITHOUT THIS LINE THIS ALARM PAGES EVERY EVENING
  # AT ~15:40, when every socket legitimately stops delivering.
  actions_enabled = false
  alarm_actions   = local.app_alarm_actions
  # A socket resuming IS a real, self-explanatory recovery — unlike the loss
  # counters, where a delta returning to zero never means the data came back.
  ok_actions = local.app_alarm_ok
}

# ---------------------------------------------------------------------------
# 14. WAL REPLAY MET A RECORD FORMAT IT CANNOT READ — the deploy-rollback
#     tick-loss path (2026-08-28, dhan-rest-only-noise-lock §2.3j)
# ---------------------------------------------------------------------------
# Alarms 3, 9 and 10 watch frames lost on the way IN. This one watches frames
# lost on the way BACK OUT, and it is the only arm of the durable floor whose
# failure leaves no evidence at all.
#
# A binary that meets a segment magic it does not recognise cannot decode the
# frames inside it. Until 2026-08-27 it returned an EMPTY replay — which is
# byte-identical, to every caller, to a segment that genuinely held nothing —
# and the segment was then staged and archived as successfully replayed. The
# frames are gone permanently: no payload left to count, no parse left to
# fail, no downstream consumer that knew they existed.
#
# The concrete path is a DEPLOY ROLLBACK. A `TVW3` segment written this
# morning, read by a rolled-back binary that knows only `TVW1` and `TVW2`,
# is destroyed silently. That is capture-at-receipt failing in the one
# direction nothing else in this system can see.
resource "aws_cloudwatch_metric_alarm" "wal_replay_unknown_magic" {
  alarm_name        = "tv-${var.environment}-wal-replay-unknown-magic"
  alarm_description = "WAL replay met a record format this binary cannot read, and the frames in that segment are PERMANENTLY LOST — they are not recoverable by replay, backfill or cross-verification, and no other signal in this system reports them. The overwhelmingly likely cause is a DEPLOY ROLLBACK: a segment written by a newer binary (record magic TVW3, which carries a receipt timestamp) being read by an older one that knows only TVW1/TVW2. Triage: (1) confirm which binary is on the box and whether it was rolled back — compare the running version against the last deploy; (2) STOP rolling further back, since each older binary destroys more segments; (3) roll FORWARD to a binary that reads TVW3, then let the remaining staged segments replay. Do NOT clear the spill directory to make this alarm stop: that destroys the segments a forward-roll could still recover. Frames already reported here do not come back."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  metric_name         = "tv_wal_replay_unknown_magic_total"
  namespace           = local.app_namespace
  # Threshold 1 / eval 1, for the same reason as alarm 9: there is no
  # acceptable number of segments to destroy, and a second confirming window
  # only lets the boot finish destroying the rest of them.
  period     = 300
  statistic  = "Sum"
  dimensions = local.app_dimensions
  # UNGATED, deliberately, and this differs from most of family (5).
  # WAL replay runs at BOOT. The box boots at 08:30, before the market-hours
  # gate opens at 09:20 — so gating this alarm would make it structurally
  # incapable of firing on the only path it exists to watch. It is also the
  # correct shape for an out-of-hours deploy, which is exactly when a rollback
  # happens.
  treat_missing_data = "notBreaching"

  alarm_actions = local.app_alarm_actions
  # NO ok_actions. The counter returning to zero means no ADDITIONAL segments
  # were destroyed — never that the destroyed ones came back. An OK page here
  # would be a false recovery of data that does not exist.
  ok_actions = []
}

# ---------------------------------------------------------------------------
# Family (5) alarm 16 — the seal writer is dead or saturated.
#
# Authorized 2026-08-28 by dhan-rest-only-noise-lock-2026-07-14.md §2.3l.
#
# This one exists because the alarm that was SUPPOSED to cover it publishes
# from inside the task whose death it reports: tv_seal_writer_drain_total
# {kind="dropped"} is incremented in the seal writer's own drain loop, so if
# that loop exits the input stops moving and the alarm reads a flat, healthy
# zero. The failure silences its own detector.
#
# This counter is incremented by the PRODUCER instead — the drain, which is
# still running — when a seal cannot be handed to the writer and is escalated
# to disk. On a healthy lane it is exactly zero.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "seal_writer_rescued" {
  alarm_name        = "tv-${var.environment}-seal-writer-rescued"
  alarm_description = "Sealed candles could not reach the seal writer and were escalated to disk. On a healthy lane this is exactly zero, so any reading means the writer is dead, its channel is saturated, or the whole session is running on the disk tier. This is the ONLY signal for that: the seal-writer drain alarm publishes from inside the writer's own loop, so a dead writer stops moving it and it reads a healthy zero. Triage: (1) check whether the seal writer task is alive at all - a panic aborts the process, so a quiet exit or a persistently full channel is the case this catches; (2) check the spill and DLQ directories for the escalated seals, which are recoverable text; (3) a restart respawns the writer, since it has none of its own. Candles already escalated are on disk, not lost - but they are not in the database until they are replayed."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  metric_name         = "tv_dhan_feed_seals_rescued_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions

  # notBreaching: the box is stopped overnight, so no-data is the normal
  # off-hours state. This alarm reports a DEFECT, never silence — the dark-lane
  # case belongs to dhan-no-ticks-flowing.
  treat_missing_data = "notBreaching"

  alarm_actions = local.app_alarm_actions
  # NO ok_actions. The counter falling back to zero means no ADDITIONAL seals
  # were escalated; it does not mean the writer recovered or that the escalated
  # seals reached the database.
  ok_actions = []
}

# ---------------------------------------------------------------------------
# Family (5) alarm 17 — an instrument was permanently refused a candle slot.
#
# Authorized 2026-08-28 by dhan-rest-only-noise-lock-2026-07-14.md §2.3l.
#
# AGGREGATOR_MAX_SLOTS is 25,000 against an authorized universe of ~24,600, and
# slots are never released — the per-minute ATM re-fit is additive, so the count
# only climbs through the session. Past the cap an instrument derives NO candles
# for the process lifetime. Its ticks are still persisted, so nothing else
# reports it.
#
# Until now the only detection was a LATCHED log line (only the first refused
# instrument ever logs) plus a dashboard chart. The metric was already
# EMF-selected; it was simply never alarmed.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "aggregator_slots_exhausted" {
  alarm_name        = "tv-${var.environment}-aggregator-slots-exhausted"
  alarm_description = "An instrument was refused a candle slot and will produce NO candles for the rest of this process. The slot table holds 25,000 and is never released, while the per-minute at-the-money re-fit keeps adding instruments, so the count only climbs through a session. Its ticks are still being persisted - only the candles are missing - so no other signal reports it, and the log line latches after the first instrument, meaning instruments two onward are silent. Triage: (1) compare the subscribed universe against the slot ceiling; (2) a restart clears the table but will refill it the same way, so this is a sizing question, not a transient; (3) candles for the refused instruments cannot be backfilled from ticks after the session. Any non-zero reading means the universe outgrew the ceiling."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  metric_name         = "tv_aggregator_slot_exhausted_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions

  # notBreaching, same reasoning as the alarm above.
  treat_missing_data = "notBreaching"

  alarm_actions = local.app_alarm_actions
  # NO ok_actions. The candles for a refused instrument do not arrive later.
  ok_actions = []
}

# ---------------------------------------------------------------------------
# Family (5) alarm 18 — tick rows set aside as permanently refused.
#
# Authorized 2026-08-28 by dhan-rest-only-noise-lock-2026-07-14.md §2.3l.
#
# A 4xx from QuestDB means the PAYLOAD is wrong and no retry can fix it, so the
# replay moves the whole file aside to let the rest of the backlog drain. That
# is correct behaviour, and it strands real ticks: one malformed row quarantines
# the entire file. The quarantine docstring records 1,292 of 1,293 rows in one
# such file being perfectly good.
#
# Those files are NEVER retried and, until this change, counted toward no size
# ceiling at all — they accumulated with no bound on the same volume that filled
# completely on 2026-08-25. They are now inside the spill ceiling, so this alarm
# is what says they exist while there is still room to act.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "tick_spill_quarantined" {
  alarm_name        = "tv-${var.environment}-tick-spill-quarantined"
  alarm_description = "A rescued tick file was QUARANTINED: QuestDB refused it with a 4xx, meaning the payload is wrong and no retry can fix it, so the file was set aside to let the rest of the backlog drain. Those rows are not in the database and will never be replayed automatically. One malformed row quarantines a whole file, so the loss is usually far larger than the defect - a recorded case had 1,292 good rows out of 1,293. Triage: (1) look in the quarantine subdirectory of the tick spill dir; the files are readable text; (2) find the malformed line, which is the actual bug, and fix the writer; (3) the remaining rows can be replayed by hand once the payload issue is understood. Quarantined bytes now count toward the spill ceiling, so leaving them there eventually refuses new rescues."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  metric_name         = "tv_tick_spill_replay_quarantined_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions

  treat_missing_data = "notBreaching"

  alarm_actions = local.app_alarm_actions
  # NO ok_actions. A quarantined file does not un-quarantine itself; the counter
  # settling means no NEW file was set aside, not that the old one was recovered.
  ok_actions = []
}

# ---------------------------------------------------------------------------
# Family (5) alarm 19 — the boot catch-up drain ran out of budget.
#
# Authorized 2026-08-28 by dhan-rest-only-noise-lock-2026-07-14.md §2.3l.
#
# The catch-up drain replays WAL segments left over from a previous process,
# bounded by WAL_CATCHUP_BUDGET_SECS / WAL_CATCHUP_MAX_ROUNDS so a large backlog
# cannot hold the boot open forever. Exhausting it is SAFE for the segments
# themselves — they stay on disk and survive to the next boot.
#
# What it is not safe for is TIME: those leftovers are then subject to the
# age-and-byte prune, which the prune's own comment admits can delete the last
# copy of a shed frame. A backlog that never drains eventually gets pruned
# instead of replayed, and nothing said so.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "wal_catchup_budget_exhausted" {
  alarm_name        = "tv-${var.environment}-wal-catchup-budget-exhausted"
  alarm_description = "The boot catch-up drain ran out of its time or round budget with write-ahead log segments still unreplayed. Those segments are NOT lost right now - they stay on disk and the next boot will try again - but they are now racing the age-and-byte prune, which deletes old segments and can take the last copy of a frame that was never folded. A backlog that never drains is eventually pruned instead of replayed. Triage: (1) measure the spill directory size against the replay budget - if the backlog is larger than one pass can carry, it will never catch up on its own; (2) the usual cause is a long outage or a slow database, so check whether the write path is keeping up now; (3) a manual replay is possible while the segments still exist. This fires at boot, so it is normal to see it once after a long outage and alarming to see it every morning."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  metric_name         = "tv_wal_catchup_budget_exhausted_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions

  # UNGATED and notBreaching, like the WAL-replay alarms: the catch-up drain
  # runs at BOOT, and the box boots at 08:30 - before the market-hours gate
  # opens - so gating it would make it structurally unable to fire.
  treat_missing_data = "notBreaching"

  alarm_actions = local.app_alarm_actions
  ok_actions    = []
}

# ---------------------------------------------------------------------------
# Family (5) alarm 19 — a writer shutdown that abandoned its queue.
#
# Authorized 2026-08-28 by dhan-rest-only-noise-lock-2026-07-14.md §2.3n.
#
# The lane joins two writer threads at shutdown. Either join can time out, and
# when it does the thread is deliberately detached rather than joined - joining
# after a timeout is the unbounded wait the grace exists to avoid. The batches
# that thread held then die with the process: up to the queue depth plus the one
# in flight, which at the depth writer's row threshold is roughly 50,000 rows.
#
# Until this alarm, no counter moved. The two alarmed loss series on this path
# are incremented by the writer thread on rows it actually refused, and a thread
# that never ran its drain increments neither - so the largest loss the shutdown
# path can produce was reported by free text in a log and by nothing else.
#
# An EPISODE counter, not a row count: the queue belongs to a thread we have
# just given up on and the batch row counts were consumed when they were sent,
# so any number here would be invented. It counts "a writer shutdown was
# abandoned", once, per writer.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_metric_alarm" "offload_writer_shutdown_incomplete" {
  alarm_name        = "tv-${var.environment}-offload-writer-shutdown-incomplete"
  alarm_description = "A persistence writer did NOT finish its shutdown: it either ran past the join grace or panicked, and the batches it was still holding died with the process. At the depth writer's row threshold that is roughly 50,000 rows, and they are not recoverable - the queue is in memory, not on disk. This counts episodes, not rows, because the row counts are gone with the thread. Triage: (1) check the tick and depth spill directories - rows the writer REFUSED were rescued there and are re-ingestable, rows still queued were not; (2) look for a preceding QuestDB stall, which is what makes a drain overrun its grace; (3) if this repeats, the grace is too short for the observed flush latency, or the writer is panicking - the accompanying log line says which. Fires once at process exit, so a return to OK means the datapoint aged out, never that the rows arrived."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 1
  metric_name         = "tv_offload_writer_shutdown_incomplete_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions

  # notBreaching: the box is stopped overnight, so no-data is the normal
  # off-hours state. The dark-lane case is owned by dhan-no-ticks-flowing.
  treat_missing_data = "notBreaching"

  alarm_actions = local.app_alarm_actions
  # NO ok_actions. This fires once, at process exit. A return to OK is the
  # datapoint aging out of its window - the abandoned queue does not come back.
  ok_actions = []
}

# ---------------------------------------------------------------------------
# 2026-09-02 — the four loss counters that had emit sites and no route
#
# Authorized by dhan-rest-only-noise-lock-2026-07-14.md §2.3r (operator
# "whatevr is needed and recommended go ahead dude okay? i just need the
# workign finalsied solution dude okay?"). All four already had real emit
# sites in crates/*/src and were EMF-selected in the same change; none had an
# alarm, so each could fire in production and reach nobody.
#
# All four are ZERO on a healthy lane by construction, so the threshold is 1
# rather than a rate: there is no baseline to calibrate against and any
# non-zero reading IS the event. All four take ok_actions = [] — the loss has
# already happened when they fire, and a counter aging out of its window is
# not a repair.
#
# M-of-N (3/1) matches the ticks_spilling precedent above: the FIRST breach
# still pages immediately, but the alarm stays in ALARM through interleaved
# zero windows instead of resolving and re-firing on a flapping condition.
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_metric_alarm" "tick_rescue_abandoned" {
  alarm_name        = "tv-${var.environment}-tick-rescue-abandoned"
  alarm_description = "The tick RESCUE thread did not finish within the shutdown budget, so a payload counted as rescued may never have reached the spill file. This is the alarm that fires EXACTLY when 'dropped equals spilled' — the equality this system reports as proof that nothing was lost — stops being true. Those rows are not in QuestDB and may not be on disk either. Triage: check the spill directory for the named file before the next session, and treat any tick-count reconciliation for that session as unproven until you have."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 3
  datapoints_to_alarm = 1
  metric_name         = "tv_tick_rescue_abandoned_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions
  treat_missing_data  = "notBreaching"

  alarm_actions = local.app_alarm_actions
  ok_actions    = []
}

resource "aws_cloudwatch_metric_alarm" "depth_rescue_abandoned" {
  alarm_name        = "tv-${var.environment}-depth-rescue-abandoned"
  alarm_description = "The same signal as tv-<env>-tick-rescue-abandoned, on the depth writer — which carries roughly 24 times the tick row volume, so it is the larger exposure of the two. Depth levels counted as rescued may never have reached the depth spill file. Triage: identical — verify the named spill file exists before trusting any depth reconciliation for that session."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 3
  datapoints_to_alarm = 1
  metric_name         = "tv_depth_rescue_abandoned_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions
  treat_missing_data  = "notBreaching"

  alarm_actions = local.app_alarm_actions
  ok_actions    = []
}

resource "aws_cloudwatch_metric_alarm" "wal_spill_shutdown_incomplete" {
  alarm_name        = "tv-${var.environment}-wal-spill-shutdown-incomplete"
  alarm_description = "The WAL frame-spill writer was abandoned at shutdown with frames still queued. Capture-at-receipt is the durable floor this whole architecture rests on, and this says the floor did not close cleanly: those raw frames were never written, so no later boot can re-glob them and no downstream counter can see the gap. Triage: this is un-recoverable loss for the frames in flight — record the session and check whether the shutdown was a deploy, a stop, or a kill."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 3
  datapoints_to_alarm = 1
  metric_name         = "tv_wal_spill_shutdown_incomplete_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions
  treat_missing_data  = "notBreaching"

  alarm_actions = local.app_alarm_actions
  ok_actions    = []
}

resource "aws_cloudwatch_metric_alarm" "wal_replay_restore_failed" {
  alarm_name        = "tv-${var.environment}-wal-replay-restore-failed"
  alarm_description = "A WAL segment the replay budget DEFERRED could not be moved back into the live directory. The deferral itself is normal and designed — whatever a boot does not reach stays a *.wal file and is re-globbed next boot — but a segment that fails to be restored is one no later boot will find. Triage: look for orphaned segments in the staging directory and move them back by hand before the next boot; the frames are still on disk until someone deletes them."

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 3
  datapoints_to_alarm = 1
  metric_name         = "tv_wal_replay_restore_failed_total"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  dimensions          = local.app_dimensions
  treat_missing_data  = "notBreaching"

  alarm_actions = local.app_alarm_actions
  ok_actions    = []
}

# ---------------------------------------------------------------------------
# Candle-fold refusal RATE (§2.3t of dhan-rest-only-noise-lock-2026-07-14.md,
# operator-authorized 2026-09-03)
# ---------------------------------------------------------------------------
#
# ⚠ THIS IS NOT A TICK-LOSS ALARM, and the description must never say it is.
# A refused tick IS persisted -- only its candle contribution is skipped. The
# emitting log line states it verbatim: "The ROWS ARE STILL WRITTEN -- this is
# not tick loss." This file has already had to withdraw that claim once today;
# it is not being reintroduced through an alarm description.
#
# ⚠ A RATIO, NOT A COUNT, and that is the load-bearing choice. The raw counter
# scales with market activity -- far higher in the opening burst than at midday
# -- so any absolute threshold either pages every morning or is invisible at
# lunch. `100 * refused / ingested` is scale-invariant.
#
# Threshold 25% is ~3x the highest share ever measured here (2.41% on
# 2026-08-27, 7.0% on 2026-08-28, ~8.5% on 2026-09-03). Below that the reading
# sits inside a range this system has run at for a week; above it, a quarter of
# all ticks carry timestamps the fold rejects, which is a vendor feed degrading.
resource "aws_cloudwatch_metric_alarm" "aggregator_refusal_rate_high" {
  alarm_name        = "tv-${var.environment}-aggregator-refusal-rate-high"
  alarm_description = "More than a quarter of incoming ticks are being refused a candle bucket. THE ROWS ARE STILL BEING WRITTEN - this is not tick loss and no price data is missing. What it means is that Dhan's timestamps are degrading: ticks stamped for another trading day, or outside a plausible time band, cannot be placed in a minute bucket. Measured baselines here are 2.4%, 7.0% and 8.5%, so a sustained reading above 25% is a vendor data-quality regression, not normal variance. Triage: (1) the 30-second WARN line in the app log breaks the refusals down by reason - stale_trading_day and out_of_band_timestamp are the two abnormal ones; (2) check whether it is one instrument or the whole universe; (3) candles for the refused ticks cannot be rebuilt later, but the ticks themselves are queryable and a backfill from them is possible; (4) nothing in this codebase can fix a vendor's clock - if it persists it is a support ticket."

  comparison_operator = "GreaterThanThreshold"
  threshold           = 25
  evaluation_periods  = 2
  datapoints_to_alarm = 2

  # No data = no ticks, which is NOT this alarm's question -
  # `dhan-no-ticks-flowing` owns that. A zero denominator yields no datapoint,
  # which this setting handles correctly.
  treat_missing_data = "notBreaching"

  metric_query {
    id          = "refusal_pct"
    expression  = "100 * refused / ingested"
    label       = "Ticks refused a candle bucket (%% of ingest)"
    return_data = true
  }

  metric_query {
    id = "refused"
    metric {
      metric_name = "tv_aggregator_tick_refused_total"
      namespace   = local.app_namespace
      period      = 300
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }

  metric_query {
    id = "ingested"
    metric {
      metric_name = "tv_dhan_feed_ingest_ticks_total"
      namespace   = local.app_namespace
      period      = 300
      stat        = "Sum"
      dimensions  = local.app_dimensions
    }
  }

  alarm_actions = local.app_alarm_actions
  # NO ok_actions. A ratio falling back is the vendor recovering or the window
  # ageing out - neither is a repair anyone performed.
  ok_actions = []
}
