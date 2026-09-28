# Implementation Plan: 2026-09-26 audit fixes — nothing blocks the tick path, nothing drops silently

**Status:** APPROVED
**Date:** 2026-09-26
**Approved by:** Parthiban (operator) — "Yes to all ensure to fix and resolve everything dude okay?"
(2026-09-26 09:03 UTC) and "Go ahead dude" (09:48 UTC). Standing asks recorded the same day:
"every code every place every functionalities every scenarios shoudl achieve O(1) always",
"because of anything fnotheing should be blocked", "ensure not to drop any instruments dude
until or unless I sat that dee whatever or whichever is subscribed everything needs to be in place".

Crates touched across the series: `app`, `core`, `storage`, `trading`, `common`, plus
`.github/workflows/`, `deploy/` and `scripts/`. Frozen and NOT touched: `crates/trading/src/indicator/`,
`crates/trading/src/strategy/`, the exit-order layer, OMS `dry_run` (stays hard-true, paper only).

Serial delivery: one PR open at a time (`pr-completion-protocol.md`). Each item below is one PR
unless it says otherwise. Evidence for every finding was re-read from source on 2026-09-26 before
this plan was written; three audit findings turned out smaller than recorded and are corrected
inline (PR2, PR8, PR14).

## Design

- [x] **PR1 — nothing on the tick path writes a log line synchronously.** (`app`, `storage`)
  - `errors.log` is `.with_writer(std::sync::Mutex::new(file))` (main.rs:1485, file from
    boot_helpers.rs:266): every WARN+ line is a blocking `write(2)` under a mutex on the calling
    thread, including the frame-drain task. stdout (main.rs:1412) is the same when enabled.
    Move both onto `tracing_appender::non_blocking` like app.log / errors.jsonl already are
    (observability.rs:385, :421), keep the `WorkerGuard`s for the process lifetime, and publish the
    appender's dropped-line counter as `tv_log_lines_dropped_total`.
  - Per-tick `error!`/`warn!` with no throttle: sequence-refused ticks (dhan_feed_stack.rs:3033,
    :3051) and the ILP append failure (tick_persistence.rs:2192). Put each behind the existing
    power-of-two throttle shape (`seal_loss_alarm.rs:131`); counters stay per-event.
  - A panic hook line is written synchronously to `errors.log` before abort, so the non-blocking
    switch cannot lose the one line that explains a crash.
- [x] **PR2 — a non-finite price can never reach an ILP float column.** (`storage`)
  - Correction: the aggregator already refuses NaN/inf LTP (multi_tf_aggregator.rs:1071, :1085),
    so no NaN candle is produced today. The WRITER is still undefended
    (shadow_candle_writer.rs:483-505) and tick-row OHLC / pct columns
    (tick_persistence.rs:2257-2267) were not checked. Add one `finite_or_skip` helper: a
    non-finite value omits the column (QuestDB stores NULL) and increments
    `tv_ilp_nonfinite_skipped_total{table}`.
  - DONE AS BUILT (2026-09-26): a counter tripwire, not a skip. QuestDB already stores a
    non-finite ILP float as NULL, so omitting the column changes nothing on disk; what was
    missing was the signal. `count_nonfinite_candle_floats` counts the six float columns of each
    sealed bar on `tv_candle_nonfinite_columns_total` before the append, and
    `count_nonfinite_tick_floats` does the same for each tick row (`ltp` plus the present optional
    OHLC and average price) on `tv_tick_nonfinite_columns_total`.
- [x] **PR3 — the tick rescue never writes to disk on the drain task.** (`storage`)
  - When the rescue queue (`RESCUE_QUEUE_DEPTH` = 2, tick_persistence.rs:2841) is full, the
    buffer is spilled inline on the drain (tick_persistence.rs:2711-2726). The capture-at-receipt
    WAL already holds every one of those frames, and `note_unapplied_range` (:2752) already marks
    WAL ranges for replay. Replace the inline spill with `note_unapplied_range` + drop the buffer
    (counted as `tv_tick_rescue_deferred_to_wal_total`), so the drain never does file I/O.
    Rows are recovered by WAL replay (PR12 makes that mid-session, not boot-only).
  - DONE AS BUILT (2026-09-26): the same shape for the depth rescue, counted on
    `tv_depth_rescue_deferred_to_wal_total`.
- [x] **PR4 — the top-volume sort runs off the tick task.** (`app`)
  - `snapshot_top_volume` (dhan_feed_stack.rs:1701) ran `rank` → `sort_unstable_by`
    (volume_leaderboard.rs:1546) inside the drain's biased `select!` timer arms (1s :6684,
    3s :6693, 5s :6702, 1m :6711). Measured cost 2.95 ms at the ceiling — the drain reads no
    frame for that long.
  - **Design change, recorded (2026-09-26):** a dedicated ranking thread was NOT taken. The
    row projection reads the candle fold (`bar_for_window`) for every row, and the aggregator
    is single-owner `&mut` on the drain by a documented decision; sharing it would put a lock
    or an epoch on the per-tick fold. Instead the sweep is SLICED on the drain:
    - the cadence arm pays O(1): `VolumeLeaderboard::begin_sweep` swaps the per-cadence work
      list into a pre-sized in-flight slot and queues a `SweepJob`;
    - an idle arm placed LAST in the biased select runs `LiveIngest::step_top_volume_sweep`,
      one bounded step of `TOP_VOLUME_SWEEP_STEP_ROWS` (512) rows at a time: collect
      (`sweep_step`), sliced merge sort (`SliceSort`), gainer walk (`GainerWalk`), candidate
      publish, projection in chunks, one hand-off;
    - a cadence that fires while its previous job is queued or running is deferred: its
      baselines are rolled so the skipped window is dropped rather than merged (a merged
      window would publish a volume change over two windows under one cadence label),
      counted on `tv_top_volume_sweep_deferred_total` and logged.
  - Files: volume_leaderboard.rs, top_volume_sweep.rs (new), dhan_feed_stack.rs, lib.rs,
    tests/dhat_top_volume_sweep.rs (new).
  - Tests: `begin_sweep_and_sweep_step_rank_identically_to_rank` (proptest, = the planned
    `merged_cadence_buffers_rank_identically`), `drain_cadence_arm_does_no_sort`,
    `slice_sort_step_matches_sort_unstable_by` (proptest),
    `dhat_top_volume_sweep_begin_and_steps_zero_allocation`,
    `sliced_sweep_step_cost_at_the_authorized_ceiling` (ignored harness).
- [x] **PR4b — the catch-up seal sweep and the baseline rolls run in slices too.** (`trading`,
  `app`) — found while doing PR4. DONE (as built):
  - `catch_up_seal_all` was O(slots × TF_COUNT) on the drain's 5 s `catchup_timer` arm
    (measured 9.67 ms at the 25,000 × 24 ceiling, 2026-08-21). The arm now only calls
    `LiveIngest::begin_catch_up_seal` (O(1)); `MultiTfAggregator::catch_up_seal_slots` seals
    `CATCHUP_SEAL_STEP_SLOTS` (256) slots per idle-arm step with a resumable cursor. A fire that
    finds the sweep still running keeps the cursor, takes the newer cutoff and is counted on
    `tv_candle_catch_up_overrun_total`. The shutdown path still seals the whole book in one call.
  - PR4's leftover: `roll_baselines` walked every dirty key on the timer arm before 09:15 and on
    a deferred window. `VolumeLeaderboard::begin_roll` now swaps the list into a pre-sized
    `rolling` slot (O(1)) and `roll_step` visits `TOP_VOLUME_ROLL_STEP_KEYS` (2,048) keys per
    idle step. If the previous roll of that cadence is unfinished (no idle time for a whole
    cadence period) it falls back to the one-pass roll, counted on
    `tv_top_volume_roll_inline_total`.
  - One idle arm drives all three kinds of sliced work, one step per call, in priority order:
    roll, catch-up seal, top-volume sweep (`LiveIngest::step_idle_work`).
  - The catch-up "dropped seals" line is now `error!` (was `warn!`; re-check gap).
  - Tests: `catch_up_seal_in_slices_seals_the_same_bars`,
    `test_begin_catch_up_seal_then_step_idle_work_seals_like_catch_up_seal`,
    `test_begin_roll_then_roll_step_rolls_like_roll_baselines`,
    `test_begin_roll_falls_back_inline_while_the_last_roll_is_unfinished`, and the drain
    ratchet `drain_cadence_arm_does_no_sort` extended to the catch-up arm.
- [x] **PR4c — each top-volume board is sorted once, at its candle close, from the candle's own
  volume.** (`app`) Owner, 2026-09-26 12:34 UTC (decision card "At close") and 12:35 UTC: "when
  canldes table respective timeframes timestamps gets finsihed and done means then its
  respective top volume also shodu lrun right dude". Per tick stays O(1) count updates. Each
  1s/3s/5s/1m board is ranked once when that timeframe's bar closes, from that same bar's volume
  (fold `bar_for_window`, TF S1/S3/S5/M1 map 1:1 to the cadences), so the board and the candle
  tables agree by construction (today the board measures by arrival time, candles by exchange
  timestamp). The sort key `window_lots_milli` is an integer, so the sliced merge sort becomes a
  sliced LSD radix sort: O(k) per close, i.e. amortized O(1) per tick. Not a per-tick sorted tree
  (O(log n) per tick × 4 boards). Tests: `radix_board_order_matches_board_order` (proptest),
  `board_volume_equals_candle_volume_for_the_same_window`.
  Split into two PRs (2026-09-26), because the two halves fail independently:
  - [x] **PR4c-1 — the sliced radix sort.** `top_volume_sweep::SliceRadixSort` replaces the sliced
    merge sort; `volume_leaderboard::board_radix_key` encodes `board_order` as a 3-word key
    (`!window_lots_milli`, `security_id`, segment). A first scan ORs/ANDs each key word so a byte
    that never varies costs no pass. Done: tests `board_radix_key_sorts_identically_to_board_order`
    (the planned `radix_board_order_matches_board_order`, named for the guard),
    `slice_radix_sort_step_matches_sort_unstable_by` (proptest),
    `slice_radix_sort_step_skips_the_digits_that_never_vary`; `dhat_top_volume_sweep` still zero-alloc.
  - [x] **PR4c-2 — rank at the window's close, from the candle's own volume.** The trigger moves
    from the wall-clock timer to the fold's exchange-time watermark crossing the window's end;
    the rank key becomes the window's bar volume (`bar_for_window`), replacing the per-cadence
    baselines and their rolls; a contract already trading in a later window stays marked for
    that window's sweep. Design (written 2026-09-26, before code):
    - **Rank key.** A new fold accessor `MultiTfAggregator::window_volume(feed, sid, seg, tf,
      bucket_open)` returns the bar's own volume for window W plus whether the contract already
      has a bar AFTER W (one hash probe, O(1), no allocation). `sweep_step` takes that reading
      through a closure instead of `volume - baseline`; `delta_units` becomes the bar volume, so
      the board figure and the `candles_<tf>` row for (contract, W) are the same number.
    - **Later activity stays marked.** A contract whose open bucket is already past W is
      re-marked dirty for its cadence after it is ranked for W, so the next window's sweep
      still visits it. No bar at all for W (open bucket two or more windows later) is counted on
      a new, non-EMF counter and the contract is left off that board, never guessed.
    - **Baselines go.** `baseline[]`, `roll_baselines`, `begin_roll`/`roll_step`, the `rolling`
      lists, `RollBegin`, the drain's roll call and `tv_top_volume_roll_inline_total` are deleted;
      the per-tick `observe` keeps its monotonic gate and dirty marking. `rank` (the synchronous
      test form) reads the same closure, so the two stay identical. This also closes re-check 3's
      one new gap (2026-09-26 17:10 UTC): `begin_roll`'s saturation fallback ran `roll_baselines`
      inline on the timer arm, O(traded) during the heaviest bursts. With no baselines there is
      nothing to roll, so a skipped or pre-09:15 window simply leaves its keys on the work list
      and the next window's read drops the ones that did not trade in it.
    - **Trigger.** Window `[o, o+p)` of a cadence closes when the fold's exchange-time watermark
      reaches `o+p` (checked after each drained frame, 4 integer compares), or when the IST wall
      clock reaches `o+p+5 s` (the existing timer arms, for a quiet exchange and the 15:30 tail).
      The job carries `window_open_ist_secs`; projection writes that as `ts`. Windows outside
      [09:15, 15:30) or from another day are skipped. A cadence still busy with the previous
      window skips W and counts it, exactly as today's deferral.
    - **Honest limit.** Bars seal per contract on that contract's next tick, so the board reads a
      bar that may still be open; a late tick from a lagging socket can grow the candle after the
      board was written. The board is the bar as of the close, not a sealed-bar guarantee.
    - **Tests.** `board_volume_equals_candle_volume_for_the_same_window`,
      `test_window_volume_reads_the_bar_of_the_named_window`,
      `window_close_fires_on_watermark_crossing_and_on_wall_clock_grace`,
      `contract_trading_in_a_later_window_is_ranked_in_both`, the DHAT sweep test unchanged at
      zero allocations, and the drain guard tests updated for the new trigger.
    - **Done (2026-09-27).** As designed, with three names changed while building it. The fold
      accessor is `MultiTfAggregator::window_bar` (it returns the bar plus the open and
      last-sealed bucket starts; `volume_leaderboard::WindowRead::from_window_bar` classifies it
      as Bar / Quiet / Missing). The close trigger is `top_volume_sweep::WindowCloseClock`, fed by
      `window_close_reference_secs` (watermark less 1 s, capped at wall + 2 s, floored at wall −
      5 s) from `LiveIngest::poll_top_volume_window_close` (timer arms) and
      `poll_top_volume_window_closes_by_candles` (after each frame). The missing-bar count is
      `tv_volume_leaderboard_refused_total{reason="window_bar_missing"}`, which is not an EMF
      metric. All six named tests exist and pass, plus clock, classifier and skip-count tests;
      `dhat_top_volume_sweep` still passes at zero allocations with half the keys re-listed.
- [ ] **PR5 — honest panic handling.** (all crates)
  - Keep `panic = "abort"` (Cargo.toml:275) — a half-dead process holding sockets is worse
    than a clean restart by systemd. The two production `catch_unwind` sites are dead under
    abort (activity_watchdog.rs:366, ws_frame_spill.rs:984): relabel their comments so they do
    not claim a recovery that cannot happen in release.
  - New CI step "abort smoke": builds a tiny release-profile test binary that panics in a spawned
    thread and asserts the process exits by SIGABRT; the unit file's `Restart=` is pinned by a
    guard test.
  - `clippy::arithmetic_side_effects` denied in `crates/core/src/parser/` (fixed-offset decode),
    with each unavoidable site `checked_*`/`saturating_*`. Other crates: follow-up, flagged.
- [ ] **PR6 — O(1) active-order count.** (`trading`)
  - Correction: there is no `ActiveOrderBook`; the O(n) scans are OMS
    `active_order_count()` (engine.rs:1660) and `active_orders()` (:1650), called on every
    order-runtime event arm (order_runtime.rs:1749, :1808). Maintain an `active` counter updated
    inside the single transition function (enter-active +1, enter-terminal −1) plus a
    per-`(security_id, segment)` active index; the existing scan stays as a `debug_assert`
    cross-check. Public signatures unchanged, so exit-order callers are not edited.
- [ ] **PR7 — benches for the paths with none.** (`trading`, `storage`)
  - Criterion benches: `MultiTfAggregator::consume_tick` (hit, new slot, seal-crossing),
    seal ring push/pop, tick ILP append, candle ILP append. Budgets added to
    `quality/benchmark-budgets.toml` from the first measured run (never guessed).
  - Re-check 6 (2026-09-27): name the seal channel that allocates on the drain
    (seal_writer_runner.rs:773); measure how often the capture-log hand-off wakes the writer thread
    and batch the wakes only if it matters (ws_frame_spill.rs:1183, :1639); the ~50 ns per-price
    conversion figure is unmeasured, so restore the per-call bench or delete the claim
    (common/src/price_precision.rs:44-50).
- [ ] **PR8 — `block_in_place` on shared runtimes.** (`app`, `storage`)
  - Correction: no per-tick dynamic metric label exists on the drain (checked 7403-8285; all
    handles pre-built), so the "label cache" half is closed with no change.
  - Six `block_in_place` sites (dhan_universe.rs:1040, order_update_events_boot.rs:113,
    dhan_feed_stack.rs:5843, order_observability.rs:488, dhan_order_push_observability.rs:185,
    seal_writer_loop.rs:398). Each is moved to its own thread or `spawn_blocking` where it runs on
    the shared multi-thread runtime; the drain-side one (5843) is reduced to the shutdown path only.
  - Re-check 6 (2026-09-27): line correction, the seal writer site is seal_writer_loop.rs:445 (live)
    and :455 (shutdown), not :398.
- [ ] **PR9 — each main-feed frame is walked once, by one audited walker, and fuzzed.** (`core`, `app`)
  - Today the connection task walks every frame for a stacked disconnect
    (connection.rs:916 → dispatcher.rs:343) and the drain walks it again (dhan_feed_stack.rs:7428).
    One `FrameWalker` iterator in `core::parser` is used by both. If moving disconnect detection
    to the drain keeps reconnect detection inside the 5 s envelope, the reader's walk is removed;
    otherwise both keep the shared walker and the second pass is flagged O(packets), bounded by
    `MAX_PACKETS_PER_FRAME`.
  - New fuzz target `main_feed_frame_walk` (stacked frames, truncated tails, unknown codes).
- [ ] **PR10 — faster hashing on per-tick maps.** (`trading`, `app`)
  - `MultiTfAggregator.index` (multi_tf_aggregator.rs:382), `PrevCloseStore.closes`
    (prev_close_store.rs:94) and `SpotPriceStore` papaya map (spot_price_store.rs:297) use
    SipHash. Switch to `ahash::RandomState` — `ahash =0.8.12` is ALREADY a workspace dependency
    (Cargo.toml:140) and already used by `TickGapDetector`, so no new dependency. Keys stay the
    composite `(security_id, segment)`. Bench before/after in the PR (PR7 benches).
  - Re-check 6 (2026-09-27): the depth writer's buffer regrows from empty after each hand-off
    (depth_persistence.rs:1606-1617); pre-size it next to the tick writer's and count the miss. The
    tick row writer has the database client re-check column names on every row (row 105,
    tick_persistence.rs:2263-2362); no item covered it until now.
- [ ] **PR11 — disk ballast and token recovery without a restart.** (`storage`, `core`, `app`)
  - Disk headroom gauges and alarms already exist (disk_pressure_boot.rs:242, app-alarms.tf:312,
    :376). Missing: an ENOSPC ballast — a pre-allocated reserve file on the data volume, released
    automatically when the disk-pressure shed reaches its last level so the spill tier has room to
    finish, with a critical coded error when released.
  - Token renewal halts for the process after 5 breaker cycles (token_manager.rs:1480,
    "manual restart required") and never re-reads SSM. After the breaker opens, poll
    `/tickvault/<env>/dhan/access-token` (READ only, allowed by the minter lock §10.3) every 60 s;
    accept a token that passes the JWT shape and expiry check and is newer than the held one, then
    re-arm the breaker. The breaker also resets at the IST day rollover.
- [ ] **PR12 — mid-session WAL catch-up.** (`app`, `storage`)
  - WAL replay runs only at boot (main.rs:1001; catch-up drain dhan_feed_stack.rs:13808).
    After QuestDB is healthy again for 60 s, a background task replays un-applied WAL ranges on
    its own ILP sender, rate-capped, never on the drain. DEDUP keys make replay idempotent.
- [ ] **PR13 — no JavaScript left in CI.** (`.github/workflows/`)
  - 14 `actions/github-script` steps: safety.yml (12) and dep-freshness-nightly.yml (2). Port each
    to the `gh` CLI with identical inputs/outputs, the way fuzz.yml / chaos-nightly.yml already were.
- [ ] **PR14 — order-update receiver cannot silently skip.** (`app`, `core`)
  - Correction: `Lagged` is already counted and logged (order_runtime.rs:1346), but events are
    still lost. Raise the broadcast capacity from 256 to 4,096 (order events are rate-limited to
    ≤ 10/s, so lag then needs a multi-minute runtime stall, which D1 removes) and make a lag
    trigger an immediate local reconcile. (Clock skew is already re-sampled periodically,
    infra.rs:716 `run_clock_skew_loop`; that audit finding is closed with no change.)
- [ ] **D1 — DH-904 backoff never stalls the order runtime.** (`trading`, `app`)
  - `api_client.rs:1904` sleeps inline up to 150 s ([10,20,40,80]); callers are awaited inside
    the single order-runtime `select!` (order_runtime.rs:1335). Move the retry ladder into a
    spawned task per request that reports back on a bounded channel; the loop keeps serving every
    other arm. Unreachable today (dry_run hard-true); `dry_run` itself is not touched.
  - Re-check 6 (2026-09-27): row 158, ladder resends are not counted against our own limits; route
    them through the per-second limit and the daily budget (api_client.rs:1851-1938). The order
    limiter's comment claims a rolling, never-looser daily window (row 162, rate_limiter.rs:114):
    make the code match it or correct the comment.
- [ ] **D2 — covered by PR3 + PR12.**
- [ ] **D3 — no silent instrument drop.** (`app`, `core`)
  - Universe over capacity (dhan_live_universe.rs:280-287): today the WHOLE widened set is
    replaced by the 4 index SIDs. Missing/unreadable master (:565-576, :706-720) does the same.
  - A parked main-feed socket (pool_supervisor.rs:2211-2269, after 805/806/808/810 or a second
    804) takes up to 5,000 instruments dark for the session.
  - Rule: never trim. Overflow and a parked socket's instruments are placed on spare authorized
    main-feed capacity (5 × 5,000); when none is free, a critical coded alarm names the count and
    the current set is kept. The exact boot-time behaviour when the master alone exceeds 25,000 is
    an operator decision, asked in the thread before this PR is written.
  - Re-check 6 (2026-09-27): late option top-up refusals do not page; fold both arms in with a
    counter and an alarm that matches (dhan_feed_stack.rs:10889-10903, :10966-10981;
    error-code-alarms.tf:802). The contract top-up log promises a retry that never comes: fix the
    text and count only contracts actually subscribed (dhan_feed_stack.rs:10761-10763, :10778-10781,
    :12428-12440). Stocks with no trade by 09:30 get no options: publish the count as a gauge at
    hand-off (dhan_feed_stack.rs:12437-12448). Depth sockets are PR44's, not this item's.
  - Owner decision recorded (2026-09-26): over 25,000, fill 25,000 by priority with a critical
    page; no 4-index fallback. Scheduled next after PR40d on 2026-09-28 at the "4 index ids"
    thread's request (a 04:26 IST deploy-run boot subscribed only the 4 index SIDs and never
    widened). Split into three PRs:
  - [x] **D3a — never 4 when a list exists.** (`app`, doc-only `common`)
    - Done: over capacity, `select_live_universe` fills the envelope by priority (indices first,
      then by `(segment, security_id)`, since the list has no rank and row order is not a contract), counts `refused_over_capacity`, and pages through the existing
      live-lane fallback alarm with `reason="truncated_to_capacity"` (seeded at 0). Today's list
      missing or unreadable: the boot takes the newest earlier day's list still on disk (lookback =
      the rider's `ARTIFACT_RETENTION_DAYS`, 7), same NTM → F&O → full precedence, empty lists
      skipped; paged through the same counter and reason as before when the boot should have
      widened, `warn!` with `source=pre_rider_boot` when it could not. The collapse alarm still
      fires only when no list is on disk at all. Stale text fixed: module doc, `main.rs`
      comment, `market_ram_store_boot.rs`, the master-off log line, the wait-arm lines,
      `MAX_DAILY_UNIVERSE_SIZE` doc, the alarm description.
    - Not done here: widening a RUNNING session (D3b); the `ntm_*` reasons are still not seeded
      and the `fno_*` widenings still count on the paging counter (pre-existing, noted for D3c).
    - Tests: `over_the_envelope_fills_the_capacity_by_priority_indices_first`,
      `exactly_at_the_envelope_is_not_a_truncation`,
      `earlier_ist_dates_walks_back_across_a_month_boundary_newest_first`,
      `the_lookback_takes_the_newest_earlier_day_and_never_rereads_today`,
      `the_lookback_skips_an_empty_list`, `the_lookback_keeps_todays_precedence_within_a_day`,
      `the_lookback_stops_at_the_retention_window`,
      `resolve_looks_back_exactly_as_far_as_the_rider_keeps_files`, property
      `an_oversized_master_fills_the_capacity_by_priority_and_reports_it`, and the extended
      `the_expected_fallback_cannot_reach_the_collapse_alarm`.
    - Re-check 7 folded in (2026-09-28): (a) the paging counter's zero seed and its one boot
      increment run microseconds apart, so the agent's first sample was already 1 and was
      dropped as the delta baseline. The earlier-list and over-capacity cases page ONLY through
      that counter. A degraded session now re-counts once a minute
      (`run_degraded_universe_heartbeat`, O(1) per tick), so the alarm stays red until a healthy
      restart. (b) The pre-rider verdict is judged again after `await_mapping_artifact`; a boot
      between 07:10 and 08:00 IST waits past the rider hour and must page if the list is still
      missing. Tests: `live_universe_degraded_reason_decodes_every_reason_and_nothing_else`,
      `every_paged_fallback_arms_the_heartbeat`, `run_degraded_universe_heartbeat_is_spawned_by_main_while_degraded`,
      `main_re_judges_the_verdict_after_the_wait_and_can_only_tighten_it`.
  - [ ] **D3b — widen a running session when today's list lands.** The lane reads the universe
    once at boot. Reuse the late attach's machinery: the set difference on the composite key goes
    to spare room on live sockets via `LiveSubscriptionCommand::Extend` and to new sockets via
    `build_feed_stack_plan`; the attach task keeps the pool until the widen is done. Each Extend
    ≤ 5,000 per socket to stay inside the 5 s top-up budget.
    - Re-check 7: the list is read once at boot, and the 08:30 boot waits only to 08:40 while
      the rider's budget is 900 s plus retries, so a slow rider always loses the race. D3b is
      the fix for both: keep watching for today's list after boot and widen when it lands.
      When D3b clears the degraded state the heartbeat stops by itself.
  - [ ] **D3c — the rest of D3:** parked-socket reassignment, late top-up refusals counted and
    alarmed, the top-up log text, the no-trade-by-09:30 gauge, and the fallback counter's
    reason hygiene (seed `ntm_*`; stop widenings paging).
    - Re-check 7 items to verify and fold in: the rider rejects the whole build when more than
      10% of the 49 NSE list downloads fail, and its error lines carry no code
      (`dhan_universe.rs`); the 4 fallback index ids got zero packets; depth-200 dials 0 of 5
      sockets during a fallback or QuestDB lag and `top_volume` goes empty, with no page; the
      universe headroom check counts spots only (~870 against 25,000) so it can never fire;
      D8's plan text says "match by symbol" but the list files carry no symbol.
- [ ] **D4 — stale-price gate on entries.** (`trading` risk, not strategy)
  - No price-age check exists (risk/engine.rs:262). Add one to `check_order_in_segment`: an ENTRY
    whose last price is older than 5 s is refused with a coded reason; exits are never gated.
  - Re-check 6 (2026-09-27): correction. As written, D4 would refuse every paper entry. The paper
    order book has had no price source since 16 September (the per-minute price pulls were removed),
    so every entry reads as having no price. D4 first wires a price source into the paper book (from
    the live feed's in-memory prices), then adds the 5 s gate; the Edge Case 'no price is a stale
    price' stays. Also (row 160): an open position with no price or a non-finite one adds zero to
    the loss (risk/engine.rs:852-869); count it and alarm, since the gate covers entries only.
- [ ] **D5 — headroom alarms.** Already present for disk (used %, fill rate) and host memory
  (app-alarms.tf:535). Only the ballast-released signal from PR11 is new; it rides that PR.
- [ ] **D6 — the two remaining shell scripts become Rust.** (`app`)
  - `deploy/aws/holiday-gate.sh` (132 lines, tickvault-holiday-gate.service:36) and
    `scripts/ensure-questdb.sh` (230 lines, tickvault.service:106 `ExecStartPre=-`) become
    subcommands of the app binary; the unit files call them; `rust_only_guard.rs` allowlist shrinks.
  - Re-check 6 (2026-09-27): corrections. The finish tests cover neither a port nor a budget on
    shell inside Rust strings, and 'allowlist shrank' is vacuous against empty allowlists, so the
    test compares against the recorded counts (files, workflow `run:` lines, shell in Rust strings).
    The log tool's doctor script is an item line, not a note (tools.rs:1252-1257), and the two Node
    launchers in .mcp.json (:4, :14) are named. The console shell grew to 393 lines with PR28a,
    including the 263-line SEBI-save program copied into reset and nuke (PR48 makes it one constant
    first). The inventory rows the plan does not cover yet (deploy and ops workflows, autopilot and
    upgrade scripts, CI gate scripts, other workflow steps, git hooks, Claude hooks, the Makefile,
    operator and dev tooling, manual-only hooks) each get a stated verdict: product path (port or
    budget) or not product path (recorded as out of the rust-only scope). The All Green matrix
    script is rule-locked to shell and needs an owner quote before it changes.


### Added 2026-09-26 (second re-check, 26 new open gaps), riskiest first

Source: the re-check comparison page, rows marked "New this check". Each location below is
from that check and is re-read against the code when its PR is written; a row that turns out
wrong is corrected in this file, not silently dropped.

Order of work: D9 rule amendments → PR4c → PR15 → PR16 → PR17, then PR5–PR14 as before (with the additions
folded into them below), then PR18, PR19 and the decisions.

- [x] **PR15 — candle seals never write a file on the drain.** (`storage`, `app`)
  - DONE (2026-09-27): the escalation thread already existed (2026-08-28); what was left was its
    4,096 queue (under 2% of one 225,000-seal close burst) and one `write(2)` per record. The queue
    is now `SEAL_BUFFER_CAPACITY` deep (~32 MB committed, stated) and the thread writes
    `SEAL_ESCALATION_BATCH` (1,024) records per write, cutting a torn tail back on failure
    (`SealSpillWriter::append_seals`). The inline fallback stays, counted, as the last resort past
    one whole burst against a stalled disk. Mid-session replay: `MidSessionReplay` in
    seal_writer_task.rs, stepped from the writer loop's tick only (never the shutdown drain):
    60 s of clean live flushes, live ring empty, ≤ 512 seals per 100 ms (half capacity, WAL apply
    lag), live file staged under the spill append lock (`with_appends_paused`), `replaying/` →
    `archive/` like boot, a failed flush discards, keeps the file's position and halves the next
    step; two failures at a step of one record skip that record (poison row). Known limits stated
    in code: a replayed older seal can overwrite a newer amended bar (same as the boot drain); the
    gate needs live traffic to reopen; a full queue on a disk that refuses writes does not drain
    inside the 5 s shutdown budget (the rest is counted as abandoned). Counters `tv_seal_replay_total{kind}` seeded at 0.
    Tests: `replay_*`, `staging_the_live_file_while_appends_race_loses_no_seal`,
    `the_escalation_thread_batches_a_burst_*`, `a_failed_batch_write_falls_back_*`,
    `the_queue_depth_is_drainable_inside_the_shutdown_budget` (rewritten),
    `one_poison_record_is_isolated_and_skipped_and_every_other_record_lands`.
  - When both seal queues are full the seal spill takes a lock and writes a file on the drain
    (seal_writer_runner.rs:466-497, seal_spill.rs:798-833). D2 covers ticks, not seals. Give the
    seal spill its own writer thread behind a bounded hand-off, the tick path's shape.
  - The seal spill is replayed only at boot (`read_all` has no mid-session caller): replay it
    after QuestDB has been healthy for 60 s, rate-capped, like PR12.
  - Dropped seals were logged with `warn!` (dhan_feed_stack.rs:6634): DONE in PR4b.
  - Re-check 6 (2026-09-27): correction. The title overstates. The inline fallback still writes on
    the drain once both queues are full, and its bounded wait is in bytes, not time, so a stalled
    disk makes it wait without limit (row 27, seal_writer_runner.rs:562-569, :575-615;
    seal_spill.rs:725-729, :795-846). The seal figure in this item and in the code comments is
    wrong: one close burst is 250,000 seals (25,000 × `TF_COUNT` 10), not 225,000, and the three
    queues (writer channel, ring, escalation queue) hold up to 750,000 between them, of which PR15
    added 245,904 by growing the escalation queue from 4,096 to 250,000. None of it is counted on an
    abort. The last three bullets of this item (the seal spill's own thread, the boot-only replay,
    and the `warn!` line) are the pre-PR15 text and are superseded by the DONE bullet. The open work
    (counting or persisting queued seals at a crash, pruning the archive and replaying folders,
    shipping the seal loss counters, the tests that prove batching, the pause and the real replay,
    and a decision on the inline fallback) is carried by PR40.
- [ ] **PR16 — nothing blocks the shared worker threads, and the drain gets its own thread.**
  (`storage`, `app`)
  - `df` is forked with no time limit from seven sites (disk_health_watcher.rs:121, :163, :239;
    disk_pressure_boot.rs:266; resource_monitor.rs:606; wal_suspension_watcher.rs:1225;
    partition_archive.rs:1724). Read free space with `statvfs` through `nix`/`libc` if already a
    workspace dependency, else `spawn_blocking` with a timeout (no new dependency without approval).
  - Spill replay reads up to 32 MiB with blocking calls (tick_spill_replay.rs:539, :593): move to
    `spawn_blocking`.
  - The disk-pressure archive gzips gigabytes on the shared pool during market hours
    (partition_archive.rs:2500-2522, disk_pressure_boot.rs:546): run it on its own thread. The
    archive stays O(bytes); only where it runs changes.
  - The frame drain shares the multi-thread runtime (dhan_feed_stack.rs:14291, main.rs:505):
    run it on a dedicated current-thread runtime on its own OS thread, so no other task can hold
    its worker.
  - Re-check 6 (2026-09-27): also, the boot candle recovery (seal_writer_loop.rs:496; main.rs:3619)
    and the 15:41 cross-check write (dhan_live_crossverify_boot.rs:617-622, :957) block a shared
    worker; the web handlers do blocking file work on shared threads
    (api/src/feed_state_persist.rs:167-195; api/src/handlers/debug.rs:146, :176, :346), including
    the board error-file endpoint's listing and sort on every poll (row 176, debug.rs:345-376); the
    drain can free the old contract-name table in place (dhan_feed_stack.rs:2443-2446), so drop it
    off the drain.
- [ ] **PR17 — spill files survive a host crash and a torn line.** (`storage`, `core`)
  - Spill, dead-letter and replay-marker files are never flushed to disk before the marker moves
    (ws_frame_spill.rs:566; tick_persistence.rs:1344-1349, :2837, :3383): `sync_data` before the
    marker advances, off the drain.
  - A torn last line quarantines the whole hour (tick_spill_replay.rs:620-623): skip and count
    the torn line, keep the rest.
  - A frame the capture log refused and later deferred to it is labelled "deferred"
    (pool_supervisor.rs:3924-4002, tick_persistence.rs:2791): fix the label; counter and alarm
    are already right.
  - Market data packed in the same frame as a disconnect message is thrown away
    (connection.rs:1885-1915): capture the frame before closing. Lands with PR9's walker if that
    PR is first.
    **Done in PR21 (2026-09-27).**
  - Re-check 6 (2026-09-27): a disk that fills in the middle of a spill write can leave a torn line
    anywhere, not only last, so skip and count every torn line (row 133,
    tick_spill_replay.rs:710-740). The sync bullet also covers the candle spill, now written in
    batches of 1,024 records and still never synced. Recorded as a limitation, not a fix: the
    capture log segment can lose up to 1 s of records on power loss (`WAL_FSYNC_INTERVAL_MS_DEFAULT`
    = 1,000), and an environment value of 0 turns syncing off.
- [ ] **Folded into existing PRs:**
  - PR5: seven more dead crash-recovery sites under `panic = "abort"` (order_leg_pnl_boot.rs:221,
    day_ohlc_orchestrator.rs:315, tf_consistency_boot.rs:2287, order_runtime.rs:437,
    dhan_order_push_observability.rs:390, disk_health_watcher.rs:302, order_readiness.rs:346).
  - PR6: the pending paper-order list is rebuilt after every event (order_runtime.rs:915-925).
  - PR7: the 162-byte full packet has no bench; the seal ring has no allocation test.
  - PR10: two more per-tick maps (volume_leaderboard.rs:651, contract_underlying_map.rs:670).
  - PR12: the tick-spill replay floods QuestDB when it comes back (tick_spill_replay.rs:1050-1124);
    the same rate cap applies.
  - D6: four more shell scripts (deploy/aws/user-data.sh.tftpl, host-tuning/apply-host-tuning.sh,
    sysctl/verify-net-tuning.sh, tickvault-host-tuning.service:119-147, operator_control.rs:1913).
- [ ] **PR18 — order-side lookups are O(1) and segment-keyed.** (`trading`, `app`)
  - Unrealised P&L for the daily-loss check walks every position (risk/engine.rs:834): keep a
    running total updated on each fill and mark.
  - Paper-fill lookups use the security id alone (order_runtime.rs:915-929, risk/engine.rs:778):
    composite `(security_id, segment)` key. Paper mode only; `dry_run` is not touched.
- [ ] **PR19 — small cleanups.** (`app`, `core`, `api`, `tickvault-logs-mcp`, CI, deploy)
  - Per-minute depth steering reloads data it never uses (depth_rebalance.rs:1888, :1895,
    :2044-2051): drop the reload.
  - Quote endpoint caches and queries by id without segment and builds an HTTP client per miss
    (api/src/handlers/quote.rs:71, :84, :91, :140-145; response_cache.rs:176-180).
  - Log-query tool runs any SQL (tickvault-logs-mcp/src/tools.rs:669-684): read-only statements only.
  - The benchmark gate does not block merges (bench.yml:56-59): make its regression fail the run.
  - Stale restart-limit comments (deploy/systemd/tickvault.service:120-121, :333).
  - Re-check 6 (2026-09-27): the token-failure bullet is removed (PR39 took it over).
    'CLAUDE.md facts' names the wrong values, for example 15 routes, not 12. Name the stale
    log messages, alarm text and the memory gauge with no source (row 188). The quote endpoint's own
    3-second client timeout still reads as 404 after the reply-status check (row 172, quote.rs:24,
    :149-155). The board error-file endpoint answers 404 with a path on any read error (row 176,
    debug.rs:345-376). A request flood fills the lossy log sink: bad-token warnings, other routes
    and unknown paths still log per request (row 173, middleware.rs:519-546; lib.rs:284-299). A
    read-only query can still tie up the database: set a server query time limit
    (docker-compose.yml; sql_gate.rs:136-161). The boot log claims a deleted tick feed for the day
    high/low tracker (main.rs:2577-2580). The in-memory tick store for the 2026-09-01 directive is
    not wired (tick_ram_arena.rs:52-60): wire it or record the directive as withdrawn. Notes that
    describe costs and limits the code lacks (response_cache.rs:102-110; public_guard.rs:1-12;
    handlers/board.rs:61-65; constants.rs:40-41; spot_price_store.rs:240-251). Code nothing calls:
    the two instrument modules with their scope guard, the disconnect builder and the code-3
    constants (core/src/instrument/mod.rs:49-50;
    storage/tests/daily_universe_scope_guard.rs:115-216), and the two spill readers with no
    production caller (seal_spill.rs:991; seal_dlq.rs:330), deleted or marked test-only. The
    cloud-log retention comment that says cold storage keeps older logs is false (logs expire after
    14 days and nothing exports them): fix the comment; an export is the owner's cost call.
- [ ] **D7 — every socket refused at once (805, second login).** (`core`) All sockets park together
  and D3 has no spare to move to (pool_supervisor.rs:738-748, :1786-1832). Owner asked
  2026-09-26; the recommended option is: wait 5 minutes, redial one socket as a test, bring the
  rest back only if Dhan accepts it, critical alert either way. Until the owner picks another option, that default is what gets built.
  - Re-check 6 (2026-09-27): row 64, state that depth stays dark until a restart after an 805,
    because every depth dial refuses once the stop switch is set, or get an owner decision on a
    separate resume permit. The one-socket test dial must watch every socket for an 805 over a
    window, since Dhan accepts a new socket and closes the oldest (pool_supervisor.rs:1818-1829;
    dhan_feed_stack.rs:12102, :12865-12887).
- [ ] **D8 — index ids from the instrument file replace the fixed four.** (`app`) When the master
  yields any index rows they REPLACE the four seeds (dhan_live_universe.rs:269-277). That swap is
  deliberate and evidence-backed (seed ids measured receiving zero packets, comment above it), so
  it stays. The gap is that an index the fixed list names (for example India VIX) can vanish
  silently if the master's index rows omit it. Default chosen: keep the swap, and when a fixed
  index has no master counterpart by symbol, keep that seed and raise one coded error naming it.
- [ ] **D9 — a second Dhan account for the depth sockets only.** (`core`, `app`, `aws-lambdas`,
  terraform) Owner, 2026-09-26 13:05 UTC: "i will get my friends accoutn as the seocnd accoputn
  oen and only to haev this extra depth 20 and dpeth 200 websockets alone"; 13:09 UTC: "yes dhan
  said go ahead with the secodn accoutn dude okay?". Not started until the
  account exists and the rule files are amended FIRST (rule-file-first law): the WebSocket scope
  lock (today ≤ 16 connections on ONE account) and the token-minter lock (§10.4 rejects a second
  Dhan minter "anywhere"). Shape when it lands: a second credential set under its own SSM path,
  its own minter publishing its own token parameter, a per-account socket budget (main feed and
  order updates stay on the owner's account), 805/807 handling and the D7 probe run per account,
  and every alarm and log line names which account.
  Dhan's answer (madefortrade topic 94246, post 4, DhanStaff, 2026-09-24): depth limits "are
  fixed and cannot be increased", "applicable on a per Client ID basis", and "you may consider
  using multiple Client IDs, as the limits are tracked independently for each Client ID". The
  question it answered described a second account in the owner's OWN name on the same server
  and static IP; a friend's account and the same-IP point were not addressed. Owner chose
  (2026-09-26 13:16 UTC, decision card): the second account is in the owner's OWN name, the
  case Dhan answered. Rule amendments are the first PR after the current fix.
  - [x] **D9a — rule amendments (#1949, merged).** `websocket-connection-scope-lock.md` § "2026-09-26 — A
    SECOND DHAN ACCOUNT…" (depth account: 5 + 5 depth sockets, total ≤ 26, own SSM path, ships OFF,
    `ROTATION_HALTED` kept process-wide) and `groww-shared-token-minter-2026-07-02.md` §10.9 (one
    minter per account). Both summary stubs updated.
  - [ ] **D9b — the code, in three PRs.**
    - [x] **D9b-1 — the depth account's minter (this PR).** `dhan_token_minter.rs` reads
      `SSM_SERVICE` against a closed list of two (`dhan`, `dhan-depth`; unset = `dhan`, anything
      else fails the mint) and threads the segment through the read and the write. New
      `deploy/aws/terraform/dhan-depth-token-minter-lambda.tf`: same zip, own role reading the
      three enumerated `/dhan-depth/` credentials and writing `/dhan-depth/access-token` only;
      schedule `state` and the not-invoked alarm gated on `var.dhan_depth_account_enabled`
      (default `false`). Primary minter sets `SSM_SERVICE = "dhan"` explicitly. Two alarm
      phrases added. Tests: `dhan_token_minter::tests::parse_ssm_service_*`,
      `a_depth_account_run_*`, `the_two_account_segments_never_share_a_parameter_path`,
      `crates/aws-lambdas/tests/dhan_depth_token_minter_wiring_guard.rs`.
    - [x] **D9b-2 — widen 16 to 26 with no behaviour change.** Per-account `PoolBudget` counters
      (depth account at global indices 16..25), `MAX_TOTAL_DHAN_CONNECTIONS`, the slot-label
      array, `RECONNECT_JITTER_SLOTS`, the per-connection arrays, `endpoint_for_slot`, the
      depth-200 tick-age exclusion range, `kernel_tuning_16ws_guard.rs` and the sysctl budget text.
      Done: `pool_budget::{DhanAccount, slot_owner, PoolBudget::try_open_on, release_on}`,
      `RECONNECT_JITTER_SLOTS = 26`, `dhan_feed_stack::endpoint_for_slot` via `slot_owner`.
      Tests: `test_slot_owner_both_accounts_tile_all_twenty_six_slots_exactly_once`,
      `test_depth_account_refuses_main_feed_and_order_update_without_mutating`,
      `test_reconnect_jitter_ms_unchanged_for_the_primary_account_slots`,
      `the_depth_accounts_slots_follow_the_same_rules`,
      `the_twenty_six_socket_budget_is_recorded_and_gates_the_depth_account`.
    - [ ] **D9b-3 — wire the pool behind `[dhan_depth_account] enabled = false`.** Own client id;
      a READ-ONLY token source re-read from `/dhan-depth/access-token` (an 807 on the depth account
      re-reads, never mints: minting from the box would fight its Lambda); `account` label on
      every depth log, counter and alarm; the second pool static (no steering) until measured.
- [ ] **D10 — no depth path relies on unsubscribe.** (`core`) Dhan depth unsubscribe (codes 25
  and 24) takes no effect and gets no reply (madefortrade topic 94234; Dhan "reviewing" as of
  2026-09-26). Depth-200 already rotates by redial and depth-20 is a static day set, but
  `send_unsubscribe` still has depth-pool call sites (pool_supervisor.rs swap paths). Verify
  each is unreachable for depth endpoints or make it redial-only, and pin that with a guard.


### Added 2026-09-27 (fourth re-check, 15 new open gaps), riskiest first

Source: re-check 4 on main 98813f1 (comparison page version 5, rows "Open" + "New this check").
Locations are that check's and are re-read against the code when each PR is written; a wrong
row is corrected here, never dropped.

Order of work: PR20 goes NEXT, ahead of PR15, because it can delete SEBI rows. Then PR21 →
PR22 → PR15 → PR16 → PR17 → PR23 → PR24, then PR5–PR14 (with the additions folded in below),
PR25–PR27, PR18, PR19 and the decisions. One PR open at a time, as before.

- [x] **PR20 — the operator console can never delete SEBI rows.** (`aws-lambdas`)
  - Docker reset and a bare nuke delete the whole database volume, SEBI tables included, when
    QuestDB does not answer; the SEBI export is skipped with one printed line
    (operator_control_action_commands.rs:90-92, :165-167). Daily-universe Quote 25 REJECTs
    exactly this. Fail closed: no volume delete unless the SEBI export for the day succeeded and
    was verified; a failed or skipped export stops the action with a coded error and a page.
  - The destructive-action lock is 09:15–15:40 but the rule says 09:00–15:45
    (operator_control.rs:63, :79, :166-174). Widen it to the rule's window and pin the two
    constants with a guard test.
  - Done 2026-09-27: `DOCKER_RESET_COMMANDS` / `DOCKER_NUKE_BARE_COMMANDS` preserve step. An
    unreachable QuestDB, or a SEBI table behind on its WAL, is copied raw off the stopped volume
    (verified by file count and bytes, restart fingerprinted) instead of stopping the action, so
    the action stays the remedy for a wedged QuestDB; every unproven step goes through
    `sebi_abort` (LAMBDA-PORTAL-01, SNS page, app re-enabled). The box re-checks the lock before
    the first deletion. `is_data_destructive_locked` + `DATA_DESTRUCTIVE_LOCK_{OPEN,CLOSE}_SECS`
    (09:00–15:45 every day). Tests: `destructive_actions_export_the_sebi_tables_before_destroying_the_volume`,
    `test_is_data_destructive_locked_boundaries`, `test_data_destructive_lock_window_matches_the_rule_file`.
- [x] **PR21 — after an 805, nothing dials another depth socket.** (`app`, `core`)
  - Only `depth_rebalance.rs:1205` checks `ROTATION_HALTED`; the morning depth attach and the
    contract top-up keep dialling, and each extra socket makes Dhan close a healthy sibling
    (dhan_feed_stack.rs attach loop 10946-12260, :11862-11876). Every depth dial site checks
    the breaker; a guard test scans for dial sites that do not.
  - A depth disconnect packed in a frame with data loses its reason code and is redialled as a
    routine drop, so an 805 there closes a sibling without setting the breaker
    (core/src/websocket/connection.rs:916-950; dhan_feed_stack.rs:8959-8963). Read the reason
    code before the data. Shares the frame walker with PR9 and the PR17 capture fix.
  - Done 2026-09-27: `classify_frame` walks depth frames with `stacked_depth_disconnect_reason`
    (depth.rs); the attach, the one production spawn (`dial_planned_connections`), the drain's
    ghost request, the connection task's ghost/probe close and probe Arm B all read
    `rotation_halted()` first, counted on `tv_depth_dial_refused_after_805_total{path}`;
    source-scan tests pin each site. Routine reconnects of an existing socket stay allowed
    (they keep coverage). The PR17 capture bullet is also done here: a disconnect stacked
    behind data now hands the frame up and closes on the next read (`pending_close`).
- [x] **PR22 — an 807 renews the token at most once.** (`core`)
  - The "already renewed" generation is read when the call starts, not when the 807 arrives,
    so two sockets can each trigger a renewal (token_manager.rs:1272-1297;
    pool_supervisor.rs:1834-1867). Capture the generation at 807 arrival and compare-and-swap.
  - Done 2026-09-27, one step earlier than planned: each feed socket records the generation
    at DIAL (`feed_token_recording_generation`, read before the token), and its post-807
    refresh calls `TokenManager::force_renewal_unless_replaced(dialled)`, compared under the
    existing single-flight gate. Arrival-time capture would still renew twice after a
    scheduled renewal replaced the token the sockets dialled with. The two boot adoption
    installs now bump the generation too (ratchet `every_token_install_bumps_the_renew_generation`).
    Not changed: the mid-session watchdog's `force_renewal()` (not an 807 path; sockets now
    re-dial instead of renewing if it replaces a fresh token).
- [ ] **PR23 — the day's last candles are sealed at the close, not at shutdown.** (`app`, `trading`)
  - Candles after ~15:36 and the day's last 15/30/60-minute candles are sealed only at shutdown
    (dhan_feed_stack.rs:6125, :4361-4365, :7668); a crash loses them unless the next boot's WAL
    replay rebuilds them (Assumed, not verified). Seal every open bar at the session close on
    the drain's idle arm, the PR4b catch-up shape, and test the crash case.
- [ ] **PR24 — a future-dated timestamp cannot move a watermark past what is durable.**
  (`storage`, `trading`)
  - One future-dated vendor timestamp can push the WAL "applied" watermark ahead of what is
    durable, and the archive ignores apply lag. Cap each watermark at the wall clock (the
    PR4c-2 `window_close_reference_secs` shape, which caps the top-volume close clock at the
    wall with no lead) and gate the archive on applied, not written.
- [ ] **PR25 — a misspelled config key fails the boot.** (`common`)
  - Unknown keys are silently ignored, so a typo in `live_subscription_from_master` silently
    runs the four-index fallback (common/src/config.rs:21-110). `#[serde(deny_unknown_fields)]`
    on every config section, with a test per section. Touches `common`, so workspace tests.
  - Re-check 6 (2026-09-27): row 179, production.toml has a fourth unread key, `max_position_lots`
    (config/production.toml:25; config.rs:2713-2717; trading_pipeline.rs:133), which PR25 must
    delete or the production boot fails; the gap list says PR25 must land after PR35 (the reason is
    re-checked when PR25 is written). A missing production settings file is skipped silently
    (main.rs:562-569): require the named environment's file. Three Dhan settings change nothing
    (base.toml:175-176, :755): delete or wire `target_rps`, `instrument_csv_url` and the
    compact-file fallback.
- [ ] **PR26 — the token minter retries at most twice in total.** (`aws-lambdas`)
  - The AWS SDK adds platform retries (up to 6 attempts) under the §10.8 cap of two TOTP
    attempts. Set the SDK retry config so the whole mint is two attempts, pinned by a test.
- [ ] **PR27 — the box reads the token instead of minting it.** (`core`, `app`)
  - The box still mints at boot while the Lambda also mints (§10.3), so a box running across
    06:05 IST can clash with the Lambda. Switch the box to READ `/dhan/access-token`, keeping a
    mint only as a loud, coded last resort if the parameter is missing or expired.
  - Re-check 6 (2026-09-27): row 128, the 807-path re-read of the stored token needs a time limit
    and must probe the token before adopting it, because the SDK client has no operation timeout and
    the renewal lock is held (secret_manager.rs:117-124; token_manager.rs:1308). The shared token
    can be overwritten by an older one (dhan_token_publisher.rs:66-74, :94-106): order the
    publishes, or check expiry before overwriting, and add a time limit.
- [ ] **D11 — Muhurat trading is captured.** (`common`, `aws-lambdas`) Sunday 2026-11-08 is never
  captured: the holiday gate and the weekday start window keep the box off
  (common/src/session_window.rs:164-181; start_watchdog.rs:127-135). Default: add a dated
  special-session entry that opens the capture window for that evening only. The owner confirms
  the date and hours before it ships.
- [ ] **Folded into existing PRs (fourth re-check):**
  - PR4c-2 (done): the reviewer's design risk ("the fold keeps only the last sealed bar, so a
    late sweep reads the wrong bar or drops rows") is handled: `window_bar` returns a bar only
    when its bucket start IS the named window, a bar already sealed over is `Missing`, counted
    (`reason="window_bar_missing"`) and left off the board, never replaced by a neighbour. The
    PR4c-2 review (2026-09-27) found that "sealed over" was the COMMON case for a contract
    trading every second, so each board-frame seal also keeps the window's volume in a
    4-deep per-contract history in the leaderboard, and the board reads it there; the close
    clock ranks windows that close together oldest first (catch-up bound 3); the candle clock
    is capped AT the wall clock (each frame's arrival time), not wall + 2 s. Tests:
    `busy_contract_is_ranked_after_the_fold_sealed_over_its_window`,
    `windows_that_close_together_are_each_ranked_in_turn`,
    `test_record_sealed_window_ranks_a_contract_the_fold_sealed_over`,
    `test_window_close_clock_catches_up_oldest_first_within_the_bound`,
    `test_far_future_watermark_never_closes_a_window_ending_after_the_wall`,
    `test_a_bar_with_no_volume_is_quiet_not_a_zero_lot_fault`.
  - PR5: about eight more `JoinError::is_panic` arms.
  - PR7: `dhat_telegram_dispatcher` runs zero tests without `--features dhat`, so it proves
    nothing in CI; make it run in the normal test lane. Tick parser measured 14.6 ns against a
    10 ns budget: re-baseline or fix, never hide.
  - PR12: state that it lifts the 512 MiB boot cap.
  - PR16: the other `df` sites (~15 callers through one probe: fix inside the probe), the
    prunes, the universe rebuild, and the token-cache write + fsync done while holding the
    renewal lock on a shared worker.
  - PR19: `/api/quote` with an unknown id does a full scan; the HTTP server has no timeouts or
    connection cap. Liveness paging stops at 15:35 against the 15:40 close.
  - D3: the wording names the contract-layer trim as well as the subscription cap.
  - D6: the cited line is tickvault-host-tuning.service:74 (not 119-147). Scope grows to the
    operator console's ~13 shell call sites, the deploy/ops scripts, the `chronyc` and `docker`
    shell-outs in Rust and 8 orphan scripts. `rust_only_guard` still allows bash/sh everywhere
    (104 shell files, ~18.2k lines, plus the Makefile, CI run steps, a systemd `sh -c` and SSM
    command strings), so the allowlist shrink D6 promised is empty today: D6 shrinks it for real.
  - D9b-3 (still paused on the owner's rotation decision): a retire bug and a flag that is read
    but ignored, per the page rows marked D9b-3.
  - CLAUDE.md: the catch-up sweep measured 4.73 ms at 25,000 × 10 timeframes; the 9.67 ms row
    assumes `TF_COUNT` 24 and is stale. Corrected in the PR that next touches that row.


### Added 2026-09-27 (fifth re-check, 138 findings), riskiest first

Source: re-check 5 on main c0e4829 (comparison page version 6; the full list with evidence is
`/mnt/project-files/audit/recheck5-gaps.md`). Locations are that check's and are re-read against
the code when each PR is written; a wrong row is corrected here, never dropped.

Order of work: PR15 finishes first. Then, riskiest first: PR28 (SEBI data off one disk, console
wipe; split 2026-09-27 into PR28a, the console, and PR28b, the locked cloud copy) → PR29 (log
tool can change the live database; PR29b closes its base_url bypass, merged as #1963) → PR36a
(console key sent to the alerts topic) → PR36b (manual deploy without All Green) → PR40 (a crash
loses queued seals) → PR41 (a replayed seal overwrites a newer candle) → PR31b (a restart can
overwrite fuller candles; shares PR41's never-replace check, so it follows it) → PR42 (order and
P&L audit rows dropped while the database is down) → PR43 (index option legs dropped from
depth-20 past 246 spots) → PR44 (a depth socket parked without an 805 stays dark) → PR45 (the
09:16 self-test never runs) → PR46 (box role and Docker socket) → PR47 (Telegram Critical page
lost in a burst) → PR48 (console follow-ups) → PR49 (budget stop follow-ups) → PR50 (holiday
gate) → PR31c (PR31a's honest limits) → PR32 (tick rescue and spill ordering) → PR33 (token and
socket gaps) → PR34 (hung app never restarted) → PR35 (deploys) → PR36 (security) → PR37 (Dhan
documentation mismatches) → PR38 (risk book across a restart) → PR39 (every error line coded,
every loss counted and shipped) → PR51 (every cited guard exists; extends PR39's guards) → the
PR4c follow-ups → PR52 (board sort; needs a Graviton measurement, so the box up), then the
remaining order from the fourth re-check (PR16, PR17, PR23, PR24, PR5–PR14, PR25–PR27, PR18,
PR19, decisions). PR30 (budget stop undone the same day) and PR31 (a restart can overwrite
fuller candles) were split 2026-09-27: PR30a and PR31a are done, PR30b stays date-bound (on or
after 2026-10-01) and takes the first slot free on that date, and PR31b and PR31c sit where
shown. The order after PR29b was set by re-check 6 (2026-09-27). One PR open at a time.

- [x] **PR28a — the console cannot wipe kept data, and nothing destructive runs in the lock.**
  (`aws-lambdas`) Split out of PR28 on 2026-09-27: this half needs no cloud change and no cost.
  - The console wipe truncated `prev_day_ohlcv` and the four `rest_*` tables, all on the
    never-delete list and named KEPT by daily-universe Quote 21. Its targets are now `ticks`,
    `market_depth` and the candle tables only (`operator_control_action_commands.rs`
    `WIPE_QUESTDB_COMMANDS`), pinned by `test_wipe_never_targets_a_never_delete_table`, which
    reads the never-delete list from `partition_manager.rs`.
  - Wipe, reset and nuke now re-check the 09:00–15:45 IST lock on the box BEFORE the first stop
    (`ON_BOX_LOCK_GUARD`, exit 3, nothing stopped); the reset's end-of-save `lock_check` stays.
    SSM gives up on a command the box has not picked up within 120 s
    (`SSM_DELIVERY_TIMEOUT_SECS`), and the console reports that as not run
    (`DeliveryTimedOut`). Tests `test_on_box_lock_guard_runs_before_the_first_stop`,
    `test_ssm_shell_sets_a_short_delivery_timeout`.
  - A kept table missing from the one `tables()` reply (reply cut short, metadata not loaded,
    folder under a renamed table's old name) was treated as absent and deleted with the volume.
    Every kept-table folder that was not exported is now copied off the volume raw and checked
    file for file (`raw_copy`, shared with the raw mode). A volume whose folders cannot be read
    stops the action.
  - The save is streamed to `s3://tv-prod-cold/sebi-preserve/<stamp>.tar` as one object and
    its size in the bucket must equal the bytes tar wrote before anything is deleted; the box
    copy is kept (Quote 25 forbids deleting it, so it is NOT capped). Test
    `destructive_actions_refuse_an_unsaved_table_and_copy_the_save_off_the_box`.
  - Reset exits no longer leave the app disabled: `cd … || exit 0` is gone, and the
    `docker-reset-FAILED` exit brings QuestDB back, re-enables and restarts the app. Test
    `test_every_exit_after_the_disable_re_enables_the_app` (the save step's only `exit 2` is
    the flock-busy line, whose lock holder re-enables).
  - Honest limits: the cloud copy goes to the unlocked cold bucket until PR28b; the size check
    is per object, not a per-file checksum (the CLI checksums each upload part); the on-box
    guard trusts the box clock.
  - Re-check 6 (2026-09-27): found four holes, carried by PR48: a reset or nuke started 08:30-08:59
    runs into the open; a reset that outlives the SSM execution time limit (3600 s by default) can
    leave the app disabled; a failed count query still prints WIPE-COMPLETE; and there is no Muhurat
    lock (that last one is D11's, per its fifth re-check fold).
- [ ] **PR28b — never-delete data does not live on one disk, and nobody can delete the cloud
  copy.** (`storage`, deploy) Waits on the owner's typed choice of lock strength
  (compliance or governance, asked 2026-09-27) and on a measured size for the budget rule.
  - `instrument_lifecycle` and `index_constituency` are pinned to ts=0, exempt from the sweep,
    and exist only on the server disk. A daily export of both to the locked location, verified
    by row count, with a coded error and a page on failure.
  - The cold bucket has no versioning (deploy/aws/terraform/main.tf:692): versioning on, with a
    short non-current expiry, so a bad delete is recoverable. Quote 24 records that versioning
    was never on; that sentence is updated in the same change.
  - SEBI audit rows leave QuestDB after 90 days (partition_manager.rs `DAY_PARTITIONED_TABLES`;
    partition_archive.rs `RetentionClass`). OWNER DECIDED 2026-09-27 11:15 UTC ("Lock the cloud
    copy"): keep the 90-day drop, and make the S3 copy of the SEBI tables write-once for 5
    years. The archive already drops a partition only after a verified upload
    (`partition_archive_guard.rs::drop_partition_requires_verified_archive_proof`); the SEBI
    tables' archive and the daily export move to a dedicated bucket created with Object Lock,
    so the drop runs only after the locked copy is verified. The reset-time save moves there
    too.
  - Terraform applies live on merge (terraform-apply.yml), and Object Lock cannot be switched
    off, so the dated owner quote goes into the daily-universe rule file first.
- [x] **PR29 — the log tool can never change the live database.** (`tickvault-logs-mcp`)
  - Free SQL goes to the live database raw, so `drop` and `truncate` pass
    (tickvault-logs-mcp/src/tools.rs:669). Reuse the operator console's read-only SQL gate, cap
    rows and reply size (:704), bound log reads, and remove the shell-outs (:679, :1017-1021).
    The runbook finder also searches `docs/error-runbooks` and `docs/claude-rules-full`
    (:609-615). This takes over the log-tool bullet in PR19.
  - Done: `sql_gate.rs` copies the console's gate (one statement, no comments, first word
    select/show/explain/with, 25 banned words) and its 1000-row cap. The test
    `gate_source_is_identical_to_the_operator_console` reads the console's source at compile
    time and fails if any copied item differs by one byte, so the two cannot drift. Refused
    text never reaches the database (`questdb_sql_refuses_a_destructive_query_before_connecting`).
    Replies over 8 MiB are refused (questdb_sql and tickvault_api). Log reads take at most the
    last 32 MiB of a file and tails at most 5,000 lines, and each reply says when a file was
    cut. `app_log_tail` refuses a `date` that is not YYYY-MM-DD (before, a path-shaped date such
    as `x/../../secret` could reach a `.log` file outside the log directory whenever a folder
    named `app.x` existed there). The runbook finder searches all four trees.
  - Shell-outs: the `aws` CLI fallback is removed (the native SigV4 path does the same read).
    `run_doctor` still runs `bash scripts/doctor.sh`; porting that script (it runs
    `cargo check` and `validate-automation.sh`) belongs to D6 with the other shell scripts.
    `git log` and `docker compose ps` stay: fixed argument lists, external programs rather than
    scripts; the only caller input is `git log`'s line count, parsed as an integer first.
  - Re-check 6 (2026-09-27): correction. The title did not hold at merge. `tickvault_api` reached
    the database's /exec endpoint around the SQL gate (tools.rs:978-1021; config.rs:125-129), and
    the prebuilt binary sessions ran predated the fix. Both are closed by PR29b (#1963); the title
    is true only once #1963 merges.
- [x] **PR29b — `tickvault_api` cannot reach the database around the SQL gate.**
  (`tickvault-logs-mcp`) Found by re-check 6 after PR29 merged: `tickvault_api` took
  `base_url` from the caller (tools.rs `tool_tickvault_api`, config.rs `endpoint_url`), so a
  GET to `127.0.0.1:9000/exec?query=DROP ...` reached QuestDB without passing the read-only
  gate, and the API bearer token went to whatever host was named.
  - Done: a caller `base_url` is refused before any connection (`API_BASE_OVERRIDE_REFUSAL`)
    and is no longer in the tool's schema; the path must be `/health` or under `/api/`, with
    no `..`, `%`, `@`, `\`, `#`, `://`, spaces or control characters (`api_path_is_allowed`);
    and the call is refused when the configured API address is the database's, spellings of
    loopback and default ports folded (`same_origin`, unparseable fails closed). The tools'
    HTTP client no longer follows redirects, so a 3xx reply cannot send the next request to
    the database (found by the security review; reqwest keeps the bearer header on a
    same-host redirect to another port). The bearer token therefore only goes to the
    configured API. The launcher no longer runs a prebuilt binary older than its sources
    (it rebuilds instead), so PR29 and this fix actually run once checked out.
  - Tests: `tickvault_api_can_never_reach_questdb_exec_around_the_sql_gate`,
    `api_path_is_allowed_only_for_the_app_read_routes`,
    `same_origin_folds_loopback_spellings_and_default_ports`,
    `tickvault_api_refuses_a_caller_base_url_before_any_network`; the redirect case is
    in `questdb_sql_and_tickvault_api_against_local_mock` (a 302 to `/exec` is reported,
    not followed).
  - Follow-ups found by the reviews, not fixed here: (a) the SQL gate's word boundary is
    Unicode-aware, so a banned word glued to a non-ASCII letter is not caught; the fix must
    land in all three copies at once (the console, the query console front and `sql_gate.rs`)
    and goes with PR19. (b) `grep_codebase` accepts a relative path that climbs out of the
    repository (older than this PR; PR19). (c) `LIMIT lo,hi` and negative limits pass the row
    cap as written, so they are bounded only by the 8 MiB reply cap.
  - Plan correction: `crates/tickvault-logs-mcp/tests/parity.rs` does not exist in the tree
    (the parity harness was retired), so there is no pin to bump.
- [x] **PR30a — a budget stop stays stopped for the day.** (`aws-lambdas`, `scripts`, deploy)
  - The 08:45 start watchdog, `aws-autopilot.sh` and the 15:50 terraform apply each undo a
    budget stop the same day (start_watchdog.rs:819-835; aws-autopilot.sh:216-224;
    terraform main.tf:498-507; terraform-apply.yml:64-66). A breach latch (one SSM parameter,
    written by the kill-switch, cleared only on a new billing day or by the operator) that all
    three read before starting the box. Pinned by a test per reader.
  - Plan correction: the latch lasts for the BILLING MONTH, not the day. The ceiling is on
    month-to-date spend, which only restarts at the next UTC month, and the hourly guard
    already stops a breached box every hour for the rest of the month; a day-long latch
    would only move the restart war to the next morning.
  - Done: `budget_stop_latch.rs`. `/tickvault-guard/<env>/budget-stop-month` holds the UTC billing
    month of the stop. Writers: the hard-stop guard's breach stop (after the stop, before
    the rule disable; write only, a failed write is paged and never blocks the stop) and
    the AWS-Budgets kill-switch (after the stop). Readers, all fail-open: the start
    watchdog skips its 08:45 self-start; the autopilot skips its up-window start; the
    terraform-apply workflow sets `TF_VAR_daily_start_enabled=false`, so `main.tf` plans
    the daily-start rule DISABLED. Least-privilege IAM per leg. One ratchet test per writer
    and reader (`budget_stop_latch_wiring_guard.rs`) plus unit tests for each leg.
  - Review fixes (security + hostile passes, same PR): the apply job re-plans on its own
    runner, so it reads the latch too (the first draft only read it in the plan job, which
    left the real apply re-enabling the rule); `deploy-aws.yml` no longer starts a latched
    box (`DEPLOY_SKIP_REASON=budget_stop`); the path moved out of `/tickvault/<env>/*`,
    where the box's own role can write, to `/tickvault-guard/<env>/`; the hourly guard
    re-enables the rule once when the latch names an earlier month (`events:EnableRule` on
    the one rule, then `released-<month>`); the kill-switch dates its latch six hours back
    (a late notification for the old month never latches the new one) and bounds the write
    at 5 s; the watchdog sends one short "box off today" note each trading morning instead
    of staying silent; pages name the outcome, never raw AWS error text.
  - Left as is: the native AWS Budget action at 90% stops the box without writing the latch,
    so after that stop the box starts again the next morning and runs until the 100% line,
    where the kill-switch and the hourly guard stop it and latch. Latching at 90% would move
    the effective ceiling to $202.50 in September and $135 from October, which is the
    owner's call.
  - Re-check 6 (2026-09-27): found three holes, carried by PR49: the shell latch readers fail open
    with a clean-day message (terraform-apply.yml:386-395); the kill-switch stop leaves the 08:30
    start rule on (budget_killswitch.rs:208-287; main.tf:646-655); a late budget notice latches the
    wrong month (budget_killswitch.rs:208-270), which the six-hour back-dating above does not
    settle, so the period is read from the notice itself.
- [ ] **PR30b — the October $150 ceiling in terraform.** (deploy; on or after 2026-10-01)
  - The October $150 ceiling is enforced in code (`effective_budget_kill_usd`) but budget.tf and
    budget-guards.tf still say $225 (budget.tf:220-222; budget-guards.tf:278). Quote 23 keeps
    $225 for September, so the terraform change is a dated PR on or after 2026-10-01, in all four
    lockstep sites.
  - Re-check 6 (2026-09-27): line correction, budget-guards.tf:278 is now :300.
- PR31 — a restart can never replace a fuller candle with a partial one. (`storage`, `app`,
  `core`, `trading`) Split 2026-09-27 into PR31a (the three smaller items) and PR31b (the
  restart rebuild itself), so each lands and is reviewed on its own.
- [x] **PR31a — recovery counts, one arrival time, and no keyless tables.** (`storage`, `app`,
  `core`)
  - The boot candle recovery said "all re-ingested" when some failed (seal_writer_task.rs:772-840):
    a refused seal now keeps its file staged for the next boot and is counted
    (`BootDrainOutcome::seals_append_failed`, `tv_seal_writer_boot_drain_total{kind="boot_append_failed"}`).
    A failed candle row is rewound off the ILP buffer with a marker, so it can no longer take the
    good rows batched with it down (`ShadowCandleWriter::append_row`).
  - Top-volume and candle tables could be auto-created by an ILP write without their DEDUP key
    (candle_ddl_boot.rs:245-290; top_volume_rank_persistence.rs:862-924): both writers now refuse
    to send until their ensure has succeeded in this process (`CANDLE_TABLES_KEYED`,
    `TOP_VOLUME_TABLES_KEYED`). Refused candles go to the disk spill and replay once keyed;
    refused top-volume rows are counted as discarded (no tick is lost, but the stored board is; closed by PR31c).
    When the boot gives up, `candle_ddl_boot::spawn_ensure_until_keyed` keeps re-running the
    ensure (30 s doubling to 10 min) instead of waiting for the next boot. Ticks and depth are
    NOT gated (refusing them would lose data); their ensure is in the background re-run, whose
    `DEDUP ENABLE` repairs a table ILP auto-created.
  - Replayed depth and tick rows got a new arrival time, so the key did not collapse them
    (depth_persistence.rs:221-222; dhan_feed_stack.rs:6936-6941): the frame handed to the drain
    now carries the SAME receipt value written to its WAL record
    (`CapturedFrame::received_at_nanos`), and the drain uses it, so the live and replayed copies
    stamp one value.
  - Tests: `test_append_row_on_a_mid_row_buffer_errs_without_dropping_good_rows`,
    `a_refused_append_keeps_the_file_staged_and_is_counted`,
    `the_frame_and_its_wal_record_carry_one_receipt_value`,
    `the_live_drain_stamps_rows_with_the_wal_receipt`,
    `test_flush_refuses_until_the_candle_tables_are_keyed`,
    `the_sink_sends_nothing_until_the_top_volume_tables_are_keyed`,
    `ensure_until_keyed_keeps_retrying_a_database_that_refuses`.
- [ ] **PR31b — the restart rebuild never overwrites a fuller candle.** (`app`, `storage`,
  `trading`)
  - A restart in market hours rebuilds the open candles from the ticks it replays, which may be
    only part of them, and the UPSERT overwrites the fuller row already stored
    (ws_frame_spill.rs:4680-4685; shadow_persistence.rs:143). Same class: a same-day replay of a
    missed slice (wal_applied_watermark.rs:186-218; dhan_feed_stack.rs:13474-13933). Rebuild the
    open bars from the stored ticks, or mark a post-restart bar partial and never let it replace
    a row with more volume. PR23's "replay may rebuild them" is verified false: PR23 tests the
    crash case without relying on replay.
  - Chosen approach: a candle-only warm-up from the WAL (archived segments included) starting at
    the oldest open bucket minus lateness, with tick writes suppressed; a replayed frame whose
    bucket ended before the warm-up start gets no candle; plus a per-slot, per-frame refusal as a
    safety net, counted.
- [ ] **PR31c — PR31a's honest limits, closed one by one (zero data loss on every path,
  owner 2026-09-27).** (`storage`, `app`) Added 2026-09-27 so none of these lives only in the
  PR #1962 text. Each lands as its own small PR after PR31b.
  - Top-volume rows refused while the tables are unkeyed are counted and DROPPED
    (`TopVolumeSink::write`, `TopVolumeWriter::flush`). That is a loss of the stored board even
    though no tick is lost. Spill them to a bounded NDJSON file, as the candles do, and replay
    them once `TOP_VOLUME_TABLES_KEYED` is set; count the spill and the replay.
  - Ticks and depth are not held back. While their table is keyless (ILP auto-created it), a
    replay can write a duplicate, and `DEDUP ENABLE` repairs only rows written after it. Route
    them into their existing spill tiers until keyed (no row refused, none lost), or prove the
    ensure always runs before the first write and record that proof as a ratchet.
  - One latch covers all nine candle tables, so one table that never keys spills every candle.
    Make the latch per table, so only that table's candles wait.
  - A refused-seal file is archived after `SEAL_REFUSED_FILE_RETRY_SECS` (2 days) and its rows
    are never re-ingested automatically. Re-ingest archived refused files once the tables are
    keyed, and page (coded error) if any remain.
  - The shared receipt stamp can run slightly ahead of the wall clock and nudge the top-volume
    close clock (review finding #8, unmeasured). Measure it; clamp the close clock to the wall
    clock if it matters.
  - Re-check 6 (2026-09-27): name the ticks from never-traded instruments that are keyed on arrival
    time (row 144).
- [ ] **PR32 — a tick batch is never marked applied before it is on disk.** (`storage`, `app`)
  - A rescue batch queued to the rescue thread but not yet written is skipped by the next
    replay if a later batch was already confirmed (tick_persistence.rs:2739-2757, :3354-3358;
    dhan_feed_stack.rs:3199-3219). Mark its range unapplied when it is queued and when shutdown
    abandons it; clear it when it lands.
  - A late append is erased at the hour boundary (tick_spill_replay.rs:896-958): truncate only
    under the writer's lock, or rename then drain.
  - The unapplied-slice table overflows silently (wal_applied_watermark.rs:342-344, :650-690):
    a counter, and stale slots cleared.
  - Rows replayed into an hour the archive already dropped (partition_archive.rs:2348-2368,
    :2680-2695): the archive sees the capture log's deferrals.
  - Boot re-reads the same leftovers while the database's apply lag persists
    (dhan_feed_stack.rs:13228-13262; ws_frame_spill.rs:3027-3040): break the loop.
  - fsync of spill, dead-letter and marker files, and the database commit mode, go to PR17
    (tick_persistence.rs:1348; wal_applied_watermark.rs:819-856).
  - Re-check 6 (2026-09-27): also, a write error inside the capture log (row 135,
    ws_frame_spill.rs:1692-1699, :1813-1852): count it per frame and mark the frames already
    deferred or marked for replay as lost; and the depth shed inline under apply lag is never marked
    for replay (row 145, ingest_shed.rs:353-360).
- [ ] **PR33 — token and socket gaps.** (`core`, `app`)
  - A token refused at the connect handshake (HTTP 401/403 on the upgrade) is never renewed
    (connection.rs:1806-1816; pool_supervisor.rs:1719, :1889): classify it as token-stale and
    refresh once before redialling.
  - A failed renewal is retried by every queued socket (token_manager.rs:1286-1362): share the
    failed outcome for a short cooldown.
  - Two other callers replace the token without the PR22 generation guard
    (token_manager.rs:1655-1705, :1723-1730; order_update_connection.rs:569): pass the caller's
    last-seen generation.
  - A socket spawned just before an 805 still connects (pool_supervisor.rs:1683-1698,
    :4845-4848): the task reads the stop switch before every first-ever dial, main feed included.
  - A queued depth-200 rotation can hide an 805 (pool_supervisor.rs:6127-6199;
    connection.rs:2023-2025): drain an owed close before a queued command. Rotation refuses a
    socket that is not depth-200 (:6127-6145).
  - Depth dials refused after an 805 are still booked as dialled (dhan_feed_stack.rs:12148-12193):
    set the flags and seed the silence detector only for sockets actually spawned.
  - The unsubscribe probe reads a refused close as "ignored" (depth_unsubscribe_probe.rs:408-436,
    :462): check the 805 refusal counter first.
  - A half-open socket is redialled before Dhan closes its end (idle_watchdog.rs:96, :105;
    reconnect_ladder.rs:60): hold the redial past Dhan's 40 s server close. A half-open
    order-update socket takes 4 hours to notice (activity_watchdog.rs:134;
    order_update_connection.rs:669-672, :737-758): a read deadline well under that.
  - Depth-20 spots are never added when the underlying list arrives late
    (dhan_feed_stack.rs:11577-11607, :12176-12178): add them when it appears, and count it.
  - The depth seed looks contracts up by id alone across exchanges (depth_seed.rs:224-232,
    :416-436): key on `(security_id, segment)` (I-P1-11).
  - The 808 policy (pool_supervisor.rs:743-748, :1831-1841) and the main-feed half of row 99
    (dhan_feed_stack.rs:11956-11979, :12865-12870) are D7's, see below.
  - Re-check 6 (2026-09-27): also, the order-update socket's connect and login have no time limit
    and no watchdog, and PR33's read deadline does not reach them (row 124,
    order_update_connection.rs:669-672, :719-722, :746-758): bound both and cap the message size.
    The scheduled token sweep is a third caller of the same renew function; define the generation a
    timer-driven caller passes (row 215, dhan_rest_stack.rs:1109-1112). Name the order-update
    same-token redial and its 4-hour renewal threshold (row 216, order_update_connection.rs:569).
    Seed the late contract top-up into the silence detector and count seed-queue refusals
    (dhan_feed_stack.rs:12008-12016).
- [ ] **PR34 — a hung app is restarted and a stuck drain is visible.** (`app`, deploy, `scripts`,
  `aws-lambdas`)
  - The systemd watchdog ping does not follow drain or runtime progress, and boot steps are
    unbounded (main.rs:2063-2079; tickvault.service:117, :155): tie the ping to progress.
  - A stuck drain keeps the liveness gauge green (dhan_feed_stack.rs:7600-7620;
    observability.rs:276-290): publish tick age from a separate task.
  - Clock health reads the last offset, so a clock service that lost its source reads as zero
    skew, and the check falls back to the same host's database (infra.rs:541-625); the chrony
    call has no time limit (infra.rs:551, :657). Read the sync status, bound the call, drop the
    fallback.
  - The autopilot restarts a crash loop every 15 minutes (aws-autopilot.sh:292-373): leave a
    unit at its restart limit alone and page. Its database repair uses the wrong folder (:385).
  - A box down at 09:20 means no liveness paging all day (market_hours_gate.rs:77-87,
    :176-183): re-check on instance start.
  - Re-check 6 (2026-09-27): once the autopilot's database repair works it must skip while a reset
    holds its lock or the app is disabled (aws-autopilot.sh:389-402). Bound the pre-ready boot steps
    too, including the Docker status call (infra.rs:1097-1110). Health reads the tick writer as
    connected while nothing lands (dhan_feed_stack.rs:3289-3295; tick_persistence.rs:2408-2419):
    report connected only on a landed batch and count hand-offs apart from good flushes. The
    spill-status check reads zero during an outage (api/src/handlers/debug.rs:106, :160, :191;
    tick_spill_replay.rs:101): count the .ilp spill files and file the candle spill under candles.
- [ ] **PR35 — a deploy cannot break the morning or leak logs.** (`.github/workflows/`, deploy)
  - A deploy can restart the app just before 09:00 (deploy-aws.yml:754-765, :958-1011): refuse
    one that cannot finish with boot before 08:55.
  - A failed deploy stops the app even after a good rollback (:1310-1318, :1452-1506); a
    half-failed one leaves new settings with an old build (:969-985, :1122-1125): use the
    deployed commit's settings and roll back on any failure.
  - Failed deploys copy app logs into public CI logs (:1010-1012, :1260-1261): keep the journal
    on the box.
  - All Green does not check terraform or the production aarch64-musl build (ci.yml:1121-1132;
    terraform-apply.yml:50-53): add both, and add them to `all-green`'s `needs:` in the same
    change (merge-gate lock §5).
  - Re-check 6 (2026-09-27): adds the ops workflows: the disk-recovery workflow can wipe market data
    during the session and leaves the app disabled on failure (emergency-fs-recover.yml:101-116,
    :171-173), and the control workflow's database restart has an octal clock guard
    (aws-control.yml:147-176, :381-388). One decimal 09:00-15:45 guard checked on the box covers
    both, and every exit re-enables the app. A Lambda-only fix waits for the next weekday apply: add
    the Lambda code to the terraform push filter (terraform-apply.yml:49-57). Docs-only merges
    trigger a full deploy: path-check before dispatch (postmerge-catchup.yml:186-229).
- [x] **PR36a — the console key is never sent to the alerts topic.** (`.github/workflows/`)
  Found by re-check 6, verified by reading terraform-apply.yml. Taken next after PR29b,
  ahead of PR31b, because it is a live secret leak. Every terraform apply publishes
  `LINK="${URL}#key=${KEY}"` (terraform-apply.yml:656) to `tv-prod-alerts`, which forwards
  to Telegram, the always-on email subscription and SMS when a phone is set; the `add-mask`
  only hides the Actions log. Publish the console URL only, never the key. Rotating the
  current key is the owner's call (past copies sit in Telegram and email history); asked in
  the fix thread on 2026-09-27, recommending rotation.
  - Done: the portal alert carries the portal URL only; the key is never read back or sent
    (terraform-apply.yml "Send the operator-portal link to Telegram"). A manual run with
    rotate_console_key = "rotate" overwrites the stored key (--overwrite) and sends a "key replaced"
    notice without the key; a failed overwrite fails the step loudly. Both console Lambdas cache the
    key for 60 s, so an old key stops working within a minute. The owner chose "Rotate after fix" on
    2026-09-27; the rotation itself runs only after the owner types "rotate" in the fix thread.
    Security review fixes in the same PR: a key is stored only after it is checked to be 40
    characters; a failed first-time save fails the step instead of announcing a live portal; a
    first-time save that finds a key already stored (the read failed for another reason) keeps it,
    and fails loudly if a rotation was asked for.
  - Tests: github_workflow_guard r20_no_workflow_publishes_the_operator_key (no workflow builds a
    #key= link or puts the key in a published message; the portal step never reads the stored key;
    rotation needs the exact word, overwrites, and fails loudly; the key length, a failed
    first-time save and a rotation that could not read the key are all loud; the input defaults to
    off). The step script, extracted from the workflow, was run locally against a stubbed
    aws/terraform/openssl in ten cases (new key, keep, rotate, wrong word, failed overwrite, failed
    first save, read flake with and without rotate, openssl failure with and without rotate): only
    a clean "rotate" overwrote, every failure exited 1 with nothing published, every stored key was
    40 characters, and no published message carried the key.
  - Accepted, not fixed: the key is passed to `aws ssm put-parameter --value` on the command line,
    so it is visible in the process list of the single-use hosted runner for that call. Anyone with
    write access can dispatch the rotation; whether the `prod` environment requires a reviewer is
    not visible from the repository (Unknown).
- [x] **PR36b — a manual deploy needs All Green on the commit it ships.**
  (`.github/workflows/deploy-aws.yml`) Found by re-check 6: `workflow_dispatch` from any
  branch reaches the `deploy` job's `environment: prod` (deploy-aws.yml:403), so a manual run
  can ship a commit that never passed All Green. Refuse a dispatch whose ref is not `main`,
  or whose head has no successful All Green, before the build.
  - Re-check 6 (2026-09-27): also restrict the deploy role's trust to the main branch
    (deploy/aws/terraform/oidc.tf:72-80), so the job check is not the only barrier
    (deploy-aws.yml:394-403).
  - Done: the preflight job refuses a manual run that is not on main, or whose commit has no
    successful All Green posted by GitHub Actions, either on the commit itself or on the head of
    the pull request squash-merged as it (deploy-aws.yml "Refuse a manual deploy of a commit that
    did not pass All Green"). Any API failure refuses; the next after-close cron retries. Every
    automatic dispatcher (after-close cron, post-merge catch-up, deploy watchdog, operator
    console) already dispatches main. Checked against the live API: main 01f216425 has no All
    Green of its own (a bot merge, whose push run GitHub suppresses) and passes through PR #1964,
    whose head passed.
  - Finding, not fixable in the repository: the role trust cannot be narrowed to main here. A job
    that runs in the `prod` environment presents the subject `environment:prod`, not its branch,
    and the trust accepts that subject, so any branch that reaches `prod` can assume the role
    (deploy, terraform-apply, downsize-instance, grow-ebs-volume, wipe-log-streams). The
    in-workflow check stops mistakes only: a branch can delete it. The real barrier is the `prod`
    environment's deployment-branch rule (main plus tags v*), a repository setting only the owner
    can change. The preflight now reports on every run whether that rule is set (warning when it
    is not, never fatal); whether it is set today is Unknown (the API path is blocked here).
  - Tests: github_workflow_guard r21_manual_deploy_needs_main_and_all_green (bite-checked: it fails
    when the merge-commit match is removed). The gate script, extracted from the workflow, was run
    against a stubbed gh in 12 cases (feature branch, tag, own All Green, via merged PR, PR head
    red, All Green from another app, no PR, open PR, PR merged as another commit, and three API
    failures): only the two genuine passes exited 0. The environment report was run in 3 cases.
- PR40 — a crash never loses queued candle seals uncounted, and PR15's loose ends close.
  (`storage`, `app`, deploy) Split 2026-09-27 into PR40a (count a crash's loss, correct the
  figure), PR40b (prune the spill folders, alarm the unrecovered seals) and PR40c (the tests
  and the inline-fallback decision), so each lands and is reviewed on its own.
- [x] **PR40a — a crash's unwritten candle seals are counted and reported, and the 250,000
  figure is right.** (`storage`, `app`, deploy)
  - Done: counting, not a durable per-window watermark, because the re-fold that would consume a
    watermark is PR31b's warm-up, which does not exist yet; the watermark rides PR31b.
    `SealWriterRunner::unwritten_seals` (writer channel + ring + escalation queue, O(1)) is set
    on the gauge `tv_seal_unwritten` every writer cycle (100 ms), so the value before a crash is
    in the metrics log group. The same count is written to `seal-unwritten.mark` in the spill
    directory at most once a second when it changes (write then rename); a clean shutdown
    rewrites it `clean=1` after the final drain. The next boot reads it before anything else and,
    when it is unclean and holds seals, fires AGGREGATOR-DROP-01 with `source =
    "crash_unwritten"` (existing errcode page, description extended; no new alarm) and adds the
    number to `tv_seal_crash_unwritten_total`.
  - Honest limits: the count is up to one second plus one cycle stale, and the seals the cycle
    in progress popped into the ILP buffer (at most `max_drain_per_cycle`) are in neither sample;
    the marker is not fsynced, so it survives a process crash, not a host crash; the seals are
    reported, not recovered.
  - Figure corrected where it states the current value: 250,000 per burst (25,000 × `TF_COUNT`
    10) in seal_writer_runner.rs, main.rs, the shutdown-budget guard, guarantees.md and a dated
    note in aws-budget.md (the ring is 42.0 MB, the escalation queue ~36 MB, up to 750,000 seals
    across the three queues). Dated history that said 225,000 at nine frames is left as written.
  - Tests: `unwritten_seals_counts_channel_ring_and_escalation_queue`,
    `unwritten_mark_writes_only_on_change_and_at_most_once_a_second`,
    `test_unwritten_seal_record_to_line_round_trips_and_parse_refuses_anything_else`,
    `test_unwritten_mark_in_dir_read_previous_reports_an_unreadable_file_as_unreadable`,
    `unwritten_mark_write_failure_is_counted_not_fatal`,
    `test_report_previous_unwritten_only_for_an_unclean_marker_holding_seals`,
    `a_clean_shutdown_rewrites_the_marker_and_the_next_boot_reports_nothing`,
    `the_loop_reads_the_previous_marker_before_it_writes_its_own`,
    `unwritten_mark_file_is_outside_every_spill_and_dlq_filter`.
- [x] **PR40b — old spill folders are pruned and an unrecovered seal pages.** (`storage`,
  deploy) The archive/replaying pruning, the replay-skipped and boot-undecodable seals as a
  coded error with an alarm (rule file first: a new page needs a dated noise-lock section).
  - Done: no new page, so no noise-lock section. Every seal a recovery path gives up on (boot
    drain: undecodable or refused; replay: every `records_skipped`) now fires the EXISTING
    AGGREGATOR-DROP-01 errcode alarm with `source = "seal_unrecovered"` and `stage`
    (`seal_writer_loop.rs::report_unrecovered_seals`), one line per drain or replay step, the
    same route as PR40a's `crash_unwritten`. The spill retention line that deletes aged files
    still holding seals carried the unregistered `SPILL-RETENTION-01` and paged nobody; it is
    now AGGREGATOR-DROP-01 with `source = "spill_retention"` (this closes PR39's
    SPILL-RETENTION-01 bullet by reuse rather than a new code). Alarm description and the
    wave-6 runbook cover (d) and (e).
  - Done: `prune_spill_files_at` now also sweeps `replaying/` (aged file = unreplayed, counted
    as lost like the top level) and `archive/` (counted apart as `archive_deleted`, not a loss),
    and both count toward `tv_seal_spill_bytes`. The live-writer guard stays top-level only.
  - Not done, by choice: no new EMF selector for `tv_seal_replay_total{kind="skipped"}`. The
    series already ships in the `/tickvault/<env>/metrics` log group and the page is the log
    line; a selector adds a paid custom metric for no extra signal.
  - Honest limit: a staged file older than 7 days that the replay is reading at the moment of
    the sweep is deleted under it; the next step cannot reopen it and the loss is counted and
    paged as `spill_retention`. Reaching it needs the replay stuck for a week.
  - Tests: `test_prune_spill_files_at_sweeps_replaying_as_loss_and_archive_as_not`,
    `test_prune_spill_files_at_applies_the_live_guard_only_at_the_top_level`,
    `test_report_unrecovered_seals_is_silent_at_zero_and_reports_the_count`,
    `test_report_unrecovered_seals_is_wired_into_both_recovery_paths`.
- [x] **PR40c — PR15's tests prove batching, the pause and the real replay, and the inline
  fallback's note is corrected.** (`storage`) The last two bullets below, except the decision,
  which is PR40d.
  - Done: `SealEscalationSink::run` returns a `SealEscalationRunSummary` (batches, records,
    spill writes; the production thread discards it), so the batching test asserts 4,103
    queued seals go out in exactly 5 batches and 5 spill writes, a batch across IST midnight in
    2 writes, and a lone seal in its own batch at once. A deterministic pause test stages the
    live file while the writer holds its handle and asserts the next seal opens a fresh live
    file. Measured by hand: with `*open = None` removed from `with_appends_paused` the new test
    fails and the older race test still passes. A chaos test drives the REAL
    `ShadowCandleWriter` over ILP/HTTP against a local stand-in that answers 503 during an
    outage and 204 after it: all 600 outage seals go to the spill, the health gate opens after
    sixty clean seconds, the mid-session replay re-sends them, and every seal (outage and live)
    is acknowledged exactly once with no spill file left. The inline fallback's doc now says the
    wait is bounded in BYTES (one 128 KiB batch holds the lock), not in TIME: a hung disk holds
    the frame drain for as long as it hangs.
  - Not done: the chaos test runs against a stand-in, not QuestDB; it proves the writer's wire
    behaviour and the replay loop, not QuestDB's DEDUP.
  - Tests: `the_escalation_thread_batches_a_burst_and_writes_every_record_in_order`,
    `the_escalation_thread_writes_a_lone_refusal_at_once_as_its_own_batch`,
    `a_batch_that_straddles_ist_midnight_files_each_record_under_its_own_day`,
    `staging_with_appends_paused_sends_the_next_seal_to_a_fresh_live_file`,
    `chaos_a_database_outage_spills_every_seal_and_recovery_replays_each_once`.
- [x] **PR40d — the inline fallback's stalled-disk wait has an owner decision.** (`storage`)
  Asked on 2026-09-27 with three options: accept the wait and make it loud (recommended), a
  second disk as a third store, or a memory overflow. The owner chose "Accept the wait" on the
  card at 23:26 UTC; recorded first as `dhan-rest-only-noise-lock-2026-07-14.md` §2.7.
  - Done: only the refused arm of `SealOverflow::escalate` reads the clock (the queued arm reads
    none, pinned by a source scan). Every inline wait adds its milliseconds to
    `tv_seal_escalation_inline_wait_ms_total`; a wait of `SEAL_INLINE_WAIT_PAGE_MS` (1,000 ms) or
    more counts on `tv_seal_escalation_inline_stall_total` and, at most once per
    `SEAL_INLINE_STALL_LOG_EVERY_SECS` (60), writes a critical `AGGREGATOR-STALL-01` line that
    folds the window's stall count and longest wait into it. Both counters are seeded at 0 with
    the other escalation counters. The code pages through the errcode alarm
    `tv-<env>-errcode-aggregator-stall-01` (one line per 300 s, `ok_recovery = false`), with its
    phone wording, a triage rule and a runbook section.
  - Not done: the line is written when the wait ENDS, so a disk that never returns is caught by
    the liveness alarms, not this one. The 1,000 ms line is a judgement, not a measurement. The
    two counters are not in the CloudWatch metric list (cost); the log line is the page.
  - Tests: `a_stalled_disk_on_the_fallback_is_timed_and_recorded_as_a_stall` (a real 1.2 s disk
    stall), `a_quick_fallback_is_timed_but_never_claims_the_stall_line`,
    `inline_stall_report_throttles_and_folds_the_window_into_the_next_line`,
    `inline_stall_report_survives_a_backwards_clock_step`,
    `the_inline_wait_page_threshold_and_names_are_pinned`,
    `escalate_reads_the_clock_only_on_the_refused_arm`.
- Original PR40 text, kept as the source for 40a–40c:
  - A crash (abort, out of memory, hard kill) loses every queued seal uncounted: up to 250,000
    in the escalation queue, 750,000 across writer channel, ring and escalation queue
    (seal_writer_runner.rs:223, :671; main.rs:4695-4718). The ticks behind them survive. Persist
    or count them: either a durable per-cadence seal watermark so the next boot re-folds every
    window after it from the capture log (PR31b's warm-up, extended to sealed but unwritten
    windows), or at minimum a shipped queue-depth gauge sampled every flush so a crash's loss is
    bounded and visible. The PR states which, with a measurement.
  - Correct the figure everywhere it is written: 250,000 per burst (25,000 × `TF_COUNT` 10), not
    225,000.
  - The archive and replaying folders are never pruned, and files moved into replaying are
    outside the 7-day retention sweep (seal_spill.rs:1245-1256): prune by age and count them
    against the disk budget.
  - Ship the seal loss counters to CloudWatch: the replay skipped counter is pre-seeded but not in
    the metric list (cloudwatch-agent.json:24); alarm AGGREGATOR-SEAL-01 on it.
  - Boot recovery skips unreadable candle seals with a warning only
    (seal_writer_task.rs:560-575, :732-737): coded error with an alarm.
  - The inline fallback waits without a time limit on a stalled disk (row 27): correct the doc
    (the bound is bytes, not time), then build a third durable tier or record an owner ruling
    that the wait is accepted.
  - PR15's tests do not prove batching, the pause or the real replay
    (seal_writer_runner.rs:1811-1846; seal_writer_task.rs:2560-2598): assert write counts, test
    replay under a no-op pause, and add an outage-and-recovery chaos test.
- [ ] **PR41 — a replayed candle never replaces a fuller one, and one stuck spill file never
  holds the rest.** (`storage`, `trading`)
  - A replayed seal overwrites a newer corrected candle (a late trade re-folded a sealed bar),
    uncounted (seal_writer_task.rs:903-917, :1127-1175; aggregator_cell.rs:261-268). PR31 states
    the never-replace rule for restarts only, and PR31a did not change this path: the honest-limits
    comment on the current checkout (95cf140, after #1962) still says "Last write wins"
    (seal_writer_task.rs:986-989). Skip or version a replayed seal older than the stored row, and
    count it, in both the mid-session replay and the boot drain. The check is shared with PR31b's
    "never let it replace a row with more volume".
  - A candle the replay cannot flush is skipped and later files wait
    (seal_writer_task.rs:1209-1262): tell a flapping database from a bad record before skipping,
    and move past a stuck file.
  - Staged spill files replay in name order (seal_writer_task.rs:476, :1109): sort by write time.
  - The replay trusts acknowledgements while the table is suspect (seal_writer_task.rs:1026-1040):
    keep the file until the table is healthy.
  - The replay gate opens only on live traffic, so a spill made after the last live write waits
    for the next boot: reopen it on a database health check too.
  - The candle spill has no record checksum (row 136, seal_spill.rs:832-841, :907-916): cut back a
    torn single-record write the way the batch does, check alignment before a batch, add a
    checksum.
- [ ] **PR42 — order and P&L audit rows survive a database outage.** (`storage`, `app`, deploy)
  - Order and P&L audit rows are thrown away while the database is down
    (order_audit_persistence.rs:478-521; pnl_audit_persistence.rs:488;
    order_leg_pnl_persistence.rs:403). These are SEBI rows. Give them a disk tier (the candle
    spill shape: bounded NDJSON spill, replayed under their DEDUP keys) and ship and alarm the
    P&L-audit and leg-P&L discard counters (cloudwatch-agent.json:24).
  - The order-push event channel to the paper order audit writer holds 1,024 events; a reader
    stalled for about 100 s of events (for example on a hung database write) skips them and they
    never become audit rows, counted on the box only with an uncoded warning. Code the warning,
    ship the lag counter, and reconcile on lag. (PR14 names the order-runtime channel and PR39 a
    different line; neither covers this one.) Paper mode only; `dry_run` is not touched.
- [ ] **PR43 — no NIFTY or BANKNIFTY depth-20 option leg is dropped silently.** (`app`)
  - Past 246 spot instruments every index option leg leaves depth-20 while the settle log says
    complete; shrinking starts at 215 spots, and today is about 208
    (depth20_static.rs:108-119, :218-226; dhan_feed_stack.rs:12242-12255). This is close to the
    owner's rule that no subscribed instrument is dropped. Count every dropped leg, make the
    settle log say how many legs are missing, and raise a critical coded alarm when any are.
  - Which side gives way at the 250-slot cap (spots or index legs), and whether the depth
    account's five depth-20 sockets (D9) take the overflow once enabled, is an owner decision
    asked before this PR is written. Until it is answered, placement does not change; this PR
    ships the count, the alarm and the honest log.
- [ ] **PR44 — a depth socket parked without an 805 comes back.** (`core`, `app`)
  - A depth-20 or depth-200 socket parked for a reason other than 805 stays dark all day
    (row 61, pool_supervisor.rs:2234-2291, :4779-4800; depth20_static.rs:137-155). D3 covers only
    the main feed. Restore it on the normal backoff ladder, or re-home its instruments on spare
    authorized depth capacity; a critical coded alarm names the socket and the count either way.
    After an 805 the socket stays down under `ROTATION_HALTED`, as D7 records.
- [ ] **PR45 — the 09:16 market-open self-test runs, or its claims go.** (`core`, `app`)
  - The self-test never runs, although config turns it on (config/base.toml:603) and
    instance_lock relies on it to silence lock renewal (market_open_self_test.rs:187;
    instance_lock.rs:745-747). Wire it (the guarantee matrix names a 09:16:30 IST self-test), or
    delete the setting, the lock-renewal silencing and the rule claims in the same PR. Wiring it
    is the recommendation, because the rules and the lock already assume it.
- [ ] **PR46 — the box cannot rewrite its own settings or drive Docker.** (deploy terraform,
  `app`)
  - The box role can write every /tickvault/prod/* parameter (main.tf:217-251): narrow write to
    the token and lock parameters.
  - The app can use the Docker socket (the gap list gives no line; located when the PR is
    written): remove that access, or confine it to the one status call that needs it.
  - The box role can overwrite any object in the cold bucket (no delete), with no undo until
    PR28b's versioning: narrow its write to the prefixes it writes.
  - The budget-action role may stop any instance (budget.tf:374-380): scope it to the box.
  - Database ports and a default login fallback (docker-compose.yml:221-224; deploy-aws.yml:1006):
    bind to the host and fail the deploy on a missing password.
- [ ] **PR47 — an urgent Telegram page is never lost uncounted.** (`core`)
  - A Critical page can be lost in a burst, uncounted, while the SMS copy still goes out
    (core/src/notification/service.rs:462, :489-494, :2109-2131, :2316-2345): honour the
    retry-after reply, count the final failure, and count SMS failures too.
- [ ] **PR48 — console destructive actions finish before the open and never report a false
  complete.** (`aws-lambdas`) The open holes of ticked PR28a.
  - A reset or nuke started 08:30-08:59 runs into the open
    (operator_control_action_commands.rs:21, :88, :172-181): refuse one that cannot finish before
    08:55.
  - A reset that outlives the SSM execution time limit (3600 s by default) leaves the app switched
    off (operator_control.rs:1958-1966; operator_control_action_commands.rs:357-359): pass an
    explicit execution time limit and enable the app before the image pull.
  - The wipe can report complete without checking (operator_control_action_commands.rs:79;
    operator_control.rs:3833-3843): a failed count query is a failure, every wiped table is
    checked, and the shell runs in a test.
  - The 263-line SEBI-save program is copied byte for byte into reset and nuke and never parsed
    (operator_control_action_commands.rs:92-354, :387-649): one constant plus a syntax-check test.
  - The reset trusts a byte count never checked on the box (operator_control_action_commands.rs:334-352):
    run one reset on a scratch volume and record the result.
  - The Muhurat lock is D11's (fifth re-check fold); this item does not duplicate it.
- [ ] **PR49 — budget stop follow-ups: fail loud, stop the start rule, the right month.**
  (`aws-lambdas`, `.github/workflows/`, `scripts`) The open holes of ticked PR30a.
  - The shell latch readers fail open with a clean-day message (terraform-apply.yml:386-395; the
    three workflow copies and the autopilot's fourth): a read failure prints a distinct coded line
    and pages. Whether a failed read keeps failing open (PR30a's choice) is the owner's call.
  - A kill-switch stop leaves the 08:30 start rule on (budget_killswitch.rs:208-287;
    main.tf:646-655): the kill-switch disables the rule too.
  - A late budget notice acts on the wrong month (budget_killswitch.rs:208-270): read the billing
    period from the notice itself.
  - The new-month release overrides a deliberate pause (hard_stop_guard.rs:643-690): release only
    what the guard itself latched.
  - False pages on budget-stopped and holiday mornings (alarm_gate.rs:64-121): the alarm gate and
    the readiness check read the latch and the holiday marker.
- [ ] **PR50 — the holiday gate actually holds the app back.** (deploy, `.github/workflows/`,
  `common`)
  - The holiday gate did not stop two weekend boots. The lead is that deploys never enable its
    unit (deploy-aws.yml:992, :1009-1011; user-data.sh.tftpl:263): find why, and enable it on
    every deploy.
  - The gate does not hold the app back (tickvault-holiday-gate.service:28, :39): the app unit
    requires the gate and treats exit 1 as failure.
  - One next-year holiday silences the coverage page (trading_calendar.rs:271-277): count
    holidays per year. Touches `common`, so workspace tests.
  - Lands before D6 ports the gate script to Rust, so D6 ports a gate that is known to work.
- [ ] **PR51 — every guard the rules cite exists and bites.** (`common` tests, `.claude/rules`,
  `.claude/hooks`, `.config/nextest.toml`)
  - Nothing checks the ID-plus-exchange rule where new code lives
    (banned-pattern-scanner.sh:497-526): a general bare-key guard over all crates.
  - Always-loaded rules cite checks that do not exist and a wrong seal-ring size
    (per-wave-guarantee-matrix.md:64-66): fix the rule files and add a test that every cited
    file exists.
  - Frontend script budgets are not exact (browser_surface_and_toolchain_guard.rs:801-822, :889):
    fail when a page is below budget too, so the budget ratchets down.
  - CI retries every failed test once (.config/nextest.toml): turn retries off. Failing All Green
    on a flaky result instead would change the All Green evaluator, which the merge-gate lock
    §5.1 allows only with a dated owner quote, so retries-off is the default.
  - Touches `common`, so workspace tests.
- [ ] **PR52 — the board sort is whichever is faster on the production host.** (`app`)
  Takes over the radix-sort bullet of the fifth re-check's PR4c follow-ups.
  - Re-check 6 measured the PR4c-1 radix sort still slower than the comparator sort it replaced,
    at every size, and by a wider margin than check 5 found: 1,385 vs 767 µs and 1,190 vs 820 µs
    at 20,220 rows (check 5: 1,126 vs 976 µs), release build on the x86 dev container under
    shared load (harness `radix_vs_comparator_at_every_measured_shape`).
  - Re-measure both sorts on Graviton (the production r8g) and keep whichever is faster there. If
    the radix sort is not faster, revert the sliced sort to `sort_unstable_by(board_order)`
    (slicing works with either). Either way the proptest equivalence
    (`board_radix_key_sorts_identically_to_board_order`,
    `slice_radix_sort_step_matches_sort_unstable_by`, or the comparator's equivalent) and the
    zero-alloc DHAT gate (`dhat_top_volume_sweep`) stay, and the CLAUDE.md complexity row for
    `top_volume_sweep.rs` is corrected in the same PR with the measured figures and the host.
  - Not dangerous (the drain waits for one step either way; the sliced sweep's worst step
    measured about 121 µs), so it is ordered with the PR4c follow-ups.
- [ ] **PR36 — security hardening.** (deploy, `core`, `app`, `aws-lambdas`)
  - SSH is open to 0.0.0.0/0 (terraform-apply.yml:123; main.tf:144-150): close 22 or pin a CIDR.
  - The scheduler role can pass any role (main.tf:624-633): scope PassRole to its own ARN.
  - The token cache is briefly readable by other users (token_cache.rs:106-113): create it with
    mode 0o600 in one step.
  - The console control secret can be replayed (operator_control.rs:230-246): signed,
    time-limited requests.
  - A weak API bearer token is accepted silently (main.rs:4185-4209; api/src/lib.rs:174-181):
    minimum length at boot.
  - `config/local.toml` holds production's database address and is loaded in production
    (main.rs:562-569): production values move to `production.toml`.
  - Re-check 6 (2026-09-27): the alert re-send of the console secret is PR36a, the any-branch deploy
    trust is PR36b, and the box role wildcard and the budget-action role are PR46. The signed
    requests here use the same console secret that PR36a stops publishing, so they build on whatever
    secret the owner's rotation decision leaves. The token cache fix must also refuse an existing
    file or link. `config/local.toml` is tracked although .gitignore excludes it (row 181,
    .gitignore:43). The console page writes box output unescaped and keeps the secret in the browser
    (operator_control_console.html:245, :249). The query console back end checks only the first
    query value (qdb_console_proxy.rs:333-356).
- [ ] **PR37 — Dhan documentation mismatches.** (`core`, `common`, `app`) Checked against the
  owner's 2026-09-27 upload (same files as 2026-09-26).
  - Quote byte 38 / Full byte 50: the PDF says "Day Close Value, only sent post market close";
    Dhan support ticket #5525125 (2026-04-10, recorded in `.claude/rules/dhan/live-market-feed.md`)
    says it is the previous day's close, and for NSE_EQ there is no code-6 packet. The two
    sources disagree, so the first step is a live measurement, not a code change: at 09:15 and
    after 15:30 compare the field with yesterday's stored close for a sample of stocks. Only then
    decide the source. The first-write-wins latch (dhan_feed_stack.rs:3894, :3928-3990) is
    changed either way so a later, different value is counted rather than silently ignored.
  - `ExpiryCode` is 0/1/2 in code and 1/2/3 in the annexure, with no production caller
    (instrument_types.rs:128-143): renumber to the doc or delete it.
  - A second, older disconnect policy disagrees with the live one on 804, 809 and 811–814 and
    has only test callers (websocket/types.rs:137-157): delete it or route it to
    `classify_disconnect`.
  - Depth-200 dials the root path; the PDF says `/twohundreddepth`. Deliberate (2026-04-23), but
    the same change also switched TLS, so the evidence does not isolate the path. No change
    without a live probe of the documented path on one socket; a recorded note only.
  - Codes 1 and 7 sizes are not documented (dispatcher.rs:168, :309, :314): assumed from the
    SDK and stated as such in the code.
  - Re-check 6 (2026-09-27): the 'latch changed so a later value is counted' step is already done
    (dhan_feed_stack.rs:3990-3999; dispatcher.rs:57-70); the missing piece is an alarm on the
    disagreement counter plus a per-exchange previous-close (code 6) packet count, both before the
    live measurement, since nothing answers 'does NSE_EQ get code 6' today (an offline scan of the
    archived capture log could, but no tool does). The codes 1 and 7 bullet does more than a
    comment: when the length stamp disagrees with the assumed size on an undocumented code, stop the
    walk and count it, so no phantom packet is decoded (dhan_feed_stack.rs:8119-8142, :8407;
    dispatcher.rs:309, :314); shares PR9's walker. Minor: the intraday `oi` parameter type is
    unverified against a PDF; if refused, the daily check fails loudly (Assumed).
- [ ] **PR38 — the risk book survives a restart.** (`trading`, `app`) A restart forgets
  positions, realised P&L and the halt latch, so an automatic halt is lifted
  (risk/engine.rs:208-228, :717-733): rebuild them at boot from the order audit, or persist
  them. The expired-contract check is never fed (oms/engine.rs:3496-3506): set `expiry_date` on
  option orders and compare against the IST date. Paper mode only; `dry_run` is not touched.
- [ ] **PR39 — every error line is coded and every loss is counted where it pages.** (all crates,
  deploy)
  - 78 production `error!` lines carry no code: add codes and a guard that every production
    `error!` has one (common/tests/error_code_tag_guard.rs). Takes over PR19's token-failure
    bullet and row 146 (main.rs:346, :2801; dhan_universe.rs:1653-1654; the dead WAL-replay
    branch main.rs:1057-1062).
  - Row losses logged as warnings become coded `error!` (dhan_rest_stack.rs:909-927;
    partition_archive.rs:2709-2720), and the phrase-list guard is fixed
    (error_level_meta_guard.rs:23-62). Lost candle spill files log an unknown code: add
    SPILL-RETENTION-01 with runbook and alarm (seal_spill.rs:1170-1181). Board and candle-table
    loss lines get codes (volume_leaderboard.rs:1405-1418; multi_tf_aggregator.rs:794-806).
  - Instrument lifecycle audit rows and index membership writes get their own code and counter,
    and index membership retries once (dhan_lifecycle.rs:599; dhan_universe.rs:1207).
  - Top-volume discards page (top_volume_rank_persistence.rs:1955); the log-drop counter ships
    and the two coded log lines alarm (cloudwatch-agent.json:24; observability.rs:438).
  - Five low-rate writers use the library's default network timeouts (ws_event_audit_persistence.rs:214
    and siblings): `retry_timeout=0`, `request_timeout=5000`, socket-audit flush off the shared pool.
  - The critical token page understates its impact (events.rs:1680-1712, :3870;
    mid_session_watchdog.rs:254, :372): reword it and its pinning test.
  - Re-check 6 (2026-09-27): corrections. 'row 146' points at a different finding; the two read_all
    sites (seal_spill/seal_dlq) are dead code (PR19); state a severity for each newly coded line;
    the guard must blank field-level test attributes and string contents before scanning;
    SPILL-RETENTION-01 lines are now seal_spill.rs:1318-1329. Adds: a coded warning cannot page
    through the log alarms (error-code-alarms.tf:221), so make each loss an error or give it a
    counter alarm; extend the literal-code guard to every crate
    (lambda_error_code_literal_guard.rs:87); prune or wire the 28 error codes never raised
    (error_code.rs); code the board and drain loss warnings (row 103,
    volume_leaderboard.rs:1514-1526; dhan_feed_stack.rs:2016-2030), the two candle warning sites
    (seal_writer_task.rs:849, :1289) and the skipped-window level (dhan_feed_stack.rs:2016); ship
    the three board loss counters in the metric list (cloudwatch-agent.json:24); the cross-check
    says recorded after its marker write failed (daily_task_marker.rs:60-76), so return the error
    and log it coded; every loss counter is pre-seeded at zero, because a series first created at a
    loss is dropped as baseline.
- [ ] **Folded into existing PRs (fifth re-check):**
  - PR16: the error summary rebuilt from 48 h of logs every minute
    (summary_writer.rs:128-172, :397-409) and the depth-seed write (depth_seed.rs:283-306).
  - PR17: fsync of spill, dead-letter and marker files; the database commit mode (from PR32).
  - PR23: test the crash case without relying on replay (see PR31).
  - PR24: name the candle watermark and the 5 s catch-up cutoff; the capture-log marker uses our
    own clock, not vendor stamps (multi_tf_aggregator.rs:1295; ws_frame_spill.rs:1943-1965).
  - PR25: `deny_unknown_fields` on the root struct too; delete `[cross_verify]`, the three dead
    keys in production.toml and the `[cadence]` keys in the same PR.
  - PR26: correction — the SDK retry setting never touches the Dhan POST. Set
    `maximum_retry_attempts = 0` on an event-invoke config for BOTH minters
    (dhan-token-minter-lambda.tf:152-166; dhan-depth-token-minter-lambda.tf:123), pinned by a
    terraform guard.
  - PR27: correction — boot already adopts the SSM token first; the fix is the 807 renew/mint
    path re-reading SSM before minting (token_manager.rs:1283-1360, :402-470).
  - PR7: drop the telegram-dispatcher sub-item (the DHAT job already runs it with the feature on,
    ci.yml:812-832). Add: the socket-reader allocation test checks a copy
    (dhat_ws_reader_zero_alloc.rs:66-78); the depth drain has none (dhan_feed_stack.rs:9060);
    the board sweep test skips publish and hand-off; two unlocked tests share the heap counter
    (dhat_live_ingest_seam.rs:243, :493); packet types and order paths missing
    (dhat_allocation.rs:106-160); three timing harnesses can time an empty workload; the board
    sweep step time is unmeasured since #1953; the production allocator is unmeasured on
    aarch64-musl. The bench gate checks less than the rules say (bench-gate.sh:167; bench.yml).
  - PR8: a quick-failure retry gate on the candle writer (shadow_candle_writer.rs:609-670).
  - PR9: since #1955 depth frames are walked twice too (connection.rs:951-970).
  - PR10: the per-seal board lookup and the tick writer's label table
    (dhan_feed_stack.rs:1702-1724; candle_contract_labels.rs:38; tick_persistence.rs:2289-2294),
    the keepalive counters (connection.rs:1893-1898, :1995-2000), the tick writer's uncounted
    buffer regrowth (tick_persistence.rs:2676-2686), the spot price table pre-size
    (spot_price_store.rs:336), and the per-tick delay timer allocating with the real exporter
    installed (dhan_feed_stack.rs:9964-10006).
  - PR4c follow-ups: a late trade left off the short boards is counted and the close margin tied
    to measured lag (top_volume_sweep.rs:94-130; volume_leaderboard.rs:1203-1229); a contract's
    first live window after a restart is marked (volume_leaderboard.rs:1265-1282); a volume spike
    after a feed gap is named on the board (aggregator_cell.rs:1405-1414); busy 1-second rows'
    empty candle columns are kept or documented (dhan_feed_stack.rs:2318-2346); the contract
    attach seeds ~20,000 instruments in one go, sliced on the idle arm (:7252-7280). Shipped as
    one PR after PR39.
    - Re-check 6 (2026-09-27): also, a mid-session stop loses queued board jobs
      (dhan_feed_stack.rs:7892-7960), so finish or count them at shutdown; the volume that arrives
      after a feed gap also stays wrong and uncounted in the candle itself, not only on the board
      (row 168, aggregator_cell.rs:1405-1414), so count it in the candle and name it there; name the
      misleading late-sweep warning (row 99) and the boot-window partial ranking (row 100). The
      radix-sort bullet of this fold is taken over by PR52. Measured by re-check 6 (release, x86 dev
      box under shared load): a full board `rank` at 20,220 contracts is 4.43 ms (check 5: 4.14 ms;
      the CLAUDE.md row still says 2.95 ms); the sliced sweep's mean step is 4.7 to 5.1 µs and its
      worst step about 121 µs (check 5: 7 to 12 µs, worst 0.17 to 4.2 ms), so the check-5
      multi-millisecond worst steps did not recur; the gainer filter is 226 µs and
      `select_depth_universe` 3.37 ms. Correct the CLAUDE.md figures in the same PR.
  - PR12: marked frames wait for a restart when the live drain queue is full
    (dhan_feed_stack.rs:5444); mid-session catch-up covers them.
  - PR19: the quote handler checks the reply status (quote.rs:149-155, :184-192); request logging
    moves inside the limiter (middleware.rs:654-678); the debug board caps the error file read
    (debug.rs:345-376); the app stops opening a browser on the server (infra.rs:443,
    :1250-1259); two uncalled candle functions are deleted or wired
    (multi_tf_aggregator.rs:600-605, :1704-1730); test builds that crash on a rescued seal
    (dhan_feed_stack.rs:4201-4214, :4728-4732); stale comments and CLAUDE.md facts;
    `plan-verify.sh` is wired into the push gate or the docs stop saying it is; bench compiles in
    PR CI (ci.yml:255); the holiday gate shares the app's config loader (main.rs:115-119).
  - D3: cite the contract-overflow drop when the spot socket's channel is closed
    (dhan_feed_stack.rs:11923-11948) and the pre-08:00 boot collapse
    (dhan_live_universe.rs:507-522, :700-719).
  - D6: a shrink-only shell budget (files, workflow `run:` lines, shell in Rust strings); name the
    console SEBI-save program and its aws CLI calls; list the deploy/ops files; the corrected
    orphan list (8 files; sync-to-integration and test-coverage-guard are called); container
    health checks use shell (docker-compose.yml:436, :511, :554).
  - D7: covers the 808 policy (default: re-read the token once, then park and page) and the
    main-feed half of row 99 (PR21 fixed depth only): after an 805, no NEW main-feed socket is
    dialled; the contracts it would have carried stay pending and come back through D7's probe
    and resume. No subscribed instrument is dropped (owner rule).
  - D11: widen to hard_stop_guard, the start-watchdog curfew, the MON–FRI start, heartbeat and
    liveness crons, the deploy no-go band and the destructive-action lock
    (hard_stop_guard.rs:242-250; main.tf:501, :527).
  - A new D12 — a full availability zone costs a day: script and test the zone move
    (main.tf:362, :384; variables.tf:39-49). Keeps the multi-AZ shape (Quote 13).
  - PR21 note: the ticked item covered depth only; its first bullet named the contract dials.
    The main-feed half is D7's (above).
  - PR4c follow-up (measured by the re-check, release build, busy 4-core x86): the PR4c-1 radix
    sort was SLOWER than the comparator sort it replaced at every size (20,220 rows: 1,126 µs vs
    976 µs; 100–2,000 rows about 2x; harness `radix_vs_comparator_at_every_measured_shape`).
    Re-measure both on the Graviton host and keep whichever is faster there; slicing works with
    either. Also: a full board `rank` measured 4.14 ms at 20,220 (the CLAUDE.md row says
    2.95 ms, stale); the sliced sweep's mean step is 7–12 µs, worst step 0.17 ms in one run and
    ~4.2 ms in two (likely a host pause, unproven until measured on a quiet host). Correct the
    CLAUDE.md figures in the same PR.

## Edge Cases

- PR1: log burst larger than the non-blocking buffer → lines dropped and counted, never blocking.
  Crash → panic hook's synchronous line survives.
- PR2: `-0.0`, subnormals and `f64::MAX` are finite and written unchanged; only NaN/±inf skipped.
- PR3: WAL writer itself down at the same time as QuestDB → the existing WAL floor alarm fires;
  the buffer drop is counted, never silent.
- PR4: ranking thread slower than the cadence → buffers merge (volume is cumulative, so a merged
  delta is exact); a dead thread is respawned by the supervisor and alarmed.
- PR6: order re-indexed under a new `order_no` (engine.rs handle_order_update) must not count twice.
- PR9: a disconnect packet stacked mid-frame after valid ticks; a truncated trailing packet.
- PR10: adversarial key collisions — keys come from the broker feed; ahash is keyed per process.
- PR11: SSM holds the SAME token that failed → not accepted (must be newer); SSM unreachable →
  keep polling, throttled log.
- PR12: QuestDB flaps during catch-up → pause and resume from the watermark.
- D3: two sockets parked at once; the universe exactly at 25,000; the master arriving after boot.
- D4: first tick of the day not yet received → entry refused (no price is a stale price).

## Failure Modes

- Non-blocking logger thread dies → lines dropped; `tv_log_lines_dropped_total` climbs and the
  errors.jsonl sink (separate thread) still carries coded errors.
- Ranking thread (PR4) panics → process aborts (panic=abort) and systemd restarts it; that is the
  same outcome as today's inline sort panicking, never worse.
- Mid-session catch-up (PR12) competes with live writes → rate cap and a separate sender; the live
  path has priority and the catch-up yields.
- Ballast (PR11) cannot be allocated at boot (disk already full) → coded warning, boot continues.
- SSM re-read (PR11) returns a malformed value → rejected by the shape check, never used.
- D3 alarm path unavailable → the coded error still reaches errors.jsonl and the metric filter.

## Test Plan

Every PR runs `cargo test -p` for each touched crate, banned-pattern, pub-fn test/wiring guards,
and CI All Green. Per item (names are the tests each PR adds):

- PR1: `errors_log_writer_is_non_blocking`, `seq_refused_log_is_power_of_two_throttled`,
  `ilp_append_failure_log_is_throttled`, `panic_hook_writes_errors_log_synchronously`.
- PR2: `nonfinite_ohlc_is_written_as_null`, `finite_extremes_are_written_unchanged` (proptest).
- PR3: `full_rescue_queue_defers_to_wal_without_file_io`, `deferred_rows_are_marked_unapplied`.
- PR4: `drain_cadence_arm_does_no_sort`, `merged_cadence_buffers_rank_identically` (proptest vs
  the current inline `rank`), DHAT on the swap.
- PR5: `release_profile_panic_is_abort`, abort-smoke CI step, `unit_file_restart_is_pinned`.
- PR6: `active_count_matches_scan_after_random_transitions` (proptest), `reindex_does_not_double_count`.
- PR7: benches compile and run in `bench.yml`; budgets added.
- PR8: `no_block_in_place_on_drain_outside_shutdown` source guard.
- PR9: `walker_matches_both_legacy_walks` (proptest), fuzz target builds in fuzz.yml.
- PR10: bench delta recorded; `per_tick_maps_use_ahash` source guard.
- PR11: `ballast_released_at_last_shed_level`, `breaker_accepts_newer_ssm_token`,
  `breaker_rejects_same_or_malformed_token`, `breaker_resets_at_ist_rollover`.
- PR12: `midsession_catchup_replays_unapplied_ranges_idempotently`.
- PR13: workflow YAML lint + `no_github_script_in_workflows` guard.
- PR14: `order_update_broadcast_capacity_is_4096`, `lag_triggers_reconcile`.
- D1: `dh904_backoff_does_not_block_runtime_loop` (paused tokio clock).
- D3: `overflow_never_falls_back_silently`, `parked_socket_instruments_are_reassigned`,
  `no_spare_capacity_raises_critical_and_keeps_set`.
- D4: `stale_entry_refused`, `exit_never_gated_by_price_age`.
- D6: `holiday_gate_matches_shell_verdicts`, `rust_only_allowlist_shrank`.

## Rollback

Each PR is independent and reverts cleanly with `git revert`. No schema changes except PR2's
column omission (NULL, readable by the old code). PR4, PR11 ballast and PR12 each ship behind a
`config/base.toml` toggle that defaults ON and whose OFF path is today's behaviour, tested.

## Observability

New metrics (each EMF-selected where an alarm reads it, otherwise gauge/counter only):
`tv_log_lines_dropped_total`, `tv_ilp_nonfinite_skipped_total`, `tv_tick_rescue_deferred_to_wal_total`,
`tv_top_volume_rank_merged_total`, `tv_top_volume_rank_lag_ms`, `tv_disk_ballast_released`,
`tv_token_ssm_reread_total{outcome}`, `tv_wal_midsession_catchup_frames_total`,
`tv_order_dh904_retry_inflight`, `tv_universe_unplaced_instruments`. New alarms only where the
cost rule in `aws-budget.md` and the noise lock allow; D3's critical rides the existing live-lane
integrity family.

## Per-Item Guarantee Matrix

See per-wave-guarantee-matrix.md. All 15 rows of the guarantee matrix and all 7 rows of the
resilience matrix apply to every item. Rows that do not apply to an item are written
`N/A — reason` in that item's PR body. Every "100%" claim in these PRs carries the §F envelope
qualifier: 100% inside the tested envelope, with ratcheted regression coverage.
