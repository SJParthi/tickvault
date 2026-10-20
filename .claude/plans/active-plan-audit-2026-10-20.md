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
  - Tests: test_top_volume_spill_age_is_measured_from_the_hour_end,
    test_unparseable_spill_name_does_not_hold_every_partition,
    test_undated_top_volume_spill_file_is_set_aside

### PR D — guards and docs (common tests, trading risk, docs)

- [ ] D1. Shell budget covers compose CMD-SHELL healthchecks and systemd units outside deploy/systemd.
  - Files: crates/common/tests/shell_budget_guard.rs
  - Tests: compose_healthchecks_are_budgeted, every_systemd_unit_is_scanned
- [ ] D2. Coded error on the tick-gap tracker emit; the code guard flags every production `error!` without `code =` (frozen §28 files allowlisted until their own approval).
  - Files: crates/trading/src/risk/tick_gap_tracker.rs, crates/common/tests/error_code_tag_guard.rs
  - Tests: every_production_error_carries_a_code
- [ ] D3. CLAUDE.md: O(1) table rows for the frame ring backlog and the silent-swap register; stale benchmark rows; `variables.tf` IOPS text.
  - Files: CLAUDE.md, deploy/aws/terraform/variables.tf
  - Tests: claude_md_codebase_map_guard

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

## Edge Cases

- No depth sink: `is_some_and` gives false; nothing changes.
- Flush fails or the offload queue is full: the writer keeps the flag; the clock
  restarts, so the next attempt is 500 ms later, never once per frame.
- Rows marked unbacked between flushes keep the oldest start.
- tokio Instant is monotonic; `saturating_duration_since` guards anyway.
- Ring idle: the timer arm still flushes at 500 ms as before.

## Failure Modes

- Extra flushes under load: at most one per 500 ms from the frame arm.
- A crash still loses unbacked rows inside the 500 ms window (unchanged, stated).

## Test Plan

Unit and property tests on the clock; the existing source assertion extended to
pin the frame-arm call; the new DHAT test; `cargo test -p tickvault-app --lib`,
`dhat_live_ingest_seam`, source size and scan budget guards, clippy, fmt.

## Rollback

Revert the PR: the frame arm loses the extra check; the timer arm's 500 ms flush
still runs whenever it is polled (the pre-PR behaviour).

PR I: revert the PR (no runtime switch, per the 2026-10-20 REJECT row); the
planner then swaps every minute as before.

## Observability

No new metric: depth flushes and drops are already counted by the writer and
`depth_dropped`. The fix shortens a window and adds no failure path.

PR I: one new label value, `outcome="min_hold"`, on the existing local
`tv_depth200_ranked_swaps_total` (pre-registered at 0; no EMF name, filter or
alarm); `held_back` and `min_hold` in the existing per-minute steering `info!`.

## Guarantee matrix

Carries the 15-row + 7-row matrix by reference to
`.claude/rules/project/per-wave-guarantee-matrix.md`; per-PR specifics are in each
PR body.
