# FINAL SPEC targets-at-fire (ship=true)

## Summary
Ship, with the review fixes folded in. Verified on origin/main (code reading only, no run):
- The cross-check's target list is built once at boot in spawn_dhan_live_crossverify and never sees widen_running_session.
- The live set exists only in the private RunningWiden.on_wire; no main-feed spot set is published anywhere.
- On a fallback day, resolve_live_universe returns 4 IDX_I targets. A complete 4x375-minute run writes the marker, and crossverify_hold_decision then releases the whole day's S3 archive.

Both reviews' fatal flaws are real:
1. In compare_day_in_scope, `if degraded {Partial} else if real {Diverged}` means passing degraded=true for trimmed minutes would hide real divergence. Fix: a separate post-compare Clean-to-Partial downgrade only.
2. The published set lives only in the process. A restart after 15:41 on a fallback day would boot today's list with every instrument marked "held all day", read Diverged, and still write the marker. Fix: a day file with min-merge, like depth-held-today.json, and a mid-session boot stamps its boot set with the boot second.

Also folded in:
- Coverage is measured in planned instrument-minutes, not in target counts. This closes the "widen at 15:29 reads 100%" hole.
- The planned set is captured when the list is chosen, not re-read from disk at 15:41.
- The covered-from boundary uses the first live bar, capped at the subscribe minute + 300 s.
- Both the vendor and live sides are trimmed.
- Date-scoped stamps, a NotPublished shortfall, the third 4-index arm, publishing when instruments are freed, coverage columns on the daily row, a once-per-day unverifiable counter, coverage facts carried back to the page, a non-retried too-many-targets case, and the Telegram vacuous phrase reworded.

Dropped, with reasons:
- Filtering the alarm on reason: forbidden by the noise lock.
- A fourth alarm: not needed; the existing vacuous source is reused.
- Dropping the IndexFallback refusal: the task explicitly asks for it. It is kept and labelled as a deliberate second page that names the upstream alarm.

Process blocker: the gate allows at most 5 active-plan files and there are already 5. The work must be a new item in an existing APPROVED plan (active-plan-feed-hardening.md), or a plan must be archived first.

AUTHORITY AND ORDER (rule-file-first). These land in the same PR, before any code:
- The rule-file edits listed at the end of this spec.
- A new item in .claude/plans/active-plan-feed-hardening.md, with the 6 sections and the 15-row and 7-row matrices. A new plan file would break the V7 cap: 5 active-plan files exist on origin/main (Verified).

The dated approval quoted verbatim, 2026-10-06: "Go ahead with whatever you want dude" and "See do everything whatever is recommended dude okay?". Per the §28.2 precedent these approve only the enumerated changes below.

=== PART A: one day-scoped record of the spot universe actually subscribed ===

A1. New module crates/app/src/subscribed_spots.rs, so dhan_live_universe.rs is not bloated.

Types:
- enum OnWire { BeforeOpen, At(u32 /*IST sec of day*/), Never }, ordered BeforeOpen < At(a) < At(b) for a<b < Never.
- struct SpotEntry { instrument: SubscribeInstrument, planned: bool, on_wire: OnWire }.
- struct SubscribedSpots { ist_day: i64, entries: Vec<SpotEntry> /*deduped on (security_id, segment)*/, master_sourcing: bool, non_fallback_seen: bool, todays_list_seen: bool }.

State:
- static SUBSCRIBED_SPOTS: arc_swap::ArcSwapOption<SubscribedSpots>.
- A process-local Mutex<Recorder> holding prior: HashMap<(u64,u8), SpotEntry> (loaded from the day file at boot, never mutated) and own: HashMap<(u64,u8), SpotEntry> (this process). Both are bounded at the main-feed capacity of 25,000; a new key past that is refused and counted on tv_subscribed_spots_refused_total.

Functions:
- merge_entries(prior, own) -> Vec<SpotEntry>. Pure. planned = OR of both. on_wire = own's value when own has the key, else prior's. One exception: when own says Never and prior says At or BeforeOpen, own wins, because a refusal in this process is current truth. Otherwise min.
- rebase_to_day(snapshot, today) -> SubscribedSpots. Pure. For a snapshot from an earlier IST day (process ran past midnight): every At/BeforeOpen becomes BeforeOpen, Never stays Never, planned and todays_list_seen reset to false.
- record_boot(...), record_widen(...) and record_freed(...) update own, then store merge(prior, own) into SUBSCRIBED_SPOTS and write the day file.
- subscribed_spots_snapshot(now) -> Option<Arc<SubscribedSpots>>. One lock-free load, O(1); the caller applies rebase_to_day.

Day file: data/instrument-cache/subscribed-spots-today.json, holding the IST day plus entries. Written as temp file, fsync, rename, the same shape as depth_subscription_view's held-today file. A file from another day loads nothing. A damaged file loads nothing, with a coded warn using code = ErrorCode::WsGapConnectionState.code_str().

Cost and placement: writes are cold. The boot write is synchronous; from the async widen task they go through spawn_blocking. Never on the frame drain, so the live lane never waits.

A2. resolve_live_universe publishes on EVERY return arm, with its real source and never one inferred from the count:
- config off (around line 1045): HardcodedIndices, master_sourcing=false;
- expected pre-rider index arm (around 1247): FellBackToIndices;
- paged no-earlier-list arm (around 1297): FellBackToIndices;
- the 'no_usable_widening' arm (around 1350), which returns selection.instruments with source FellBackToIndices: FellBackToIndices;
- earlier-day list arm: that selection's source, with non_fallback_seen=true and planned=false;
- today's list arm: todays_list_seen=true, planned=true for every list member.

Boot stamp: OnWire::BeforeOpen when the boot IST second is below the comparison session start (09:15:00). Otherwise At(boot_secs). This is what makes a post-close restart honest. The public signature is unchanged.

A3. RunningWiden (dhan_feed_stack.rs) calls a new pure helper widened_spot_entries(boot_keys, today, on_wire, freed, now_secs). The result feeds record_widen or record_freed. It runs:
- after any attempt where on_wire changed: placed > 0, OR reconcile_pending_topups freed > 0 (freed instruments become Never for this process; a later re-placement becomes At(time of that placement));
- once at finish, the 15:30 hard stop, or list-overdue.

Every member of today.instruments is recorded with planned=true and todays_list_seen=true, including members never placed (OnWire::Never). With dial_short, the planned-and-sent instruments stay At(s), exactly as on_wire keeps them today, so a socket that never opened shows up after the grace period instead of being hidden. Cost: once per 60 s attach attempt, O(today list), never per tick.

A4. spawn_dhan_live_crossverify(deps) drops the main_feed parameter. The main.rs call site drops &main_feed_instruments. Add a source-order guard that the resolve_live_universe call precedes the spawn_dhan_live_crossverify call in main.rs. The TEST-EXEMPT text is updated.

A5. run_once builds the targets at fire through a new targets_at_fire(snapshot) -> FireTargets { targets: Vec<XverifyTarget>, skipped_fno: usize, coverage: CoverageFacts }:
- It reuses crossverify_targets_with_skipped over entries whose on_wire != Never.
- XverifyTarget gains on_wire_from: Option<u32> (None for BeforeOpen).
- The XVERIFY_UNVERIFIABLE_COUNTER increment and its warn are gated once per IST day by a static AtomicI64 holding the last day, so up to 4 attempts a day do not multiply them.

run_once returns AttemptOutcome { result: Result<(), AttemptFailure>, targets: usize }. run_day passes the count to report_final_failure.

=== PART B: refuse the marker when the planned universe was not measured ===

B1. Pure O(planned) function target_coverage(snapshot: Option<&SubscribedSpots>) -> CoverageFacts { verdict: CoverageVerdict, targets: u32, planned: u32, coverage_permille: u32 }. Verdicts, in order:
- NotPublished: no snapshot.
- Ok(MasterOff): master_sourcing is false; the 4 indices are the plan.
- IndexFallback: master on and !non_fallback_seen.
- NoTargets: zero targetable entries with on_wire != Never.
- Ok(PlannedUnknown): todays_list_seen is false, so the session ran on an earlier-day list. Ok, plus a warn naming it.
- Otherwise, coverage over the planned targetable set. Window W = [SESSION_START, SESSION_END - TAIL_UNSEALED_MINUTES*60). Per planned instrument: covered = |W ∩ [bound, ∞)|, where bound = SESSION_START for BeforeOpen, floor_minute(s)+60 for At(s), and the instrument contributes 0 for Never or when absent. coverage_permille = Σcovered*1000 / (planned*|W|), with planned==0 giving Ok(PlannedUnknown), so it never divides by zero.
- BelowPlan when coverage_permille < MIN_MARKER_COVERAGE_PERMILLE (950).

MIN_MARKER_COVERAGE_PERMILLE is a named constant, lockstep-tested against MAX_MARKER_REST_FAILURE_PERCENT (950 == 1000 - 10*5). It is documented as Assumed until measured on a real widen day.

B2. AttemptFailure gains UnderCovered(CoverageFacts) (all fields Copy, so the enum stays Copy; label "under_covered") and TooManyTargets (label "too_many_targets"). New is_retryable(): false only for these two.

classify_attempt gains coverage_ok: bool and over_target_cap: bool (targets > LIVE_COVERED_INSTRUMENTS = 2,000). Order:
1. Vacuous
2. Incomplete (not measured)
3. NotPersisted (stays retryable, so a transient QuestDB failure re-persists the audit; the next attempt then reaches UnderCovered)
4. TooManyTargets
5. Incomplete (run not complete)
6. UnderCovered
7. Ok

Ok stays the only path to write_daily_marker. The debug_assert becomes verdict.is_ok() == (should_write_marker && complete && coverage_ok && !over_target_cap).

B3. The comparison still runs and is persisted (audit evidence). The vendor tape goes only to dhan_rest_1m_tape, never to ticks or candles_*. run_day checks failure.is_retryable() before retry_delay_secs; a non-retryable failure goes straight to report_final_failure, one page per day (§12.15.5).

B4. report_final_failure gets a separate error! arm for UnderCovered:
- code = ErrorCode::WsGapConnectionState.code_str(), literal source = "xverify_vacuous";
- fields reason="under_covered", shortfall (not_published / index_fallback / no_targets / below_plan), targets, planned, coverage_pct;
- message: "today's planned instrument list was NOT verified — only N of M planned instruments' minutes were compared — today's S3 archive stays held to the hold ceiling". For index_fallback it adds "the live list fell back to the 4 indices this morning (already alerted separately)".

TooManyTargets joins the existing xverify_failed arm with reason "too_many_targets".

New local /metrics-only counter tv_dhan_xverify_under_covered_total{shortfall}, all four labels seeded at 0 at boot. No new alarm, no terraform change, no new EMF name: $0 (Verified that the xverify_vacuous filter matches the source literal).

B5. dhan_live_crossverify_daily gains nullable columns via ALTER ADD COLUMN IF NOT EXISTS, with the DEDUP key unchanged:
- targets_compared LONG, targets_planned LONG, coverage_permille LONG;
- coverage_verdict SYMBOL (ok / master_off / planned_unknown / not_published / index_fallback / no_targets / below_plan);
- rest_minutes_before_subscription LONG, live_minutes_before_subscription LONG.

They are written on every attempt, so the SEBI-kept daily row never reads clean on a refused day without saying why.

=== PART C: minutes before a widened instrument was subscribed ===

C1. Per target with on_wire_from = Some(s):
- base = floor_minute(s)+60;
- cap = base + WIDEN_FIRST_RECEIPT_GRACE_SECS (300);
- m = the target's first live bar minute from the live rows already read, or +∞ if none;
- covered_from = min(max(base, m), cap).

A target with None is not trimmed.

C2. Pure trim_before_subscription(rest, live, targets) -> Trimmed { rest, live, dropped_rest, dropped_live }. It drops BOTH sides' minutes before covered_from, from the comparison inputs only. dhan_rest_1m_tape persists the full raw answer. It runs inside run_cross_verification after both sides are read and before compare_day_in_scope; the 9 call sites keep their signature.

C3. After compare: outcome = apply_scope_trim(outcome, dropped_rest+dropped_live > 0). This turns only Clean into Partial; Diverged, Degraded, Blind and NoData are unchanged. The `degraded` input is NOT touched. The is_catastrophic_divergence page is unaffected. RunReport gains rest_minutes_before_subscription and live_minutes_before_subscription, logged in the finished line and persisted (B5).

UNCHANGED: the §12.15.6 option pass (run_option_pass), the disk-pressure archive leg (ungated), MAX_CROSSVERIFY_HOLD_DAYS, and the fact that the live lane never waits on the verifier.

=== RULE AND DOC EDITS (land first, same PR) ===

- docs/claude-rules-full/project/no-rest-except-live-feed-2026-06-27.md: new §12.15.7, dated 2026-10-06, with both quotes verbatim, enumerating A to C. REJECT rows:
  - targets built once at boot;
  - a marker on an under-covered run;
  - a source inferred from the count;
  - retrying UnderCovered;
  - an in-RAM-only subscribed set with no day file;
  - pre-subscription minutes hidden from the tape, or folded into `degraded`, or labelled Clean;
  - the option pass affecting coverage or the marker;
  - raising the grace above 300 s or lowering the coverage floor without a dated quote.
- Same file, §12.15.3 honest envelope: NOT claimed: instruments the lane intended but Dhan never confirmed; a boot-set dial shortfall is counted as subscribed (conservative, shows as missing_live); a quiet stock's genuine gap inside the 300 s grace is hidden (counted, outcome capped at Partial); a full-master day above 2,000 targets cannot be verified (too_many_targets, about 334 ms per target against the 600 s budget).
- .claude/rules/project/no-rest-except-live-feed-2026-06-27.md (summary stub): add 'writes a marker on an under-covered run (§12.15.7)'.
- docs/claude-rules-full/project/dhan-rest-only-noise-lock-2026-07-14.md: new §2.5a, dated, with the quotes. It states that tv-<env>-errcode-ws-gap-03-xverify-vacuous also fires with reason=under_covered, as a deliberate second page on a universe-fallback day that names the upstream alarm; same filter, no ok_actions, $0. Also amend in place, with dated notes, the §2.3f gap-table row and the alarm-table row that say 'compared ZERO minutes'. REJECT: a fourth xverify alarm, filtering on reason, paging every attempt.
- .claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md (summary stub): add a §2.5a note to the §2.5 bullet.
- crates/aws-lambdas/src/telegram_webhook.rs ALARM_PHRASES 'errcode-ws-gap-03-xverify-vacuous': "🔷 DHAN: the end-of-day price cross-check did not cover today's full instrument list — today's backup to S3 is on hold". This is a phone-visible wording change and needs a Lambda deploy.
- docs/error-runbooks/dhan-live-crossverify-error-codes.md: add under_covered and too_many_targets, what each means (archive held to the ceiling) and the operator action (check the universe-fallback alarm and the daily rider; override only via the existing hold override).
- CLAUDE.md O(1) table: add a row for crates/app/src/subscribed_spots.rs::{SUBSCRIBED_SPOTS, merge_entries, target_coverage}. Cite symbols, not line numbers: O(1) snapshot load; O(set ≤ 25,000) merge, write and coverage, cold (boot, once per widen attempt, once per verifier attempt).

## Tests
["subscribed_spots: merge_entries dedups on (security_id, segment); planned is OR; on_wire is min over prior and own except own Never beats prior At (refusal is current truth)","subscribed_spots: rebase_to_day turns At/BeforeOpen from an earlier IST day into BeforeOpen, keeps Never, clears planned and todays_list_seen","subscribed_spots: day file round-trips via temp+rename; a file from another IST day or a damaged file loads nothing and never panics; the temp file is never left behind","subscribed_spots: refuses a new key past 25,000 and counts it; a tracked key is never evicted","subscribed_spots: a snapshot is None before any publish and reflects the latest store after two records (local ArcSwapOption)","dhan_live_universe guard: every return arm of resolve_live_universe records before returning — the config-off arm with master_sourcing=false, and all THREE 4-index arms (pre-rider, paged, no_usable_widening) with FellBackToIndices; the live_universe_widen_pending guard still counts 3 arming sites","dhan_live_universe: a boot second before 09:15 stamps BeforeOpen; a boot at 15:50 stamps At(15:50)","dhan_feed_stack: widened_spot_entries marks every today-list member planned, stamps placed ones At(now), unplaced ones Never, freed ones Never, and a re-placed freed one At(the later placement)","dhan_feed_stack source scan: widen_running_session records when placed > 0, when freed > 0, and at finish / hard-stop / overdue","boot: target_coverage table — None=NotPublished; master off + 4 indices=Ok(MasterOff); master on + FellBackToIndices only=IndexFallback; 0 targetable=NoTargets; earlier-day list only=Ok(PlannedUnknown); 865 planned all BeforeOpen=Ok; 865 planned widened at 09:40 (~93%)=BelowPlan; widened at 09:30=Ok; widened at 15:29=BelowPlan; planned 0 never divides","boot: test_regression_restart_after_fallback_day_never_writes_marker — prior file has 4 indices BeforeOpen with !non_fallback_seen, then a 15:50 boot records today's 865 at At(15:50): verdict BelowPlan, classify gives UnderCovered","boot: test_regression_fallback_day_four_index_targets_never_write_marker — measured, persisted, complete 4 IDX_I run with master on and FellBackToIndices gives Err(UnderCovered{IndexFallback})","boot: proptest — with master on and non_fallback_seen false, target_coverage is never Ok for any counts or stamps","boot: proptest — coverage_permille is monotone non-increasing as any At(s) moves later","boot: MIN_MARKER_COVERAGE_PERMILLE lockstep with MAX_MARKER_REST_FAILURE_PERCENT (950 == 1000 - 10*5)","boot: test_classify_attempt_agrees_with_the_marker_rule_everywhere gains coverage_ok and over_target_cap: Ok iff should_write_marker && complete && coverage_ok && !over_target_cap","boot: test_classify_attempt_names_the_first_problem — order Vacuous > NotMeasured > NotPersisted > TooManyTargets > Incomplete > UnderCovered","boot: test_attempt_failure_labels_are_distinct includes under_covered and too_many_targets; is_retryable is false for exactly those two","boot: run_day source scan — is_retryable checked before retry_delay_secs; a non-retryable failure goes straight to report_final_failure (replaces the literal in test_run_day_retries_through_the_pure_bound)","boot: test_targets_built_at_fire_not_at_boot — spawn_dhan_live_crossverify takes no main_feed and does not call crossverify_targets_with_skipped; run_once calls subscribed_spots_snapshot and targets_at_fire; main.rs resolve_live_universe precedes spawn_dhan_live_crossverify and no longer passes &main_feed_instruments","boot: test_marker_write_needs_a_complete_run also asserts target_coverage( appears inside run_once itself before the Ok arm","boot: the unverifiable counter and warn fire once per IST day across 4 attempts","boot: test_every_xverify_alarm_source_has_a_live_error_emit stays green; new test_under_covered_page_uses_vacuous_source_inside_error pins the UnderCovered arm as error! with code and literal source = xverify_vacuous plus the shortfall/planned/coverage_pct fields","boot: every tv_dhan_xverify_under_covered_total shortfall label is seeded at 0","crossverify: trim_before_subscription — None target untouched; covered_from = min(max(base, first live minute), base+300); both sides trimmed and counted; other targets unaffected","crossverify: apply_scope_trim turns only Clean into Partial; Diverged/Degraded/Blind/NoData unchanged","crossverify: test_regression_widen_day_real_divergence_stays_diverged — a widen day with trimmed minutes AND a real cell divergence on a boot-set target stays Diverged","crossverify: a widened instrument whose first live bar is 2 minutes after the send (withheld bucket) gives Partial with missing_live_traded = 0","crossverify: a widened instrument with no live bars at all and traded vendor bars after base+300 gives Diverged (a dead socket is not hidden)","crossverify: proptest — trimming never increases minutes_compared, cells_diverged or missing_rest, and never yields Clean when anything was dropped","crossverify: the raw dhan_rest_1m_tape row count is unchanged by the trim","persistence: the daily-row ensure adds the six coverage columns with ALTER ADD COLUMN IF NOT EXISTS; the DEDUP key is unchanged (dedup_segment_meta_guard green)","telegram_webhook: the vacuous phrase no longer says 'compared nothing at all'; alarm_phrase_coverage_guard green","option pass unchanged: test_run_option_pass_never_touches_the_marker_the_daily_row_or_a_page and test_run_option_pass_runs_after_the_spot_retry_loop_ends stay green"]

## Guards to update
["crates/app/src/dhan_live_crossverify_boot.rs: test_marker_write_needs_a_complete_run, test_classify_attempt_agrees_with_the_marker_rule_everywhere, test_classify_attempt_names_the_first_problem, test_attempt_failure_labels_are_distinct, test_run_day_retries_through_the_pure_bound (literal 'run_day(&deps, &targets, today, day_start_ist_nanos)' goes away), test_run_once_waits_for_the_token_before_failing (split on run_once), the crossverify_targets_with_skipped tests, and the TEST-EXEMPT text on spawn_dhan_live_crossverify","crates/app/src/dhan_live_crossverify_boot.rs: test_every_xverify_alarm_source_has_a_live_error_emit must stay green with the second xverify_vacuous error! arm","crates/app/src/dhan_live_universe.rs: live_universe_widen_pending_is_armed_by_every_off_today_arm (stays 3), plus a new every-return-records guard covering all four index-returning arms","crates/app/src/main.rs: spawn_dhan_live_crossverify call site, plus a new source-order guard (resolve before spawn)","crates/app/src/dhan_feed_stack.rs: widen_running_session record calls (placed, freed, finish)","crates/storage/src/dhan_live_crossverify_persistence.rs: daily-row schema/ensure columns; dedup_segment_meta_guard","crates/aws-lambdas/src/telegram_webhook.rs ALARM_PHRASES and alarm_phrase_coverage_guard.rs","pub-fn-test-guard and pub-fn-wiring-guard: record_boot, record_widen, record_freed, subscribed_spots_snapshot, merge_entries, rebase_to_day, target_coverage, targets_at_fire, trim_before_subscription, apply_scope_trim, widened_spot_entries, is_retryable","error_code_rule_file_crossref: docs/error-runbooks/dhan-live-crossverify-error-codes.md documents under_covered and too_many_targets under WS-GAP-03","claude_md_codebase_map_guard: new CLAUDE.md O(1) row for crates/app/src/subscribed_spots.rs (symbols, not line numbers)","per-item-guarantee-check.sh / plan-gate.sh: the new item in active-plan-feed-hardening.md carries the 6 sections plus the 15-row and 7-row matrices, and references the app, storage and aws-lambdas crates"]

## Residual risks
["Assumed: the 95% instrument-minute coverage floor and the 300 s first-receipt grace are not measured on a real widen day. Both are named constants, and coverage_permille is persisted so they can be tuned from data.","Assumed: on a normal pre-rider day the boot runs on an earlier-day list (about the same size) and the widen adds a small delta, so coverage stays near 100%. If routine widens happen after about 09:34, healthy days would be refused and page.","Assumed: when the session ran on an earlier-day list and today's list never landed (planned_unknown), the day should still release S3. Refusing instead would hold every rider-failure day to the 3-day ceiling. Flagged for the operator.","Limitation: the 300 s grace can hide a quiet stock's genuine gap just after its widen. It is counted and caps the outcome at Partial, never Clean.","Limitation: the record is what the lane intended to put on the wire, not what Dhan confirmed. A boot-dial shortfall or an unopened widen socket shows as missing_live, so Diverged. That is conservative and honest.","Limitation: a full-master day (about 4,565 spots) cannot be verified, because the live read caps at 2,000 targets and about 334 ms per target overruns the 600 s budget. That day is now one non-retried too_many_targets attempt plus an xverify_failed page and the hold ceiling. This was true before; building targets at fire only makes it more reachable.","Risk: xverify_vacuous now has two meanings. It depends on the §2.5a row, the in-place amendments to the §2.3f and alarm-table rows, and the Telegram phrase change landing in the same PR; the phrase change needs a Lambda deploy.","Risk: on a universe-fallback day the operator gets a second page at 15:41 for the same root cause as the morning collapse alarm. This is deliberate per the task, and the message names the upstream alarm.","Risk: a refused marker holds that day's archive on disk until MAX_CROSSVERIFY_HOLD_DAYS. This is intended (§12.15.2), and the disk-pressure leg stays ungated.","Process blocker: 5 active-plan files exist (the V7 cap), so this must be a new item in an existing APPROVED plan (active-plan-feed-hardening.md), or a merged plan must be archived first.","Limitation: a crash between a widen and its day-file write loses that stamp. The next process then sees those instruments as absent or Never, which lowers coverage: refuse more, never release more."]

