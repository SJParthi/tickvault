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
  - [x] **D3b — widen a running session when today's list lands.** The lane reads the universe
    once at boot. Reuse the late attach's machinery: the set difference on the composite key goes
    to spare room on live sockets via `LiveSubscriptionCommand::Extend` and to new sockets via
    `build_feed_stack_plan`; the attach task keeps the pool until the widen is done. Each Extend
    ≤ 5,000 per socket to stay inside the 5 s top-up budget.
    - Re-check 7: the list is read once at boot, and the 08:30 boot waits only to 08:40 while
      the rider's budget is 900 s plus retries, so a slow rider always loses the race. D3b is
      the fix for both: keep watching for today's list after boot and widen when it lands.
      When D3b clears the degraded state the heartbeat stops by itself.
    - Done: `resolve_live_universe` arms `LIVE_UNIVERSE_WIDEN_PENDING` in its three off-today
      arms (earlier-day list, expected 4-index boot, paged 4-index boot); `main.rs` hands the
      late attach a `TodaysUniverseSource` on a trading day. Each attach attempt, BEFORE the
      contract selection, `widen_running_session` reads today's list with the boot's own
      precedence and selection (`read_todays_live_universe`, today only, silent on a miss),
      takes the composite-key set difference against what is on the wire (`widen_delta`), and
      places it on the boot spot connection's spare room, then the other live connections'
      room (`send_extend_chunks`, split out of the contract top-up so both mark and reconcile
      the same way), then new main-feed connections from the pool; no new connection after an
      805. Every slot it takes comes out of the frozen contract capacity. Placed instruments
      are seeded into the silence detector. When the delta is empty and every ack is
      answered, `finish_live_universe_widen` clears the page (or moves it to
      `truncated_to_capacity`, and the heartbeat now reads the current reason). With every
      connection full it stops with one error and drops nothing. The attach keeps the pool
      after handing depth to the rebalance, and neither the success return nor the 10:00
      give-up ends it while the widen is pending; the 15:30 hard stop still does. Instruments
      on the boot list but not today's stay subscribed (nothing is dropped). Log and alarm
      text no longer tell the owner to restart. The 08:40 boot wait stays: the session now
      widens after it, so dialing early on the earlier list beats waiting to 09:10.
      Tests: `widen_delta_is_the_composite_key_set_difference_in_todays_order`,
      `running_widen_new_seeds_the_wire_set_from_the_boot_set_and_only_adds`,
      `widen_slots_put_the_spot_connection_first_and_write_back_every_room`,
      `the_widen_runs_before_contract_selection_and_holds_the_attach_open`,
      `todays_master_with_never_takes_an_earlier_day`, `todays_master_with_keeps_the_boot_precedence`, `read_todays_live_universe_is_none_while_no_list_is_on_disk`,
      `finish_live_universe_widen_clears_the_page_or_moves_it_to_truncated`, `record_widen_list_overdue_pages_with_the_fresh_boot_reason`,
      `live_universe_widen_pending_is_armed_by_every_off_today_arm`.
    - Left for D3c (named, not done here): the 15:41 cross-verification still gets the boot
      set (main.rs, `spawn_dhan_live_crossverify`), and the spot contract labels come from the
      boot publish plus the contract attach, so a list that lands after contracts dial has no
      labels for the added spots until the next boot. The contract attach already running
      when today's list lands keeps its capacity net of the spots (not re-sized upward).
      From the D3b review (all LOW, none drops an instrument): a top-up ack that frees
      instruments does not give their room back, so a re-offer charges the room and the
      contract capacity twice and can end the widen early as "full"; a list read in the
      milliseconds between the rider writing the full mapping and the NTM file takes the
      full mapping; an 805 halt and a dial shortfall page under `truncated_to_capacity`;
      the widen counts `planned` connections but the older contract dial still counts
      `dialed`.
    - Review fixes in D3b itself: a new widen connection takes a whole connection's room
      off the contract capacity; a widen dial shortfall keeps the page on; the contract
      give-up still pages at 10:00 while the widen keeps running; an early boot whose list
      never lands pages once at the rider hour plus the boot wait (08:10) and keeps the
      alarm fed (`widen_list_is_overdue_matches_the_boot_wait_deadline`).
  - [ ] **D3c — the rest of D3:** parked-socket reassignment, late top-up refusals counted and
    alarmed, the top-up log text, the no-trade-by-09:30 gauge, and the fallback counter's
    reason hygiene (seed `ntm_*`; stop widenings paging).
    - Re-check 7 items to verify and fold in: the rider rejects the whole build when more than
      10% of the 49 NSE list downloads fail, and its error lines carry no code
      (`dhan_universe.rs`); the 4 fallback index ids got zero packets; depth-200 dials 0 of 5
      sockets during a fallback or QuestDB lag and `top_volume` goes empty, with no page; the
      universe headroom check counts spots only (~870 against 25,000) so it can never fire;
      D8's plan text says "match by symbol" but the list files carry no symbol.
    - Split (mapped 2026-09-28 against c7e27de88), serial:
      - **D3c-1** (`app`: `dhan_universe.rs`, `dhan_live_universe.rs`): the rider writes the
        artifacts even when more than 10% of the index lists fail (coded error; reject only when
        every list failed, or drop only the NTM artifact when its own lists failed); `code` on
        the rider's uncoded error lines; the narrowed-artifact reasons (`fno_*`, `ntm_*`) move
        to their own unpaged counter, seeded; the headroom check counts spots plus contracts
        after the contract selection, with D3a's fill-by-priority wording.
        **DONE 2026-09-28** (`write_narrowed_spot_artifacts`, `NARROWING_FALLBACK_COUNTER`,
        `report_spots_and_contracts_headroom`, `HeadroomStage`). As built: the FULL mapping
        stays refused past 10% (its membership would be partial and read as complete); the
        F&O and NTM files are written from the lists that did download, and the NTM file is
        still refused when its own list resolved nothing. The rider's error lines now carry
        `code` and `source`. The combined headroom is a `warn!`, not a page, because the
        contract selection fills its room by design and pages a shrink itself. Tests:
        `write_narrowed_spot_artifacts_runs_before_the_index_list_reject`,
        `narrowing_fallback_counter_is_separate_from_the_paged_counter`,
        `report_spots_and_contracts_headroom_adds_the_contracts_to_the_published_spots`.
      - **D3c-2** (`app` + terraform + agent config, observability only): a late top-up
        refusal counter with an alarm, `source` on its errors, one final reconcile before the
        attach returns, "queued" wording and counts from the `Held` ack; an unpriced-underlyings
        gauge at the contract dial; the dial-incomplete counter and a depth-200 give-up alarm;
        a `top_volume` rows-written counter with an alarm on zero in market hours.
      - **D3c-3** (`core` + `app`, behaviour): a parked main-feed socket's instruments are
        re-offered to the other live connections (never dropped silently: no room pages and
        counts), and the top-up senders outlive the attach so a park after 09:30 is covered.
      - D8 re-scope (text): the artifact rows do carry `symbol`; the reader drops it, and index
        rows are NSE-only so SENSEX never matches. D8 parses `symbol` and includes BSE indices.
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
  - Series (2026-10-01, owned by the "Replace shell scripts with Rust" thread; one PR each,
    serial; the audit-plan thread skips D6):
    - [x] D6a — shell budget first: `crates/common/tests/shell_budget_guard.rs` freezes the 105
      shell files (46 developer tooling by file set; 59 others by file set AND line ceiling) and
      pins each systemd unit's shell `Exec*=` count (1 + 3 + 1). Rule lock §0.10. Test-only.
      Tests: `no_new_shell_files`, `shell_lists_shrink_only`, `ops_shell_files_never_grow`,
      `systemd_units_never_add_shell`, `shell_budget_guard_self_test`.
    - [x] D6b — host tuning (3 of the 5 boot shell programs: verify-net-tuning.sh,
      apply-host-tuning.sh, the BBR `/bin/sh -c`) becomes `tickvault host-tuning` in `app`, same
      behaviour, pure core + thin I/O shell, unit tests on every branch. Unit pin 3 → 0; two
      ops entries removed; user-data `chmod` lines removed; the three host-tuning guards re-pointed.
    - [x] D6c — holiday gate becomes `tickvault holiday-gate` in `app` (IMDSv2 via reqwest; SSM
      marker, SNS page and StopInstances via the existing workspace AWS SDK pins). Same fail-open
      contract: stop only on a definitive holiday verdict. Unit pin 1 → 0.
    - [x] D6d — QuestDB self-heal becomes `tickvault ensure-questdb` in `app` (same ladder:
      running → start → pull → compose v2 → v1 → plugin path → docker run), and the operator
      console's SSM strings call the binary. Unit pin 1 → 0.
    - Shipped (verified on main 2026-10-04): D6b in #2001 (`crates/app/src/host_tuning.rs`, unit
      runs `tickvault-host host-tuning`); D6c in #2004 (`crates/app/src/holiday_gate.rs`,
      `deploy/aws/holiday-gate.sh` deleted); D6d in #2004 as R3-12 (`ensure_questdb.rs`,
      `scripts/ensure-questdb.sh` deleted). Only D6e onward is open.
    - [ ] D6e onward — the rest by risk: SSM command strings and `sh -c` spawns in Rust get a
      budget, then workflow `run:` steps and the Makefile get a budget, then operator scripts
      are ported or deleted (orphans first), each PR lowering the D6a lists.


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
    the torn line, keep the rest. **Done 2026-10-04** (`tick_spill_replay.rs::isolate_refused_lines`): a
    permanently refused chunk is bisected at line boundaries; refused single lines go to
    `quarantine/<file>.rejected-lines` (synced, uploaded like any quarantined file), counted on
    `tv_tick_spill_replay_lines_rejected_total`, and the file keeps draining. Covers a torn line
    anywhere, not only last (re-check 6). Whole-file quarantine stays for a chunk with no
    accepted row or past the caps (1,024 POSTs, 64 lines). Tests:
    `a_torn_line_is_set_aside_and_the_rest_of_the_file_is_replayed`,
    `a_torn_tail_is_set_aside_with_a_newline`,
    `a_chunk_with_every_line_refused_still_quarantines_the_file`,
    `split_at_line_boundary_splits_whole_lines_near_the_middle`.
  - The candle seal spill was never synced. **Done 2026-10-04**
    (`seal_spill.rs::SealSpillWriter::sync_open_file`, called from
    `seal_writer_runner.rs::SealEscalationSink::run`): the escalation thread syncs the day file
    after each batch that empties its queue, at least once a second under a burst, and once at
    exit; the day rotation syncs the closing file. The lock is held only to `dup` the handle, so
    an inline append never waits for the device; `append_seal` itself never syncs. Failures on
    `tv_seal_spill_sync_failed_total`, one coded `error!` per failing episode. Tests:
    `sync_open_file_is_a_no_op_with_nothing_open_and_keeps_the_handle_open`,
    `the_drain_reachable_append_never_syncs_and_the_sync_holds_no_lock` (bite-tested), and the
    two escalation-summary tests now pin `syncs`. Not done: the seal DLQ is still not synced,
    and a seal the drain writes inline (escalation queue full) waits for the thread's next sync, or for the day rotation if the thread has exited.
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
- [x] **D10 — no depth path relies on unsubscribe.** **OBSOLETE 2026-10-04:** the design reversed. #1994 swaps depth-200 in place (code 25 then 23), the owner ruled for unsubscribe/subscribe on 2026-10-01, and Dhan's 2026-09-30 reply confirms code 25; a ghost is answered by `request_ghost_unsubscribe`. Original text kept below. (`core`) Dhan depth unsubscribe (codes 25
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
- [x] **PR30b — the October $150 ceiling in terraform.** (deploy; on or after 2026-10-01)
  - The October $150 ceiling is enforced in code (`effective_budget_kill_usd`) but budget.tf and
    budget-guards.tf still say $225 (budget.tf:220-222; budget-guards.tf:278). Quote 23 keeps
    $225 for September, so the terraform change is a dated PR on or after 2026-10-01, in all four
    lockstep sites.
  - Re-check 6 (2026-09-27): line correction, budget-guards.tf:278 is now :300.
  - Done 2026-10-01: `budget.tf limit_amount`, `budget-guards.tf BUDGET_KILL_USD`,
    `budget_digest::BUDGET_USD` and `hard_stop_guard::DEFAULT_BUDGET_KILL_USD` all read $150,
    pinned by `budget_ceiling_lockstep_guard.rs`. The month clamp stays as the backstop and now
    changes no month. The native 90% stop line is $135.00 again; keeping October under $150
    still needs one of the owner levers in daily-universe §0 Quote 23.
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
- [x] **PR31b — the restart rebuild never overwrites a fuller candle.** (`app`, `storage`,
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
  - **Re-checked 2026-10-01.** The core overwrite is already covered by plan item 47
    (`mark_replay_gap`, `finish_replay`, the randomized `restart_differential.rs`). What is left
    is split in two.
  - [x] **PR31b-1 — the crash marker covers the candle queue drained last (2026-10-01).** The
    seal writer marked its crash marker (`seal-unwritten.mark`) clean at the end of its own final
    drain, but the escalation queue drains after it and can hold up to 250,000 sealed candles,
    so a kill during that wait lost them behind a clean marker. Now: on cancel the writer writes
    the count at once (`UnwrittenSealMark::record_now`), before its final drain; after the drain
    it marks clean only when it holds nothing; `main` finishes the marker after the escalation
    step whatever its outcome (`finish_unwritten_mark_at_shutdown`, 2 s budget on a blocking
    thread, new step 5b-2c), and no later write to that marker lands. A failed marker write and
    an unreadable marker at boot are coded (AGGREGATOR-SEAL-01). Honest limits: a count written
    after the final drain includes residue the drain already paged (counted twice, never
    missed); the marker is not updated during the escalation drain, so a kill there over-counts;
    an overrun or lane-timeout page names no count; a writer that exits mid-session leaves the
    marker unfinished. Tests: 5 in `seal_writer_loop::pr31b1_tests`,
    `the_loop_reads_the_previous_marker_before_it_writes_its_own` updated,
    `crates/app/tests/seal_unwritten_mark_shutdown_guard.rs` (4).
  - [x] **PR31b-2 — the rest.** (a) The WAL warm-up with archived segments and row 253, as the
    chosen approach above. (b) NEW 2026-10-01: a CLEAN shutdown mid-session writes truncated
    open bars as complete. `seal_open_buckets_at_close()` runs at lane exit
    (dhan_feed_stack.rs, at the lane's shutdown seal) with no session gate, and
    `restart_differential.rs` simulates crashes only, so it cannot see this. Gate the seal to
    after the close (or mark those bars partial) and add the clean-shutdown case to the
    differential.
    - (b) delivered by PR #2009, folded into #2004 on 2026-10-03: a mid-session exit seals what
      the catch-up would seal and withholds the rest (`seal_complete_buckets_at_mid_session_exit`).
    - Hostile review 2026-10-03: until (a) lands, those withheld bars are LOST, not rebuilt: a
      restart's replay skips segments already applied, so a market-hours deploy leaves every
      open 3m-60m bar, and quiet contracts' bars that ended inside the late-trade margin, missing
      (counted). Before (b) they were written short. (a) must re-read the archived segments that
      cover them; that is #2010 part 2, not written yet.
    - Also from that review, for (a): `withhold_open_buckets` skips cells with no open bucket, so
      a settled late-trade carry that `force_seal_all` would re-emit is dropped uncounted.
    - **(a) MEASURED 2026-10-03 and NOT built.** The `restart_differential.rs` model was run
      with a restart that re-reads EVERY saved frame before the stop (the best a warm-up could
      do) against one that re-reads none (what a clean exit leaves). Over 4,000 random days
      (1,229 clean exits): of 25,832 bars that ended before the exit and are missing or short
      in the database, the full re-read wrote 1; of 22,591 bars that spanned the exit, 0;
      after crashes, 13 of 54,480. Each withheld bar either spans the restart's downtime or
      ended within the late-trade margin of the exit, and the restart rules (rounds 18-25)
      withhold both whatever was re-read. Deploys are also already blocked 09:00-15:45 IST.
      So the warm-up would add boot time and about 1,800 lines for no rebuilt bars; the
      10-second boot question is moot. **DROPPED 2026-10-03** (owner: "simply go ahead",
      relayed by the coordinator, taking the recommendation to drop it).
    - [x] Carry count (2026-10-03): `withhold_open_buckets` now drops an outstanding carry on a
      timeframe with no bucket open and counts it on
      `tv_candle_refold_partial_suppressed_total` (`AggregatorCell::discard_carry`); it is not
      written, since a running process would settle it into a bucket the exit cannot know.
      The exit log and docs no longer promise a rebuild. Test:
      `test_regression_withhold_open_buckets_counts_a_dropped_carry` (fails without the fix).
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
    `test_mark_clean_and_sample_write_only_on_change_and_at_most_once_a_second`,
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
- PR41 — a replayed candle never replaces a fuller one, and one stuck spill file never holds the
  rest. (`storage`, `trading`) Split 2026-10-01 into PR41a (the never-replace rule and the file
  order) and PR41b (the stuck file, the suspect table, the replay gate and the record checksum).
  PR41b split again 2026-10-01: the record checksum, the torn single-record cut-back and the
  batch alignment check moved to PR41c, because a checksum needs a new spill format version and
  its own reader migration.
- [x] **PR41a — a replayed candle never replaces a fuller one.** (`storage`)
  - A replayed seal overwrote a newer corrected candle (a late trade re-folded a sealed bar),
    uncounted (seal_writer_task.rs:903-917, :1127-1175; aggregator_cell.rs:261-268). The honest
    limit said "Last write wins" (seal_writer_task.rs:986-989).
  - Verified before the change: only the most recently sealed bucket of a timeframe can be amended
    (`aggregator_cell.rs`, both `AmendedLate` arms compare against `last_sealed[ord]`), and every
    later copy of a bar has more ticks (`fold_late_hlc` adds one) or more volume (the day-close
    carry). So "fuller" is `(tick_count, volume)` in that order, and one entry per slot and
    timeframe is enough.
  - Done 2026-10-01: `seal_spill_ledger.rs`. The spill writer keeps, under its append lock, the
    newest spilled bucket of every `(security_id, segment, feed, timeframe)` and that copy's
    fullness; capacity `SEAL_SPILL_LEDGER_CAPACITY` = `SEAL_BUFFER_CAPACITY`, allocated once,
    never grown, O(1) per operation. (1) After every clean live flush, `drain_once` calls
    `SealAbsorptionPipeline::note_live_commits`: a committed copy fuller than the spilled one is
    appended to the spill behind it, so every replay ends on it (one atomic load when nothing was
    spilled this process). (2) A spill append of a copy less full than the one held is not
    written. (3) The mid-session replay drops a record the spill holds a fuller copy of
    (`SealSpillWriter::replay_is_superseded`). (4) The boot drain never writes a copy less full
    than one it already wrote in that drain (`BootDrainOutcome::seals_superseded`), which covers a
    dead-letter file holding an older copy than a spill file read before it.
  - Done 2026-10-01: staged files replay oldest write first, name second (`sort_by_write_time`),
    in both the boot drain and the mid-session replay. Name order put a day file before the file
    set aside from the same day (`<name>.<n>`), although the set-aside part was written first.
  - Counted: `tv_seal_spill_superseded_total{kind="mirrored"|"older_not_written"|
    "replay_older_skipped"|"mirror_failed"|"untracked"}` and
    `tv_seal_writer_drain_total{kind="boot_superseded"}`. A failed mirror append is a coded
    `error!` (AGGREGATOR-SEAL-01).
  - Honest limits: a crash between a live flush and its mirror append (one drain cycle), or a
    failed mirror append, still lets the next replay write the older copy over the stored row.
    The boot drain does not read the stored row to compare (a database read per recovered seal).
    PR31b's restart rebuild shares the rule but not this mechanism.
  - Tests: `seal_spill_ledger::tests::*` (6),
    `pr41a_an_amended_copy_committed_live_is_appended_behind_its_spilled_original`,
    `pr41a_an_older_copy_is_not_appended_after_the_fuller_one`,
    `test_pr41a_replay_is_superseded_when_the_spill_holds_a_fuller_copy`,
    `test_pr41a_note_live_commits_does_nothing_while_the_spill_has_held_nothing`,
    `pr41a_a_failed_mirror_append_is_counted_and_reported_as_nothing_appended`,
    `pr41a_mid_session_replay_writes_only_the_fuller_copy`,
    `pr41a_boot_drain_never_writes_an_older_copy_after_a_fuller_one`,
    `pr41a_boot_drain_writes_both_copies_when_the_older_comes_first`,
    `pr41a_staged_files_replay_oldest_write_first`.
- [x] **PR41b — one stuck spill file never holds the rest.** (`storage`)
  - A candle the replay cannot flush is skipped and later files wait
    (seal_writer_task.rs:1209-1262): tell a flapping database from a bad record before skipping,
    and move past a stuck file.
  - The replay trusts acknowledgements while the table is suspect (seal_writer_task.rs:1026-1040):
    keep the file until the table is healthy.
  - The replay gate opens only on live traffic, so a spill made after the last live write waits
    for the next boot: reopen it on a database health check too.
  - Row 36 (c6#36): replay the dead-letter file mid-session, or record boot-only replay.
  - Done 2026-10-01, all in `seal_writer_task.rs::MidSessionReplay`:
    (1) `classify_replay_flush_failure`: a failed replay flush whose error is the transport
    (`SocketError`, `CouldNotResolveAddr`, `TlsError`), the server's configuration or
    authentication, or the candle tables not yet keyed (`CandleTablesNotKeyed`) never counts as a
    strike against the record. Any other failure at a step of one record is a strike, the first
    always and each later one only when the database accepted a write since the last failure (a
    clean live flush or a clean probe). After `SEAL_REPLAY_STUCK_FAILURES` (12) failures at one
    position with no progress, the file is parked where it stopped for `SEAL_REPLAY_PARK_SECS`
    (600) and the next staged file goes ahead; the parked file resumes at the same record.
    (2) While the QuestDB WAL-suspension watcher reports a suspended or lagging table, or cannot
    see, nothing replays. A file read to its end waits until two more clean probes have reported
    before it is archived; if suspicion begins first, it, the file being read and every parked file
    are read again from their start (the DEDUP keys collapse what had landed). With no watcher
    running, a finished file is archived at once, as before.
    (3) The gate also opens once a clean probe has reported after the last failure and
    `SEAL_REPLAY_HEALTHY_SECS` have passed since it. `AppliedWatermark::clean_probe_count` is new;
    the runner feeds `ReplayProbe::current()` into `observe_probe` every cycle.
    (4) Row 36 decided: the dead-letter file replays at boot only. A seal reaches it only when the
    spill append itself failed, and the DLQ has no paused-append staging, so moving its file
    mid-session could lose a seal appended at the same instant. Recorded in the replay's module
    notes.
  - Counted: `tv_seal_replay_total{kind="files_parked"|"files_rewound"}` (new), beside the
    existing kinds. Parking is a coded `error!` (AGGREGATOR-SEAL-01); a rewind is a `warn!`.
  - Honest limits: parking lets a later file reach the database before an older one; an older copy
    of a bucket the spill also holds newer is dropped by the PR41a ledger, but a key the ledger could
    not track is not. The strike evidence rule adds little beyond the gate, which already demands a
    clean live flush or a clean probe before the retry; the transport classification is the real
    separation. A refusal the database reports without naming a line (for example a 5xx after the
    client's own retries) still counts as a strike.
  - Tests: `classify_replay_flush_failure_tells_the_transport_from_a_refusal`,
    `a_transport_failure_never_skips_a_record_and_a_stuck_file_is_parked_so_the_next_goes_ahead`,
    `a_parked_file_resumes_at_its_record_after_the_park_window`,
    `a_clean_probe_reopens_the_gate_without_live_traffic`, `a_suspect_table_closes_the_gate`,
    `a_finished_file_waits_for_two_clean_probes_before_it_is_archived`,
    `suspicion_before_confirmation_reads_the_file_again_from_its_start`,
    `observe_probe_rewinds_the_file_being_read_and_every_parked_file_on_suspicion`,
    `test_replay_probe_current_reads_the_process_watermark`,
    `wal_applied_watermark::tests::clean_probe_count_counts_only_clean_probes`.
- [x] **PR41c — every spilled candle record can be checked.** (`storage`)
  - The candle spill has no record checksum (row 136, seal_spill.rs:832-841, :907-916): cut back a
    torn single-record write the way the batch does, check alignment before a batch, add a
    checksum. The 128-byte record is full, so the checksum needs a new format version (for
    example reusing the legacy low-32 id at bytes 0..4, the full id being at 120..128) and a reader
    that still accepts the current version during the rollout.
  - Done 2026-10-01, in `seal_spill.rs`: spill format version 5. Bytes 0..4 hold a CRC-32
    (IEEE, the in-crate `crc32_ieee`, no new dependency) of bytes 4..128, the version byte
    included; the id is read from 120..128 only. `decode_spill_record` applies the version gate
    and the checksum for every spill reader (the boot drain, the mid-session replay and
    `read_all`), so they cannot disagree. Readers accept versions 4..=5
    (`SEAL_SPILL_OLDEST_READABLE_VERSION`), because 4 → 5 renumbered no timeframe; a v4 record
    must have matching low-32 and full ids, which is what refuses a v5 record whose version byte
    flipped to 4. The dead-letter readers accept the same range. A record whose checksum fails
    is refused and counted, and the read continues with the next record.
    A failed single-record write is cut back to its last whole record, as the batch path does
    (set aside if the cut fails); the day file is cut back when it is opened; a batch checks the
    length it starts from and cuts a partial record off first.
  - Counted: refused records add to the existing undecodable / `records_skipped` counts; one coded
    `error!` (AGGREGATOR-SEAL-01) per file read or per replay step names how many failed the
    checksum.
  - Honest limits: a v4 record still on disk during the rollout has only the id cross-check, not a
    checksum. A record refused for its checksum is one candle not replayed; its bytes stay in the
    archived file. A rollback to the v4 build refuses and archives every v5 record it drains, and
    nothing re-reads `archive/`, so those candles need a manual re-ingest. If a day file ends
    mid-record and can be neither cut back nor set aside, appends to it are refused and each seal
    goes to the dead-letter tier.
  - Tests: `to_bytes_writes_a_checksum_of_bytes_4_to_128_at_bytes_0_to_4`,
    `decode_spill_record_refuses_a_flipped_bit_anywhere_in_the_record` (every bit of the record),
    `a_v4_record_written_by_the_previous_build_still_decodes`,
    `test_reseal_record_for_test_restores_a_valid_checksum`,
    `read_all_skips_a_damaged_record_and_keeps_reading`,
    `cut_back_to_whole_records_removes_only_a_partial_record`,
    `a_torn_day_file_is_cut_back_when_it_is_opened`,
    `a_batch_starts_on_a_record_boundary_even_on_an_open_handle`,
    `boot_drain_reads_a_v4_record_and_refuses_a_damaged_one`, `boot_drain_reads_a_v4_dlq_line`,
    `replay_skips_a_damaged_record_and_reingests_the_rest`.
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
  - [x] **PR42a — the losses are seen (2026-10-01).** One new shipped counter,
    `tv_order_audit_chain_lost_total{source}`, counts P&L audit rows discarded
    (`pnl_audit_discarded`), leg P&L rows discarded (`order_leg_pnl_discarded`) and order-push
    updates a lagging consumer skipped (`order_push_lagged`); each source is seeded at 0. It is
    the sixth leg of `tv-<env>-order-audit-chain-loss`. One name, not three: the CloudWatch agent
    folds the label, the alarm needs only the sum, and the coded log line names the source
    (+$0.30/mo, aws-budget.md COST NOTE 2026-10-01). The lag warning is now a coded AUDIT-06
    error (`source = "order_push_lagged"`). The per-writer counters stay local.
    Tests: `order_side_paging_wiring_guard` (emit and seed at every source, coded lag arm, alarm
    sums m6, selector carries the name); EMF count ratchet 102 → 103.
  - [x] **PR42b — the rows survive (2026-10-01).** New module `storage::audit_spill`. A failed
    flush of `order_audit`, `pnl_audit` or `order_leg_pnl` writes the batch's exact ILP bytes
    (whole rows only) to one immutable file under `data/spill/audit/<table>/` (tmp, fsync,
    rename, fsync dir) and the flush reports `Ok`. One drain task per table, spawned after its
    `ensure_*_table`, POSTs the files to `/write` at once and every 60 s: a 2xx deletes the
    file, a permanent 4xx moves it to `quarantine/` (kept) and counts its rows on
    `tv_order_audit_chain_lost_total{source="<table>_spill_lost"}`, anything else stops the
    round and keeps the file. No session-window filter (unlike the tick drain). Bounded at
    64 MiB and 50,000 files per table; past that the batch is discarded and counted as before.
    Replay is safe to repeat: each row keeps its own `ts`, the first column of every DEDUP key.
    Tests: 15 in `audit_spill`, 3 or 4 per writer (spill, half-appended row left out, refused
    spill still discards, production dir), two consumer tests updated (spilled rows count as
    appended). Hostile review fixes: the drain pages once per backlog episode when the oldest
    waiting file is 30 min old (`tv_order_audit_persist_errors_total{stage="spill_backlog"}`, an
    existing leg of the order-audit chain-loss alarm, so no new metric or alarm cost); the cap
    no longer counts `quarantine/`; a failed directory sync after the rename keeps the rows
    instead of counting them lost; the drain re-runs the table's ensure before replaying a
    backlog; the replay URL states `precision=n`; a stale `.tmp` counts its rows as lost; the
    leg-P&L flush runs under `block_in_place`; the two consumer tests remove the spill files they
    write. Honest limits: the daily reconcile counts a spilled row as appended while it is still
    on disk (the 30-min page covers that window); a file over 8 MiB whose second chunk is
    refused counts all its rows lost, though the first chunk was stored.
  - [ ] **PR42c — reconcile on lag. Needs an owner decision.** The order-push consumer holds no
    copy of the paper OMS order map, and fetching the broker order book is REST outside the
    allowed classes (`no-rest-except-live-feed-2026-06-27.md`). Options: share a read handle on
    the OMS order map, or accept counted loss. Not started.
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

### Added 2026-09-28 (seventh re-check, main 13e405f), riskiest first

Source: `/mnt/project-files/audit/recheck7-gaps.md` (15 ranked items and the per-area gap lists,
each with file:line). "Verified" below means this thread read the code on `origin/main` at
13e405f. Every other line is carried from the re-check and is re-verified when its item starts.

Order of work: PR #1975 (D3a) finishes first. Then PR53 (it can destroy a kept table), D3b, D3c-1,
PR58 (every day's first trade is missing from its first candle), PR59 (read-only query), D3c-2, D3c-3,
PR54, PR55, PR56, PR57, then the order already set: PR41, PR31b, PR42–PR50, PR31c, PR32–PR39, PR51,
the PR4c follow-ups, PR52. PR30b stays on or after 2026-10-01.

- [x] **PR53 — a boot never drops a kept table, and a clean-up marker means the clean-up ran.**
  (`storage`, `app`)
  - Verified: `drop_legacy_candle_objects` runs `DROP TABLE IF EXISTS candles_1s` (step 2,
    shadow_persistence.rs:833-840) whenever its marker is missing, unreadable or on an older sweep
    version. `candles_1s` is a live fold table today (`TfIndex::S1`, tf_index.rs:445), so any
    future sweep-version bump, or a lost `data/state` marker, drops every 1-second candle.
  - Verified: the marker is written after the sweep whatever the drops returned; a failed drop is
    logged and the marker still says done.
  - Fix: remove `candles_1s` from the drop list (a guard test pins that no drop list names a live
    `TfIndex` table or a KEEP table); write each marker only when every drop in its sweep
    returned 2xx; PR31a's background re-run waits for the database to answer before it sweeps;
    correct the false "retry next boot" line (candle_ddl_boot.rs:119-120).
  - Done: step 2 removed (`shadow_persistence::drop_legacy_candle_objects`); both sweeps collect
    `answered` from `run_drop_ddl` (now `-> bool`) and write their marker only when it holds;
    `drop_status_counts_as_answered` counts 2xx AND 4xx as answered (a 4xx is QuestDB refusing
    the form, e.g. a matview DROP on a plain table, which the sweep always treated as a no-op;
    requiring 2xx would re-run the sweep every boot), and a transport error or 5xx as not
    answered. That gate is what makes the background re-run safe while the database is down, so
    no separate wait was added. Docs corrected in shadow_persistence.rs and candle_ddl_boot.rs.
    Tests: `no_drop_sweep_names_a_kept_candle_table` (name sets vs live candle tables, plus no
    literal candle-table DROP in either sweep body; bite-proven by re-adding the literal),
    `drop_status_counts_as_answered_only_when_questdb_answered`. Non-candle KEEP tables stay
    pinned by the existing `retired_drop_list_never_names_a_live_table`.
- [ ] **PR54 — a depth socket that never sends a first frame pages.** (`app`, `core`)
  - From re-check 7: a morning-dial depth socket that never delivers a frame pages nothing, and
    the comment that says RISK-GAP-03 covers it is false for depth. PR44/PR45 do not cover it.
  - Fix: a per-socket first-frame deadline (O(1) per socket) that pages through an existing
    live-lane alarm, and the comment corrected.
- [ ] **PR55 — no deploy input reaches a shell, and every deploy path needs main + All Green.**
  (`.github/workflows/`, deploy terraform) Widens PR36b.
  - Verified: `${{ github.event.inputs.confirm_market_hours }}` is pasted into `run:` in
    deploy-aws.yml:420 and :850 (and terraform-apply.yml:454 per the re-check). Pass it through
    `env:` in every workflow.
  - From re-check 7: a pushed `v*` tag, a re-run of an older dispatch run, terraform-apply /
    resize / disk-grow / log-wipe from another branch, and a PR that edits terraform-apply.yml
    all bypass the PR36b gate; the PR36a key guard checks text only, so `--message "$TOK"`
    passes (terraform-apply.yml:757-763). Close each; test the guards against stubs in CI.
- [ ] **PR56 — arrival time never runs ahead after a backward clock step.** (`storage`, `app`)
  Widens PR31c's close-clock bullet.
  - From re-check 7: after a backward step nothing re-anchors or clamps the receipt anchor, so
    the feed-delay gauge (a false Dhan feed-delay page at about 60 s or more), silence stamps,
    the board close clock and depth times all skew for the rest of the process. Re-anchor, and
    alarm on the size of the refused jump rather than on `refused_backward` (read noise).
- [ ] **PR57 — the read-only status check proves the feed shape, not only that it ticks.**
  (`api`, `app`, `.github/workflows/aws-control.yml`) Asked 2026-09-28 by the coordinator for
  the owner's live proof: the 2026-09-28 live check proved about 137,000 ticks and about 3,400
  instruments a minute, but not these three. The `status` action (read-only) also reports:
  (1) the exact subscribed main-feed instrument count, per connection and in total, read from
  the app's own `/health` (Rust), not recomputed in shell; (2) each depth-20 and depth-200
  socket's state (connected / reconnecting / parked, instruments held), plus `market_depth`
  row growth over the last minute as the cross-check; (3) every `tv-<env>-*` CloudWatch alarm
  whose state is not OK, by name and since when. Read-only: no action, no restart, no write.
  Any new logic goes in Rust (the `/health` payload); the workflow only prints it.
- [x] **PR58 — the day's first trade is counted in its first candle.** (`trading` candles, not
  indicator/strategy) Reported 2026-09-28 by the "Ticks vs Dhan chart mismatch" thread (owner
  compared HDFCBANK-29Sep2026-780-CE `candles_5s` with Dhan's 5 s chart).
  - Verified in code (`multi_tf_aggregator.rs`, `consume_tick`): the first ACCEPTED tick of a
    slot seeds `last_cumulative` with its own day-cumulative volume, so the first bar gets
    `cum - cum = 0` for that tick. The previous day's connect snapshot is refused earlier
    (`stale_trading_day`, before the slot lookup) and never seeds, and `force_seal_all` resets
    the seed at day end. So when the feed is up before the open, the first trade of every
    contract (cumulative 650, say) is left out of the 09:15 bar on every timeframe.
  - Seeding is right only when we joined after trading began (a mid-session restart or a late
    top-up; test `a_mid_session_slot_creation_must_not_put_a_whole_days_volume_in_one_bar`).
  - Fix: seed at 0 when there is per-slot evidence we were watching before today's first
    trade: a same-day `stale_trading_day` refusal seen for this key (lookup only, no slot
    created), or the key's first packet received before the session open. Otherwise keep
    seeding. O(1) per tick, no allocation; the seeded counter gains a `baseline` label
    (`zero` / `first_tick`).
  - Test: stale-day snapshot, then a 09:15:02 trade at cumulative 650 -> the 09:15:00 5 s and
    1 m bars carry 650; the mid-session test still passes.
  - As built: the proof is the RECEIPT second of the latest such packet (Dhan re-sends one on
    every book or open-interest change): a prior-day last-trade time refused by the RECEIPT-day
    gate, a zero price beside a prior-day trade time, or a zero trade time with a zero price (a
    zero field beside a live one contradicts itself and is no proof). It moves only forward, and a receipt day
    before the fold watermark's (a replayed frame) is ignored; the watermark-day gate records
    no proof. `untraded_proof_holds(proof, trade, segment)` holds only when the first trade is
    the same IST day, at most `UNTRADED_PROOF_MAX_SKEW_SECS` (5 s) before the proof, and within
    `UNTRADED_PROOF_MAX_AGE_SECS` (60 s) of the proof, counted from 09:15 only where nothing can
    trade before it: options always (futures left the subscription 2026-09-18), equities only
    for a proof taken after the pre-open match (`PRE_OPEN_MATCH_DONE_SECS_OF_DAY_IST`, 09:12, plus
    the 5 s skew limit).
    So a socket that was down and reconnects with a morning's cumulative still seeds, and so
    does an equity whose 09:08 match packet was lost. A refused packet may take a slot to hold
    the proof, but only below the last 1/20 of the table (`UNTRADED_PROOF_SLOT_RESERVE_DIVISOR`,
    23,750 of 25,000, above the measured 22,996 peak), with no exhaustion count or log; it opens
    no bucket (the three tests that pinned "no slot" now pin "no bucket on any timeframe"). New
    counter `tv_aggregator_slot_volume_baseline_zero_total`.
  - Limitations (stated in the code): if the first trade's packet is lost and the next trade
    arrives inside 60 s, that bar carries both, so a 1 s / 3 s / 5 s bar can hold up to 60 s of
    volume after such a gap (before PR58 both were missing), and the window stretches by any
    read lag, since the proof is when we READ the packet. The option rule keys on the segment
    code, so if futures (which have a pre-open) return it must key on the instrument type. Nothing yet compares our first bar
    with Dhan's own chart; PR59's read-only query is the tool for that check on the live box.
  - Review 2026-10-01 (four parallel attack passes after merging main's plan 47): the
    lost-packet limit above was WORSE than stated, since the 60 s window crosses minute and
    higher bucket edges (a 1 m bar written at 150 against a true 50; an equity's whole auction
    in its 09:15 bars). Fixed: the zero baseline also needs the first trade's day cumulative
    to EQUAL its own last-trade quantity, so it never over-reports
    (`test_regression_a_lost_first_trade_never_lands_in_the_next_minute`,
    `test_regression_an_equity_auction_is_never_poured_into_the_open_bar`). No proof is
    recorded during a WAL replay: a replayed snapshot's slot got the hand-over gap and
    withheld every first bar (`test_regression_no_proof_is_recorded_during_a_wal_replay`).
    The 09:15 extension names its segments (`test_regression_a_currency_proof_is_never_extended_to_the_open`).
    Stated limits: the first bar's net direction is null; proof slots are bounded by the
    subscribed set, not the traded set.
  - Docs line (not a bug): candle `volume` is signed (negative on a down bar), while charting
    "Net Volume" is 0 on a flat bar and compares the first bar with the previous close. Say so
    where the candle columns are described.
- [ ] **PR59 — a read-only database query the owner can run on the live box.** (`api` or `app`,
  `.github/workflows/aws-control.yml`) Asked 2026-09-28 by the "Ticks vs Dhan chart mismatch"
  thread, to confirm PR58 on real rows (HDFCBANK-29Sep2026-780-CE `ticks` and `candles_5s`).
  - A `query` action on the existing read-only control workflow: main branch only, same
    concurrency group as `status`, no restart and no write.
  - The SQL is validated in Rust, not in shell (Rust-only rule): SELECT or WITH only, one
    statement (no `;`), a banned-keyword list (INSERT, UPDATE, DELETE, DROP, ALTER, TRUNCATE,
    CREATE, RENAME, COPY, BACKUP, SNAPSHOT, VACUUM, REINDEX, GRANT and the rest of QuestDB's
    write set), and `LIMIT 500` added when absent or capped when larger. The input travels
    base64-encoded and is never put into a shell command line.
  - Output is capped at 200 KB, written to the job summary and uploaded as an artifact.
  - Tests: every banned keyword refused (any case, inside comments and quoted names too), a
    second statement refused, the limit added and capped, a valid SELECT passed through.

Corrections and widenings to existing items:

- D3b: also the boot wait (ends 08:40, not 09:10: dhan_live_universe.rs:847, :975-989), and the
  page says "restart" until D3b makes a restart unnecessary.
- D3c: one failed index-member download must not reject the day's list
  (dhan_universe.rs:879, :965); make the 50 MiB / 60 s master cap and the 0.1% bad-row skip
  visible (constants.rs:1119, :1183); page when depth-200 dials nothing and `top_volume` stays
  empty; the 15:41 check verifies only the 4 dead ids on a collapsed day.
- D8: as written it keeps the dead SENSEX seed forever, because the master's index rows are
  NSE-only (dhan_universe.rs:564). Re-scope before starting.
- D1: verified, nothing in production calls `clear_order_halt` (engine.rs:457 has test callers
  only), so a latched halt clears only on restart; and rate_limiter.rs:234 says the daily reset
  clears the budget while the halt stays. Fix the comment and give the halt a production clear
  path (paper mode; `dry_run` is not touched).
- PR40b: the 7-day boot prune runs before the boot drain (main.rs:1819 vs the recovery further
  down), so after 7+ days off it deletes unreplayed candles; and `.bin.N` files in `archive/`
  and `replaying/` are never pruned or counted. Move the prune after recovery and match the
  suffixed names.
- PR40a follow-up (goes with PR31b): the crash marker is written clean before the escalation
  queue drains, so a kill in the shutdown wait loses up to 250,000 queued candles and the next
  boot reports none; a failed marker write is an uncoded warning.
- PR40d follow-up: the once-a-minute throttle is keyed on the time the fold passes in
  (seal_writer_runner.rs:691, fed from dhan_feed_stack.rs:9960), not the wall clock.
- PR31b: include row 253 (crash-lost seals; the warm-up gives no candle to buckets that ended
  before it starts).
- PR31c: there are ten candle tables, not nine; one keyed switch per family means a single
  top-volume column-repair refusal discards every board for the session; a refused candle while
  tables are unkeyed goes to spill but pages as a loss; nothing keeps paging while tables stay
  unkeyed.
- PR39: the error-code ratchet matches `code =` as a substring (error_code_tag_guard.rs:303,
  :309, :504), so two uncoded lines count as coded; name the seven uncoded loss warnings; the
  SPILL-RETENTION-01 bullet is obsolete (PR40b reused AGGREGATOR-DROP-01).
- PR46: the box role can also READ the console key and the GitHub token (main.tf:215-251);
  narrow read as well as write. "Confine the socket to one status call" is not possible on a
  Unix socket; name a privileged helper instead.
- PR50: the deploy start step needs the holiday check, and every deploy re-enables and restarts
  the app even during a console reset (deploy-aws.yml:1087-1106); deploys take the reset lock.
- PR19: the log tool's code search reads `~/.aws`, `~/.ssh` and `environ`
  (tickvault-logs-mcp/src/tools.rs:1683-1688); refuse out-of-repo paths. The second database
  server in `.mcp.json` and the unpinned session servers (row 86) go with it.
- The remaining per-area items in the gap file (ops/deploy/security, storage, feed, Dhan
  mismatches) are folded into the item each names when that item starts; any that names no
  item gets its own item before PR51.

### Added 2026-09-28 (re-check 7 coverage map: every open and never-O(1) row has an owner)

Source: `/mnt/project-files/audit/recheck7-plan-map.md`. It lists all 404 page rows; 83 were open
and 45 never O(1). Of those, 66 rows had no owner in this plan or only a partial one. Each is
assigned below. "Row N" is the page position, and "c6#N" is the re-check 6 number where one
exists. The file:line evidence is in the map and is re-verified when the owning item starts.

New items:

- [ ] **PR40d-f — sub-second inline seal waits page too.** (`storage`, deploy)
  - Row 111: a wait under 1 s never pages (seal_writer_runner.rs:279, :672-693). Page on the
    summed inline wait per window, or alarm the shipped inline-fallback count. This item also
    owns row 110 (the throttle is keyed on the fold's time instead of the wall clock).
- [ ] **PR40c-f — finish the batching and append-pause proofs.** (`storage`)
  - Row 266 (c6#235): count writes at the disk call, and test an append racing the rename
    (seal_spill.rs:878-918, :962-968).
- [ ] **PR40b-f — the spill prune never deletes unreplayed or refused candles.** (`storage`,
  `app`)
  - This item owns the PR40b corrections above: rows 187 (c6#168; `.overflow` as well as
    `.bin.N`) and 194 (exempt refused and poison files; move the prune after recovery).
  - Row 296: report staged-for-retry seals as pending rather than unrecovered, page once, and
    fix the wiring test that pins the over-count (seal_writer_loop.rs:321-331, :1709-1750).
- [ ] **OWNER-202 — exits refused at 25,000 tracked orders.** (decision only, no code)
  - Row 202 (c6#178): the order cap also refuses cancels (engine.rs:331-381, :2543-2593). The
    exit layer is frozen, so the owner is asked whether cancels may skip the cap. Nothing is built
    until the owner answers.

Rows folded into existing items (the fix is named here so the item carries it):

- D3c: row 136 (depth-20 index legs are dark on a collapsed day; give them a price source that
  does not depend on the main feed, or record that D3a/D3b remove the cause); row 137 (a
  depth-only hard stop is counted, and the planning-refused error gets its alarm field); row 153
  (ship `tv_dhan_live_universe_instruments` and chart it); row 66 (c6#66, the headroom warning
  blames option chains, so correct its text); row 124 (c6#120, skip the mid-session master
  rebuild when today's file is on disk).
- PR54: row 139 (stamp depth delivery before the disk-pressure shed decision); row 140 (check a
  slow morning's logs, and delay the steering heartbeat check until hand-off); row 318 (ship
  `tv_dhan_dial_incomplete_total` and `tv_depth_dial_refused_after_805_total`, or add their alarm
  field; the alarm half goes with PR39).
- PR24: row 121 (cap the spot store's held time at the wall clock); row 101 (c6#101, count
  contracts pushed off the 1 s board by broker clock skew, and tie the close to the measured
  skew; the board half goes with the PR4c follow-ups).
- PR32: row 169 (c6#150, split a refused batch so only the bad row is quarantined); row 176
  (c6#157, order spill files by sequence, not by name).
- PR31c: rows 172 (c6#153, top-volume boards get the spill tier during a DB outage), 193 (count
  the refused-candle retry window from the first refusal and fold the in-session replay into
  it), 196 (check live tables at boot and rebuild any keyless one), 224 (clear the keyed latch
  on a table-missing error), 272 (behaviour tests for PR31a's part-way rewind, the keyed latch
  after a refusal, and live plus replay on one key).
- PR31b: row 299 (mark an overrun exit so the next boot does not page it again); row 192 (write
  the crash marker from the escalation thread and code its failure).
- PR19: rows 177 (c6#158, remove the no-capture-number spill path), 225 (the "2 s lateness"
  note; the constant is 240 s), 273 (delete `depth20_layout` / `depth20_ranked_steer` or mark
  their CLAUDE.md rows dormant), 335 (log-tool code search reads regular files only, with a
  deadline), 336 (the log tool refuses a build older than the tree, or builds through cargo),
  340 (list the IP monitor as unused code, or route its exit through shutdown), 237 (the quote
  and stats notes as well as the board note), 370 (pin both npx servers and the CI action by
  commit), 86 (c6#86, remove the second database server or put it behind the SQL gate, and stop
  passing it to the unattended triage and mobile-command workflows), 270 (c6#239, the operator
  CLI tools' stale container name).
- PR38: rows 199 (c6#175, count and alarm updates for unknown orders), 204 (c6#180, halt trading
  when a fill cannot enter the risk book), 201 (c6#177, persist the day's order count and reset
  it only on the trading day), 210 (c6#186, paper order ids survive a restart).
- D1: rows 200 (c6#176, charge the order budget after the per-second limit and the breaker) and
  212 (order windows follow the wall clock; count a step).
- PR25: row 250 (delete or wire the main-feed address and socket-cap settings).
- PR37: row 341 (cap the 15:41 target list, or count data requests per day); row 116 (c6#113,
  log the first text body on a feed socket).
- PR51: row 274 (archive `cadence-error-codes.md`; fix the fresh-start reset view comment).
- PR39: rows 300 (alarm or raise the medium seal-on-disk code) and 301 (the coverage guard treats
  a source-scoped filter as covering that source only; PR51 fixes the runbook half).
- PR46: row 369 (scope the box role's log groups and metrics to the app's own).
- PR36 follow-up (goes with PR55): row 372 (delete the three lines that read a console key from
  the URL); row 403 (drop the public-address link from the 08:30 ping, or point it at the tunnel).
- PR55: row 373 (require up-to-date branches or a merge queue, and gate the push deploy on the
  merge commit's All Green); row 395 (pin `needs: preflight`, the output condition and the step
  order in `github_workflow_guard`); row 385 (c6#325, run the budget-latch shell and workflow
  legs against stubs).
- PR35: rows 398 (the watchdog skips while a deploy run is in flight), 399 (the docs-only path
  check also covers the watchdog and the after-close cron), 400 (dispatch the deploy from the
  merge, or alarm when the catch-up has not run for an hour), 401 (refuse a stop after a deploy
  that finishes just before 08:30), 402 (dispatch terraform when any commit since the last apply
  touched it).
- PR10: row 113 (c6#110, also the depth writer's ~10 name checks per row).
- PR43: row 127 (c6#123, also count and page the spots cut at the 250 cap).
- PR42: row 188 (c6#169, also the order-update and position-update event writers).
- PR41b: row 36 (c6#36, replay the dead-letter file mid-session, or record boot-only replay
  below). Decided 2026-10-01: boot-only, recorded in PR41b.
- PR7: rows 115 (c6#112, measure contention on the shared capture counter) and 267 (c6#236,
  time a full 250,000-record shutdown drain on the production volume).
- PR11: row 189 (c6#170, candle escalation and inline writes respect the free-space floor).
- PR18: row 213 (the paper reconcile keeps its sets between cycles, keyed with the segment).
- PR47: row 166 (c6#147, cap the alert tasks or record the bound from storm folding).
- D11: row 245 (c6#215, the Muhurat flag is re-read with the trading day).
- PR50: row 320 (the order-update socket's first dial checks the calendar).

Accepted limitations (each stays as it is; recorded so it is not re-found as an open gap):

| Row | Limitation | Why it stays |
|---|---|---|
| 14 (c6#14) | Order-book imbalance is O(levels), and its comment says O(1) | Indicator area is frozen (§28) and the code is dormant. The comment fix needs the owner's approval; the CLAUDE.md O(1) table gets a row with PR19. |
| 75 (c6#75) | A reconnect loses the ticks in the gap | The feed has no replay; bounded by the reconnect ladder (zero-loss charter §1). |
| 128 (c6#124) | A process alive the next day plans on yesterday's file | The box stops at 17:30 every weekday. |
| 165 (c6#146) | Subscribe pacing pauses the reader up to 25 ms per gap | Only at boot and top-up, which is Dhan's pacing rule. |
| 198 (c6#174) | Day volume resets only at shutdown | The box stops daily (PR23 covers the midnight case if it ever runs through). |
| 209 (c6#185) | Cancel or modify is refused when a budget tier is used up | Matches Dhan's documented limits. |
| 232 (c6#203) | One caller can use up the shared API rate limit | Documented at the site (public_guard.rs:61-96). |
| 234 (c6#205) | The failed-login log names a caller-written header | Documented at the site; log text only, never used to decide anything. |
| 313 (c6#268) | Codes 811-814 redial all day as routine drops | Accepted by the audit. PR37 decides whether to count them. |
| 314 (c6#269) | An 805 sent as a bare reset is not recognised | No disconnect code arrives in that case. D7 re-checks it with the 805 work. |
| 319 | A socket one 804 away from parking is not visible in the cloud | Shipping it needs a dated noise-lock quote from the owner. |
| 329 (c6#281) | A full disk blocks the console reset | The save-first rule is deliberate: nothing is erased before its copy is safe. |
| 337 (c6#286) | The October $150 stop depends on the cost service answering | Accepted by the audit. PR49 checks what else stops the box if the cost service is down. |
| 36 (c6#36) | The dead-letter file is replayed only at boot, unless PR41 lifts it | Recorded here until PR41 decides. |

The five never-O(1) rows that are real cost shapes (14, 115, 189, 213, 267) have owners above.
PR19 adds a CLAUDE.md O(1)-table row for each one not already listed.

Ticked items with rows still open: D3a (#1975) merged after the audited commit, so rows 149,
151 and 154 are re-checked on main when D3b starts. PR53 closes row 226 when it merges. PR36a's
row 371 waits on the owner typing "rotate". Row 339 is already owned by PR39; the audit's "no
plan item" is out of date.

### Added 2026-10-01 (reality check on main 6f0b6ca, 8 new problems), riskiest first

Source: the whole-system reality check of 2026-10-01 (artifact "Tickvault Reality Check"). None
of the eight was in this plan. Owner approval: "bro dont blcok anyhtign evrythign is good to goa
hea dude okay?" (2026-10-01 11:12 UTC), relayed with "fix the 8 new problems". Owned by the
reality-check thread; every other item in this plan stays with its own thread. Each R-item is
verified in source on `origin/main` before its PR; R1 and R2 ship together (two small,
independent live-path fixes).

- [x] **R1 — a bad day open, high, low or close no longer drops a good tick.** (`storage`)
  - Verified: `TickRow::from_parsed_tick` refused the whole row when any of LTP or the four day
    OHLC fields was non-finite (tick_persistence.rs:402-423), with an unthrottled `error!` per
    tick on the drain; every replay refused the row again, and the candle fold (which reads the
    LTP) still counted the tick, so `ticks` and `candles_*` disagreed.
  - Fix: only the LTP is mandatory. A non-finite day OHLC field becomes NULL through the existing
    optional-price path (`opt_price`, counted on `tv_tick_optional_price_dropped_total`, warn
    throttled to powers of two), exactly like the average price.
  - Tests: `a_non_finite_ltp_is_refused_not_emitted_as_a_poison_ilp_row`,
    `a_non_finite_day_ohlc_field_is_nulled_and_the_tick_is_kept`.
- [x] **R2 — a tick stamped later today than its receipt cannot freeze a price.** (`app`)
  - Verified: `SpotPriceStore::record` refused yesterday and tomorrow but stored a same-day
    future stamp (spot_price_store.rs:384-391); later-time-wins then refused every honest tick
    as `OlderThanHeld` until that time arrived, and the depth and contract selectors read the
    frozen price.
  - Fix: `record` takes the frame's receipt; a trade time more than
    `FUTURE_TRADE_TIME_SKEW_SECS` (5 s) ahead of the receipt is held at that ceiling and counted
    on `tv_spot_price_store_future_time_capped_total`. The price is kept (nothing is dropped);
    no receipt (`<= 0`) means no cap. O(1): one divide and one compare.
  - Tests: `a_trade_stamped_hours_ahead_of_its_receipt_cannot_freeze_the_price`,
    `a_trade_inside_the_skew_or_with_no_receipt_is_not_capped`,
    `trade_time_ceiling_is_the_receipt_in_ist_seconds_plus_the_skew`.
- [x] **R3 — a failed token renewal after an 807 pages at once.** (`app`, `core`)
  - Verified: on renewal failure the 807 path only `warn!`s (dhan_feed_stack.rs:13719-13727);
    the Critical page waits for the profile watchdog (~30 min).
  - Fix: `TokenManager::force_renewal_unless_replaced` sends the existing family (3)
    `AuthenticationFailed` page once per token generation (one atomic swap on
    `stale_credential_paged_generation`, so 16 sockets failing together send one page), and
    never for the mint-cooldown skip or the RESILIENCE-03 refusal, which already page. The
    app-side `warn!` is a coded `error!` throttled to powers of two. Both family (3) bodies now
    name the Dhan live feed sockets. No new Telegram family.
  - Tests: `test_stale_credential_failure_pages_only_on_terminal_failure_of_the_current_token`, `test_stale_credential_page_latch_fires_once_per_token_generation`, `test_force_renewal_unless_replaced_pages_family_3_once_per_token` (all in `token_manager.rs`).
- [x] **R4 — an order update the parser cannot read is flagged, not hidden.** (`core`)
  - Verified: a frame that fails to deserialise is counted as a non-order message at `debug!`
    (order_update_connection.rs:961-987), so a vendor format change would drop every order
    update silently. Paper mode only today.
  - Fix: a frame carrying the order envelope that fails the typed parse is counted on
    `tv_order_update_frames_dropped_total{reason="unparseable_order"}` and logged as
    ORDER-EVT-02 stage `typed_parse_failed`, throttled to powers of two, with the serde line
    and column and the existing client-id-redacted excerpt.
  - Tests: three in `order_update_connection.rs` (`looks_like_order_update` and the arm).
- [x] **R5 — the candle fold starts a clean day if the process runs past midnight.**
  (`app`, `trading`) Verified first; if the day-rollover path already handles it, the item
  closes with the evidence instead of a code change.
  - Verified: `force_seal_all` is the fold's only day reset and its only caller was the
    shutdown seal; the drain's midnight branch called only `reset_ranking_daily`. A next-day
    cumulative below yesterday's (and below the 2^31 restart floor) read as a stale packet, so
    the day's bars were written at volume 0.
  - Fix: `LiveIngest::roll_trading_day` runs the day-close seal, then the ranking reset; the
    midnight branch calls it. Seals the writer refuses are counted on the existing drop total,
    which the 30 s AGGREGATOR-DROP-01 report already pages. O(slots × TF_COUNT) once a day.
  - Tests: `roll_trading_day_reseeds_the_fold_so_the_next_day_counts_volume` (control without
    the roll reads 0, with it 300); the wiring guard
    `the_ranking_daily_reset_fires_only_on_a_real_midnight_crossing` now pins the roll call.
- [x] **R6 — a bar opens at its earliest trade, not its first arrival.** (`trading`) Verified
  first against the restart differential; ships only if the oracle and the replay rules agree.
  - Fix: `LiveCandleState::open_ts_ist_secs` (0 = pinned). The official day open and the
    repeat-quote day-open stamp pin the open; a trade open records its own second.
    `fold_in_bucket` and `fold_late_hlc` replace the open only with a strictly earlier trade,
    so the first arrival keeps it within one second. State 152 -> 160 B, `BufferedSeal`
    168 -> 176 B (now exactly at its assert), about +6 MB at the ceiling (`aws-budget.md`).
  - Tests: `open_is_the_earliest_trade_not_the_first_arrival`,
    `within_one_second_the_first_arrival_keeps_the_open`,
    `the_official_day_open_is_never_superseded`,
    `a_late_earlier_trade_amends_the_sealed_bars_open`, two proptests in `fold_properties.rs`;
    each fails with the guard removed. `restart_differential` and
    `first_trade_restart_differential` at 20,000 cases in release: 0 failures.
    `dhat_multi_tf_fold` passes. Bench gate not run locally.
- [x] **R7 — the instance lock re-reads after renewal and a machine that lost it stops
  dialling.** (`core`, `app`) SSM has no compare-and-set, so this narrows the window and makes
  the loss loud; it cannot close the race.
  - Fix: every renewal and stale-takeover write is read back and classified from the SSM
    versions (`classify_write`: held / held after contention / lost / inconclusive, the last
    never treated as held). A takeover settles 10 s before its read-back and is not trusted
    past a 5 s read-to-write window. Every Dhan socket sink carries the lock flag
    (`WalRingSink::with_dial_permit`); while the lock is not held a dial waits, counted on
    `tv_instance_lock_dial_refused_total` with one RESILIENCE-01 error per episode, and no live
    socket is closed. Runbook §3.6.
  - Tests: `classify_write_*`, renew/takeover read-back tests against the SSM stub,
    `with_dial_permit_follows_the_lock_flag_and_defaults_to_permitted`,
    `test_run_connection_waits_without_dialling_while_lock_not_held`.
  - Honest limit: a process that lost the lock does not re-acquire it without a restart.
- [x] **R8 — the CLAUDE.md speed table matches the code.** (docs) Five rows corrected after a
  re-check in source (`gainer_eligible`, `plan_depth20_ranked_minute`, `catch_up_seal_all`,
  `atm_pair_for`, `SpotPriceStore`), each as a dated note appended to its row.

R-items Z+ and guarantee matrix: covered by the shared matrix at the end of this plan. Tick
path: R1 removes four compares per tick; R2 adds one divide and one compare per spot tick; no
allocation in either (zero-alloc DHAT gates unchanged).

### Added 2026-10-02 (zero-loss audit on main c4e66b6), riskiest first

Owner: "see i dont want any ticks loss or single data loss". Every path a tick takes was traced
(socket, write-ahead log, database, candles, every delete). Done in this change:

- [x] **Z1 — frames read while a socket closes are kept.** (`core`) `await_close_handshake`
  discarded every frame Dhan delivered between our Close and its reply, uncounted, on every
  redial, rotation and park. Each data frame now reaches the sink through `close_capturing`
  (the only close the supervisor uses); counted on `tv_dhan_ws_close_drain_frames_total`.
  - Files: crates/core/src/websocket/connection.rs, crates/core/src/websocket/pool_supervisor.rs
  - Tests: `test_regression_the_close_handshake_hands_on_every_data_frame`,
    `test_regression_frames_read_during_close_reach_the_sink`,
    `every_production_close_passes_its_frames_to_the_sink`.
- [x] **Z2 — the write-ahead log syncs the last batch before a quiet spell.** (`storage`) Only a
  new record could trigger the rate-limited sync, so after a lull the last batch waited for the
  kernel's writeback. The idle arm now syncs an unsynced segment, at most once per interval.
  - Files: crates/storage/src/ws_frame_spill.rs
  - Tests: `test_regression_a_lull_syncs_the_last_batch`.
- [x] **Z3 — the candle INT self-heal drops only a table proven empty.** (`storage`) It ran every
  boot and dropped any candle table with an INT `security_id` without checking it was empty.
  - Files: crates/storage/src/shadow_persistence.rs
  - Tests: `test_regression_only_a_zero_count_proves_a_candle_table_empty`.
- [x] **Z4 — a same-day future trade time cannot open a future candle.** (`trading`, `app`) R2 capped
  the spot store; the fold had no cap. Refused (row kept) past `FOLD_FUTURE_TRADE_TIME_SKEW_SECS`
  = 60 s ahead of receipt.
  - Files: crates/trading/src/candles/multi_tf_aggregator.rs, crates/app/src/spot_price_store.rs
  - Tests: `test_regression_a_same_day_future_stamp_never_opens_a_future_bucket`,
    `test_a_same_day_stamp_within_the_skew_margin_still_folds`,
    `test_future_skew_never_exceeds_the_candle_fold`.
- [x] **Z5 — the error-log filter no longer turns on TRACE for the whole process; the catch-up
  stop reason says "clock" when time ran out.** (`app`)
  - Files: crates/app/src/log_coalescer.rs, crates/app/src/dhan_feed_stack.rs
  - Tests: `test_regression_combined_error_filter_keeps_the_error_level_hint`,
    `test_wal_catchup_lag_step_covers_every_permutation`.

Second round, 2026-10-02 (owner: "yes fix evrryhtign ddue okay?", then "approved entirley fully
auotmate ddue okay?"):

- [x] **Z6 — a replayed candle never replaces a fuller copy of its bar.** (`storage`, commit
  `ed4ded9fd`) The PR41a ledger is now one entry per bar (slot and bucket), holding the fullest copy
  on disk and the fullest committed; every rule is a max, so the result does not depend on the order
  copies reach the disk. Covers the four paths found: a later bucket spilled first, an original
  still in the escalation queue, a dead-lettered original, a parked file resumed after a later one.
  The boot drain keeps the fullest copy per bar across spill and DLQ. Pre-sized, never grows; at
  capacity it writes and counts (`kind=mirrored_overflow`, `untracked`, `boot_untracked`).
  **Honest limit:** a DLQ write does not consult the ledger before writing (it records after); a
  key the ledger could not track at capacity still replays in file order.
  - Files: crates/storage/src/seal_spill_ledger.rs, crates/storage/src/seal_spill.rs, crates/storage/src/seal_writer_task.rs, crates/storage/src/seal_writer_runner.rs, crates/storage/src/seal_absorption.rs, crates/storage/src/seal_writer_loop.rs
  - Tests: `test_regression_z6_later_bucket_spilled_does_not_hide_amend`,
    `test_regression_z6_original_in_escalation_queue_is_not_written_after_live_amend`,
    `test_regression_z6_dead_lettered_original_mirrors_live_amend`,
    `test_regression_z6_parked_file_resumed_after_later_bucket_skips_older_copy`,
    `test_regression_z6_boot_drain_keeps_fullest_across_spill_and_dlq_with_intervening_bucket`,
    `test_regression_z6_ledger_at_capacity_fails_toward_writing`.
- [x] **Z7 — a queued rescue batch holds the persisted watermark below it.** (`storage`, commit
  `b9ef33dda`) The drain holds a floor (fixed 8-slot CAS table per sink, no allocation, no lock)
  before handing a batch to the rescue thread, retracts it if the hand-off is refused, and the
  rescue thread releases it only after the spill is synced or the range is marked unapplied. A full
  table marks the range unapplied and counts `tv_wal_rescue_floor_full_total`.
  - Files: crates/storage/src/wal_applied_watermark.rs, crates/storage/src/tick_persistence.rs, crates/storage/src/depth_persistence.rs
  - Tests: `test_regression_tick_queued_rescue_holds_a_floor_until_the_rescue_thread_settles_it`,
    `test_regression_depth_queued_rescue_holds_a_floor_until_the_rescue_thread_settles_it`,
    `test_regression_tick_refused_rescue_hand_off_retracts_its_floor`,
    `test_regression_an_abandoned_rescue_survives_into_the_persisted_file`; DHAT
    `dhat_rescue_floor_zero_alloc` (CI storage DHAT step 3 → 4).
- [x] **Z8 — durability below the watermark.** (`storage`, commit `b9ef33dda`) (a) The writer-thread
  and rescue-thread spills `fdatasync` before a batch counts as rescued; a failed sync is a failed
  rescue. (b) QuestDB stays on its default commit mode (sync measured 6–8× slower per flush);
  instead the persisted watermark is the value acknowledged at least
  `WATERMARK_DURABILITY_LAG_SECS` (60 s) earlier. **Honest limits:** (b) relies on kernel writeback
  timing, not an fsync of QuestDB's files; the drain's own inline spill stays unsynced so the drain
  never waits on the disk; `confirm_replayed` archives on the acknowledgement.
  - Tests: `test_regression_persisted_watermark_lags_acks_by_the_durability_lag`,
    `test_regression_tick_rescue_sink_reports_a_failed_sync_as_a_failed_rescue`,
    `spill_rescue_sync_guard.rs::the_spill_helper_fdatasyncs`.
- [~] **Z9 — data with no stored copy is pruned.** PARTLY DONE.
  - 45f done (`7fae8d31b`): the cold bucket has versioning, no expiration, `raw-frames/` goes to
    DEEP_ARCHIVE. Test `test_terraform_cold_bucket_keeps_everything`.
  - 45e-1 done (`66c3f8cd2`, `25c78fd65`): every WAL prune (age, byte ceiling, 5% floor) deletes a
    segment only after a gzip copy is in `s3://<cold>/raw-frames/<IST date>/` and verified by
    HeadObject (size + SHA-256); refusals counted on `tv_wal_prune_refused_not_uploaded_total`.
    Tests `test_regression_age_prune_needs_a_matching_upload_marker`,
    `test_regression_byte_prune_refuses_segments_without_a_verified_copy`,
    `test_regression_floor_prune_needs_a_verified_copy_too`,
    `test_regression_size_mismatch_after_upload_writes_no_marker`.
  - 45g D7 (`9e2e86535`, a table with rows is renamed aside, never dropped), D8 (`8f4bcc857`, a
    retired table is dropped only when a count proves it empty), D10 (`a229cb398`, the console
    refuses every data-deleting action, `CONSOLE_DATA_WIPES_AUTHORIZED = false`) done.
  - Open: 45e-2 (offload / re-hydrate), the seal-spill and quarantine prunes (D5/D6), 45h.
  - **Trade-off chosen (default):** the prune gate fails CLOSED. If the bucket is unreachable for
    days, the WAL disk fills instead of deleting an uncopied segment. Turning it off needs
    `[raw_frame_archive] require_upload_before_prune = false`.
- [~] **Z10 — a full WAL disk can deadlock replay.** PARTLY DONE: the uploader runs every 2 minutes
  outside 09:00–15:40 IST and at once on disk pressure, so verified segments become prunable. Still
  open: if S3 is also unreachable, replay below 40 GiB free still waits (45e-2).
- [x] **Z11 — smaller loss paths.**
  - [x] (a) WAL replay resyncs past a bad record instead of abandoning the rest of the segment
    (`9714b4de2`, `65c306dbb`): `decode_record_at`, resync ceiling `WAL_RESYNC_MAX_FRAME_BYTES`
    (4 MiB, const-asserted against every WAL-bound frame cap), skipped bytes counted and logged
    (WS-SPILL-02 `source="mid_segment_resync"`). Tests
    `test_regression_resync_recovers_records_after_a_mid_segment_crc_flip`,
    `test_regression_corrupt_length_past_eof_is_counted_not_a_silent_tail`.
  - [x] (b) the panic hook gives the WAL writer up to 2 s to flush everything queued before the abort
    (`99e2f0456`, `a8fc3b9be`; tests `the_panic_hook_drains_the_wal_before_it_aborts`,
    `test_regression_drain_for_abort_returns_once_the_queue_is_empty`). **Honest limit:** a panic on
    the WAL writer thread itself still loses its queue, and an out-of-memory kill runs no hook.
  - [x] (c) a frame refused by the WebSocket size cap logs a coded `error!` with
    `source="frame_oversize"` and the cap, throttled to powers of two per endpoint; no new alarm
    (`dcb26b1bf`; `test_regression_oversize_refusal_logs_error_with_source_frame_oversize`).
  - [x] (d) shutdown closes the feed sockets first (up to 5 s; frames read during the close still
    reach the WAL), then the lane, seals and WAL; records left in the writer's channel at exit are
    counted (`0574db4b6`; `shutdown_closes_the_sockets_before_the_lane_and_the_wal`,
    `test_regression_run_connection_parks_with_shutdown_and_captures_close_frames`,
    `test_regression_records_enqueued_after_writer_exit_are_counted_not_silent`). Default chosen: a
    shutdown park skips the park counter and the "parked permanently" error (logged at info), so the
    `dhan-socket-parked` alarm does not page on every stop or deploy; the alarm itself is unchanged.
    Stop budgets now sum to 125 s against `TimeoutStopSec=145` (the guard's 20 s floor).
- [x] **Waste found by the sweep, fixed.** (`app`, `core`, `api`) The per-minute depth steering no
  longer loads ~22,000 candidates and the movers every minute (`3b1adb524`; `plan_minute`,
  `top_mover_pick` deleted; guard `the_steering_loop_runs_no_per_minute_candidate_or_movers_load`);
  `PoolSupervisor::poll_all` (no caller) deleted; `/api/quote` uses the shared client, keys its cache
  on `(security_id, segment)`, answers 409 on an ambiguous id (`c4e33f325`); `/api/stats` and the board
  also reuse the shared client with a 3 s per-query timeout (`c3a3604ff`; tests
  `test_stats_uses_the_shared_client_and_builds_none`, `test_board_uses_the_shared_client_and_builds_none`).
- [x] **Z12 — CLAUDE.md speed table rows.** Added 2026-10-02: `classify_frame`, `blocking_flush` /
  `append_inline_depth`, `rebuild_pending_paper` / `active_order_count`, plus rows for the new
  rescue floors and `DurabilityLag`, `raw_frame_upload::run_pass`, `resync_from`; the PR41a ledger and
  `SpotPriceStore` rows corrected. Original finding: `connection.rs::classify_frame` is O(packets) on the
  socket read task; `blocking_flush` runs `block_in_place` on every flush. The 2026-10-02 workspace
  sweep adds: `append_inline_depth` writes 10 rows per full packet; per order, `rebuild_pending_paper`
  and `active_order_count` are O(orders) on every order event; the per-minute depth steering builds
  ~22,000 candidate rows and two database queries used only for a log count (their consumers
  `plan_minute` and `top_mover_pick` have no production caller); `/api/quote` builds a new HTTP
  client per request. None is per tick. Full list: the 2026-10-02 audit page.

Z-items Z+ and guarantee matrix: covered by the shared matrix at the end of this plan. Tick path:
Z4 adds one add and one compare per tick; Z1 runs only on the close path; Z2 adds one flag test per
idle poll. No allocation on the tick path.

### Added 2026-10-02 (round 3: remaining loss paths, never-blocks, owner's 06:32 and 08:00 asks)

- [x] **R3-1 — WAL sync off the writer thread.** `ws_frame_spill.rs`: `wal-syncer` thread; the
  writer only flags a due sync. Test `test_writer_keeps_writing_while_sync_is_wedged` (bite-proved),
  plus `tv_wal_fsync_backlog` / `_pending_ms` / `_inline_fallback_total`.
- [x] **R3-2 — Failed flush/write counts its buffered records as lost.** `UnflushedTally`,
  `tv_ws_frame_spill_unflushed_lost_total`, WS-SPILL-02. Test
  `test_failed_segment_writer_counts_its_buffered_records_as_lost`.
- [x] **R3-3 — Backward wall-clock step re-anchors the receipt clock.** Test
  `test_backward_wall_step_reanchors_instead_of_freezing`.
- [x] **R3-4 — Archive drop waits for applied WAL** (`partition_archive.rs`, `writerTxn ==
  sequencerTxn` before export and before drop, fail closed, `STORAGE-GAP-04`).
- [x] **R3-5 (D5/D6 part 1) — Spill and quarantine prunes gated on a verified S3 copy;**
  quarantine never overwrites (`raw_frame_upload.rs`, `seal_spill.rs`, `tick_persistence.rs`,
  `tick_spill_replay.rs`). Part 2 (rehydrate for the after-close pass) stays OPEN under 45e-2.
- [x] **R3-6 (45h) — Unstored packet classes persisted:** `ticks.oi_day_high/low`, new
  `feed_aux_packets` table with `feed` in the DEDUP key (`feed_aux_persistence.rs`).
- [x] **R3-7 (D7, main feed only) — 805 overflow probe** in `pool_supervisor.rs`; ROTATION_HALTED is
  never cleared (source-checked); depth sockets stay parked pending an owner decision.
- [x] **R3-8 — `feed_gap_audit` table** (`feed_gap_audit_persistence.rs`, `ws_audit_consumer.rs`);
  `ws_event_audit` carries the real close code, `down_secs` and attempts.
- [x] **R3-9 — WAL-refused frames are not treated as WAL-backed** (`CapturedFrame.wal_backed`).
- [x] **R3-10 — Kernel receive-queue sampler** (`kernel_rx_queue_sampler.rs`), WS-GAP-03 log on a
  sustained backlog; no alarm (needs a dated quote).
- [x] **R3-11 — Never-blocks:** dedicated reader runtime (`reader_runtime.rs`,
  `TICKVAULT_WS_READER_THREADS`, 0 = rollback), Prometheus-only telemetry
  (`hot_path_telemetry.rs`), ratchet `crates/common/tests/hot_path_no_blocking_guard.rs`
  (bite-proved). CPU pinning NOT added: needs `libc` as a direct dependency (owner approval).
- [x] **R3-12 (D6d) — `scripts/ensure-questdb.sh` replaced by `tickvault ensure-questdb`.**
  Delivered by PR #2005, folded into #2004 on 2026-10-03 (the earlier copy here was reverted
  first so only #2005's version lands).
- [ ] **R3-13 (D11) — Special sessions (Muhurat).** Built inert on `wip/d11`, NOT merged: needs the
  owner to confirm date, hours and cost, and a compile + test run.
- [x] **R3-14 — WAL segment names from a monotonic source** (replay order across a clock step). **Done 2026-10-04** (`ws_frame_spill.rs::next_segment_name_nanos`): a new segment is named `max(wall nanos, highest name in the directory + 1)`, the highest seeded once per directory from the live, `replaying/` and `archive/` names, so names only rise across a backward clock step or a restart; a clamped name is counted on `tv_wal_segment_name_clamped_total`. Tests: `test_regression_segment_names_keep_rising_across_a_backward_clock_step`, `test_regression_segment_names_seed_past_every_name_on_disk_after_a_restart` (both fail with the clamp removed), `test_segment_name_nanos_parses_only_segment_names`. Storage lib 1,763 passed, integration tests all passed.
- [x] **L10 (audit thread, 2026-10-04) — the boot sequence probe trusted a corrupt record.**
  **Done 2026-10-04** (`ws_frame_spill.rs::highest_frame_seq_in_segment`). Two defects, both
  fixed: (1) the record giving the highest header sequence is now read whole and its CRC
  checked; if it fails, the segment is re-walked verifying every record
  (`tv_wal_seq_probe_corrupt_record_total`), so a flipped bit can no longer seed `capture_seq`
  near the top of its range; (2) found while testing: the header walk's seek counted the CRC
  twice, so every segment yielded only its FIRST record's sequence and the restart seed sat up to
  one segment below the true high-water mark. Tests:
  `test_regression_high_water_probe_reads_every_record_of_a_segment` (fails on the old seek),
  `test_regression_a_corrupt_sequence_never_seeds_the_counter` (fails with the CRC check removed),
  `test_first_frame_seq_refuses_a_corrupt_first_record`.

### Added 2026-10-02 (round 4: owner approved decisions 2 to 5 and in-place resubscribe)

Operator 2026-10-02: "go ahea ddude" / "dont b;ock go ahea ddude" (11:51), "what happend to
unsusbcribe resubscribe fucntionality as well dude can you add this alsod due okay?" (11:59) and
"go ahead approved everyhtign dude okay?" (12:40, naming the stall alarm and the libc dependency).
Each is recorded first in its rule file.

- [x] **R4-1 (decision 2) — Depth sockets recover on their own after 805.** `pool_supervisor.rs`
  depth overflow episode (one probe process-wide, doubling wait 5 to 30 min, at most 6 probes),
  `ROTATION_HALTED` never cleared; scope lock 2026-10-02 section. 9 tests incl. a 50,000-step
  random driver; `tv_dhan_ws_depth_overflow_probe_total{outcome}`.
- [x] **R4-2 (decision 5) — Backup copy of the top 1,000 contracts on the spot main-feed socket**
  (`main_feed_backup.rs`, `[dhan_universe] backup_top_n`, 0 disables). First copy wins at the drain;
  the WAL keeps both. Free-slot count (~2,662) is derived, not re-measured. Scope lock section.
- [x] **R4-3 — In-place unsubscribe/resubscribe on every socket kind** (`LiveSubscriptionCommand::
  Resubscribe`, unsubscribe batches first, per-socket caps refused and counted). Includes PR #1994
  (depth-200 swap and ghost resend in place). No production sender yet: no live policy removes
  instruments mid-session. Scope lock 2026-10-02 section.
- [x] **R4-4 (decision 4) — Socket reader threads pinned to their own core** (`libc =0.2.185`,
  `TICKVAULT_WS_READER_CORE`, default core 1 when allowed, never core 0, `tv_ws_reader_pinned_core`).
  Benefit not measured.
- [x] **R4-5 (decision 3) — Phone page HOT-PATH-STALL-01** when a hot task stalls 2 s in session
  (`hot_path_telemetry.rs` StallAlarm, CloudWatch filter + alarm, noise lock 2.8). Cannot fire
  if the whole process freezes.
- [x] **R4-6 — Attack-pass fixes on round 4** (1 high, 3 medium, 4 low):
  WAL replay dedups backup copies against the persisted set (`main_feed_backup.rs`
  `write_backup_set` / `ReplayBackup`, `dhan_feed_stack.rs` `refold_wal_frames`); the set is
  published before the Extend; the dedup table is built off the drain and adopted by pointer
  swap; same-socket repeats kept, newest trade time never goes back; stall page quiet after
  shutdown starts (`hot_path_telemetry::begin_shutdown`); an 805 episode with nothing parked
  finishes after its wait (`pool_supervisor.rs` `OverflowEpisode::poll`); only reader workers
  are pinned, helper threads restored (`reader_runtime.rs` `build_reader_runtime`); kernel
  queue sampler reads `/proc` on the blocking pool; CLAUDE.md complexity rows for 9 structures.
  Tests: test_regression_an_episode_with_nothing_parked_recovers_after_its_wait,
  test_regression_build_reader_runtime_leaves_blocking_threads_unpinned,
  test_regression_begin_shutdown_and_is_shutting_down_latch_the_alarm_quiet,
  test_regression_replay_unknown_socket_drops_the_second_copy,
  test_regression_refold_wal_frames_folds_one_copy_of_a_backup_packet,
  test_regression_subscribe_main_feed_backup_publishes_before_the_extend,
  test_regression_publish_backup_set_builds_off_the_drain_and_adopt_swaps,
  test_regression_same_socket_identical_repeat_is_kept,
  test_regression_newest_ltt_never_goes_backwards.
- [x] **R5 — Round-5 attack-pass fixes** (2 medium, 4 low; 2026-10-02):
  F1 the persisted backup set is a bounded history of publications and the replay uses the one
  in force at each frame (`main_feed_backup.rs` `PersistedBackupHistory`, `ReplayBackup::select`);
  F2 a new publication carries dedup state for contracts that stay (`BackupDedup::swap_in`);
  F3 the main-feed widen flag is published only under the lock and an unprocessed 805 forces no
  (`pool_supervisor.rs` `OVERFLOW_WIDEN_STATE`); S1 the boot drain's older-copy guard survives a
  stopped drain (`seal_writer_task.rs` `drain_recovered_seals`, `seal_spill_ledger.rs`
  `BootWritten`); S2 a verified upload whose marker write failed still satisfies the prune
  (`raw_frame_upload.rs`); S3 the frame sequence is seeded above the persisted applied watermark
  (`ws_frame_spill.rs` `seed_frame_seq_from_disk`, `wal_applied_watermark.rs`).
  Tests: test_regression_805_never_published_over_by_a_stale_step,
  test_regression_persisted_history_keeps_earlier_publications_bounded,
  test_regression_replay_uses_the_publication_in_force_at_each_frame,
  test_regression_replay_stops_a_publication_at_a_later_process_start,
  test_regression_replay_switch_carries_state_like_the_live_adopt,
  test_regression_adopt_carries_state_for_contracts_that_stay_in_the_set,
  test_regression_s1_older_copy_left_staged_by_a_stopped_drain_is_refused_next_boot,
  test_regression_s1_summary_survives_two_stopped_drains,
  test_regression_s1_summary_is_bounded_and_counts_what_it_drops,
  test_regression_s2_verified_upload_with_failed_marker_write_satisfies_the_prune,
  test_regression_s2_unverified_file_is_never_covered_and_a_delete_forgets_the_record,
  test_regression_s3_persisted_high_water_reads_only_a_valid_own_file,
  test_regression_s3_reseed_clears_the_persisted_applied_watermark_with_no_segments.

R3 Z+ and guarantee matrix: covered by the shared matrix at the end of this plan. Tick path adds
one histogram bucket update per frame (R3-11) and one bool per frame (R3-9); no allocation by
construction (an allocation test for the telemetry is still open).

### Added 2026-10-03 (health check on main 2fabc2e), riskiest first

Operator 2026-10-03 09:28 UTC: "go", on the offered order: restart data-loss fix, then the token
write, then candle warm-up (PR31b-2 (a), already listed above). Each fix ships as its own PR.

- [x] **H1 — A catch-up drain that stops early no longer lets its leftover backlog be archived
  unread.** The live lane's acks lifted the applied watermark past the segments the drain left,
  and the next boot's replay skipped them as applied. The not-drained arm now marks
  `[lowest waiting first seq, ceiling − 1]` unapplied and persists it before the live ring exists
  (`ws_frame_spill.rs` `guard_pending_backlog`, `dhan_feed_stack.rs`). O(waiting segments) header
  reads, once per boot, cold. Tests:
  test_regression_leftover_backlog_is_replayed_after_live_acks_pass_it,
  test_guard_pending_backlog_ignores_segments_at_or_above_the_ceiling,
  an_unfinished_catchup_guards_its_leftover_backlog_before_the_live_drain.
- [x] **H2 — Token renewal no longer writes the token cache file on the socket reader worker.**
  After an 807/809 renewal, `token_cache::save_token_cache` (a sync write and fsync) runs on the
  single `tv-ws-reader` worker, so a slow disk stalls every socket. Move the write to the blocking
  pool. Files: `crates/core/src/auth/token_manager.rs`, `crates/core/src/auth/token_cache.rs`.
  Done: `save_current_token_to_cache` hands the write to `spawn_blocking` (`offload_blocking`),
  inline only when no runtime exists; `TOKEN_CACHE_WRITE_LOCK` keeps two writes in order and each
  reads the newest token at write time. `token_cache.rs` needed no change. Tests:
  test_regression_h2_token_cache_write_never_runs_on_the_calling_worker,
  test_offload_blocking_without_a_runtime_runs_inline,
  test_regression_h2_cache_save_goes_through_the_offload.
- [x] **H3 — Deploy security (PR36c; owner 2026-10-03: "security yes and deploy yes").** A pushed
  v*.*.* tag passes the same All Green gate as a manual deploy and must name a commit already on
  main; a pull request's terraform plan job holds no AWS credentials (fmt + offline validate only;
  the live plan still runs on the push to main); SSH has no rule unless the `TF_VAR_OPERATOR_CIDR`
  secret is set, and `emergency-fs-recover.yml` opens 22 to its own runner only for the run. Files:
  `.github/workflows/{deploy-aws,terraform-apply,emergency-fs-recover}.yml`,
  `deploy/aws/terraform/{main,variables}.tf`. Test: r21_manual_deploy_needs_main_and_all_green.
  Honest limit: the long-lived keys stay repository secrets until the owner moves them into the
  `prod` environment.
- [x] **H4 — The live feed never waits on a slow disk for its own backup write.** Unbacked tick and
  depth rows with a busy rescue thread are parked in a bounded in-memory queue (8 batches, 128 MiB
  per sink), retried on every flush and rescue, handed to the rescue thread at shutdown, written
  inline only past the bound, counted (`tv_{tick,depth}_rescue_parked_total`, `_parked_bytes`).
  Seals already park in the 250,000-deep escalation queue; unchanged. Files:
  `crates/storage/src/{tick,depth}_persistence.rs`, `crates/storage/tests/spill_rescue_sync_guard.rs`.
  Tests: test_regression_h4_a_parked_rescue_is_handed_on_by_the_next_flush,
  test_regression_h4_closing_the_rescue_queue_hands_on_the_park,
  test_regression_h4_a_parked_rescue_survives_a_dead_rescue_thread,
  test_regression_h4_wal_backed_rows_are_deferred_not_parked,
  test_regression_h4_the_depth_park_is_handed_on_or_written_never_dropped,
  the_drain_parks_a_refused_rescue_and_never_waits_to_retry_it. Honest limit: a parked batch is
  lost on a crash, where the old unsynced inline write would have kept it.
- [x] **R1 — The startup replay waits for the database instead of re-deferring its own rows.**
  Measured 3 Oct 2026 (deploys 962-964 and Friday's): every boot re-folded the same 173 WAL
  segments (1,477,447 frames, 14.2M depth rows) faster than QuestDB absorbs them, so the producers
  hit their retention bound and rescued ~27.7M rows back to the WAL as unapplied ranges, the 30 s
  ack wait timed out, and 168 segments were left for the next boot: the backlog never shrank and
  each boot logged ~10,000 coded lines. Outside the capture window the boot pass and each catch-up
  round now pace every size-triggered flush on the writer threads (wait for a drain, flush again
  only after a drain), until the catch-up budget's wall clock; inside it they behave as before, so
  the sockets dial as soon as they did. Counted on `tv_wal_replay_pace_waits_total`, logged as
  `pace_waits`. Files: `crates/app/src/dhan_feed_stack.rs`,
  `crates/app/src/dhan_feed_stack/feed_aux_tests.rs`,
  `crates/app/tests/wal_applied_watermark_wiring_guard.rs`. Tests:
  test_pace_after_replay_flush_waits_for_the_writers_and_lands_its_rows,
  an_unpaced_replay_flushes_once_and_never_waits,
  a_paced_replay_stops_waiting_at_its_deadline_without_a_flush,
  a_paced_replay_past_its_deadline_behaves_as_before,
  test_wal_replay_pace_until_is_none_inside_the_capture_window, both_replay_passes_are_paced. Honest
  limits: past the deadline, or inside the capture window, a replay still rescues as before; and
  an after-hours deploy that stops the box within a minute of the replay persists little progress,
  because the applied watermark is written 60 s behind the acks.
- [x] **S1 — The drain's flushes no longer hand over their worker.** The frame drain called
  every tick and depth flush inside `block_in_place` (about five a second) although the flush is
  a `try_send` to the writer thread. The writers now move the worker aside themselves at the only
  blocking steps (synchronous ILP round trip, inline spill write) via the crate-internal
  `off_worker`; the drain calls `flush` bare. Files: `crates/storage/src/off_worker.rs`,
  `crates/storage/src/{tick,depth}_persistence.rs`, `crates/storage/tests/spill_rescue_sync_guard.rs`,
  `crates/app/src/dhan_feed_stack.rs`. Tests: every_blocking_writer_step_runs_off_the_worker,
  a_nested_call_runs_inline_and_does_not_panic, runs_inline_on_a_current_thread_runtime_without_panicking,
  test_drain_never_flushes_bare_on_the_async_worker. Measured: block_in_place p50 191 ns / p99
  309 ns uncontended (debug), bare call 30 / 41 ns.
- [x] **S2 — Disk probes, audit flushes and the error summary no longer block a shared worker.**
  The disk-health watcher (`df`-style statvfs), the resource monitor (fd count, RSS, memory
  ceiling, spill free space from `/proc`), the WAL auto-resume free-space probe and the hourly
  error-summary rewrite ran as plain blocking calls on tokio workers; the WebSocket audit
  consumer flushed its ILP writer bare. Each now runs in `spawn_blocking` (probes, summary) or
  through `blocking_flush` (audit), and the summary skips files last written before its window
  instead of re-reading the whole directory. Files: `crates/storage/src/{disk_health_watcher,
  resource_monitor,wal_suspension_watcher}.rs`, `crates/core/src/notification/summary_writer.rs`,
  `crates/app/src/{ws_audit_consumer,order_observability}.rs`. Tests:
  test_audit_consumers_never_flush_bare_on_the_worker,
  regenerate_summary_skips_a_file_last_written_before_the_window.
- [x] **S3 — Spill replay, retention sweeps and the upload listing no longer block a shared worker.**
  The tick spill replay read 8 MiB chunks and trimmed its quarantine with `std::fs` on a tokio
  worker; the retention loop in `main.rs` ran the WAL archive and active prunes, the spill sweep
  and the DLQ size reading inline; the raw-frame upload listed both directories (one stat and one
  marker read per file) inline. The replay round now runs on the blocking pool (its HTTP posts
  still run on the runtime via `Handle::block_on`), the retention pass runs in one
  `spawn_blocking`, and the listings run through `off_worker`. Files:
  `crates/storage/src/{tick_spill_replay,raw_frame_upload}.rs`, `crates/app/src/main.rs`,
  `crates/app/tests/disk_retention_wiring_guard.rs`. Tests:
  the_drain_round_runs_on_the_blocking_pool, test_retention_sweeps_run_on_the_blocking_pool,
  every_listing_runs_off_the_worker. Honest limit: the work is still O(files) per pass; it no
  longer holds a worker the drain and readers share.
- [x] **S4 — API debug scans and the feed-state write no longer block a runtime worker.**
  `/api/debug/logs/jsonl/latest`, `/api/debug/spill/status` and `/api/debug/cross-verify/latest`
  listed directories with `std::fs` on the request worker and sorted every match to pick the
  newest; the feed toggle wrote and fsynced `data/feed-state.json` inline. The scans and the
  write now run on the blocking pool, the newest-file pick is one pass keeping the greatest name
  (O(files), no list, no sort), and a failed join reads as a failed write. Files:
  `crates/api/src/handlers/{debug,feeds}.rs`. Test:
  every_handler_scan_runs_on_the_blocking_pool.
- [x] **S5 — The loops that keep running through the session no longer block a shared worker.**
  A read-only audit of every production path left 14 sites that did disk, child-process or ILP
  work bare on a tokio worker. Each now runs through `tickvault_storage::off_worker::off_worker`
  (made public for this) or `spawn_blocking`: the disk-pressure `df` probe (60 s); the depth
  attach retry loop's artifact, symbol-map, spot-list and seed reads (15 s / 60 s); the
  held-today file write (per minute, on growth) and read; the close-of-session seed write and the
  probe day latch; the audit spill drain's directory scans (60 s per table); the hourly log
  retention sweeps and the `errors.log` cap; the OOM cgroup read (60 s); the partition export's
  file steps, per-chunk gzip writes, `df` probe and audit flush (disk pressure can run it
  mid-session); the stale-artifact sweep; the post-close ILP flushes (cross-verify, timeframe
  consistency, scoreboard, connection and table-storage rollups) and the daily markers; the
  `/board` RSS read. Files: `crates/storage/src/{lib,off_worker,audit_spill,oom_monitor,
  partition_archive}.rs`, `crates/app/src/{main,disk_pressure_boot,dhan_contract_universe,
  depth20_static,depth_seed,depth_subscription_view,depth_rebalance,dhan_universe,
  daily_task_marker,dhan_live_crossverify_boot,tf_consistency_boot,feed_scoreboard_boot,
  ws_connection_rollup,table_storage_rollup}.rs`, `crates/api/src/handlers/board.rs`,
  `crates/app/tests/off_worker_sweep_guard.rs`. Tests:
  every_session_loop_step_runs_off_the_worker, the_guard_bites_on_a_bare_call. Honest limits:
  each step still costs what it did; only the worker it holds changes. Boot-only and shutdown-only
  steps (the WAL replay, the mapping-artifact wait, shutdown joins) are left as they are.

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

### Reconciled 2026-10-04 against main `e461c3995` (every unticked item re-read in source)

Four read-only checks re-read each open item against the code on main. No open item was fully
done except the D6b/c/d boxes ticked above, and D10, which the in-place swap made obsolete.
Status of the rest, so the next session does not re-audit:

| State | Items |
|---|---|
| Partly done (remaining work named in each item) | PR5, PR8 (shutdown/boot `blocking_flush`, seal-writer cycle, order observability flushes still `block_in_place`), D2 (waits on PR12), D3 (D3a/b/c-1 done; D3c-2/3 open), D5 (waits on PR11), D6 (D6e onward), PR16 (`df` fork with no timeout, drain still on the shared runtime, boot seal drain bare), PR17 (seal DLQ not synced; torn-line and seal-spill sync done 2026-10-04), PR18, PR19, D7 (808 policy open), D9 (D9b-3 open), PR24, PR28b (owner lock mode), PR31c, PR32 (hour-boundary late append in tick spill replay; unapplied-table overflow uncounted; archive blind to capture-log deferrals), PR33, PR36 (SSH done), PR39, PR42 (42a/42b done; 42c owner; order/position update event writers have no spill tier), PR55 (manual and tag deploys gated; input-in-shell and branch checks open), PR56 (alarm open), PR40b-f (S3 copy gate mitigates; `.bin.N` / `.overflow` never matched; boot prune still runs before the boot drain) |
| Open, nothing built | PR6, PR7, PR9, PR10, PR11, PR12, PR13, PR14, D1, D4, D8, PR23, PR25, PR26, PR27, PR34, PR35, PR37, PR38, PR43, PR44, PR45, PR46, PR47, PR49, PR50, PR51, PR52, PR54, PR57, PR59, PR40c-f, PR40d-f |
| Waiting on the owner | OWNER-202, PR42c, PR28b lock mode, D11/R3-13 (no `wip/d11` branch exists any more; the box curfew still blocks Sundays; the session is 2026-11-08) |
| Dormant | PR48 (console wipes switched off by `CONSOLE_DATA_WIPES_AUTHORIZED = false`) |
| Done in a later fold | PR40a follow-up (`escalation_pending` counted in `unwritten_seals`) |
