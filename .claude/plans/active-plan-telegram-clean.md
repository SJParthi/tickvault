# Implementation Plan: Telegram Cleanliness Overhaul (S1–S4)

**Status:** IN_PROGRESS
**Date:** 2026-07-15
**Approved by:** directive relayed via the coordinator session 2026-07-15 — coordinator relay, not a direct in-session operator message; pending operator ratification on the PR

Guarantee matrices: this plan carries the 15-row + 7-row guarantee matrices by
cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md` (the
sentinel matrices apply verbatim; nothing here weakens any row). Honest 100%
claim: 100% inside the tested envelope, with ratcheted regression coverage —
`crates/core/tests/telegram_body_format_guard.rs` fails the build on any
regression of line budgets, banned sentinel phrases, verdict-first ordering,
or footnote leaks; loud-failure arms (BLIND / NEEDS-ATTENTION / ExternalStop /
Aborted / token Criticals) are byte-preserved or louder, never silenced.
Beyond the envelope, the ratchet is fixture-based, not enum-exhaustive — new
variants must add fixtures (stated in the ratchet header).

Scope note (guardrail): NO `.claude/rules/` file is created or edited by this
plan — mechanical enforcement lives ONLY in the new ratchet test; a PROPOSED
rule-file text is drafted outside the repo for the operator to land themselves.

## Design

Crates core + app only (no `common`, no new ErrorCode, no schema, no config
key). S2: a `data/state/daily/<task>-YYYY-MM-DD.marker` once-per-trading-day
marker (fail-open on IO error, bounded 7-day sweep) gates ONLY the
`RunCatchUp` arm of the 15:40 IST timeframe verifier; the pure
`should_notify_summary` predicate silences the `no_data` Telegram (log-only —
name + semantics matched to the groww-live-off branch's planned predicate so a
double landing merges as a trivial union); the marker is written ONLY after a
terminal-quality summary (`pass` / `no_data`) is dispatched — FAIL-class
outcomes (mismatch / degraded / blind) never gate and never mark (Rule 11).
S3: pure `classify_shutdown` (signal kind × AWS-runtime × 16:25–16:45 IST
window × weekday × trading-day truth table; any ambiguity → ExternalStop,
never quieter) + a `ShutdownClass` payload on `ShutdownInitiated`
(ScheduledStop/OperatorStop = Low one-liners; ExternalStop = Medium, stays
loud); the signal-kind string main.rs already produces (currently dropped) is
threaded into the event. S1: the 15:45 IST scorecard body re-renders —
verdict line FIRST (one emoji + one sentence), ≤2 aligned `<code>` per-feed
stat lines via the new `aligned_feed_line` helper (unmeasured `-1` fields
OMITTED, never "?"), optional incidents line, exactly ONE caveat line, all six
footnotes deleted, `rest_leg_line` deleted (pull totals fold into the feed
lines via `aggregate_pulls_per_feed`); the TF PASS body becomes ONE line. S4
(SEVERABLE final commit): `EpisodeFamily::{DhanRest, GrowwRest}` fold the
surviving REST Degraded/Recovered pair families into one live-edited bubble
per leg; token Criticals stay legacy (family-(3) contract byte-unchanged).
Files: `crates/core/src/notification/events.rs`,
`crates/core/src/notification/episode.rs`,
`crates/app/src/daily_task_marker.rs` (new),
`crates/app/src/shutdown_class.rs` (new),
`crates/app/src/tf_consistency_boot.rs`, `crates/app/src/main.rs`,
`crates/app/src/lib.rs`.

## Edge Cases

Marker IO failure → fail-open re-send (one `warn!`, wording avoids the
error-level meta-guard flush/persist phrases). FAIL-class TF day → marker
unwritten, restart re-runs + re-pages. `TICKVAULT_TF_VERIFY_NOW` bypasses the
marker gate entirely (RunNow never consults it); the real 15:40 fire
(SleepThenRun) never consults it either. A forced past-date backfill writes
the marker for the VERIFIED date, never today's (a backfill can never
suppress today's catch-up). Manual/budget stop inside the 16:25–16:45 weekday
window or on a holiday classifies ScheduledStop (quiet) — documented
residual; the budget alarm pages independently and holidays carry no market
exposure. All-sentinel scorecard feed → one honest "no numbers recorded
today ⚠️" line (never silent, never "?"). Feed intentionally OFF → exactly
one "OFF today (excluded from verdict)" line. Recovered with no open episode
→ SendLegacy passthrough (existing FSM, never a drop). Merge race with
`claude/groww-live-off-rest-only`: `should_notify_summary` name + semantics
byte-matched; S4 maps only variants that survive that branch.

## Failure Modes

Classifier bug → ExternalStop (today's Medium loudness), never quieter.
Marker write failure → duplicate card next restart (noisy-not-silent, the
Rule-11 direction). Episode edit failure → existing fallback ladder sends a
fresh bubble (never-drop). `[notification] episode_mode = false` restores
legacy per-event dispatch for the new S4 families at runtime. DHAT regression
on `episode_key` (String-payload variants) → the build-failing
`dhat_telegram_dispatcher` test catches it. A dropped `notify` before the
marker write leaves the marker unwritten → re-delivery, never a silent skip.

## Test Plan

`cargo test -p tickvault-core -p tickvault-app`; `cargo test -p
tickvault-core --features dhat --test dhat_telegram_dispatcher`. Updated
pins: events.rs in-module tests (~20 scorecard body tests re-pointed at the
new format, TF pass/coverage asserts, `test_shutdown_initiated_message` →
3 per-class tests, severity pins ×2, `test_episode_key_none_for_non_episode_
variants` re-shaped for the class payload), `event_formatting_coverage.rs`
TF asserts, `tf_consistency_wiring_guard.rs` (+2 needles), service.rs
construction sites (×2). New: `daily_task_marker` unit suite (tempdir;
exists/absent, fail-open, sweep, path shape), `classify_shutdown` truth-table
suite (all rows + window boundaries), `should_notify_summary` suite,
`crates/core/tests/telegram_body_format_guard.rs` (~12 render-based ratchet
tests), S4: `episode_rest_family_wiring_guard.rs` +
`episode_runtime_family_wiring_guard.rs` extension.

## Rollback

Pure-revert PR: no schema, no new config keys, no common-crate change.
`rm data/state/daily/*` re-arms a day; `TICKVAULT_TF_VERIFY_NOW=1` forces a
run regardless; `[notification] episode_mode = false` kills the S4 routing at
runtime; the classifier's worst case is a wrong severity on one stop line —
never a suppressed High/Critical.

## Observability

Every suppressed send is replaced by one `info!` line (marker skip, no_data
silence). Existing counters unchanged (`tv_telegram_*`). One `warn!` on a
marker write failure. Episode folds stay visible via
`tv_telegram_episode_events_total{action}`. No new ErrorCode; no CloudWatch
change; no cost delta.

## Plan Items

- [ ] Item 0 — plan file added (the two stale groww plans were already
      archived upstream in #1576; active-plan count stays ≤ 5)
      — Files: .claude/plans/active-plan-telegram-clean.md — Tests: plan-gate.sh PASS
- [ ] Item 1 — daily_task_marker.rs + TF RunCatchUp gate + should_notify_summary
      — Files: crates/app/src/daily_task_marker.rs, crates/app/src/lib.rs,
        crates/app/src/tf_consistency_boot.rs,
        crates/app/tests/tf_consistency_wiring_guard.rs
      — Tests: test_daily_marker_path_shape, test_daily_marker_exists_*,
        test_write_daily_marker_*, test_should_notify_summary_*,
        wiring-guard needles
- [ ] Item 2 — shutdown_class.rs + ShutdownClass payload + bodies + severities
      — Files: crates/app/src/shutdown_class.rs, crates/app/src/lib.rs,
        crates/app/src/main.rs, crates/core/src/notification/events.rs,
        crates/core/src/notification/service.rs (test constructions)
      — Tests: test_classify_shutdown_* truth table + window boundaries,
        test_shutdown_initiated_message_* ×3, per-class severity pins
- [ ] Item 3 — scorecard rewrite + TF-pass one-liner + telegram_body_format_guard.rs
      — Files: crates/core/src/notification/events.rs,
        crates/core/tests/telegram_body_format_guard.rs,
        crates/core/tests/event_formatting_coverage.rs
      — Tests: aligned_feed_line / aggregate_pulls_per_feed suites, re-pointed
        scorecard body tests, TF pass one-liner tests, ~12 ratchet tests
- [ ] Item 4 (SEVERABLE) — DhanRest/GrowwRest episode routing
      — Files: crates/core/src/notification/events.rs,
        crates/core/src/notification/episode.rs,
        crates/core/tests/episode_rest_family_wiring_guard.rs,
        crates/core/tests/episode_runtime_family_wiring_guard.rs
      — Tests: episode_rest_family_wiring_guard.rs (routing table, roles,
        rest_slot map, snapshot label round-trip, once-per-day None pins),
        family-guard extension, dhat rerun

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Restart at 16:10 after a delivered 15:40 PASS | RunCatchUp skipped (marker), one info! line, no Telegram |
| 2 | Restart at 16:10 after a 15:40 MISMATCH day | Re-run + re-page (marker never written on FAIL class) |
| 3 | no_data TF day | log-only, no Telegram; marker written |
| 4 | EventBridge 16:30 SIGTERM on AWS weekday | ScheduledStop, Low, one line |
| 5 | SIGTERM on AWS at 11:00 on a trading day | ExternalStop, Medium, loud |
| 6 | Ctrl+C anywhere | OperatorStop, Low |
| 7 | Scorecard with all -1 lag fields | fields omitted; no "?", no "not measured" |
| 8 | Dhan feed off day | "Dhan: OFF today (excluded from verdict)" single line |
| 9 | REST Degraded → 5 recurrences → Recovered (S4) | one bubble, edits in place, green close |
