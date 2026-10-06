# FINAL SPEC missing-minutes-page (ship=true)

## Summary
The problem is real. Checked against origin/main 60bdfd97a (includes PR #2022). (1) Lost minutes never page: is_catastrophic_divergence (dhan_live_crossverify_boot.rs:196) looks only at price fields, and run_once's xverify_diverged emit (line 624) runs only when that check passes. (2) Partial hides Diverged: degraded = truncated || budget_elapsed || rest_failures>0 (dhan_live_crossverify.rs:1951) is checked before real. (3) Blocker for any missing-minutes page: catch_up_cutoff() = min(watermark, wall) - 240 s (dhan_feed_stack.rs:5122). The watermark moves only on folded ticks, so the cutoff can stall near 15:36. The QuestDB WAL settle check also cannot see sealed bars that are still queued or spilled, or shed frames that are still unapplied. Each of these turns a not-yet-written minute into a false 'missing' minute. Both reviews are folded in. The excused end-of-day window now comes from a PUBLISHED real cutoff, not the wall clock. Missing minutes are judged only when the seal pipeline is drained, nothing is unapplied today, nothing was spilled today and QuestDB has settled. Index pages need a run of 2 or more minutes where the vendor bar shows movement. Zero-volume gaps no longer break a stock run. An unsettled run is Incomplete only while another attempt remains, so the xverify_failed trigger is NOT widened. The settle wait is a caller parameter (off for the option pass) inside the attempt bound. Unjudged misses are stored as their own cell kind. The work goes in as a new ITEM in active-plan-feed-hardening.md, because 5 active plans already exist (Verified). It ships in two PRs. PR-A: the verdict fix plus judging gates plus persisted shadow `missing_minutes_would_page`, no page change. PR-B: the page wiring plus noise-lock §2.11, merged only after a measured backtest or shadow baseline, because QuestDB could not be reached from here and the baseline is Unknown.

FINAL SPEC: missing-minutes-page. Base: origin/main 60bdfd97a. Authority: operator 2026-10-06, verbatim: "Go ahead with whatever you want dude" and "See do everything whatever is recommended dude okay?". Both rule rows quote this AND point at the enumerated plan ITEM below, because §2.3k accepts a broad go-ahead only when it answers an enumerated list. Delivery is TWO serial PRs. Rule files are edited first in each PR.

=== PLAN (before any crates/*/src edit) ===
P1. Add a new ITEM to .claude/plans/active-plan-feed-hardening.md. Do NOT create a new plan file: origin/main already has 5 active-plan files (Verified), so a sixth trips plan-gate V7. The ITEM carries the six required sections (Design, Edge Cases, Failure Modes, Test Plan, Rollback, Observability), the 15-row and 7-row matrices, and the Z+ checklist. Status DRAFT, then APPROVED with the dated quotes. It lists sub-items A1–A9 (PR-A) and B1–B5 (PR-B), each with files and tests.

=== PR-A: correct verdict, judging gates, shadow measurement (no page change, no alarm change) ===

A1. Separate what was incomplete (crates/app/src/dhan_live_crossverify.rs)
- compare_day_in_scope replaces `degraded: bool` with `Completeness { live_truncated: bool, rest_incomplete: bool, missing_judgeable: MissingJudgeable, tail_excuse_from_secs_of_day: i64 }`.
- `MissingJudgeable` is a `Copy` enum: Judged, plus one variant per blocker, in priority order: LiveTruncated, LiveUnsettled, SealBacklog, SealSpilledToday, UnappliedFramesToday, CutoffUnknown.
- The test-only compare_day wrapper keeps `degraded: bool` and maps it to rest_incomplete with Judged and the old tail. The ~54 existing call sites stay untouched.
- Verdict:
  - judged = missing_judgeable == Judged.
  - real = cells_diverged>0 || (judged && (missing_live_traded>0 || missing_live_index>0)).
  - If real: Diverged.
  - Else if rest_incomplete || !judged || missing_rest>0 || (unjudged misses > 0): Partial.
  - Else Clean.
  - Vacuous outcomes (Degraded / Blind / NoData) are unchanged.
- Why this is sound (Verified): a target never fetched has no REST bars, so it can add only missing_rest, never a false missing_live. A failed fetch therefore cannot hide loss it could not have caused.
- Truncation keeps judging off for the whole day. The read is ORDER BY ts ASC LIMIT, so it cuts the end. Partial judging up to the last live ts is optional and out of scope.

A2. End-of-day allowance from the REAL seal cutoff (crates/app/src/dhan_feed_stack.rs plus the verifier)
- Add `pub static CATCHUP_CUTOFF_PUBLISHED: AtomicU64`, which packs (IST day number << 32 | cutoff secs-of-day).
- begin_catch_up_seal stores it with one Relaxed store after computing the cutoff (non-zero only). That is the 5 s timer arm, O(1), no allocation, NOT per tick. Add a dhat assertion in the existing catch-up dhat phase.
- Make CATCHUP_LATENESS_MARGIN_SECS pub(crate). Add `SEAL_SETTLE_SLACK_SECS: i64 = 60` (seal-writer 100 ms cycle, ILP flush and commit, with margin).
- The verifier reads the published value just before the live read:
  - Wrong day or 0: CutoffUnknown (not judged).
  - Otherwise tail_excuse_from = min(cutoff_secs - SEAL_SETTLE_SLACK_SECS, SESSION_CLOSE - TAIL_UNSEALED_MINUTES*60). The existing 2-minute rule stays the minimum.
- Every in-session minute whose bucket END is later than tail_excuse_from is a TailUnsealed finding: counted and stored, never missing_live.
- Why this replaces the wall-clock rule (review A/B, Verified): the cutoff stalls when the watermark stops, so a retry at 15:56 or later still excuses whatever is truly unsealed.
- Add a const assert that 60 + TAIL floor covers a cutoff at least CATCHUP_LATENESS_MARGIN_SECS behind close. Record the reasoning beside the constant.
- Honest limit, recorded in §12.15.7: excused end-of-day minutes are never judged on a day whose first attempt completes, and no later fire time fits before 17:30 (pinned by test).

A3. Seal pipeline drained (crates/storage)
- Expose a read-only `SealBacklogProbe` (Clone; holds the existing Arc<AtomicUsize> from SealSink::pending_count() and the spill's escalation_pending(), which becomes pub through the probe) and wire it into the crossverify deps.
- Add `pub static SEAL_SPILLED_DAY_IST: AtomicU32`: one Relaxed store of today's IST day number on every seal spill append and every DLQ write (error path, O(1)).
- Results:
  - SealBacklog when pending > 0 or escalation_pending > 0 at read time (sampled once after the settle wait).
  - SealSpilledToday when the stamp equals today.
- This is the conservative direction: any spill today means missing minutes are not judged for the day, which is recorded in the fields.
- Why (review A/B, Verified): wal_tables cannot see seals that never reached the sequencer.

A4. No shed frames left unapplied today
- Use `applied_watermark().snapshot().range_has_unapplied(lo, hi)`, where lo/hi are the capture sequences of today's 09:00 and the read instant. If true: UnappliedFramesToday.
- The time-to-sequence mapping must reuse the WAL's own sequence base (Assumed that sequences are receipt nanos; UNAPPLIED_BUCKET_SHIFT=38 suggests so). The implementer verifies this in ws_frame_spill.
- If no exact mapping exists, use "any unapplied mark recorded today" (snapshot over the day bucket range), which also fails toward not judging.
- Cold path: one snapshot a day.

A5. QuestDB settle wait (bounded, caller-chosen)
- run_cross_verification takes `settle: Option<Duration>`. The spot pass passes Some(XVERIFY_SETTLE_MAX_WAIT_SECS = 60). run_option_pass passes None.
- Take `started` BEFORE the settle, so attempt_max_secs still bounds an attempt and the 17:30 fit test stays true. The wait comes out of run_budget_secs and stops as soon as the table is settled (typically one poll).
- Reuse partition_archive::wal_apply_verdict / WalApplyVerdict (make pub). Do not add a third wal_tables parser.
- Poll `select * from wal_tables()` every 5 s. Settled = candles_1m not suspended AND writerTxn >= the sequencerTxn of the first poll.
- Suspended, Lagging past the wait, NotReported, TxnUnreadable or an HTTP error: LiveUnsettled.
- A non-zero bufferedTxnSize alone does not count as settled.

A6. Attempt and marker semantics (boot.rs). xverify_failed trigger NOT widened.
- run_is_complete is unchanged.
- classify_attempt gains `live_unsettled: bool, final_attempt: bool`. Unsettled and not final gives Err(Incomplete), and the existing 900 s retry runs. Unsettled and final gives a complete measured run: Partial unless prices are real, marker written.
- final_attempt is computed purely as `retry_delay_secs(...)` returning None for the next attempt.
- The final-attempt case emits one `warn!(code = ErrorCode::WsGapConnectionState.code_str(), source = "xverify_unsettled_final", ...)`. It is a warn, not an error, so no filter matches and no page fires.
- Keep the debug_assert agreement between should_write_marker and classify_attempt with the new arguments.
- Why: the S3 archiver already refuses unapplied tables (wal_applied_gate), so the marker does not need to protect S3 from apply lag.

A7. Missing-minute runs (single pass, O(1) per key, three integers of state)
- all_keys is a BTreeSet ordered by (security_id, segment, minute). Per (security_id, segment), per I-P1-11:
  - (a) Stock run: consecutive judged missing traded minutes (vendor volume > 0). A zero-volume missing vendor minute is transparent. Any live or compared key of the same instrument, an excused tail minute or a new instrument breaks the run. The gap between two missing traded minutes is at most (1 + MAX_ZERO_VOLUME_GAP_MINUTES=5) * 60 s.
  - (b) Index run: consecutive missing minutes where the vendor bar shows movement (high > low, or open != the vendor previous-minute close for that index; one map probe). Flat vendor minutes break nothing and count nothing. Why: Dhan sends packets on change, so a flat index minute can legitimately have no packet.
- New DayComparison fields:
  - missing_live_index (all, for the record)
  - missing_live_index_moved
  - missing_live_runs (runs of at least MIN_PAGED_MISSING_RUN_MINUTES = 2, stocks and moved indices)
  - missing_live_index_runs
  - missing_live_longest_run_minutes
  - missing_live_run_instruments
  - missing_live_first_run_minute_ist
  - missing_judgeable
  - missing_live_unjudged
- When not judged, misses are counted in missing_live_unjudged and runs are not computed.
- Pure `divergence_page_reason(&DayComparison) -> Option<DivergencePageReason{Prices, MissingMinutes, Both}>` in boot.rs:
  - Prices = the existing is_catastrophic_divergence, UNCHANGED (more than half, §2.3k).
  - MissingMinutes = judged && missing_live_runs >= 1.
- Thresholds:
  - A run of 2 consecutive minutes survives the known single-minute shapes: boundary merge, the withheld hand-over bucket on a restart, the withheld open bucket on a mid-session exit.
  - Not a percentage, because no baseline exists. Assumed until the measurement step.

A8. Shadow measurement (PR-A only)
- run_once computes the reason but does NOT emit an error for MissingMinutes.
- The info "finished" line on every attempt carries: reason, missing_live_traded, missing_live_index, missing_live_index_moved, missing_live_runs, longest_run, run_instruments, first_run_minute, missing_judgeable, missing_live_unjudged, rest_failures, live_unsettled, mid_session_start (if the deps already expose the capture start; otherwise omit, Assumed).
- The existing price emit is byte-for-byte unchanged.
- run_option_pass keeps the price-only check, has no settle, and never pages.

A9. Persistence (crates/storage/src/dhan_live_crossverify_persistence.rs)
- Daily row and DAILY_COLUMNS gain:
  - LONG: missing_live_index, missing_live_index_moved, missing_live_runs, missing_live_longest_run_minutes, missing_live_unjudged
  - SYMBOL: missing_judgeable
  - BOOLEAN: live_unsettled, missing_minutes_would_page
- Created through the existing ALTER ADD COLUMN IF NOT EXISTS self-heal. No drop. DEDUP key unchanged.
- The cell audit adds kind `missing_live_unjudged` for misses that were not judged. kind is in the DEDUP key, so a later judged run never collides.
- Update the module-header DDL comment in the same change.

A10. Rule and runbook text in PR-A
- docs/claude-rules-full/project/no-rest-except-live-feed-2026-06-27.md new §12.15.7 (2026-10-06, both quotes plus the ITEM pointer):
  - Partial no longer hides Diverged.
  - The judging gates A2–A5.
  - The final-attempt acceptance; the marker is still exactly classify_attempt == Ok.
  - The settle wait is bounded inside the attempt.
  - The disk-pressure leg stays ungated and the live lane never waits for the check.
  - The end-of-day honest limit.
  - Triage section for reason=missing_minutes.
- One line in the stub .claude/rules/project/no-rest-except-live-feed-2026-06-27.md.
- Cross-link from docs/error-runbooks/dhan-live-crossverify-error-codes.md.
- Behaviour change, recorded: some days move from outcome=partial to diverged. git grep shows no reader of the outcome except the table. S3 is unaffected because is_measured covers both.

=== MEASUREMENT GATE between PR-A and PR-B (Blocker) ===
M1. Offline recompute on the box (read-only), for every trading day since 2026-09-24: run compare_day_in_scope with the PR-A rules over dhan_rest_1m_tape against today's fully applied candles_1m. Report per day: missing_live_runs, index-run count, how many sit in 15:30–15:39, and the days with a restart or late boot.
- Do NOT use stored cell_audit kind='missing_live' rows as the baseline. They come from the old 2-minute tail, retried runs keep stale rows (kind is in the DEDUP key), and the option pass writes NSE_FNO rows into the same table.
M2. Or accept ≥5 trading days of shadow rows where missing_minutes_would_page=true only on days with explained loss.
M3. If any unexplained would-page day remains, re-derive the run length or the index rule before PR-B. Record the measured counts in §2.11.

=== PR-B: page wiring ===
B1. Noise-lock new §2.11 (2026-10-06, both quotes verbatim plus the ITEM pointer plus the M1/M2 measured numbers) in docs/claude-rules-full/project/dhan-rest-only-noise-lock-2026-07-14.md. It widens the TRIGGER of the existing tv-<env>-errcode-ws-gap-03-xverify-diverged alarm to divergence_page_reason != None:
- Same pattern (byte-identical), period, threshold, notBreaching, no ok_actions.
- No new alarm, no EMF name, $0.
- States that it is a separate trigger, not a lowering of the §2.3k price threshold. Keeps the §2.3k row "claims the feed is verified on the strength of this alarm being quiet".
- Expected page causes: a mid-session restart or late boot that withholds 2 or more consecutive minutes. One latch per day across both reasons, so a second reason found by a later attempt appears only on the info line and the daily row.
- REJECT rows:
  - paging on a single isolated stock or index minute;
  - paging on a flat-vendor index minute;
  - paging when missing_judgeable != Judged;
  - ok_actions, a dimension, or a fourth xverify alarm;
  - paging from the option pass;
  - an end-of-day allowance below the published cutoff minus slack, or one derived from the wall clock;
  - removing the seal-backlog or unapplied gates.
- Add a bullet and the REJECT headlines to the stub .claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md.

B2. run_once replaces the price-only condition with `if let Some(reason) = divergence_page_reason(c) && !*divergence_paged`.
- One error! keeps code = ErrorCode::WsGapConnectionState.code_str() and source = "xverify_diverged".
- Fields: reason plus the A8 fields.
- One literal message: "our 1-minute candles disagree badly with Dhan's own record: wrong prices, missing minutes, or both (see reason)".

B3. terraform deploy/aws/terraform/error-code-alarms.tf ws-gap-03-xverify-diverged: rewrite desc only, to cover both reasons and point at §12.15.7. desc + suffix must stay ≤ 1024 characters. The pattern stays byte-identical.

B4. crates/aws-lambdas/src/telegram_webhook.rs ALARM_PHRASES text for that key: "🔷 DHAN: our recorded candles disagree badly with the broker's own record — wrong prices or missing minutes." The key is unchanged.

B5. Complexity: O(keys log keys) once a day, as today. Cold path. The only hot-adjacent additions are one Relaxed store on the 5 s catch-up arm and one on the spill/DLQ error path.

## Tests
["PR-A dhan_live_crossverify.rs: a_failed_fetch_no_longer_hides_a_real_divergence. rest_incomplete plus a judged missing traded minute or a diverged price gives Diverged. Must fail on origin/main.","a_rest_incomplete_run_with_no_finding_is_partial_never_clean.","each_unjudged_blocker_keeps_missing_minutes_out_of_the_verdict. For each of LiveTruncated, LiveUnsettled, SealBacklog, SealSpilledToday, UnappliedFramesToday and CutoffUnknown: missing_live gives Partial, missing_live_unjudged is counted and missing_live_runs = 0, while a price divergence still gives Diverged.","tail_excuse_follows_the_published_cutoff_not_the_wall_clock. Cutoff frozen at 15:36:00 and a read at 15:56: the 15:35 and 15:36 index minutes are TailUnsealed, never MissingLive. Cutoff at 15:39: the 2-minute floor applies.","published_cutoff_from_another_day_or_zero_is_cutoff_unknown.","const assert or test that SEAL_SETTLE_SLACK_SECS plus the TAIL floor covers CATCHUP_LATENESS_MARGIN_SECS.","begin_catch_up_seal_publishes_its_cutoff (dhan_feed_stack.rs), and the existing dhat catch-up phase stays at 0 allocations with the store.","stock_run_two_consecutive_traded_minutes_is_one_run; isolated_minutes_do_not_count; zero_volume_vendor_minutes_are_transparent_up_to_the_gap_cap; a_live_minute_between_breaks_the_run; runs_never_merge_across_security_id_or_segment (same id, different segment, per I-P1-11); tail_minutes_break_and_never_count.","index_run_needs_vendor_movement. Two flat vendor index minutes missing give no run. Two moved minutes give one index run. One moved minute gives no run but is counted in missing_live_index_moved.","proptest_missing_run_counts_are_bounded: 2*runs <= missing_live_traded + missing_live_index_moved, longest >= 2 when runs > 0, run_instruments <= runs, runs == 0 when not judged, and the verdict is never Clean when anything is incomplete or unjudged.","wal_settle_reuses_wal_apply_verdict. Suspended candles_1m with writerTxn == sequencerTxn is unsettled. Non-zero bufferedTxnSize alone is not settled. An absent row or HTTP error is unsettled.","Source scan run_cross_verification_settles_before_the_live_read_and_after_started: `started` precedes the settle, the settle precedes read_live_side, it is bounded by XVERIFY_SETTLE_MAX_WAIT_SECS, and run_option_pass passes None.","boot.rs: classify_attempt_unsettled_is_incomplete_only_before_the_final_attempt. Extend test_classify_attempt_agrees_with_the_marker_rule_everywhere with live_unsettled and final_attempt. final_attempt is derived from retry_delay_secs.","boot.rs: test_worst_case_day_fits_every_attempt_before_the_evening_stop and test_option_pass_budget_fits_its_target_cap_at_the_pacer still pass unchanged.","boot.rs: test_divergence_page_reason_cases. Prices when more than half the price fields differ. MissingMinutes for one stock run of 2. MissingMinutes for one moved-index run of 2. Both. None for an isolated stock minute. None for a single moved index minute. None for flat index minutes. None when not judged. None when vacuous.","boot.rs PR-A source scan: run_once has exactly one xverify_diverged emit and its condition is still is_catastrophic_divergence (shadow phase). The info finished line contains missing_live_runs and missing_judgeable.","Update compare_day_degraded_downgrades_clean_to_partial_never_up and test_run_report_degraded_flag_forces_the_verdict_down to the new semantics. Their old assertion (degraded plus real gives Partial) is the defect being fixed.","Update the comparison() helper in boot.rs and test_run_day_retries_through_the_pure_bound / test_marker_write_needs_a_complete_run for the new classify_attempt arguments.","storage dhan_live_crossverify_persistence.rs: the daily DDL and ALTER list contain the nine new columns. The DEDUP key is unchanged. The cell kind missing_live_unjudged round-trips. The header DDL comment matches DAILY_COLUMNS.","storage: seal_spill_and_dlq_writes_stamp_seal_spilled_day; SealBacklogProbe reads pending and escalation counts.","PR-B boot.rs: test_divergence_page_is_one_latched_error_emit. The xverify_diverged emit is inside error!, uses divergence_page_reason, is guarded by divergence_paged, and has one literal message.","Unchanged and must stay green: test_every_xverify_alarm_source_has_a_live_error_emit, test_run_option_pass_never_touches_the_marker_the_daily_row_or_a_page, test_is_catastrophic_divergence_needs_more_than_half_the_price_fields.","Scoped runs: cargo test -p tickvault-app; cargo test -p tickvault-storage; cargo test -p tickvault-aws-lambdas (alarm_phrase_coverage_guard); cargo test -p tickvault-common --test cloudwatch_app_alarms_wiring --test error_code_alarm_coverage_guard --test error_code_paging_filter_drift_guard.","Measurement gate M1/M2 (Blocker before PR-B, run on the box, read-only): offline recompute of compare_day_in_scope with the PR-A rules over dhan_rest_1m_tape against fully applied candles_1m for every day since 2026-09-24, or ≥5 trading days of shadow missing_minutes_would_page rows. Record the counts in §2.11."]

## Guards to update
["crates/app/src/dhan_live_crossverify_boot.rs::test_every_xverify_alarm_source_has_a_live_error_emit: no edit, must stay green.","crates/app/src/dhan_live_crossverify_boot.rs::test_run_option_pass_never_touches_the_marker_the_daily_row_or_a_page: no edit; the option pass gets settle=None and no emit.","crates/app/src/dhan_live_crossverify_boot.rs::test_is_catastrophic_divergence_needs_more_than_half_the_price_fields: kept, threshold unchanged; add test_divergence_page_reason_cases beside it.","crates/app/src/dhan_live_crossverify_boot.rs::test_classify_attempt_agrees_with_the_marker_rule_everywhere, test_run_day_retries_through_the_pure_bound, test_marker_write_needs_a_complete_run, test_worst_case_day_fits_every_attempt_before_the_evening_stop, test_option_pass_budget_fits_its_target_cap_at_the_pacer, and the comparison() helper: thread the new arguments and fields through.","crates/app/src/dhan_live_crossverify.rs::compare_day_degraded_downgrades_clean_to_partial_never_up and test_run_report_degraded_flag_forces_the_verdict_down: rewrite to the new semantics.","crates/common/tests/cloudwatch_app_alarms_wiring.rs: pattern unchanged, stays green.","crates/common/tests/error_code_alarm_coverage_guard.rs: no new code, stays green.","crates/common/tests/error_code_paging_filter_drift_guard.rs::every_alarm_description_fits_the_aws_ceiling_with_its_suffix and every_alarm_description_in_every_tf_file_fits_the_aws_ceiling: the rewritten diverged desc plus suffix must be ≤ 1024 characters (PR-B).","crates/aws-lambdas/tests/alarm_phrase_coverage_guard.rs: key unchanged; the phrase text is reworded in telegram_webhook.rs ALARM_PHRASES (PR-B).","crates/storage/tests/dedup_segment_meta_guard.rs and crates/storage/tests/table_schema_lockstep_guard.rs: DEDUP key unchanged, daily columns added, stays green.","The existing dhat catch-up phase (dhat_multi_tf_fold / the dhan_feed_stack catch-up dhat) must stay zero-alloc with the CATCHUP_CUTOFF_PUBLISHED store.","deploy/aws/terraform/error-code-alarms.tf ws-gap-03-xverify-diverged: pattern byte-identical, desc only (PR-B)."]

## Residual risks
["Unknown: no production baseline exists. QuestDB at 127.0.0.1:9000 refused the query from this container. The 2-minute run length and the vendor-movement index rule are Assumed until M1/M2. This is why the page is split into PR-B behind the measurement.","Assumed: Dhan sends packets on change and holds no state back for a whole minute. If the vendor tape prints moved index bars that our feed never receives (closing window 15:30–15:40, or halts), PR-B pages wrongly. M1 reports the 15:30–15:39 clustering explicitly.","Restart and late-boot days that withhold 2 or more consecutive minutes will page. That is true of the stored record and is named in §2.11 as an expected cause; exempting it needs a separate dated row.","Conservative gates (any seal spilled today, any unapplied frame today, seal backlog, unsettled) turn off missing-minute judging for the whole day after a morning QuestDB blip. That is a false negative by design, visible as missing_judgeable on the daily row and the info line.","A4's time-to-capture-sequence mapping is Assumed (sequence base = receipt nanos). If it is not exact, the fallback is any unapplied mark today, which fails toward not judging.","The settle wait (at most 60 s) comes out of run_budget_secs. On a slow vendor day budget_elapsed (Incomplete) becomes slightly more likely. The 17:30 fit stays intact because `started` precedes the settle.","On the final attempt an unsettled run writes the marker as measured Partial. S3 is still protected by wal_applied_gate, but the day's missing minutes go unjudged.","One divergence_paged latch is held in memory: a second reason found by a later attempt does not page, and a process restart before the marker can page twice the same day. The second already exists today for the price page.","Two daily rows can exist for one day (DEDUP includes outcome). Readers must take the row with missing_judgeable=Judged and the outcome that was measured; the new columns make the rows distinguishable.","Days that stored partial now store diverged. Nothing in the repository reads outcome except the table (git grep), and the marker and S3 are unaffected.","Authority: the operator quotes are general. The rows rely on them answering the enumerated plan ITEM (the §2.3k shape). Assumed sufficient; a reviewer may ask for a narrower quote."]

