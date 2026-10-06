# FINAL SPEC missing-run (ship=true)

## Summary
The gap is real. Verified on origin/main 60bdfd97a:
- The verifier task is a bare tokio::spawn; nothing awaits its handle.
- No alarm watches for a trading day that ends with no marker and no final page.
- A late boot starts a first attempt of up to 960 s without checking it can finish before the 17:30 stop.
- An afternoon boot after today's marker already exists sleeps until tomorrow, so earlier unmarked days are never caught up.
- An unmarked day is archived unverified once it is past MAX_CROSSVERIFY_HOLD_DAYS (3).

Review 1's first finding changes the design, and it is a defect already on main. apply_crossverify_hold runs over the whole daily worklist. Candles partitions first reach it at age 16 and Standard ones at age 91, but write_daily_marker sweeps any marker more than 7 days old (DAILY_MARKER_SWEEP_DAYS). So verified days read as unverified and take OverrideAfterCeiling. The STORAGE-GAP-04 'archived WITHOUT a cross-verification' error therefore fires on routine passes today. A filter on it, as first designed, would have paged every day. Fix: a new BeyondGateReach decision that archives silently, with a counter and an info line, once a partition is older than the 7-day marker reach. Only ages 4 to 7 can override.

The deadline race (both reviews) is fixed by:
- an attempt end bound of 17:23 (62,580 s), enforced by tokio::time::timeout;
- the worker evaluating the missing check itself, after its day's work;
- a supervisor fallback at 17:24 (62,640 s = SCHEDULED_STOP_WINDOW_START (17:25) minus 60 s), used only if the worker never reported.

Other changes folded in:
- Retry interval 900 to 760 s, so the locked '4 worst-case attempts fit' property still holds.
- The 'final page sent' state is now a persisted marker, so it survives a restart.
- The missing line is edge-triggered: it fires for today, or for a day at its last chance, never every day.
- Days at their last chance are attempted first.
- A late attempt runs with a shrunk budget instead of being refused; a cut-short run is Incomplete and writes no marker.
- The marker write is re-read after writing.
- The past-day catch-up runs whatever today's marker says.
- Reviewer-found guards added: ALARM_PHRASES, cloudwatch_app_alarms_wiring floor 4 to 5, desc length cap.

Dropped, with reasons:
- The 'host only' dimension: errcode-map filters carry no dimensions.
- A non-trading-day catch-up: on a non-trading weekday the holiday gate stops the box at about 08:30, and it does not run at weekends.
- The PR #2026 plan conflict: #2026 has merged.

The supervisor restart can be reached only in debug and test builds, because release builds use panic = abort. Detection is in-process only.

Approval (2026-10-06, quoted verbatim): 'Go ahead with whatever you want dude' / 'See do everything whatever is recommended dude okay?'.

ORDER: rule files and the plan item first, code after, one PR. Every new error! carries code = ErrorCode::X.code_str(). Everything here is cold path; nothing touches the frame drain.

0. RULE FILES AND PLAN, WRITTEN FIRST. Quote verbatim, dated 2026-10-06: "Go ahead with whatever you want dude" and "See do everything whatever is recommended dude okay?".

(a) docs/claude-rules-full/project/no-rest-except-live-feed-2026-06-27.md, new section 12.15.7. It supersedes these 12.15.5 rows:
- 'Gap between attempts': 900 becomes 760 s.
- 'Last start': an attempt must finish by XVERIFY_LAST_END = 17:23 IST, not 17:30.
- 'Worst case': all 4 worst-case attempts still fit, re-pinned.
It also supersedes the 12.15.1 / 12.15.3 'ONCE per trading day' wording: one post-close session per running trading day may verify today plus up to MAX_CROSSVERIFY_HOLD_DAYS earlier unmarked trading days. It records:
- the past-day catch-up;
- the first-attempt fit and budget shrink;
- the attempt timeout;
- the marker re-read;
- the supervisor;
- the missing-run line;
- the BeyondGateReach archive decision.
Record as a FIX that today's boot catch-up already breaks the 12.15.5 REJECT row 'starts an attempt that could still be running at 17:30'.
REJECT rows for 12.15.7:
- a past-day attempt during 09:00-15:41 or the pre-open;
- verifying a day older than MAX_CROSSVERIFY_HOLD_DAYS;
- a marker for any day on anything but classify_attempt Ok plus a successful marker re-read;
- an attempt that can end after 17:23;
- paging per past day;
- a past-day option pass;
- weakening the S3 gate, the hold ceiling or the disk-pressure leg.
NOT claimed:
- out-of-process detection;
- same-day detection when the box is down all afternoon;
- that a past day is compared against that day's own subscription (it uses today's target list, so unsubscribed instruments read missing_live);
- that a day whose candles the ungated disk-pressure leg dropped can be verified (it reads vacuous: no marker, overridden at the ceiling).
Mirror the headlines in the .claude/rules/project stub.

(b) docs/claude-rules-full/project/dhan-rest-only-noise-lock-2026-07-14.md, new section 2.11, with:
- the 2-row alarm table (E below) and its exact patterns;
- the shape: dimensionless (errors.jsonl carries no host), notBreaching, ok_recovery=false, period 3600, eval 1, dta 1;
- cost (Assumed list price): +2 alarms $0.20/mo, plus 2 sparse log-derived metrics billed only in hours with datapoints, worst case about $0.60/mo, no EMF name;
- the noise budget per unverifiable day: at most 1 xverify_missing on the day itself (suppressed if xverify_failed or xverify_vacuous already paged), at most 1 at the last chance, plus 1 hold-override page;
- the known extra noise: a Muhurat or special session on a day the box is off pages as missing and then as overridden, and a hold-override whose S3 copy keeps failing re-pages once per day.
REJECT rows for 2.11:
- ok_recovery=true;
- any dimension;
- a filter on the bare code without the $.source clause;
- per-day or per-attempt pages;
- filtering xverify_task_*, xverify_catchup_* or xverify_attempt_* sources;
- removing the edge trigger.
Add the 2.11 bullet to the stub's allowed set.

(c) .claude/rules/project/observability-architecture.md 'Which codes page', inside the 'Filtered+alarmed codes' block: add STORAGE-GAP-04 (source crossverify_hold_override only) and the WS-GAP-03 xverify_missing arm.

(d) Runbooks:
- docs/error-runbooks/dhan-live-crossverify-error-codes.md: xverify_missing triage, xverify_task_panicked / xverify_task_dead / xverify_task_exited, xverify_catchup_skipped, xverify_attempt_timed_out, xverify_marker_write_failed.
- docs/error-runbooks/wave-2-error-codes.md: the STORAGE-GAP-04 hold-override arm and its alarm.

(e) Plan: add one item to .claude/plans/active-plan-audit-fixes-2026-09-26.md (APPROVED; names crates/app and crates/storage). It needs all 6 required sections, the 15-row and 7-row matrices and the Z+ checklist. Do NOT add a 6th active-plan file (V7 cap = 5, exactly 5 exist).

A. TIME BOUNDS (crates/app/src/dhan_live_crossverify_boot.rs)
- XVERIFY_DEADLINE_SECS_OF_DAY_IST: u64 = shutdown_class::SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST as u64 - 60. That is 62,640 = 17:24.
- XVERIFY_LAST_END_SECS_OF_DAY_IST = XVERIFY_DEADLINE - 60 = 62,580 = 17:23.
- XVERIFY_RETRY_INTERVAL_SECS: 900 becomes 760.
- const_asserts:
  - RUN_SECS + attempt_max_secs(600) + 3 * (760 + attempt_max_secs(600)) <= LAST_END, i.e. 56,460 + 960 + 3 * 1,720 = 62,580;
  - LAST_END < DEADLINE < SCHEDULED_STOP_WINDOW_START < EVENING_STOP;
  - RUN_SECS < LAST_END.
- retry_delay_secs and option_pass_fits bound on LAST_END instead of EVENING_STOP. EVENING_STOP stays defined as 17:30, for documentation and the assert.
- New pure fn attempt_budget_secs(now_secs, config_budget) -> Option<u64>:
  - room = LAST_END - now - token wait (300) - PERSIST_MARGIN (60), saturating;
  - returns Some(min(config_budget, room)) if that is >= XVERIFY_MIN_ATTEMPT_BUDGET_SECS = 120, else None.
  Every attempt start calls it: today's first attempt, each retry, each past-day attempt. A shrunk run ends with budget_elapsed=true, so classify_attempt returns Incomplete and no marker is written (fail closed). None logs warn!(code = WsGapConnectionState, source = "xverify_catchup_skipped", day, reason = "no_time_before_stop"). No filter matches that source; the missing line reports the consequence.
- Every attempt is wrapped in tokio::time::timeout(Duration::from_secs(attempt_max_secs(budget))). On elapse: warn! source "xverify_attempt_timed_out", Err(AttemptFailure::Incomplete). The synchronous persist_report cannot be cut halfway, which is accepted: its time is inside PERSIST_MARGIN.

B. MARKER RE-READ
- crates/app/src/daily_task_marker.rs: add #[must_use] pub fn write_daily_marker_checked(task, date) -> bool. It calls write_daily_marker, then returns daily_marker_exists(task, date). Existing callers are unchanged.
- run_once (and the past-day attempt) uses it in the Ok arm. On false: warn!(code = WsGapConnectionState, source = "xverify_marker_write_failed", day), return Err(AttemptFailure::NotPersisted).
- In daily_task_marker.rs add const _: () = assert!(DAILY_MARKER_SWEEP_DAYS >= tickvault_storage::partition_archive::CROSSVERIFY_GATE_REACH_DAYS).

C. PERSISTED PAGE STATE
- New marker tasks: CROSSVERIFY_PAGED_MARKER_TASK = "dhan_live_crossverify_paged" and CROSSVERIFY_MISSING_PAGED_MARKER_TASK = "dhan_live_crossverify_missing_paged".
- report_final_failure writes the paged marker for today in both arms, after the error!.
- The missing line writes the missing-paged marker for today after it emits.
- divergence_paged stays in memory per run_day. That a same-day restart can re-page xverify_diverged is recorded as a limit; it is existing behaviour.

D. WORKER: one post-close session per running day
Replace the body of the spawn loop with post_close_session(deps, targets, today, now).

Wake rule: at boot, if now >= RUN_SECS and now < DEADLINE and today is a trading day, run the session immediately, WHATEVER today's marker says. This fixes the afternoon-boot hole. Otherwise sleep to the next RUN_SECS.

Steps, in order:
1. last = past_days_at_last_chance(...). Pure; see E.2. One attempt each, oldest first, while attempt_budget_secs is Some.
2. If today is a trading day and today's marker is absent: run_day(today), with retries as now, bounded on LAST_END.
3. run_option_pass(today), as now, guarded by option_pass_fits on LAST_END.
4. The remaining past_days_to_verify not in last: one attempt each, oldest first, while the budget fits.
5. let report = missing_run_report(...); if Some, emit the missing line (E.3) and write the missing-paged marker.
6. Store today's days-since-epoch into reported_day: Arc<AtomicI64>, shared with the supervisor.

Past-day attempt = run_past_day(deps, targets, day): wait_for_jwt, then run_cross_verification(day, day_start_of(day), targets, config with the shrunk budget), then persist_report, then classify_attempt, then the B marker write for THAT day on Ok only. It never pages and has no option pass. Divergence on a past day is logged at warn! with source "xverify_catchup_diverged" and no page.
Counter tv_dhan_xverify_catchup_days_total{outcome} with outcomes recorded, vacuous, failed, incomplete, not_persisted, timed_out, skipped. All seeded at 0 at spawn; local /metrics only, no EMF.
REST volume: at most 3 days x ~870 paced calls = about 2,600, against the Data API's 100,000/day. Post-close only, so the live lane never waits for it.
Factor the shared attempt core (jwt, run, persist, classify, checked marker) out of run_once into one fn, so the today and past-day paths cannot diverge. run_once keeps its paging behaviour.

E. PURE DECISIONS (app, no IO; the calendar and marker lookups are passed in as closures)

E.1 past_days_to_verify(today, is_trading, has_marker) -> ArrayVec<NaiveDate, 3>
Trading days D with 1 <= (today - D).num_days() <= MAX_CROSSVERIFY_HOLD_DAYS and !has_marker(D), oldest first. O(3).

E.2 last_chance(D, today, next_trading_day) -> bool
(next_trading_day(today) - D).num_days() > MAX_CROSSVERIFY_HOLD_DAYS. Tomorrow's ~15:45 archive pass overrides D before tomorrow's session can reach it. This covers the Friday plus Monday-holiday case. past_days_at_last_chance is the subset of E.1 that is last chance.

E.3 missing_run_report(today, is_trading, has_marker, today_paged, next_trading_day) -> Option<MissingRun>
- today_unverified = is_trading(today) && !has_marker(today) && !today_paged.
- last_chance_days = { D in past_days_to_verify(today) | last_chance(D) }. Today itself also counts as last chance if last_chance(today) holds.
- Fires iff today_unverified || !last_chance_days.is_empty(), and the missing-paged marker for today is absent.
- MissingRun carries:
  - all_unverified (today plus E.1);
  - oldest_unverified;
  - overrides_at = next_trading_day(today) for the last-chance days;
  - worker_alive.

The line:
error!(code = ErrorCode::WsGapConnectionState.code_str(), source = "xverify_missing", today_unverified, unverified_days = %list, oldest_unverified = %d, last_chance_days = %list, overrides_at = %date, worker_alive, "Dhan 1-minute cross-verification: these trading days have NO measured check. Their S3 archive is held; the last-chance days are archived UNVERIFIED at the archive pass on <overrides_at> (intraday tables; candles stay in QuestDB 15 days)").

Gauge tv_dhan_xverify_unverified_days: local only.

F. SUPERVISOR
spawn_dhan_live_crossverify returns the supervisor's JoinHandle; main.rs keeps binding it. CrossverifyBootDeps derives Clone; targets is Arc<[XverifyTarget]>.
The supervisor holds worker: Option<JoinHandle<()>>, polled only while Some (no poll after completion), and a restarts count. It runs tokio::select! over two arms.

Worker arm:
- First Err(JoinError): error!(code = WsGapConnectionState, source = "xverify_task_panicked", ?err), increment tv_dhan_xverify_task_restarts_total (seeded at 0), respawn once. The new worker uses the D wake rule.
- Second Err: error! source "xverify_task_dead"; worker = None.
- Ok(()): error! source "xverify_task_exited"; worker = None.
None of these three sources is filtered.

Timer arm:
- Sleep until the next DEADLINE.
- If today is a trading day and reported_day != today, run the same E.3 evaluation (worker_alive = worker.is_some()), emit the line if Some, write the missing-paged marker, then re-arm.
- The arm keeps running after the worker has died.
Under panic = abort (Cargo.toml release), a panic aborts the process before the worker arm can run; in production, systemd restart plus the D wake rule is the real recovery. State this in the TEST-EXEMPT comment and in 12.15.7.

For testability, the supervisor and E take a clock: Fn() -> (NaiveDate, u64 secs_of_day) plus marker closures. Production passes today_ist / now_ist_secs_of_day / daily_marker_exists.

G. STORAGE (crates/storage/src/partition_archive.rs)
- pub const CROSSVERIFY_GATE_REACH_DAYS: i64 = 7, with const_assert CROSSVERIFY_GATE_REACH_DAYS > MAX_CROSSVERIFY_HOLD_DAYS.
- New variant CrossverifyHold::BeyondGateReach.
- crossverify_hold_decision order: Unparseable, then is_verified gives Proceed, then age > REACH gives BeyondGateReach, then age > MAX gives OverrideAfterCeiling, else Hold.
- apply_crossverify_hold:
  - BeyondGateReach keeps the partition; tv_partition_archive_hold_beyond_reach_total (seeded at 0) is incremented once per pass by the count; one info! per pass.
  - The existing override error! gains source = "crossverify_hold_override" and oldest_partition_day. The code stays StorageGap04S3ArchiveFailed and the message is unchanged.
- No other change: same gate closure in main.rs, same ceiling, the disk-pressure leg stays ungated, same per-run cap order.

H. TERRAFORM (deploy/aws/terraform/error-code-alarms.tf, inside error_code_alerts, same shape as the xverify entries)
- "ws-gap-03-xverify-missing": pattern { $.code = "WS-GAP-03" && $.level = "ERROR" && $.source = "xverify_missing" }.
- "storage-gap-04-hold-override": pattern { $.code = "STORAGE-GAP-04" && $.level = "ERROR" && $.source = "crossverify_hold_override" }.
- Both: period 3600, threshold 1, eval 1, dta 1, ok_recovery = false, notBreaching, no dimensions.
- Each desc is plain English, names Dhan, gives the runbook path, and stays under 1,024 chars.
- The measured-count comment at about line 108 goes from 23 to 25.
- crates/aws-lambdas/src/telegram_webhook.rs ALARM_PHRASES gains:
  - errcode-ws-gap-03-xverify-missing: 'DHAN: the end-of-day price cross-check never finished for some days — their backup to S3 will go unchecked'
  - errcode-storage-gap-04-hold-override: 'DHAN: a trading day was backed up to S3 without its price cross-check'
  Both keep the diamond emoji prefix.

Complexity: E is O(MAX_CROSSVERIFY_HOLD_DAYS + 1) calendar lookups and stats, at most twice a day. The catch-up is O(days x targets) paced REST, post-close. G is O(1) more per partition.

## Tests
["dhan_live_crossverify_boot: test_deadline_and_last_end_derive_from_the_scheduled_stop_window (DEADLINE == SCHEDULED_STOP_WINDOW_START - 60, LAST_END == DEADLINE - 60, ordering vs RUN_SECS and EVENING_STOP)","test_worst_case_day_fits_every_attempt_before_the_last_end (re-pinned: 4 attempts, budget 600, interval 760 end exactly at 62_580)","test_retry_delay_secs_bounds re-pinned to LAST_END","test_option_pass_fits_bounds_on_last_end","test_attempt_budget_secs_shrinks_then_refuses: 15:41 -> Some(600); LAST_END-300-60-200 -> Some(200); below 120 -> None; u64::MAX saturates to None","test_every_attempt_start_consults_attempt_budget_secs_and_is_wrapped_in_timeout (source scan of run_day and run_past_day bodies)","test_past_days_to_verify_oldest_first_trading_only_inside_window (weekends, holidays, today excluded, age>MAX excluded, marked excluded, len<=3)","test_last_chance_handles_friday_and_monday_holiday (next_trading_day Tuesday makes Friday last-chance on Friday itself)","proptest_missing_run_report: over today trading/holiday x today marker x today_paged x missing_paged x 0..=3 unmarked past days x weekend/holiday gaps -> fires iff (silent unmarked trading today) or (some last-chance unmarked day) and missing_paged absent; never fires on a clean week; never fires for a final-paged today alone","test_post_close_session_runs_last_chance_days_before_today (source scan / mock ordering)","test_boot_after_run_time_with_today_marked_still_runs_catch_up (wake rule, injected clock + marker closures)","test_past_day_attempt_writes_marker_only_for_that_day_on_ok_and_checked_write (source scan: write_daily_marker_checked(CROSSVERIFY_MARKER_TASK, day) only in the Ok arm)","test_past_day_path_never_pages (source scan: no xverify_failed / xverify_vacuous / xverify_diverged / xverify_missing literal in run_past_day)","test_marker_write_failure_returns_not_persisted (checked write false -> Err(NotPersisted))","test_report_final_failure_writes_paged_marker_in_both_arms (source scan)","test_supervisor_respawns_once_then_stops_and_deadline_still_fires (tokio paused clock, injected clock and marker store, worker closure that panics twice; JoinHandle never polled after completion)","test_supervisor_deadline_skips_when_worker_reported_today","test_every_xverify_alarm_source_has_a_live_error_emit extended with xverify_missing","test_catchup_and_restart_counters_seeded_at_zero","daily_task_marker: test_write_daily_marker_checked_true_on_success_false_on_unwritable_dir (temp dir) + const assert DAILY_MARKER_SWEEP_DAYS >= CROSSVERIFY_GATE_REACH_DAYS","partition_archive: crossverify_hold_decision_beyond_reach_proceeds_silently (age 8, 16, 91 unverified -> BeyondGateReach; age 4..=7 -> OverrideAfterCeiling; age 0..=3 -> Hold; verified any age -> Proceed)","partition_archive: test_regression_verified_day_aged_16_and_91_never_overrides (marker swept by real sweep_old_markers_in semantics via the gate closure shape -> BeyondGateReach, no error)","partition_archive: test_hold_override_line_carries_its_alarm_source (source scan: error! in apply_crossverify_hold has source = \"crossverify_hold_override\"; tf has the matching filter)","partition_archive: const assert CROSSVERIFY_GATE_REACH_DAYS > MAX_CROSSVERIFY_HOLD_DAYS + unit mirror","aws-lambdas alarm_phrase_coverage_guard green with the two new phrases (both name DHAN)","common cloudwatch_app_alarms_wiring: WS-GAP-03 filter floor 4 -> 5 with comment re-derived","storage alarm_description_length_guard green (<1024 chars)","existing guards re-run green: error_code_paging_filter_drift_guard, error_code_alarm_coverage_guard, critical_errcode_alarm_coverage_guard, live_lane_paging_lockstep_guard, partition_archive_guard, dhan noise-lock and REST-lock phrase guards"]

## Guards to update
["crates/app/src/dhan_live_crossverify_boot.rs tests: test_worst_case_day_fits_every_attempt_before_the_evening_stop (rename/re-pin to LAST_END + 760), test_retry_delay_secs_bounds, option_pass_fits test, test_every_xverify_alarm_source_has_a_live_error_emit (+xverify_missing), TEST-EXEMPT comment on spawn_dhan_live_crossverify","crates/storage/src/partition_archive.rs tests: an_unverified_day_past_the_ceiling_is_archived_anyway and the MAX bound test gain the BeyondGateReach rows","crates/storage/tests/partition_archive_guard.rs (re-run; gate, ceiling, and disk-pressure pins unchanged)","crates/aws-lambdas/src/telegram_webhook.rs ALARM_PHRASES + crates/aws-lambdas/tests/alarm_phrase_coverage_guard.rs","crates/common/tests/cloudwatch_app_alarms_wiring.rs WS-GAP-03 floor 4 -> 5 and its comment","crates/common/tests/error_code_paging_filter_drift_guard.rs (needs the observability-architecture.md paging list edit)","crates/common/tests/error_code_alarm_coverage_guard.rs (comment listing xverify filters)","crates/storage/tests/alarm_description_length_guard.rs and critical_errcode_alarm_coverage_guard.rs (re-run)","deploy/aws/terraform/error-code-alarms.tf measured-count comment 23 -> 25","dhan noise-lock and REST-lock phrase guards (live_lane_paging_lockstep_guard and similar) re-run after the 2.11 and 12.15.7 edits"]

## Residual risks
["Verified from code, not from live logs (CloudWatch MCP unreachable): the routine STORAGE-GAP-04 override line on candles (age 16) and Standard (age 91) partitions fires today. The BeyondGateReach fix removes it; the count of past occurrences is Unknown.","After the fix, an unverified day's candles partitions (first reached at age 16) archive silently as BeyondGateReach. The day was already paged by xverify_missing (on the day and at its last chance) and, Assumed, by the override page for its intraday-class partitions at age 4. If a day has no intraday-class partitions, only the missing line reports it.","Assumed: Dhan intraday serves a 1-3-day-old day unchanged (docs/dhan-ref says last 5 years; whether the vendor amends the tape later is unverified). An amendment would show up as divergence on the catch-up.","A past day is compared against today's target list. Instruments not subscribed that day read missing_live, so a Partial result can still write a marker. That is the same rule as same-day runs and is recorded in 12.15.7.","Assumed: QuestDB still holds candles_1m for the past day. The ungated disk-pressure leg (pressure_hot_days) can drop them, which makes the catch-up vacuous: fail-closed, no marker, overridden at the ceiling.","Detection is in-process only. If the box is down all afternoon, the first report comes at the next running day's session or 17:24. Friday plus a Monday holiday: Friday is reported on Friday only if the process ran Friday; otherwise only the hold-override page reports it.","The supervisor restart arm can run only in debug and test builds, because the release profile uses panic = abort. In production, recovery is the systemd restart plus the D wake rule.","The shrunk-budget and timeout paths cut long runs short as Incomplete. On a day when QuestDB or the vendor is slow, more days may end unmarked and be overridden. This is the intended fail-closed trade.","Retry interval 760 s and end bound 17:23 move values locked by 12.15.5. They are valid only if 12.15.7 lands in the same PR, first.","If tomorrow's archive pass fails and retries, the hold-override page can repeat once per day. A Muhurat or special session on a day the box is off pages as missing up to twice and as overridden once. Both are recorded noise in 2.11.","The approval is a general go-ahead ('Go ahead with whatever you want dude' / 'See do everything whatever is recommended dude okay?'). Whether this exact item list was shown to the operator before it is Unknown, so reviewers may require the enumerated recommendation list to be cited next to the quotes.","Cost (+$0.20/mo alarms, about $0.60/mo worst-case sparse metrics) is Assumed from list prices; the budget was not read live.","EventBridge stop jitter is covered by the existing 17:25 SCHEDULED_STOP_WINDOW_START plus 60 s for log shipping; the shipping time was not measured."]

