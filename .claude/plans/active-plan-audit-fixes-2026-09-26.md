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
- [ ] **PR8 — `block_in_place` on shared runtimes.** (`app`, `storage`)
  - Correction: no per-tick dynamic metric label exists on the drain (checked 7403-8285; all
    handles pre-built), so the "label cache" half is closed with no change.
  - Six `block_in_place` sites (dhan_universe.rs:1040, order_update_events_boot.rs:113,
    dhan_feed_stack.rs:5843, order_observability.rs:488, dhan_order_push_observability.rs:185,
    seal_writer_loop.rs:398). Each is moved to its own thread or `spawn_blocking` where it runs on
    the shared multi-thread runtime; the drain-side one (5843) is reduced to the shutdown path only.
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
- [ ] **D4 — stale-price gate on entries.** (`trading` risk, not strategy)
  - No price-age check exists (risk/engine.rs:262). Add one to `check_order_in_segment`: an ENTRY
    whose last price is older than 5 s is refused with a coded reason; exits are never gated.
- [ ] **D5 — headroom alarms.** Already present for disk (used %, fill rate) and host memory
  (app-alarms.tf:535). Only the ballast-released signal from PR11 is new; it rides that PR.
- [ ] **D6 — the two remaining shell scripts become Rust.** (`app`)
  - `deploy/aws/holiday-gate.sh` (132 lines, tickvault-holiday-gate.service:36) and
    `scripts/ensure-questdb.sh` (230 lines, tickvault.service:106 `ExecStartPre=-`) become
    subcommands of the app binary; the unit files call them; `rust_only_guard.rs` allowlist shrinks.


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
  - Token-failure `error!` lines carry no error code (token_manager.rs:1436, :1464).
  - Quote endpoint caches and queries by id without segment and builds an HTTP client per miss
    (api/src/handlers/quote.rs:71, :84, :91, :140-145; response_cache.rs:176-180).
  - Log-query tool runs any SQL (tickvault-logs-mcp/src/tools.rs:669-684): read-only statements only.
  - The benchmark gate does not block merges (bench.yml:56-59): make its regression fail the run.
  - Stale restart-limit comments (deploy/systemd/tickvault.service:120-121, :333).
- [ ] **D7 — every socket refused at once (805, second login).** (`core`) All sockets park together
  and D3 has no spare to move to (pool_supervisor.rs:738-748, :1786-1832). Owner asked
  2026-09-26; the recommended option is: wait 5 minutes, redial one socket as a test, bring the
  rest back only if Dhan accepts it, critical alert either way. Until the owner picks another option, that default is what gets built.
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
- [ ] **PR26 — the token minter retries at most twice in total.** (`aws-lambdas`)
  - The AWS SDK adds platform retries (up to 6 attempts) under the §10.8 cap of two TOTP
    attempts. Set the SDK retry config so the whole mint is two attempts, pinned by a test.
- [ ] **PR27 — the box reads the token instead of minting it.** (`core`, `app`)
  - The box still mints at boot while the Lambda also mints (§10.3), so a box running across
    06:05 IST can clash with the Lambda. Switch the box to READ `/dhan/access-token`, keeping a
    mint only as a loud, coded last resort if the parameter is missing or expired.
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
wipe; split 2026-09-27 into PR28a, the console, and PR28b, the locked cloud copy) → PR29 (log tool can change the live database) → PR30 (budget stop undone the same day) →
PR31 (a restart can overwrite fuller candles) → PR32 (tick rescue and spill ordering) → PR33
(token and socket gaps) → PR34 (hung app never restarted) → PR35 (deploys) → PR36 (security) →
PR37 (Dhan documentation mismatches) → PR38 (risk book across a restart) → PR39 (every error
line coded, every loss counted and shipped), then the remaining order from the fourth re-check
(PR16, PR17, PR23, PR24, PR5–PR14, PR25–PR27, PR18, PR19, decisions). One PR open at a time.

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
  - Follow-ups found by the reviews, not fixed here: (a) the SQL gate's word boundary is
    Unicode-aware, so a banned word glued to a non-ASCII letter is not caught; the fix must
    land in all three copies at once (the console, the query console front and `sql_gate.rs`)
    and goes with PR19. (b) `grep_codebase` accepts a relative path that climbs out of the
    repository (older than this PR; PR19). (c) `LIMIT lo,hi` and negative limits pass the row
    cap as written, so they are bounded only by the 8 MiB reply cap.
  - Plan correction: `crates/tickvault-logs-mcp/tests/parity.rs` does not exist in the tree
    (the parity harness was retired), so there is no pin to bump.
- [ ] **PR30 — a budget stop stays stopped for the day.** (`aws-lambdas`, `scripts`, deploy)
  - The 08:45 start watchdog, `aws-autopilot.sh` and the 15:50 terraform apply each undo a
    budget stop the same day (start_watchdog.rs:819-835; aws-autopilot.sh:216-224;
    terraform main.tf:498-507; terraform-apply.yml:64-66). A breach latch (one SSM parameter,
    written by the kill-switch, cleared only on a new billing day or by the operator) that all
    three read before starting the box. Pinned by a test per reader.
  - The October $150 ceiling is enforced in code (`effective_budget_kill_usd`) but budget.tf and
    budget-guards.tf still say $225 (budget.tf:220-222; budget-guards.tf:278). Quote 23 keeps
    $225 for September, so the terraform change is a dated PR on or after 2026-10-01, in all four
    lockstep sites.
- [ ] **PR31 — a restart can never replace a fuller candle with a partial one.** (`storage`,
  `app`, `trading`)
  - A restart in market hours rebuilds the open candles from the ticks it replays, which may be
    only part of them, and the UPSERT overwrites the fuller row already stored
    (ws_frame_spill.rs:4680-4685; shadow_persistence.rs:143). Same class: a same-day replay of a
    missed slice (wal_applied_watermark.rs:186-218; dhan_feed_stack.rs:13474-13933). Rebuild the
    open bars from the stored ticks, or mark a post-restart bar partial and never let it replace
    a row with more volume. PR23's "replay may rebuild them" is verified false: PR23 tests the
    crash case without relying on replay.
  - The boot candle recovery says "all re-ingested" when some failed (seal_writer_task.rs:772-840):
    keep the file and report the real count.
  - Top-volume and candle tables can be auto-created by an ILP write without their DEDUP key
    (candle_ddl_boot.rs:245-290; top_volume_rank_persistence.rs:862-924): refuse the write until
    the DDL has succeeded.
  - Replayed depth rows get a new arrival time, so the key does not collapse them
    (depth_persistence.rs:221-222; dhan_feed_stack.rs:6936-6941): derive the replayed arrival
    time the live way, or take arrival time out of the depth key.
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
