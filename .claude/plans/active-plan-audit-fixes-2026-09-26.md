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
- [ ] **PR4b — the catch-up seal sweep runs in slices too.** (`trading`, `app`) — found while
  doing PR4. `catch_up_seal_all` is O(slots × TF_COUNT) on the drain's 5 s `catchup_timer`
  arm; measured 9.67 ms at the 25,000 × 24 ceiling (2026-08-21), ~3.6 ms at today's
  TF_COUNT = 9. Same shape of fix: a resumable cursor over the slots, advanced from the idle
  arm. Tests: `catch_up_seal_in_slices_seals_the_same_bars` (proptest vs `catch_up_seal_all`).
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
