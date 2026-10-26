# Implementation Plan: 2026-10-20 seven-agent code audit fixes

**Status:** IN_PROGRESS
**Date:** 2026-10-20
**Approved by:** Parthiban, 2026-10-20: "Dee fic everything dude I need zero defects zero errors dude"

Source: the seven parallel read-only audits of 2026-10-20 (Rust-only sweep, hot
path, storage and audit, hostile review of the last 8 commits, zero-tick-loss and
reconnect paths, build run, branch check). One PR per lettered group, serially
(pr-completion-protocol.md).

Crates touched across the plan: `tickvault-app` (crates/app), `tickvault-core` (crates/core), `tickvault-storage`
(crates/storage), `tickvault-common` (crates/common/tests guards), `tickvault-trading`
(crates/trading, risk only; indicator/ and strategy/ need their own §28 approval).

## Plan Items

### PR A — depth rows the WAL refused never wait past 500 ms, plus backup-dedup allocation proof (app)

- [x] A1. Hand off unbacked depth rows from the frame arm when they have waited
  `FLUSH_INTERVAL`, so a saturated ring (biased select, timer arm not polled)
  cannot hold them in RAM for seconds. O(1) per frame.
  - Files: crates/app/src/dhan_feed_stack.rs, crates/app/src/dhan_feed_stack/unbacked_depth.rs
  - Tests: test_unbacked_clock_never_due_without_unbacked_rows,
    test_unbacked_clock_due_once_the_limit_has_passed,
    test_unbacked_clock_restarts_after_it_fires,
    test_unbacked_clock_resets_when_the_rows_leave,
    test_unbacked_clock_due_reads_the_wall_only_while_unbacked,
    proptest_unbacked_clock_fires_at_most_once_per_limit_and_never_late,
    the_late_depth_paths_use_the_late_trigger_and_the_live_drain_does_not
- [x] A2. DHAT test that installs a backup set and runs two-socket duplicate
  traffic through every dedup arm under the production recorder.
  - Files: crates/app/tests/dhat_main_feed_backup_dedup.rs
  - Tests: dhat_backup_dedup_admits_and_drops_without_allocating

### PR B — frame ring allocation under backlog (app, hot path)

- [x] B1. (merged #2110) Replace the frame ring's growable block list with a pre-allocated
  bounded queue plus a wake, so a backlog never allocates on the socket reader.
  Design reviewed by hot-path, security and hostile agents before code.
  - Files: crates/core/src/websocket/frame_ring.rs (new), crates/core/src/websocket/mod.rs,
    crates/core/Cargo.toml, crates/core/src/websocket/pool_supervisor.rs,
    crates/app/src/dhan_feed_stack.rs
  - Design (agent-reviewed 2026-10-20): `crossbeam_channel::bounded` storage
    (already a pinned workspace dep; all slots allocated at boot) plus a
    `tokio::sync::Notify` wake; strong-sender count for the close, so the
    drain still ends when every socket is gone; `recv` wrapped in
    `tokio::task::coop::cooperative` so a backlog still yields like tokio's
    channel; register (`Notified::enable`) before checking the queue, so no
    wake is lost.
  - Tests: test_frame_ring_delivers_in_order_then_none_after_the_last_sender,
    test_frame_ring_try_send_refuses_past_capacity_and_hands_the_item_back,
    test_frame_ring_try_send_after_the_receiver_is_dropped_is_closed,
    test_frame_ring_zero_capacity_is_raised_to_one,
    test_frame_ring_downgrade_gives_a_weak_sender_that_does_not_keep_the_ring_open,
    test_frame_ring_upgrade_succeeds_while_a_strong_sender_lives,
    test_frame_ring_try_recv_len_and_is_empty_track_the_queue,
    test_frame_ring_a_cloned_sender_keeps_the_ring_open,
    test_frame_ring_cross_thread_wake_never_loses_an_item,
    test_frame_ring_recv_dropped_inside_select_loses_nothing,
    test_frame_ring_a_full_backlog_yields_to_other_tasks,
    dhat_wal_ring_sink_accept_allocates_nothing_in_steady_state_backlog_or_shed

### PR C0 — WAL apply-lag latch releases on a staircase drain (storage) — live disk risk

Found by the 2026-10-20 live AWS check (read-only): on 14, 15 and 16 Oct the
after-close depth pass wrote 0 rows and cleared 0 buckets (stop: Deadline)
while pinned WAL depth segments grew 74.6 → 118.1 GB against an 80.5 GB
ceiling. The pass pauses on the apply-lag latch; `market_depth` drained its
backlog 12 txn at a time with flat polls between, and the latch reset its
fall count on every flat poll, so it never released from late morning to
17:15. Operator chose "Pass ignores steady lag (Recommended)" on 2026-10-20.

- [x] C0. A flat poll neither counts as a fall nor resets the fall count; only a
  rise does. Jitter still never releases; a table stuck flat still latches.
  - Files: crates/storage/src/wal_suspension_watcher.rs
  - Tests: a_staircase_drain_with_flat_polls_releases_the_latch,
    a_re_arm_after_a_release_needs_two_fresh_falls,
    falls_before_the_latch_fires_do_not_count_toward_its_release,
    two_falls_an_hour_apart_do_not_release_the_latch,
    a_slow_staircase_stays_latched_instead_of_sawtoothing,
    a_rise_between_two_falls_keeps_the_latch_even_with_flat_polls,
    one_falling_poll_does_not_release_the_latch_but_two_do,
    a_lag_pinned_perfectly_flat_above_the_floor_still_latches,
    an_uneven_staircase_re_arms_but_pages_at_most_once_an_hour,
    a_new_episode_after_the_floor_pages_at_once,
    test_untrusted_count_holds_a_staircase_release_for_the_watermark
  - Proof: the two staircase tests FAIL on the old rule; the four hostile-review
    tests FAIL on the first version of this change (a fall count left stale
    after a fire or release, and no expiry for an old fall); all pass now.
    A second review found re-arm pages every 8-11 polls on uneven steps and the
    watermark trusting a staircase release: pages now wait an hour inside one
    episode, and the watermark keeps the old two-falls-in-a-row rule.

### PR W — bounded WAL unapplied ranges (stops the shed-overflow WAL growth on sessions that end with clean probes) (storage, app)

Found by the 2026-10-20 live check: the catch-up backlog guard marked a
3,176-bucket span into the 128-slot unapplied table, which overflowed; the
overflow was persisted as one bit with no height and reloaded at `u64::MAX`,
so every later segment read as unapplied and the prune could delete nothing.
Converged design (three judges).

- [ ] W1. Bounded unapplied ranges: at most 32 disjoint ranges (merge the two
  closest when full, coverage only grows) in a CRC-checked sidecar beside
  `applied.tvaw`; `applied.tvaw` stays version 1, keeps its overflow bit set
  while any range exists and names the sidecar generation in its pad bytes; an
  overflow carries a height in RAM. Boot fold once per process before the WAL
  threads spawn: inherited overflow to `[oldest waiting, own_floor]`, earlier
  buckets to exact ranges, boot tail `[boot_applied_floor + 1, own_floor]`.
  The backlog guard writes one range; ranges trim below the oldest waiting
  segment after the boot refold and each catch-up round; the drained reset
  clears them. The catch-up summary names an early stop (`frame_cap`,
  `memory`, `disk`, `deferred`, `ack_timeout`, `replay_failed`), never
  `drained`. NOT changed: the prune gate, deferred-depth pins, the per-frame
  shed and drain paths, catch-up caps and pacing, alarms, EMF names and
  filters. **This stops new growth; it does not shrink the backlog already on
  disk**, which stays protected until a catch-up drain reads it.
  **⚠ CHANGED 2026-10-20 (review):** the overflow bit is no longer kept set
  while any range exists (an older binary seeds it at `u64::MAX` for ever, so
  a rollback would have stopped every prune on a clean box): each range is
  also written into the version-1 bucket table (less the boot's frameless
  gap) when it fits, and the bit is set only for an in-RAM overflow, a
  migrated overflow, or a range that does not fit. The migrated range and the
  guard start a reorder slack below the oldest waiting segment; a folder that
  cannot be listed fails closed (fold from 1, guard from the boot floor, trim
  refuses); the trim clips ranges to the waiting segments' spans instead of
  only cutting below the oldest; the suspension probe and the settle recovery
  write ranges, not buckets, and the fold uses the overflow's low end when
  known.
  **⚠ CHANGED 2026-10-20 (review round 3):** the fold wrapper rounds
  `own_floor` up to a frame base and ratchets the frame counter to it, so
  no frame of this process lands in the span treated as frameless, and the
  drained reset uses the safer boundary below `own_floor`; the boot floor
  is `min(hwm_ticks, hwm_depth)` less the slack whatever the depth flag
  says; a full range set merges across the lowest gap with nothing waiting
  (else the lowest pair), the fold gets the waiting spans, and the guard
  is clamped to the boot frame high; the migrated overflow carries its span
  (sidecar version 2, version 1 still read) and clears once no range
  meets it; the trim cuts below the lowest readable span when the only
  unreadable segments sit in `settling/unreadable/`, and counts every
  refusal (local metric only); the boot tail starts no lower than the
  oldest waiting segment; the overflow low end is removed; the probe's
  range writes and an immediate persist run off the runtime worker; the
  tail probe caps a record read at 4 MiB. A backlog that never drains still
  keeps one segment per boot until it drains (stated, pinned by a
  property test over 40 to 55 boots); a full disk and a crash between the
  two file writes still fail toward replaying more (stated, not fixed).
  **⚠ CHANGED 2026-10-20 (review round 3, second pass):** the range written
  when a suspension episode ends, and the in-session overflow bound, carry
  one reorder slack on their tops, so they reach the first record of any
  segment a reordered frame of theirs can sit in (a lower margin in the
  segment check was tried and rejected: the lifetime property tests showed
  it pins the segment above every range end); the replay skip and the prune
  share one rule, `AppliedSnapshot::segment_is_applied`; the in-session
  overflow bound resets the overflow bounds after each bound (compare-and-
  swap, flag cleared first, at most four passes), so a later overflow of the
  session is bounded on its own instead of from the first one's low end; the
  overflow arms lower the low end before raising the height; the tail probe
  returns an I/O error as an error, so a segment it cannot read is never
  taken for empty. **Scope of the growth claim:** the WAL stops growing only
  on sessions that end with clean QuestDB probes. While apply lag keeps a
  table latched as untrusted, every ack parks, the episode ranges and the
  next boot's tail cover the whole unconfirmed stretch, the catch-up drain
  stops on growing lag and the settle step cannot archive, so such a
  session still adds its WAL to the pinned set (Assumed common on the live
  box: apply lag measured 11,503 to 27,089 on 2026-09-08). **⚠ CORRECTED
  2026-10-21 (round 6):** "adds its WAL to the pinned set" understates it.
  ONE suspicion episode between the catch-up drain's first acked batch and
  its promotion to `archive/` (the drain's own apply-lag pause starts one:
  reported tables are a subset of untrusted ones; a failed probe does too)
  holds the whole process's settled output for the next boot to read again,
  so that boot's drain progress is undone and the migrated backlog does not
  shrink at all; pinned bytes stay flat. See (E) in the round-6 section and
  plan item W3. Verify after
  deploy over several sessions: the suspicion episodes per session
  (`AppliedWatermark::suspect_episode_count`, not published as a metric;
  `tv_questdb_wal_apply_lag_max` is the published proxy; **⚠ CORRECTED
  2026-10-20 (round 5): superseded by round 4 (C) below, which publishes
  `tv_wal_suspect_episodes_total` and `tv_wal_suspect_seconds_total`**) and
  `tv_wal_unapplied_pinned_bytes` / `tv_wal_unapplied_pinned_segments`;
  pinned bytes rising every day means the live box still grows. The
  structural fix (confirm parked live acks once a full durability lag and
  two clean probes follow them, as `settling/` already does for replays) is
  NOT in this PR and needs its own plan item (see "Follow-up" below).
  **⚠ CHANGED 2026-10-20 (PR W review round 4).** (A) Per-frame shed
  BUCKET marks (`note_unapplied`, bucket = `seq >> 38`) carry no reorder
  slack, so a frame shed just below a `2^38` boundary inside a segment whose
  first record is just above it made that segment read applied (skipped by
  replay, prunable). `segment_is_applied` now reads the bucket table and the
  in-RAM overflow height from `reorder_reach_lo(first)` (one slack lower,
  never below 1); the range lookup stays at `[first, hi]`, so checks G and
  M3 are unchanged except that G no longer expects a segment starting
  within one slack above the boot's shared bucket to read applied (the
  stated cost). The boot fold gives each folded bucket one slack on its
  top; the deferred-depth prune lookup and `segments_for_bucket` (the
  after-close pass) reach one slack below a segment's first record. The
  lifetime property test starts one new segment in ten just above a bucket
  boundary with a record half a slack below its first; seed
  `16702864118144502203` (pinned as a plain test) fails Z on the old rule.
  Rollback limit: an older binary still reads bucket marks from the first
  record (check O exempts exactly that shape; **⚠ CORRECTED round 5:** the
  filter exempted every frame outside the segment's buckets, whatever
  protected it; now only a shed frame, on either side). (B) Honest limit: WAL growth
  can also continue through the deferred-depth table, whose overflow is
  still sticky and height-blind (W2 below). (C) The suspect state is now
  published, local `/metrics` only, registered at 0 at boot:
  `tv_wal_suspect_episodes_total` and `tv_wal_suspect_seconds_total` (whole
  seconds, remainder carried; O(1) once per watcher poll, never per frame).
  **Post-deploy verify list (replaces the one above):**
  `tv_wal_unapplied_pinned_bytes` and `tv_wal_unapplied_pinned_segments`,
  read together with `tv_wal_suspect_seconds_total` and
  `tv_wal_suspect_episodes_total` per session;
  `tv_wal_deferred_depth_kept_segments`; and the deferred table's overflow
  flag (`overflowed` on the after-close pass's start log line, or the
  overflow flag of `deferred_depth.tvdd`). The claim "growth stopped" is
  made ONLY after pinned bytes stay flat across several sessions measured
  together with the suspect time of each; pinned bytes that rise on
  sessions with suspect time and stay flat on sessions without it mean the
  follow-up below is what remains.
  **⚠ CHANGED 2026-10-20 (PR W review round 5).** (A) The upper side of a
  segment: a segment's last record can sit up to one slack ABOVE the next
  segment's first record, so a bucket mark on it sat above the segment's
  range and the segment read applied (replay skip, prune; for shed depth the
  after-close pass never re-read it). `segment_is_applied` reads the bucket
  table up to `reorder_reach_hi(hi)`; the boot-folded buckets and the
  in-session overflow bound get one slack on their bottom too; the
  deferred-depth prune uses `segment_is_deferred` and `segments_for_bucket`
  reaches one slack above a range end. Cost: a segment ending within one
  second below a marked bucket's start is held. The property generator
  rotates one segment in ten across a boundary the other way when `mirror`
  is on; four seeds that fail Z under the old rules are pinned. (B) The
  never-draining bound, corrected: each session keeps its tail, PLUS every
  segment overlapping one of its per-frame unapplied bucket marks (ring
  shed, append failure, rescue failure), PLUS, for a session with ANY
  suspicion episode (closed or open at the stop), the episode's stretch;
  "everything else prunes" holds only for a session with no shed bucket and
  no suspicion episode. The tail starts at the depth watermark, which stops
  at the 15:40 IST depth window, so it covers every segment from the last
  in-window depth ack to the stop (count per day to be read after deploy).
  The stuck property test now sheds and closes episodes and checks that
  bound. (C) `tv_wal_unapplied_trim_refused_total{reason}` is registered
  at 0 at boot; the pinned gauges deliberately are not (0 would read
  "nothing pinned" before any trim ran).
  **Post-deploy verify list (round 5, adds to the round-4 list):** beside
  pinned bytes and suspect seconds, read the ring-shed counters
  (`tv_dhan_ws_ring_full_total{endpoint}`, of which
  `tv_dhan_ws_ring_bytes_full_total` is the byte-budget subset) and the
  append-failure counters (`tv_ticks_dropped_total{feed}`,
  `tv_depth_rows_dropped_total`) per session, so a rise in pinned bytes on a day with no suspect
  time can be put down to shed buckets; an ABSENT
  `tv_wal_unapplied_pinned_*` gauge means the trim did not run or was
  refused, read `tv_wal_unapplied_trim_refused_total{reason}` with it.
  **Limit (round 5), recorded:** during round 4, `git checkout --` on
  `crates/storage/tests/wal_unapplied_ranges_properties.proptest-regressions`
  discarded two unstaged seed lines that cannot be recovered. They are
  PROBABLY the two seeds `test_regression_round4_bucket_boundary_reorder_seeds`
  pins (Assumed; not confirmed). To regain coverage a one-off sweep of 1,024
  lifetime cases (start state, stuck and mirror drawn per seed) and 1,024
  stuck-backlog cases (40 to 55 boots) was run on the round-5 code before
  merge: 0 failures in 2,048 cases. The four seeds still in the
  regressions file are now also pinned as plain tests
  (`test_regression_saved_proptest_seeds_replay_as_plain_cases`). Pinned
  `run_case` tests, not the regressions file, are the durable form.
  **⚠ CHANGED 2026-10-20 (PR W review round 4, pass 2).** (A) The pinned
  gauges are published after the boot fold and after the backlog guard
  too, whether or not a replayed batch is acknowledged; "nothing waiting"
  publishes 0. Before, only a trim set them and a trim runs only after an
  acknowledged batch, so a box under the replay disk floor never set them
  and read the same as a clean box. **Honest limit (disk floor):** the
  migrated backlog drains only while the WAL volume stays above the replay
  disk floor (`wal_replay_disk_floor_bytes`, 40 GiB on the 600 GB box,
  unchanged), and the drain itself uses QuestDB disk (25–75 GB per
  session's backlog, measured 2026-09-03). Below the floor every pass is
  refused, nothing is trimmed, and the backlog plus every later session's
  leftovers stay pinned until space is freed. (B) Both suspension ranges
  start two reorder slacks below their watermark: a frame acked after the
  last clean probe can be the last record of a segment whose successor's
  first record sits up to one more slack below it, and that segment's
  judged range ended under a one-slack range (replay skip, prunable).
  Ranges derived from an ack watermark need two slacks on the bottom.
  (C) The trim's spans are built in write order (segment names), each
  starting at the highest first written so far less the slack; in
  first-sequence order a long segment written after a tiny one, with its
  first record just below the tiny one's, lost its middle from every span
  and a frame shed there was skipped uncounted. (D) The orphaned comment
  block in `ws_frame_spill.rs`'s tests is deleted (16 lines under the pin).
  The lifetime generator gains `extra` (rotation across `H − slack` at a
  suspicion, inverted segment pairs); seeds that fail Z under each old rule
  are pinned (`test_regression_round4_pass2_extra_generator_seeds`).
  **Post-deploy verify list (pass 2, corrects the round-5 line above):**
  read `tv_wal_unapplied_pinned_bytes` and `tv_wal_unapplied_pinned_segments`
  after each boot (they are now set every boot); allow a tolerance of one
  segment per session when judging "flat across sessions" (the gauge
  measures with the trim's spans, the prune with `[first, next first − 1]`,
  so the boundary segment can be counted on one side and not the other).
  Read with them `tv_wal_replay_disk_floor_stops_total` and the catch-up
  summary line's `stop_reason` (`disk`, `frame_cap`, `ack_timeout`,
  `apply_lag`): a rising pinned set with disk-floor stops is the deadlock
  above, not new growth. An ABSENT gauge now means only that no scan could
  read the waiting set (a folder that cannot be listed, or an unboundable
  unreadable first record outside `settling/unreadable/`); read
  `tv_wal_unapplied_trim_refused_total{reason}` for which. After a drained
  reset the gauges keep the value published before it until the next boot
  (reads high, never low).
  **⚠ CHANGED 2026-10-21 (PR W review round 6).** (A) The whole-segment
  rule (`AppliedSnapshot::segment_is_applied`) needs the segment's end
  plus one slack strictly below `skip_below` for both sinks, and the boot
  tail and the backlog guard start at `AppliedWatermark::boot_range_floor`
  (`boot_applied_floor` less one more slack, at a frame base): an unacked
  frame just above `skip_below`, the last record of a segment whose
  successor starts up to a slack below it, made that segment read applied
  with no suspicion at all (replay skip, prunable). The lifetime test gains
  `straddle` (the plain-watermark straddle, then an in-session prune and a
  crash or a clean stop); three seeds fail Z with either old rule and are
  pinned; G and M3 allow two slacks. (B) `ack_gate` (a `Mutex<()>`) around
  the suspect-flag check and the update in both ack functions and around
  the flip and reads at both probe transitions. (C) The write-order check
  compares each first with the running maximum
  (`firsts_within_slack_of_running_max`), in `order_by_name` and
  `bound_unreadable`. (D) The boot fold's summary and a failed fold
  persist's warning are written by `log_boot_fold_summary`, called from
  `main.rs` right after the subscriber's `init`. (E) Honest limit: the
  migrated backlog (and every per-session leftover behind it) shrinks only
  on a boot whose catch-up batches are promoted to `archive/`; one episode
  before the promotion holds the process's whole settled output, so a boot
  that pauses even once makes zero net progress. (F) `waiting_spans` is
  documented as the test seam it is. (G) The pinned gauges' staleness
  limit is corrected: within a session they can read low or high, and a
  present gauge can be stale.
  **Post-deploy verify list (round 6, adds to the lists above):** per boot,
  read `tv_wal_replay_confirmed_segments_total` (must be above 0 for the
  backlog to shrink), `tv_wal_replay_settle_held_segments_total` and
  `tv_wal_replay_settle_recovered_segments_total`, with the catch-up
  summary's `stop_reason=apply_lag` and `tv_wal_catchup_lag_pause_total`.
  FLAT pinned bytes with held above 0 and confirmed at 0 means the backlog
  is NOT draining, not that growth stopped. Read the pinned gauges right
  after a boot, with `tv_wal_unapplied_trim_refused_total`. The boot log
  now carries the fold's summary line ("WAL unapplied record folded at
  boot") and, on a failing disk, `source=unapplied_ranges_persist_failed_at_boot`.
  - Files: crates/storage/src/wal_unapplied_ranges.rs,
    crates/storage/src/wal_applied_watermark.rs,
    crates/storage/src/ws_frame_spill.rs, crates/storage/src/wal_replay_settle.rs,
    crates/storage/src/lib.rs, crates/storage/tests/error_level_meta_guard.rs,
    crates/storage/tests/wal_unapplied_ranges_properties.rs,
    crates/app/src/dhan_feed_stack.rs, crates/app/src/dhan_feed_stack/catchup_stop.rs,
    crates/storage/src/wal_deferred_depth.rs (doc only in round 4;
    `segment_is_deferred` in round 5),
    crates/storage/src/ws_frame_spill/tests/frame_base_tests.rs,
    crates/app/src/main.rs (counter registration, rounds 4 and 5; the
    boot fold log call, round 6),
    CLAUDE.md
  - Tests: overflow_taints_only_ranges_starting_at_or_below_its_height,
    test_regression_reloaded_overflow_clears_after_a_drain_past_its_height,
    test_v1_overflow_bit_migrates_to_range_bounded_by_own_floor,
    test_migrated_range_covers_replaying_segments,
    test_rollback_older_binary_decode_keeps_overflow_bit_while_migrated_ranges_exist,
    test_rollback_old_decoder_on_a_clean_box_sees_buckets_not_overflow,
    test_range_buckets_without_a_trusted_sidecar_fold_back_into_their_spans,
    test_migrated_range_keeps_a_record_below_its_segment_s_first_sequence,
    test_fold_migrates_a_known_overflow_up_to_its_height,
    test_waiting_segments_fail_closed_when_a_folder_cannot_be_listed,
    test_waiting_spans_clip_a_gap_to_the_segment_s_last_record,
    test_trim_to_waiting_clips_a_range_over_a_stretch_with_no_waiting_segment,
    test_unapplied_ranges_clip_to_splits_keeps_and_drops,
    test_clip_unapplied_ranges_to_clears_migrated_when_empty,
    test_suspicion_across_an_overnight_gap_is_a_range_not_an_overflow,
    test_regression_da13c_boot_rereads_segments_left_settling_by_a_crash,
    test_sidecar_to_bytes_from_bytes_round_trip,
    test_sidecar_bad_crc_magic_short_or_malformed_is_refused,
    test_corrupt_or_missing_sidecar_falls_back_to_buckets_or_an_unbounded_overflow,
    test_regression_next_day_shed_no_longer_overflows_after_boot_fold,
    test_unapplied_ranges_full_merges_lowest_pair_and_never_shrinks_coverage,
    test_trim_below_drops_and_cuts,
    test_boot_applied_floor_frozen_at_bind_and_ignores_later_acks,
    test_boot_tail_registered_before_writer_spawn,
    test_regression_multi_day_backlog_never_overflows_the_bucket_table,
    test_guard_never_calls_note_unapplied_range,
    test_regression_leftover_backlog_is_replayed_after_live_acks_pass_it,
    test_guard_pending_backlog_ignores_segments_at_or_above_the_ceiling,
    test_per_record_skip_honours_ranges_after_live_acks_pass_them,
    test_prune_never_deletes_a_segment_under_a_range_or_an_unbounded_overflow,
    test_regression_20_oct_shape,
    test_catchup_stop_reason_frame_cap_is_not_drained,
    proptest_wal_lifetimes_never_skip_or_prune_an_unapplied_frame,
    test_add_range_avoiding_merges_across_a_gap_no_span_intersects,
    test_fold_at_boot_with_spans_merges_across_a_free_gap,
    test_fold_wrapper_ratchets_the_frame_counter_past_own_floor,
    test_ratchet_frame_seq_to_makes_every_later_mint_greater,
    test_round_up_to_frame_base_rounds_up_and_saturates,
    test_drained_reset_keeps_a_suspension_range_above_the_ceiling,
    test_regression_depth_untracked_file_keeps_depth_frames_between_the_watermarks,
    test_guard_pending_backlog_is_clamped_to_the_boot_frame_high,
    test_boot_tail_starts_at_the_oldest_waiting_segment_without_a_file,
    test_migrated_flag_clears_once_no_range_intersects_the_migrated_span,
    test_sidecar_from_bytes_reads_a_version_1_file,
    test_trim_to_waiting_cuts_below_a_segment_set_apart_as_unreadable,
    test_note_questdb_probe_writes_ranges_off_the_runtime_worker,
    test_record_at_capped_refuses_a_length_past_the_cap,
    proptest_stuck_backlog_keeps_later_sessions_prunable,
    test_regression_episode_end_range_keeps_a_segment_holding_a_reordered_frame,
    test_segment_is_applied_is_the_range_check_on_both_sinks,
    test_bound_known_overflow_locked_bounds_a_known_height_only,
    test_regression_a_second_overflow_after_a_persist_is_bounded_on_its_own,
    proptest_repeated_in_session_overflows_are_bounded_one_by_one,
    test_regression_a_segment_that_cannot_be_read_is_never_taken_for_empty,
    test_overflow_arms_lower_the_low_end_before_raising_the_height,
    test_regression_bucket_mark_holds_a_segment_starting_just_above_the_boundary,
    test_regression_overflow_height_holds_a_segment_starting_just_above_it,
    test_regression_prune_keeps_a_segment_holding_a_frame_shed_below_the_boundary,
    test_regression_deferred_mark_reaches_a_segment_starting_above_the_boundary,
    test_regression_round4_bucket_boundary_reorder_seeds,
    test_reorder_reach_lo_is_one_slack_lower_and_keeps_zero,
    test_note_questdb_probe_counts_suspect_episodes_and_seconds,
    test_regression_bucket_mark_holds_a_segment_ending_just_below_the_boundary,
    test_regression_overflow_bound_holds_a_segment_ending_just_below_its_low,
    test_reorder_reach_hi_is_one_slack_higher_and_saturates,
    test_segment_is_deferred_reaches_one_slack_each_side,
    test_regression_prune_keeps_a_segment_whose_last_record_is_shed_above_the_boundary,
    test_regression_deferred_mark_reaches_a_segment_ending_below_the_boundary,
    test_segments_for_bucket_places_an_unreadable_segment_with_the_reach,
    test_regression_round5_mirror_boundary_rotation_seeds,
    test_regression_saved_proptest_seeds_replay_as_plain_cases,
    test_fold_at_boot_and_guard_pending_backlog_publish_pinned_without_an_acked_batch,
    test_regression_trim_keeps_a_range_over_a_long_segment_written_after_a_tiny_one,
    test_order_by_name_sorts_by_write_order_and_refuses_an_unknown_order,
    test_regression_suspension_range_holds_a_segment_whose_last_record_was_acked_late,
    test_suspension_range_lo_is_two_slacks_lower_at_a_frame_base,
    test_regression_round4_pass2_extra_generator_seeds,
    test_regression_plain_watermark_straddle_keeps_a_segment_whose_last_record_was_never_acked,
    test_regression_round6_plain_watermark_straddle_seeds,
    test_ack_gate_serialises_the_flag_check_with_the_update,
    test_acks_racing_probe_transitions_are_always_in_an_episode_range,
    test_order_by_name_refuses_falls_that_add_up_past_the_slack,
    test_regression_waiting_spans_cover_every_first_after_falls_that_add_up,
    test_log_boot_fold_summary_writes_the_recorded_fold_once_after_logging_starts,
    test_module_header_names_the_drain_progress_counters

- [ ] W2. The deferred-depth table honours its overflow height (found by the
  PR W review round 4; NOT in PR W). `wal_deferred_depth` has the
  sticky-overflow shape PR W removed from the unapplied table:
  `DeferredDepthSnapshot::kinds_in_range` returns `SHED_DEPTH_ALL` for EVERY
  range while `overflowed`, ignoring `overflow_high_seq`, so one collision
  pins every later segment until an after-close pass completes a sweep at
  or below the height. A trading-hours shed collides with a mark about 13
  days old (2 × 2048 × 2^38 ns = 13 d 0 h 45 min; one step of 6 d 12 h
  22 min moves the time of day outside the session). Fix: a range whose low
  end is above `overflow_high_seq` is not tainted by the overflow (an
  unknown height, `0` or `u64::MAX`, still taints everything); a known
  height is persisted in the file header that already carries it, and the
  after-close pass sweeps only up to it. Keeps more, never less: every
  range at or below the height still reads `SHED_DEPTH_ALL`.
  - Files: crates/storage/src/wal_deferred_depth.rs,
    crates/app/src/dhan_feed_stack/deferred_depth_pass.rs (if the sweep
    bound moves), CLAUDE.md (the `DeferredDepth` row)
  - Tests (to write): test_overflow_taints_only_ranges_at_or_below_its_height,
    test_unknown_overflow_height_still_taints_every_range,
    test_reloaded_overflow_height_round_trips_through_the_file,
    test_prune_deletes_an_applied_uploaded_segment_above_the_overflow_height,
    a property test that a collision never lets a prune drop a marked frame

- [ ] W3. The replay settle step promotes batch by batch (found by the PR
  W review round 6; NOT in PR W). `SettleState` keeps ONE mark for every
  settled segment of the process, and `promote` holds the whole set (and,
  once `held`, every later batch) when any suspicion episode began after
  the mark. The catch-up drain's own apply-lag pause starts such an episode
  (reported tables are a subset of untrusted ones), so a boot whose drain
  pauses once makes zero net progress on the migrated backlog: the next
  boot recovers those segments, reads them first, causes lag, and holds
  again. Fix: a mark per settled batch (its own ack instant, clean-probe
  count and episode count), promote each batch whose own durability lag
  and two clean probes passed before an episode began, and hold only the
  batches whose ack came before an episode's start; `held` no longer
  sticks for the rest of the process. Keeps more, never less: a batch acked
  inside or before an episode is still held for the next boot.
  - Files: crates/storage/src/wal_replay_settle.rs, crates/storage/src/ws_frame_spill.rs
    (callers, if the API changes), CLAUDE.md (the settle row)
  - Tests (to write): test_a_batch_promoted_before_an_episode_stays_archived,
    test_an_episode_holds_only_batches_acked_before_its_start,
    test_held_no_longer_sticks_for_the_rest_of_the_process,
    a property test over interleaved batches, probes and episodes that no
    batch acked before an episode's start reaches `archive/`
### PR H — no data-silence redial on a weekend or NSE holiday (core, app) — live finding

Found by the 2026-10-20 live AWS check (read-only): on the Dussehra holiday the
frame-silence watchdog redialled every socket 569 times between 09:15 and 15:30
IST, because its gate checked the time of day only.

- [x] H1. The silence gate reads the trading calendar (`tickvault-core`
  `silence_calendar`); a weekend or NSE holiday keeps the data-silence redials
  and the 805 probe window shut, unless at least 3 sockets delivered data in
  the last 10 s (hostile review HIGH: a wrong calendar on an open market).
  - Files: crates/core/src/websocket/silence_calendar.rs, crates/core/src/websocket/mod.rs,
    crates/core/src/websocket/pool_supervisor.rs, crates/app/src/dhan_feed_stack.rs
  - Tests: test_holiday_suppresses_the_silence_redial_but_not_the_ping_watchdog,
    test_holiday_leaves_close_code_handling_unchanged,
    test_market_is_streaming_needs_three_fresh_sockets,
    test_override_on_evidence_opens_a_holiday_only_when_streaming,
    test_session_secs_of_day_now_overrules_a_holiday_while_streaming

### PR I — depth-200 10-minute minimum hold (app) — live finding

Found by the 2026-10-20 live log read (read-only, 14, 15, 16 and 19 Oct):
depth-200 swapped about 1,200 times a day, about 2 frames per swap. Owner
tapped "Hold 10 min (Recommended)" on 2026-10-20; rule recorded in
`websocket-connection-scope-lock.md` "2026-10-20 — DEPTH-200 HOLDS A SWAPPED-IN
CONTRACT FOR AT LEAST 10 MINUTES".

- [x] I1. A contract the steering loop swaps onto a depth-200 socket is not
  swapped off for 600 s (`DEPTH200_MIN_HOLD_SECS`, a constant: the rule REJECTs
  any other value). `Depth200HoldBook` keeps each socket's swap-in instant
  (monotonic, stamped after `apply_decision` returns; compared against a
  reading taken after the probe block) and the one before it, so a refused,
  reverted swap gets its original time back. `locked_mask` gives the planner
  the locked socket's underlying, so a same-underlying candidate is never put
  on a second socket (the 2026-09-24 "top 5 distinct underlyings" rule).
  Candidates the hold keeps waiting are `held_back` (logged), never
  `unplaced` / `socket_empty`. An empty socket, a dial-placed contract and a
  contract gone from the eligible set (not a stock option in today's
  contract-to-underlying map) are never held. Caps, `ROTATION_HALTED`, 805
  handling, the silent-swap resubscribe, depth-20 and the main feed unchanged;
  no new alarm, page, filter or EMF name (`min_hold` is a local `/metrics`
  label value on the existing counter).
  - Files: crates/app/src/depth200_min_hold.rs, crates/app/src/depth200_ranked_steer.rs,
    crates/app/src/depth_rebalance.rs, crates/app/src/lib.rs, CLAUDE.md
  - Tests: test_depth200_min_hold_keeps_a_contract_for_600s,
    test_depth200_min_hold_releases_after_600s,
    test_depth200_min_hold_never_blocks_an_empty_socket,
    test_depth200_min_hold_drops_an_ineligible_contract,
    test_depth200_held_underlying_needs_a_stock_option_in_todays_map,
    test_depth200_hold_book_observe_restores_the_prior_swap_in_on_a_revert,
    test_depth200_hold_book_revert_restores_the_original_swap_in_inside_the_hold,
    test_depth200_hold_book_note_sent_swaps_skips_a_swap_that_was_not_queued,
    test_depth200_hold_book_locked_mask_is_false_for_a_mismatched_or_short_book,
    proptest_depth200_min_hold_no_socket_changes_twice_within_600s,
    plan_ranked_minute_skips_a_min_hold_socket_and_funds_the_next,
    a_held_contract_keeps_its_underlying_so_a_same_name_candidate_is_not_placed_elsewhere,
    the_planner_peeks_then_caps_then_consumes
  - Rollback: revert the PR. There is deliberately NO runtime switch: the
    2026-10-20 REJECT list bars a hold other than 600 s without a fresh dated
    quote, and a switch that turns it off is such a hold.


### PR C — top_volume spill replay window (storage)

- [x] C1. A spilled hour is set aside by the age of its END and gets a second
  evening; an unparseable spill name no longer holds every partition's archive.
  - Files: crates/storage/src/tick_spill_replay.rs, crates/storage/src/partition_archive.rs
  - Tests: test_spill_file_older_than_measures_a_top_volume_spill_from_the_hour_end,
    test_unparseable_spill_name_does_not_hold_every_partition,
    test_undated_top_volume_spill_file_is_set_aside

### PR D — guards and docs (common tests, trading risk, docs)

- [x] D1. Shell budget covers compose CMD-SHELL healthchecks and systemd units outside deploy/systemd.
  - Files: crates/common/tests/shell_budget_guard.rs
  - Tests: compose_healthchecks_are_budgeted, every_systemd_unit_is_scanned
- [x] D2. Coded error on the tick-gap tracker emit; the code guard flags every production `error!` without `code =` (frozen §28 files allowlisted until their own approval).
  - Files: crates/trading/src/risk/tick_gap_tracker.rs, crates/common/tests/error_code_tag_guard.rs
  - Tests: every_production_error_carries_a_code
- [ ] D3. CLAUDE.md: O(1) table rows for the frame ring backlog and the silent-swap register; stale benchmark rows; `variables.tf` IOPS text.
  - Files: CLAUDE.md, deploy/aws/terraform/variables.tf
  - Tests: claude_md_codebase_map_guard

### Follow-up (own plan item, not in this plan)

- PR W growth under latched apply lag: confirm parked live acks once a full
  durability lag (`WATERMARK_DURABILITY_LAG_SECS`) and two clean probes follow
  them, as `settling/` does for replays, so a session that ends while a table
  is latched untrusted stops pinning its whole WAL. Needs its own design (it
  changes what an ack under suspicion is trusted for) and the live
  measurement named in W1 first: `tv_wal_unapplied_pinned_bytes` read
  together with `tv_wal_suspect_seconds_total` and
  `tv_wal_suspect_episodes_total` over several sessions (2026-10-20, PR W
  review round 4). While any table is latched, every probe is unclean, acks
  park, and on a box whose backlog does not drain that is about one session
  of WAL per lag-latched day.
### PR G — holiday gate enabled on deploy, weekday-holiday-only (app, aws-lambdas, common tests, storage tests, deploy workflows, terraform)

Root cause, **Assumed** until `systemctl is-enabled tickvault-holiday-gate` is
read on the box: the deploy copied `tickvault-holiday-gate.service` on every run
but enabled only `tickvault` and `tickvault-host-tuning`; only first-boot
user-data enabled the gate, terraform ignores `user_data` changes, so the live
box ran on the 2026-10-02 and 2026-10-20 NSE holidays. Enabling it as-is would
have stopped every non-trading boot, weekends included (`--check-trading-day`
exit 75 = weekend or holiday): the post-merge catch-up deploy (every 30 minutes,
all seven days), a console start, filesystem recovery, downsize, and a manual
Sunday Muhurat start on 2026-11-08, each with a false "NSE holiday" page.

- [ ] G1. The deploy enables all three units in one `systemctl enable` element
  after `systemctl daemon-reload`; the gate unit's copy is unconditional, so a
  missing file fails the deploy instead of silently failing the combined enable.
  The guard helper counts a later `systemctl disable|mask` as removing a unit.
  - Files: .github/workflows/deploy-aws.yml, crates/app/tests/host_tuning_boot_wiring_guard.rs,
    crates/common/tests/shell_budget_guard.rs (deploy-aws 95 / 11,980 → 94 / 11,907,
    run lines 643 → 642 → 640 with G3, shrink only)
  - Tests: deploy_enables_the_holiday_gate_after_daemon_reload,
    holiday_gate_enable_check_bites, ssm_workflow_shell_never_grows,
    workflow_run_lines_never_grow
  - Honest limits: the combined enable keeps `|| true`, so a failed enable is
    not reported by the deploy. A `systemctl is-enabled tickvault-holiday-gate`
    echo in an existing verify element was considered (review, 2026-10-20) and
    NOT added: the deploy-aws.yml SSM payload budget (94 elements / 11,907
    bytes) was measured exactly with no slack, so any echo grows it, and the
    budget only shrinks. The enabled state stays unverified by the deploy; read
    it on the box (the root-cause check above).
- [ ] G2. The gate stops the box ONLY on a Monday-to-Friday NSE holiday with no
  Muhurat or mock session, at a boot inside 08:25 to 09:15 IST (the 08:30 IST
  EventBridge start); every other boot (weekend, Muhurat, trading day, outside
  the window, unreadable config) starts the app with one log line saying why.
  Marker and SNS page unchanged for the stop. `--check-trading-day` unchanged.
  The window START is `shutdown_class::HOLIDAY_GATE_STOP_WINDOW_START_SECS_OF_DAY_IST`
  itself (08:25, no copy), which is also the `market_open_readiness` Lambda's
  holiday self-stop floor (`HOLIDAY_SELF_STOP_EARLIEST_IST_MINUTES`, pinned by a
  source-scan test). `shutdown_class`'s quiet window END moves 09:00 → 09:25 =
  the gate's 09:15 end + `HOLIDAY_GATE_STOP_LAG_SECS` (600 s: the gate unit's
  300 s outer bound + 300 s **Assumed** for EC2 to signal the app), so every gate
  stop classifies `ScheduledStop`; a compile-time assert holds gate window ⊆
  quiet window with equal starts. (Review fix 2026-10-20: the first draft started
  the gate at 08:00, so a gate stop 08:00–08:25 paged as an external stop and the
  readiness Lambda read it as "box not running".)
  - Files: crates/app/src/holiday_gate.rs, crates/app/src/shutdown_class.rs,
    crates/app/src/main.rs, crates/app/src/boot_helpers.rs,
    crates/aws-lambdas/src/market_open_readiness.rs (doc pin only),
    crates/storage/tests/aws_deploy_safety_guard.rs
  - Tests: test_weekday_holiday_inside_the_scheduled_start_window_stops,
    test_weekday_holiday_outside_the_window_starts, test_weekend_boots_start_at_any_hour,
    test_sunday_muhurat_starts, test_trading_day_starts_inside_and_outside_the_window,
    test_unreadable_calendar_fails_open_even_at_the_scheduled_start,
    test_classify_calendar_day, test_scheduled_start_window_is_0825_to_0915_ist_around_the_0830_start,
    holiday_gate_window_start_matches_the_readiness_lambdas_floor,
    holiday_gate_unit_timeout_covers_the_marker_retries,
    test_classify_shutdown_at_the_holiday_gates_latest_stop_is_scheduled,
    test_classify_shutdown_holiday_gate_window_consts_are_0825_and_0925_ist,
    test_classify_shutdown_holiday_gate_self_stop_window_only,
    test_trading_day_gate_reads_the_same_layered_config_as_the_boot,
    holiday_gate_is_wired_and_fail_open,
    test_decide_second_start_lets_only_a_same_day_restart_through,
    test_marker_put_is_tried_twice_and_bounded, test_names_match_the_script
  - Review fixes (2026-10-20, second round): (a) a deliberate restart inside
    08:25 to 09:15 IST on a holiday-stop day was stopped again and paged again.
    Before it stops, the gate now reads its own `/tickvault/<env>/holiday-stop-date`
    (`read_holiday_marker`, 30 s ceiling, `ParameterNotFound` = absent) and the
    pure `decide_second_start` lets the boot start with one log line when the
    marker is today's IST date; absent, an older date, empty, or a FAILED read
    stop as before (reviewer's choice: a failed read cannot tell the first boot
    from the second, and the first is the common case). The page now says a
    second start today stays on, and says so only when today's stop was
    recorded. (b) The marker put is retried once after 2 s
    (`MARKER_PUT_ATTEMPTS` = 2); with the read, the stop path grows by at most
    62 s, inside the unit's 300 s `TimeoutStartSec`. (c) The readiness Lambda's
    08:25 floor doc now says "launched at or after about 08:25" and names the
    few-seconds skew between the EC2 launch time it reads and the gate's own
    clock.
  - Honest limits: a deploy or manual start that lands inside 08:25 to 09:15 IST
    on a weekday holiday, BEFORE the gate stopped the box that day, is stopped
    like the scheduled start; the `/opt/tickvault/ALLOW_HOLIDAY_RUN` marker
    overrides it. A manual or budget stop on a non-trading day between 08:25 and
    09:25 IST now renders quiet (was 08:25–09:00). A same-day reboot of a box
    whose stop CALL failed now starts the app instead of trying the stop again.
    After a second start the marker still reads today, so the start-watchdog,
    the deploy-watchdog and the market-hours alarm gate treat the running box as
    holiday-stopped for the rest of the day (no self-start, no automatic
    redeploy, no liveness page; `deploy-aws.yml` run by hand still deploys a
    running box, and will not start it again if it stops). When both marker puts fail, a second start
    the same day is stopped again (errs toward the stop). A launch in the last
    seconds before 08:25 whose gate runs after 08:25 is stopped but read by the
    readiness Lambda as launched before its floor, so it pages "box not
    running".
- [ ] G3. The deploy chain never restarts a box the gate stopped. The
  deploy-watchdog Lambda (instance-start event, 08:50 / 15:51 IST crons) reads
  `/tickvault/<env>/holiday-stop-date` through the start-watchdog's
  `holiday_stop_is_today` (same parameter, IST date, fail-open) after a stale
  verdict and does not dispatch when it is today: one `info!` line, result
  `"skipped": "holiday_stop"`, no page (it has no counters; the mismatch metric
  is still published). Terraform grants it `ssm:GetParameter` on exactly that
  parameter and sets `HOLIDAY_STOP_PARAM`. `deploy-aws.yml` reads the same
  marker beside the budget-stop latch, before `start-instances`, and skips green
  (`DEPLOY_SKIP_REASON=holiday_stop`); `deploy-aws-after-close.yml` and
  `postmerge-catchup.yml` both end in that check (comments only there). The 4
  new run lines are paid by merging two 4-line state branches (run lines
  642 → 640; SSM payload and awk/jq budgets untouched).
  - Files: crates/aws-lambdas/src/deploy_watchdog.rs,
    crates/aws-lambdas/src/start_watchdog.rs (`SdkSsm.client` `pub(crate)`),
    deploy/aws/terraform/deploy-watchdog-lambda.tf, .github/workflows/deploy-aws.yml,
    .github/workflows/deploy-aws-after-close.yml, .github/workflows/postmerge-catchup.yml,
    crates/common/tests/shell_budget_guard.rs, crates/storage/tests/aws_deploy_safety_guard.rs
  - Tests: handle_reads_the_holiday_stop_marker_before_it_dispatches,
    holiday_marker_blocks_only_on_todays_ist_date (in start_watchdog.rs since
    the second review round, next to the `run`-driven holiday-marker checks; it
    tests `start_watchdog::holiday_stop_is_today`),
    deploy_chain_does_not_restart_a_box_the_holiday_gate_stopped (its
    deploy-watchdog check is scoped to `handle`'s body, so the Lambda's own test
    module literal cannot satisfy it),
    deploy_watchdog_handle_scope_bites_on_a_test_module_literal,
    workflow_run_lines_never_grow
  - The `deploy-aws.yml` skip notice now says: start the box from the console
    (a second start the same day is let through), then re-run with
    `confirm_market_hours=yes` (09:00-15:45 IST) or after 15:45 IST. Same line,
    run-line budget unchanged.
  - Honest limits: on the instance-start event the watchdog can fire before the
    gate writes the marker in that same boot, so that one firing can still
    dispatch; the deploy it starts skips at the marker once the box is stopped,
    or meets the stop during its verify (reported as an external stop, not a
    crash). A holiday-skip run counts as covering its SHA for
    `postmerge-catchup`, so that SHA's redeploy is the watchdog's on the next
    weekday boot. An unreadable marker deploys (fail-open, as the budget latch).
    The terraform grant needs an apply before the Lambda can read the marker;
    until then the read fails open and the watchdog dispatches as before.
    A merge on a holiday-stop day is NOT deployed that day: the next weekday
    08:30 boot runs the stale binary, the deploy-watchdog (instance-start
    event, 08:50 IST cron) reports it stale (`binary-sha-stale` metric) and
    dispatches the redeploy inside the 08:30-09:00 prep window. The watchdog's
    skip ignores box state: it reads only the marker, so after a manual start
    on a holiday-stop day (let through by the gate's second-start rule, or
    after 09:15 IST) a stale binary is not auto-deployed that day; re-run
    `deploy-aws.yml` by hand (a running box deploys).

### PR J — 2026-10-26 assurance-audit fixes (storage, app, core, api, common tests)

Approved by Parthiban, 2026-10-26: "go ahead fix everything fully parallel dude".
Source: the 2026-10-26 seven-dimension audit plus the WebSocket-continuity
attack (adversarially verified). Implemented in parallel on sub-branches
`claude/assurance-fix-g1..g5`, merged into one draft PR.

- [x] J1 (storage) Tick and depth rescue deferral checks the WAL flushed-through
  sequence (`wal_frame_fate::flushed_high_seq`) before trusting the WAL; a frame
  the WAL has not flushed goes to the inline spill instead. Correct the
  `no_segment` comment in `ws_frame_spill.rs`. Contract labels sanitised once at
  publish, not per tick row. Hot-path telemetry slots cache-line padded.
  - Files: crates/storage/src/tick_persistence.rs, crates/storage/src/depth_persistence.rs, crates/storage/src/ws_frame_spill.rs, crates/storage/src/hot_path_telemetry.rs
  - Tests: storage unit tests for the deferral decision; dhat_flush_buffer_recycle unchanged
- [x] J2 (app) The frame drain's shutdown arm drains the ring and counts the
  frames still queued (WAL-backed as deferred, unbacked as lost, coded
  WS-SPILL-02); rescue/offload shutdown abandonment counts ROWS on
  `tv_ticks_lost_total{source="rescue_abandoned"}`.
  - Files: crates/app/src/dhan_feed_stack.rs
  - Tests: app unit tests for the shutdown drain count
- [x] J3 (app, storage tests) `instrument_lifecycle` is ensured in production
  beside its audit twin and the write is held when the ensure is refused;
  order-side audit ensures retry until keyed; AUDIT-WS-01 drop warn carries a
  code and is latched once per episode; a guard requires DEDUP UPSERT KEYS on
  every CREATE TABLE literal (dated allowlist for the two scratch tables).
  - Files: crates/app/src/dhan_lifecycle.rs, crates/app/src/order_observability.rs, crates/app/src/dhan_rest_stack.rs, crates/storage/tests/
  - Tests: ensure-wiring source guard; create_table_has_dedup guard
- [x] J4 (core, api) Order-update WebSocket connects with explicit bounded
  limits; the connect-config guard scans every `connect_async*` site; the error
  summary writer streams and bounds its read; the ping/pong counter uses
  pre-resolved handles; `/api/debug/logs/jsonl/latest` is size-capped.
  - Files: crates/core/src/websocket/order_update_connection.rs, crates/core/src/websocket/connection.rs, crates/core/src/notification/summary_writer.rs, crates/api/src/handlers/debug.rs
  - Tests: config limit unit test; guard extension; summary writer bounded-read test; debug cap test
- [x] J5 (common tests, scripts, CI) Rust-only guard holes closed: interpreter
  aliases, case-insensitive `<script`, JS byte ceiling, `javascript:` URLs,
  banned frontend extensions and SVG script scan, Rust SSM-sender discovery,
  workflow line-continuation join; `brew install` removed; unversioned
  `cargo install` / `tool:` rejected and pinned.
  - Files: crates/common/tests/rust_only_guard.rs, crates/common/tests/shell_budget_guard.rs, crates/common/tests/browser_surface_and_toolchain_guard.rs, scripts/setup-observability.sh, .github/workflows/ci.yml
  - Tests: a self-test row per new token / extension / discovery rule
- [x] J6 (core) Dhan's documented `expiryTime` shape with milliseconds
  ("2026-01-01T00:00:00.000") is parsed instead of silently falling back to
  now + 24 h.
  - Files: crates/core/src/auth/types.rs
  - Tests: test_parse_expiry_time_documented_millisecond_string_is_parsed_exactly

### PR K — 2026-10-26 wave-2 audit fixes (storage spill + WAL, app drain hold, core silence spacing)

Approved under the same session quotes as PR J ("go ahead fix everything fully
parallel dude", "fix and reosleve and implement evrythign dude ... im ggiving you
all teh permissiosn"). Each item closes a gap the wave-1 slice audits and the
WebSocket-continuity attack confirmed in source; none changes an operator-locked
rule (808 park, $75 stop, token self-heal and frame-silence `act` stay as they are).

- [ ] K1 (storage, app) Spill replay and spill files: only HTTP 400/413/422 count
  as a bad payload (other 4xx are retried like a 5xx); a round stops after 3 files
  in a row are quarantined with the same status; the bisection POST cap follows
  the 64-line cap; a whole framed depth record that QuestDB refuses increments the
  paged rejected-lines counter; a spill append first writes a newline when the
  file's last byte is not one; the shared QuestDB client ignores proxy variables;
  at shutdown the parked rescue hand-off uses a deadline, then spills inline or
  counts the rest abandoned; the drain hold also counts depth framed spill files.
  - Files: crates/storage/src/tick_spill_replay.rs, crates/storage/src/depth_spill_frame.rs, crates/storage/src/tick_persistence.rs, crates/storage/src/depth_persistence.rs, crates/storage/src/http_client.rs, crates/app/src/drain_hold.rs
  - Tests: one regression test per change (status classification, quarantine streak stop, POST cap, set-aside counter, newline guard, park deadline, depth folder counted)
- [ ] K2 (storage) WAL core: a failed segment sync raises the lost mark so the
  rescue stops deferring to it, logged `error!` with its code; a shutdown that
  abandons a stuck writer is counted and never logged "complete"; a segment that
  cannot be read (I/O error) stays in place with a retry cap instead of being
  archived unread; a failed listing of `replaying/` refuses the pass; staging
  failures are coded and counted; a discarded segment is synced and cleared from
  the open registry; the queue byte cap is resolved at construction, not on the
  first frame; the stale re-capture comment is corrected.
  - Files: crates/storage/src/ws_frame_spill.rs, crates/storage/src/wal_replay_settle.rs
  - Tests: one regression test per change
- [ ] K3 (core, app) The 300 s silence fallback redials at most one socket per
  kind per `CONFIRMED_REDIAL_SPACING_SECS`, so a market-wide silence no longer
  redials every main-feed socket in the same second; the tick-gap detector's
  LTT-backward and volume-jump verdicts are counted (pre-resolved counters, local
  `/metrics` only) instead of discarded.
  - Files: crates/core/src/websocket/pool_supervisor.rs, crates/core/src/pipeline/tick_gap_detector.rs, crates/app/src/dhan_feed_stack.rs
  - Tests: five silent sockets for 1,800 s never redial two inside one spacing; verdict counters move on a crafted backward LTT and volume jump; DHAT on the observe path unchanged

### Not in this plan (blocked by a rule, need their own dated quote)

- Frame-silence fast path to `act`: the scope lock REJECTs defaulting to `act`
  without a dated measured row (websocket-connection-scope-lock.md 2026-10-10).
- Uncoded `error!` in `indicator/engine.rs` and `strategy/hot_reload.rs`: §28 frozen area.

## Design

PR A: a tiny clock (`UnbackedDepthClock`, Option<tokio Instant>) owned by the
drain. Each frame arm pass reads `has_unbacked_pending()` (one bool). While false
the clock is cleared; while true it starts once, and when `FLUSH_INTERVAL` has
passed the frame arm calls `flush_depth` and restarts it. The timer arm keeps its
own 500 ms check, so whichever arm runs first hands the rows on. No new task, no
lock, no allocation.

PR J: each fix keeps the existing tiers and only closes a gap: the rescue deferral asks the WAL fate table (one atomic load) before trusting replay; the shutdown drain is a bounded try_recv loop; ensures reuse spawn_ensure_until_keyed; core limits reuse websocket_config; guards are source scans.

PR K: each fix keeps the existing tiers: replay classification and streak stop are pure functions; the WAL sync failure reuses the existing lost mark; the silence spacing reuses the existing per-kind CAS; verdict counters are pre-resolved handles.

## Edge Cases

- No depth sink: `is_some_and` gives false; nothing changes.
- Flush fails or the offload queue is full: the writer keeps the flag; the clock
  restarts, so the next attempt is 500 ms later, never once per frame.
- Rows marked unbacked between flushes keep the oldest start.
- tokio Instant is monotonic; `saturating_duration_since` guards anyway.
- Ring idle: the timer arm still flushes at 500 ms as before.

- PR J: a WAL that has not flushed through a batch (disk full) sends the batch to the inline spill instead of trusting replay; a shutdown ring backlog larger than the budget is counted, never silently dropped.

- PR K: a QuestDB that refuses every line (schema drift) stops the round after 3 quarantines instead of emptying the backlog; a 401/403 is retried, never quarantined; all main-feed sockets silent at once are staggered, one redial per spacing.

## Failure Modes

- Extra flushes under load: at most one per 500 ms from the frame arm.
- A crash still loses unbacked rows inside the 500 ms window (unchanged, stated).

- PR J: an inline spill that also fails is counted lost (unchanged accounting, now honest).

- PR K: a retried 4xx that never heals keeps the file waiting (counted, not quarantined); a staggered silence redial recovers the pool more slowly (at most 5 × spacing).

## Test Plan

Unit and property tests on the clock; the existing source assertion extended to
pin the frame-arm call; the new DHAT test; `cargo test -p tickvault-app --lib`,
`dhat_live_ingest_seam`, source size and scan budget guards, clippy, fmt.

PR J: per-crate `cargo test -p` for storage, app, core, api, common; new self-test rows for every guard; clippy and fmt.

PR K: per-crate `cargo test -p` for storage, app, core; one regression test per item; clippy and fmt.

## Rollback

Revert the PR: the frame arm loses the extra check; the timer arm's 500 ms flush
still runs whenever it is polled (the pre-PR behaviour).

PR I: revert the PR (no runtime switch, per the 2026-10-20 REJECT row); the
planner then swaps every minute as before.

PR J: revert the merge commit of each sub-branch; no config flag or schema change.

PR K: revert the merge commit of each sub-branch; no config flag or schema change.

## Observability

No new metric: depth flushes and drops are already counted by the writer and
`depth_dropped`. The fix shortens a window and adds no failure path.

PR W (2026-10-20, PR W review round 4): local `/metrics` only, no CloudWatch
filter, no EMF name, no alarm: `tv_wal_unapplied_pinned_segments` and
`tv_wal_unapplied_pinned_bytes` (after every trim, and since pass 2 after
the boot fold and the backlog guard), `tv_wal_replay_disk_floor_stops_total`
(existing, read with them),
`tv_wal_unapplied_trim_refused_total{reason}`, and, registered at 0 at boot,
`tv_wal_suspect_episodes_total` and `tv_wal_suspect_seconds_total`.
PR I: one new label value, `outcome="min_hold"`, on the existing local
`tv_depth200_ranked_swaps_total` (pre-registered at 0; no EMF name, filter or
alarm); `held_back` and `min_hold` in the existing per-minute steering `info!`.

PR J: `tv_ticks_lost_total{source="rescue_abandoned"}` and `{source="shutdown_ring_unbacked"}`, registered at 0 at boot, local `/metrics` only (they ride the existing loss-group filter only after a dated noise-lock row).

PR K: tick-gap verdict counters and WAL staging-failure counters, local `/metrics` only, no EMF name, filter or alarm.

## Guarantee matrix

Carries the 15-row + 7-row matrix by reference to
`.claude/rules/project/per-wave-guarantee-matrix.md`; per-PR specifics are in each
PR body.
