# FINAL SPEC read-too-early (ship=true)

## Summary
The bug is real (Verified on origin/main 60bdfd97a). The check reads candles_1m at 15:41:00. It excuses only TAIL_UNSEALED_MINUTES = 2 (15:38 and 15:39). A quiet instrument's bucket is not sealed until catch_up_cutoff = min(watermark, wall) - 240 s has passed the bucket's end, so 15:36 and 15:37 are often still open at 15:41. The check also never waits for the seal writer or for QuestDB to apply the rows. The missing minutes become missing_live_traded, then Diverged. Diverged counts as measured, so the day is marked on the first attempt with no retry, and the false Diverged row is kept by the DEDUP key that includes outcome.

The fix adds a readiness phase that runs inside the existing 300 s token-wait window, so the worst case is still 4 attempts before 17:30. It has three barriers:
(1) Sweep progress. A new handle, CatchUpProgress, records the floor cutoff of each completed sweep. The floor is never raised by an overrun.
(2) Seal writer drained. It reuses the existing LAST_DRAINED_UNIX_SECS sample, which is taken after each 100 ms cycle, plus a check for spill files staged today.
(3) QuestDB applied through a snapshot. It reuses the archive's wal_tables() code, now shared.

The tail excuse is now derived from the constants (LATE_SEAL_WINDOW_MINUTES), never a literal. A read that may be early is retried instead of marked; only the vendor tape is persisted from it. The last attempt is decided once, conservatively, so the read decision and run_day always agree.

From the reviews: I took the seal-ring/flush false barrier and fixed it by reusing the writer's own drained sample instead of new counters. I also took the overrun floor cutoff, the start-vs-end last-attempt mismatch, frozen vs still-moving below-close, tape persistence, option-pass gating, the 5-plan cap and the history of the tail_unsealed column. I dropped SEAL_FLUSH_MAX_SECS and the per-site hand-off counters, and deferred shed-tick flagging, with reasons given below.

FINAL SPEC: read-too-early (Dhan 1-minute cross-verification reads candles_1m before our last session bars have landed)

**Dated approval (quote verbatim in every rule edit):** operator, 2026-10-06: "Go ahead with whatever you want dude" and "See do everything whatever is recommended dude okay?"

**Base:** origin/main 60bdfd97a (includes PR #2022). Crates touched: app, storage. Every error!/warn! carries code = ErrorCode::WsGapConnectionState.code_str(). No new ErrorCode, no new alarm source, no new EMF name, no new page.

## 0. Problem (Verified unless marked)

- **When the read happens.** spawn_dhan_live_crossverify fires at XVERIFY_RUN_AT_SECS_OF_DAY_IST = RUN_SECS_OF_DAY_IST = close + 60 = 15:41:00. run_once waits only for the JWT, then run_cross_verification calls read_live_side (the candles_1m SELECT) first. Nothing checks the seal writer or wal_tables().
- **What is excused.** TAIL_UNSEALED_MINUTES is the literal 2 (its doc still says "At 15:31"), so is_tail_minute excuses only 15:38 and 15:39.
- **When a quiet bucket seals.** A bucket with no later tick seals only when catch_up_cutoff() = min(aggregator watermark, top_volume_wall_secs) - CATCHUP_LATENESS_MARGIN_SECS (240) reaches the bucket's end. At wall 15:41:00 the cutoff is at most 15:37:00, so the 15:36 and 15:37 buckets of quiet instruments can be open. If no trade stamp at or after 15:40 reaches the fold, the watermark freezes and the 15:35 to 15:39 buckets stay open until the 17:30 shutdown seal. Whether Dhan sends post-close stamps is Unknown.
- **Seal writer and QuestDB.** Sealed bars go through the crossbeam SealSender into the ring, then an ILP flush. Acknowledged rows are invisible to SELECT until QuestDB applies them (writerTxn reaches sequencerTxn).
- **Effect.** A missing traded or IDX_I minute outside the 2-minute tail sets real = true, so the outcome is Diverged. Diverged is measured, so classify_attempt returns Ok, the marker is written, the S3 hold is released, and run_day breaks with no retry. The daily row (outcome is in its DEDUP key) keeps the false Diverged permanently.
- **Retry budget.** The §12.15.5 locked worst case is 4 × 960 + 3 × 900 = 6,540 s = 17:30 minus 15:41. Any wait added on top of attempt_max_secs loses the 4th attempt.

## A. Derived constants (crates/app/src/dhan_live_crossverify.rs)

1. In dhan_feed_stack.rs, make CATCHUP_LATENESS_MARGIN_SECS pub(crate). CATCHUP_SEAL_INTERVAL_SECS is already pub. Also make MEASURED_MAX_DELIVERY_LAG_SECS pub(crate) if it is not already.

2. Add `pub const LIVE_FINAL_SECS_OF_DAY_IST: i64 = SESSION_CLOSE_SECS_OF_DAY_IST + CATCHUP_LATENESS_MARGIN_SECS as i64 + CATCHUP_SEAL_INTERVAL_SECS as i64;`. Today that is 15:44:05.
   - It is a sleep FLOOR only. Correctness comes from the barriers in B.
   - It must be at least close + MEASURED_MAX_DELIVERY_LAG_SECS, so a late amendment of the 15:39 bar has reached the drain by then.

3. Add `pub const LATE_SEAL_WINDOW_MINUTES: i64 = ((CATCHUP_LATENESS_MARGIN_SECS as i64) + SECS_PER_MINUTE).div_ceil(SECS_PER_MINUTE);`. Today that is 5 (15:35 to 15:39).

4. Compile-time asserts:
   - LIVE_FINAL_SECS_OF_DAY_IST > RUN_SECS_OF_DAY_IST
   - LIVE_FINAL_SECS_OF_DAY_IST <= RUN_SECS_OF_DAY_IST + TOKEN_WAIT_POLL_SECS * TOKEN_WAIT_MAX_POLLS (15:46)
   - LIVE_FINAL_SECS_OF_DAY_IST >= SESSION_CLOSE + MEASURED_MAX_DELIVERY_LAG_SECS
   - LATE_SEAL_WINDOW_MINUTES * 60 >= CATCHUP_LATENESS_MARGIN_SECS

   The TOKEN_WAIT constants live in the boot module. Put this assert there, or move the constants into dhan_live_crossverify.rs.

5. Delete TAIL_UNSEALED_MINUTES. Replace is_tail_minute with `pub fn is_late_window_minute(minute_ts_ist_nanos, day_start_ist_nanos, sealed_through_secs_of_day: Option<i64>) -> bool`:
   - With Some(st): true when the bucket end is after st.
   - With None: true when the bucket is one of the last LATE_SEAL_WINDOW_MINUTES of the session.
   - O(1).

   Update the doc comments at the old lines 105-107 and 141-148, and the citation in .claude/plans/active-plan-feed-hardening.md (~line 1729).

6. RUN_SECS_OF_DAY_IST, the fire time and the audit stamp are UNCHANGED, so upserts stay stable.

## B. Readiness phase (crates/app/src/dhan_live_crossverify_boot.rs)

### B1. Sweep progress handle (no global, so tests do not race)

- **New type** `pub struct CatchUpProgress(AtomicU64)` in dhan_feed_stack.rs.
  - High 32 bits: the completed sweep's FLOOR cutoff, in fold seconds (IST-wall-as-epoch u32).
  - Low 32 bits: the completion time in UTC unix seconds (fits until 2106).
  - 0 means none.
  - Methods: `publish(floor, done_unix)` stores with Release; `load() -> Option<(u32, u32)>` loads with Acquire. Both O(1).
- **Floor cutoff.** CatchUpSweep gains `floor_cutoff: u32`. begin_catch_up_seal sets it once when it creates a sweep. The overrun branch keeps `sweep.cutoff = sweep.cutoff.max(cutoff)` but NEVER touches floor_cutoff, because slots already visited were judged against the older cutoff.
- **Publishing.** step_idle_work publishes (floor_cutoff, Utc::now().timestamp() as u32) only in the arm where a sliced catch-up sweep finishes its last slot. That is one clock read and one store per completed sweep (every ~5 s), cold, zero allocation, never per tick. The completed sweep sealed every bucket ending at or before floor_cutoff in every slot.
- **Wiring.** main.rs creates `Arc<CatchUpProgress>`. It is passed to the Dhan feed-stack deps (cloned into every LiveIngest, including respawns) and to a new field `CrossverifyBootDeps.catch_up_progress`. Tests construct their own.

### B2. Seal writer drained

- In seal_writer_loop.rs, expose `pub fn last_seal_drained_unix_secs() -> Option<i64>` (an Acquire load of the existing LAST_DRAINED_UNIX_SECS; 0 means None). Change its store to Release.
- Verified: note_drained runs on every 100 ms cycle, after run_one_cycle returns, with unwritten_seals() = channel + ring + escalation_pending. Seals popped from the ring and still being flushed cannot coexist with a sample, and a failed flush keeps them in the ring. An empty sample therefore means every seal handed off before it is acknowledged by QuestDB or written to the spill.
- **Spill check.** Also count seal spill records staged for TODAY: seal_spill::is_spill_record_name over the spill top level and replaying/, never archive/ or refused/, restricted to today's IST date taken from the file name. Verify the day-file naming in seal_spill.rs before coding. This is O(files), cold, at most once every 5 s.
  - Staged files mean SealsPending.
  - Files left after the replay has given up on them (parked under SEAL_REPLAY_STUCK_FAILURES) get their own reason, SealSpillParked.
  - Spill files are not specific to candles_1m. That is stated as a limit.

### B3. QuestDB applied-through barrier

- In wal_suspension_watcher.rs:
  - Add `pub async fn fetch_wal_tables(client: &reqwest::Client, exec_url: &str) -> anyhow::Result<Vec<WalTableRow>>`. Move the body from partition_archive's method, together with WAL_TABLES_PROBE_SQL and a capped body reader. Keep `skipped > 0 => Err` byte-identical. partition_archive's method becomes a one-line call to it.
  - Add a pure `pub fn wal_applied_through(rows: &[WalTableRow], table: &str, target_txn: i64) -> AppliedThrough { Reached, Behind { writer_txn, target_txn }, Suspended, NotReported, TxnUnreadable }`. It returns Reached only when the table is not suspended and writerTxn >= target_txn. Scanning the rows is O(tables) with an O(1) EXEMPT marker.
- wal_apply_verdict (the archive's exact-equality check) is unchanged.
- The barrier is applied-THROUGH a snapshot, not equality, so it cannot livelock under continuing writes.

### B4. `async fn wait_live_final(deps, day_start_ist_nanos, deadline: Instant) -> LiveReadiness`

The deadline is attempt start + TOKEN_WAIT_POLL_SECS * TOKEN_WAIT_MAX_POLLS. Every step is bounded by it.

1. Sleep until the wall time is day_start + LIVE_FINAL_SECS_OF_DAY_IST. This is a no-op on retries and late catch-ups. Record the readiness start R (UTC unix).
2. Every TOKEN_WAIT_POLL_SECS, sample P = catch_up_progress.load() and keep the first and the latest sample.
3. **Completeness**, evaluated each poll. close_epoch = day_start_secs + SESSION_CLOSE; floor and done come from P.
   - **Unknown:** P is None, or floor's IST day is not today, or done_unix < R (no sweep has completed since readiness began, so the drain is stalled or the lane is off).
   - **Final:** floor >= close_epoch and done_unix >= the UTC instant of day_start + LIVE_FINAL.
   - **BelowCloseMoving:** floor < close_epoch, and floor has advanced since the first sample or the deadline is not yet reached.
   - **BelowCloseFrozen { sealed_through_secs_of_day }:** floor < close_epoch, unchanged from the first to the last sample across the whole window, with sweeps still completing. Decided only at the deadline.
4. **Durability**, checked once completeness is Final or BelowCloseFrozen, or at the deadline. The reference instant T is done_unix when P is known, otherwise R.
   - First require last_seal_drained_unix_secs() >= T + 1 (strict, to avoid a same-second race) and no staged spill record for today. Otherwise the result is SealsPending, or SealSpillParked.
   - Then take ONE fetch_wal_tables snapshot and record S = sequencerTxn('candles_1m').
   - Poll fetch_wal_tables until wal_applied_through(rows, "candles_1m", S) == Reached. Otherwise the result is NotApplied(AppliedThrough). A probe error counts as NotApplied(NotReported).
5. Return `LiveReadiness { durability: Durability { Applied, SealsPending, SealSpillParked, NotApplied(AppliedThrough) }, completeness: Completeness { Final, BelowCloseFrozen { sealed_through_secs_of_day }, BelowCloseMoving, Unknown } }`.

Pure helpers carry the logic, and the async function only gathers inputs:
- `classify_completeness(first, latest, close_epoch, live_final_utc, readiness_start, at_deadline)`
- `classify_durability(drained, ref_instant, spill_staged, spill_parked, applied)`

**Running both waits.** run_once runs `tokio::join!(wait_for_jwt(), wait_live_final(..))`, both bounded by the same deadline, so attempt_max_secs is unchanged. If readiness ends early with a decision, the token wait still bounds the join. read_live_side is never called before readiness returns.

## C. Read decision and the retry-instead-of-marker rule

### C1. Last attempt is decided once, conservatively

- In run_day, before each run_once: `is_last = retry_delay_secs(attempts, start_secs_of_day + max_attempt_secs, max_attempt_secs).is_none()`. Pass it into run_once.
- After the attempt, run_day uses the SAME value:
  - is_last: call report_final_failure.
  - otherwise: sleep XVERIFY_RETRY_INTERVAL_SECS. The actual end is at most start + attempt_max, so the retry is guaranteed to fit.
- The read decision and run_day can never disagree.
- Worst-case check: attempts start at 15:41, 16:12, 16:43 and 17:14, and the 4th is the last. Still 4 attempts.

### C2. `pub fn decide_read(r: &LiveReadiness, is_last: bool) -> ReadPlan { Proceed(LateWindowPolicy), Retry(AttemptFailure) }`

Pure, O(1), total over the product.

| Durability | Completeness | Not the last attempt | Last attempt |
|---|---|---|---|
| SealsPending | any | Retry(SealsPending) | final failure |
| SealSpillParked | any | Retry(SealSpillParked) | final failure |
| NotApplied | any | Retry(LiveNotApplied) | final failure |
| Applied | Final | Proceed(Strict) | Proceed(Strict) |
| Applied | BelowCloseFrozen{st} | Proceed(Excuse{Some(st)}) | Proceed(Excuse{Some(st)}) |
| Applied | BelowCloseMoving | Retry(LiveNotFinal) | Proceed(Excuse{Some(latest floor)}) |
| Applied | Unknown | Proceed(Strict) + the post-check in C4 | Proceed(Excuse{None}) |

- A Retry before the read makes NO REST call, persists nothing, writes no marker, and does not page.
- BelowCloseFrozen proceeds even on early attempts because the frozen buckets cannot seal before the 17:30 shutdown seal, so retrying would only burn vendor quota.
- A "final failure" goes through report_final_failure into the existing `xverify_failed` arm. The marker is withheld, and the S3 hold runs to MAX_CROSSVERIFY_HOLD_DAYS with its existing loud error.

### C3. compare_day_in_scope gains `late: LateWindowPolicy { Strict, Excuse { sealed_through_secs_of_day: Option<i64> } }`

- The test helper compare_day passes Strict.
- DayComparison gains:
  - `missing_live_late`: the real-missing minutes (traded or IDX_I, using the same rules) inside the late window. It is always computed, in the same single pass.
  - `late_excused`.
- **Strict:** no minute is excused (the 15:38/15:39 amnesty is gone). A late real-missing minute is MissingLive and real.
- **Excuse:** a late real-missing minute counts toward late_excused with cell kind LateExcused (a new symbol value) and is not real.
  - The outcome becomes Partial only if it would have been Clean and late_excused > 0.
  - It is never upgraded. Degraded stays Partial, and Diverged from minutes outside the window or from cells_diverged stands.
- **tail_unsealed and the TailUnsealed kind** are retained for history and written as 0 from this change. Add a dated note in the persistence module doc.
- **Schema:** add `late_excused LONG` to the daily table through the existing ALTER ADD COLUMN IF NOT EXISTS self-heal. DEDUP keys are unchanged.

### C4. Post-compare rule

Applies when the plan was Proceed(Strict) from Unknown, !is_last, and missing_live_late > 0. Return Err(AttemptFailure::LiveNotFinal) after:
- appending the vendor tape only (append_rest_tape; its DEDUP key has no outcome, so the append is idempotent; verify the key in the persistence module);
- NOT appending the daily row or the cell findings;
- NOT writing the marker.

The `xverify_diverged` page is unchanged (§12.15.5 locks "fires on the first attempt that measures it"). cells_diverged counts only minutes present on both sides, so it does not depend on late minutes missing.

### C5. classify_attempt and should_write_marker gain `live_final: bool`

- live_final is false only on the LiveNotFinal path.
- The marker stays solely classify_attempt == Ok. The debug_assert_eq! in run_once gains it on both sides.

### C6. AttemptFailure gains LiveNotApplied, SealsPending, SealSpillParked, LiveNotFinal

- Stable labels: live_not_applied, seals_pending, seal_spill_parked, live_not_final.
- report_final_failure maps them to the existing `xverify_failed` arm (exhaustive match).
- Per-attempt not-ready logs use warn! with a new non-paging source `xverify_attempt_not_ready`, which no terraform filter matches. Labels go to tv_dhan_xverify_retries_total{reason}, local /metrics only.

### C7. run_option_pass

- run_day passes the final attempt's plan. The pass is skipped when durability was never Applied, with the new outcome label `skipped_not_ready` (registered at 0 in XVERIFY_OPTION_PASS_OUTCOMES).
- Otherwise it uses Strict when completeness was Final, and Excuse with the same sealed_through otherwise.
- It never writes the marker and never pages, per §12.15.6.

## D. Plan and rule files (rule-file-first, same PR)

- **Plan:** add this as a new ITEM in .claude/plans/active-plan-feed-hardening.md. origin/main already has exactly 5 active plans, so a 6th trips the V7 cap. The item carries Design, Edge Cases, Failure Modes, Test Plan, Rollback and Observability, the 15-row and 7-row matrices, and the Z+ checklist, and names crates app and storage.
- **docs/claude-rules-full/project/no-rest-except-live-feed-2026-06-27.md: new §12.15.7** "2026-10-06: the check reads our 1-minute candles only after they have landed". It quotes the approval verbatim and records:
  - the problem;
  - the derived constants;
  - the three barriers;
  - the decision table;
  - the conservative last-attempt rule;
  - tape-only persistence on a retried read;
  - Excuse means Partial, never Clean;
  - the honest limits: a frozen watermark gives Partial every day; spill files are not specific to candles_1m; QuestDB apply lag can withhold the marker.

  REJECT rows:
  - reading candles_1m before readiness;
  - a marker, daily row or findings from a read retried as LiveNotFinal;
  - excusing late minutes when completeness is Final;
  - restoring a literal tail-minute count;
  - lengthening attempt_max_secs for readiness;
  - raising the floor cutoff on overrun;
  - deciding the last attempt from the start time.
- **The summary stub** for that file gets one line pointing to §12.15.7, with its REJECT headlines.
- **docs/claude-rules-full/project/dhan-rest-only-noise-lock-2026-07-14.md: §2.5 addendum, dated 2026-10-06, same quote.**
  - `xverify_failed` now also fires (final attempt only) for live_not_applied, seals_pending, seal_spill_parked and live_not_final.
  - No new alarm, source, EMF name or ok_actions.
  - `xverify_diverged` timing is unchanged.
  - The new warn source `xverify_attempt_not_ready` never pages.

  Keep the .claude summary stub in sync.
- **docs/error-runbooks/dhan-live-crossverify-error-codes.md:** add the four reasons, their meaning, and the operator action:
  - check `SELECT name, writerTxn, sequencerTxn, suspended FROM wal_tables() WHERE name='candles_1m'`;
  - list the seal spill dir;
  - frozen post-close watermark means expected Partial.

## E. Not in this change (with reasons)

- **Per-site SEALS_HANDED_OFF / SEALS_RESOLVED counters:** replaced by the writer's existing post-cycle drained sample. Same guarantee, and no risk of a missed increment site.
- **SEAL_FLUSH_MAX_SECS in LIVE_FINAL:** dropped. A failed cycle can block about 30 s, so the sum was not a true bound, and the drained barrier makes it unnecessary.
- **Flagging ticks shed today (which are re-folded only at the next boot):** a real but separate defect. It needs a capture-seq-to-time mapping to ask AppliedSnapshot::range_has_unapplied for today's session. Recommendation: follow-up task.
- **Pre-checking index minutes on the live side to skip REST on Unknown:** Unknown should be rare once the handle is wired, so it is not worth the code.
- **Recommendation (follow-up, own plan):** after close + margin, let the catch-up cutoff follow wall - CATCHUP_LATENESS_MARGIN_SECS. Every quiet bucket then seals by about 15:44 and Final becomes the normal case. It touches the fold and restart_differential.

## Complexity (honest)

- New decisions are O(1).
- The comparison is unchanged: one pass, O(n log n) through the BTreeMap.
- wal_tables() is O(tables, about 30) per poll, cold, at most every 5 s for 300 s.
- The spill listing is O(files), cold.
- The drain adds one clock read and one Release store per completed 5 s sweep, never per tick, with zero allocation; the DHAT gates must stay green.

## Tests
["dhan_live_crossverify::live_final_secs_is_derived_from_close_margin_and_sweep_interval (const == close + CATCHUP_LATENESS_MARGIN_SECS + CATCHUP_SEAL_INTERVAL_SECS; 15:44:05 today; >= close + MEASURED_MAX_DELIVERY_LAG_SECS)","dhan_live_crossverify_boot::live_final_fits_inside_the_token_wait_window (LIVE_FINAL <= RUN_SECS + TOKEN_WAIT_POLL_SECS*TOKEN_WAIT_MAX_POLLS)","dhan_live_crossverify::late_seal_window_minutes_is_derived_from_the_catch_up_margin (== (margin+60).div_ceil(60) == 5; no literal tail count remains: source scan finds no TAIL_UNSEALED_MINUTES)","dhan_live_crossverify::is_late_window_minute_with_known_sealed_through_excuses_only_buckets_ending_after_it","dhan_live_crossverify::strict_policy_counts_a_missing_1539_traded_minute_as_real_diverged (the 15:38/15:39 amnesty is gone when Final)","dhan_live_crossverify::missing_live_late_counts_traded_and_index_minutes_only (zero-volume equity late minute not counted)","dhan_live_crossverify::excuse_downgrades_clean_to_partial_only_when_late_excused_gt_zero_never_upgrades_degraded_or_diverged","dhan_live_crossverify::excuse_cells_are_late_excused_kind_and_tail_unsealed_stays_zero","dhan_live_crossverify::the_late_window_reaches_neither_half_of_the_traded_split_under_excuse (successor of compare_day_tail_amnesty_reaches_neither_half_of_the_split)","dhan_live_crossverify::sealed_through_fold_secs_and_day_start_share_the_ist_wall_epoch","dhan_feed_stack::catch_up_progress_publishes_the_floor_cutoff_only_when_a_sliced_sweep_completes (begin -> partial steps leave it unchanged -> last step publishes floor and done time)","dhan_feed_stack::an_overrun_raises_the_sweep_cutoff_but_never_the_published_floor (begin at c1, step part-way, overrun at c2 > c1, finish -> published floor == c1)","dhan_feed_stack::catch_up_progress_store_is_off_the_per_tick_path (source scan: publish only in the step_idle_work completion arm)","dhan_feed_stack::catch_up_progress_pack_roundtrips_and_zero_is_none","dhan_live_crossverify_boot::classify_completeness_table (Unknown: none / other day / no sweep since readiness start; Final; BelowCloseMoving when floor advanced or before deadline; BelowCloseFrozen only at deadline with unchanged floor and advancing done time)","dhan_live_crossverify_boot::classify_durability_needs_drained_strictly_after_the_reference_sweep (drained == T -> SealsPending; drained == T+1 and applied -> Applied; staged spill -> SealsPending; parked spill -> SealSpillParked)","dhan_live_crossverify_boot::a_seal_in_the_ring_or_a_failed_flush_keeps_readiness_not_final (model: channel empty, ring non-empty -> no drained sample -> SealsPending)","dhan_live_crossverify_boot::decide_read_table_is_total (proptest over Durability x Completeness x is_last -> exact ReadPlan per the C2 table)","dhan_live_crossverify_boot::retry_plans_never_call_rest_never_persist_never_write_a_marker (source scan: Retry branch returns before run_cross_verification)","dhan_live_crossverify_boot::is_last_is_decided_once_from_start_plus_attempt_max (proptest over every start second 15:41..17:30 and every attempt count: !is_last implies retry_delay_secs at any end <= start+attempt_max is Some)","dhan_live_crossverify_boot::test_worst_case_day_fits_every_attempt_before_the_evening_stop (unchanged: 4 attempts; attempt_max_secs unchanged by readiness; conservative is_last yields attempts at 15:41/16:12/16:43/17:14)","dhan_live_crossverify_boot::readiness_is_bounded_by_the_token_wait_deadline (tokio paused clock: never applied -> NotApplied at deadline, total <= TOKEN_WAIT_POLL_SECS*TOKEN_WAIT_MAX_POLLS)","dhan_live_crossverify_boot::run_once_joins_token_and_readiness_waits_before_the_read (source scan: tokio::join! of wait_for_jwt and wait_live_final precedes run_cross_verification)","dhan_live_crossverify_boot::a_live_not_final_read_persists_the_tape_only (source scan + unit: append_rest_tape called; daily row and findings not appended; no write_daily_marker)","dhan_live_crossverify_boot::the_divergence_page_still_fires_on_the_first_measured_attempt (unchanged §12.15.5 behaviour)","dhan_live_crossverify_boot::test_classify_attempt_agrees_with_the_marker_rule_everywhere (extended with live_final; exhaustive)","dhan_live_crossverify_boot::new_attempt_failure_labels_are_distinct_and_map_to_xverify_failed","dhan_live_crossverify_boot::xverify_attempt_not_ready_source_matches_no_alarm_filter (terraform xverify filter patterns do not match it)","dhan_live_crossverify_boot::test_every_xverify_alarm_source_has_a_live_error_emit (unchanged set; no new source)","dhan_live_crossverify_boot::option_pass_is_skipped_not_ready_when_durability_was_never_applied_and_uses_excuse_unless_final","wal_suspension_watcher::wal_applied_through_reaches_only_when_writer_at_or_past_target_and_not_suspended (Reached/Behind/Suspended/NotReported/TxnUnreadable; immune to sequencer growth after the snapshot)","wal_suspension_watcher::fetch_wal_tables_fails_closed_on_skipped_rows","partition_archive::wal_applied_gate_behaviour_unchanged_after_fetch_wal_tables_moves (existing gate tests stay green)","seal_writer_loop::last_seal_drained_unix_secs_reports_the_latest_empty_sample_and_none_before_any","dhan_live_crossverify_persistence::late_excused_column_self_heals_and_dedup_keys_unchanged","dhat_multi_tf_fold and the drain DHAT tests stay zero-alloc"]

## Guards to update
["crates/app/src/dhan_live_crossverify.rs: is_tail_minute_excuses_trailing_two_minutes_not_counted_as_missing (rewrite against is_late_window_minute / LATE_SEAL_WINDOW_MINUTES)","crates/app/src/dhan_live_crossverify.rs: compare_day_tail_amnesty_reaches_neither_half_of_the_split (move to the Excuse policy)","crates/app/src/dhan_live_crossverify.rs: every compare_day / compare_day_in_scope call site (new LateWindowPolicy parameter; the compare_day test helper uses Strict) and the run_cross_verification source scan near line 3539","crates/app/src/dhan_live_crossverify_boot.rs: test_classify_attempt_agrees_with_the_marker_rule_everywhere, test_classify_attempt_names_the_first_problem, test_attempt_failure_labels_are_distinct, test_should_write_marker_needs_a_measured_persisted_comparison, test_marker_write_needs_a_complete_run, test_run_day_retries_through_the_pure_bound, the run_once source scans (wait_for_jwt().await, a single write_daily_marker call, write after classify), the run_day scans (report_final_failure), the persist_report scan, test_the_fetched_vendor_tape_is_persisted_not_discarded, and the run_option_pass scans (no marker, outcomes list)","crates/app/src/dhan_live_crossverify_boot.rs: test_worst_case_day_fits_every_attempt_before_the_evening_stop (keep; assert attempt_max_secs unchanged)","crates/app/src/dhan_feed_stack.rs: drain source-scan tests around step_idle_work / the catch-up timer arm, CatchUpSweep construction sites (new floor_cutoff field), and the LiveIngest deps constructors used by tests and restart_differential (new CatchUpProgress handle)","crates/app/src/main.rs: feed-stack deps and CrossverifyBootDeps construction (shared Arc<CatchUpProgress>)","crates/storage/src/partition_archive.rs: fetch_wal_tables / wal_applied_gate tests after the body moves to wal_suspension_watcher (behaviour byte-identical); crates/storage/tests/partition_archive_guard.rs if it source-scans fetch_wal_tables","crates/storage/src/seal_writer_loop.rs: LAST_DRAINED_UNIX_SECS store becomes Release; new pub accessor needs a test and a call site (pub-fn test and wiring guards)","crates/storage/tests/wal_seal_pubfn_coverage_guard.rs and the pub-fn wiring/test hooks for wal_applied_through, the shared fetch_wal_tables and last_seal_drained_unix_secs","crates/storage/src/dhan_live_crossverify_persistence.rs: the daily-table schema/self-heal test (late_excused column), the cell-kind symbol test (LateExcused), the DEDUP meta-guard (keys unchanged)","crates/app/tests DHAT gates (dhat_multi_tf_fold and the drain DHAT tests) must stay zero-alloc","docs/error-runbooks/dhan-live-crossverify-error-codes.md crossref (the new reason labels; still ErrorCode::WsGapConnectionState)","deploy/aws/terraform xverify metric-filter tests: unchanged sources; add an assertion that xverify_attempt_not_ready is not matched",".claude/plans/active-plan-feed-hardening.md: new item plus fix the stale TAIL_UNSEALED_MINUTES citation (~line 1729); per-item-guarantee-check.sh and plan-gate must pass at 5 active plans"]

## Residual risks
["Assumed, not measured: whether Dhan sends trade stamps at or after 15:44 (equity post-close session) that keep the watermark moving. If it does not, every day reads BelowCloseFrozen and is recorded as Partial, with the 15:35 to 15:39 quiet-instrument minutes counted in late_excused. That is honest but only partly verified. The fix is the separate feed-stack follow-up (post-close cutoff follows the wall clock).","Unknown: candles_1m's own QuestDB apply lag after the close. In session, total apply lag has been measured at 11,503 to 27,089 transactions, and the after-close deferred-depth pass competes for apply. If candles_1m cannot reach the snapshot inside each 300 s window, every attempt returns NotApplied, xverify_failed pages every evening, and the S3 archive runs to MAX_CROSSVERIFY_HOLD_DAYS. That direction is honest, but the page may be frequent. Before relying on it, measure `SELECT name, writerTxn, sequencerTxn FROM wal_tables() WHERE name='candles_1m'` from 15:41 to 17:00 on a live day (QuestDB was not reachable from this session).","Verdicts change in both directions. Once completeness is Final, the 15:38 and 15:39 amnesty is gone, so real loss in the closing session reads Diverged. IDX_I minutes are always real. If index ticks stop near 15:30 while Dhan prints flat index bars to 15:39, Final days would flip to Diverged. 15:31 to 15:37 are already strict today, so the cell audit history answers this. Unknown until queried; state it in the PR.","The seal barrier relies on wall-clock seconds across two threads: drained >= sweep done + 1 s. Assumed sound because crossbeam channel length reads are linearizable and the drain hands off before it publishes. A clock step backwards between the two reads could delay readiness, which fails closed into a retry.","A seal spill file left parked by the replay blocks the marker on every day until the 3-day ceiling. Spill files are not specific to candles_1m, so a parked 5m seal blocks the 1m verdict. It is reported under its own reason, seal_spill_parked.","Ticks shed today (ring full, failed append) are re-folded only at the next boot. A same-day verdict can therefore still read Diverged for minutes inside shed ranges. Not fixed here; it needs a capture-seq-to-time mapping. Follow-up recommended.","Dhan's own REST bars for 15:38 and 15:39 may not be final at the read. Moving the read to 15:44 or later helps; this is recorded as a vendor-side limit.","On an Unknown-completeness day with late gaps, the about-868-call REST sweep can repeat up to 3 times. The cost is bounded by the 4-attempt cap at 5 rps, and the tape is kept, so it is never refetched for loss."]

