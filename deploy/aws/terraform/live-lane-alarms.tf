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
  evaluation_periods  = 1
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
  evaluation_periods  = 1
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
  alarm_name        = "tv-${var.environment}-ticks-lost-spill"
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
