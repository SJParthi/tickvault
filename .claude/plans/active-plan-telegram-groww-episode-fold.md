# Implementation Plan: Telegram Noise PR-2 — fold the Groww reject storm into ONE live-edited bubble (F1 PR-A)

**Status:** APPROVED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) — 2026-07-14 noise directive, relayed via the coordinator session (design `design-root.md` PR-A scope approved; PR-B = MainFeedPool + Health families deferred)

> **Per-item guarantee matrix:** this plan cross-references
> `.claude/rules/project/per-wave-guarantee-matrix.md` (the 15-row + 7-row
> matrices apply per that file; item-specific proof artefacts are the
> ratchet tests listed below).

## Plan Items

- [x] Generalize the WS-flavored episode renderers via per-family `&'static str` phrasing fns
  - Files: crates/core/src/notification/episode.rs
  - Tests: test_render_regression_ws_families_byte_identical, test_groww_feed_render_phrasing_no_ws_literals
- [x] Add `EpisodeFamily::GrowwFeed` (badge/feed_desc/snapshot_label/from_snapshot_label/episode_config_for arms)
  - Files: crates/core/src/notification/episode.rs
  - Tests: test_from_snapshot_label_family_phase_roundtrip_unknown_none, guard_groww_family_uses_default_episode_config
- [x] Route GrowwSidecarRejected + Groww FeedDown/FeedRecovered into the GrowwFeed episode (episode_key + episode_role)
  - Files: crates/core/src/notification/events.rs
  - Tests: guard_groww_runtime_events_are_episode_routed, guard_non_groww_feed_incidents_stay_legacy, guard_groww_boot_pings_unchanged
- [x] FSM + registry behavior for the new family (open→edit→resolve→close; tick promote/expire; snapshot)
  - Files: crates/core/src/notification/episode.rs
  - Tests: test_groww_reject_open_then_progress_edits_no_second_page, test_groww_feed_included_in_tick_scans_and_snapshot
- [x] episode_attempts_hint: document GrowwFeed → 0 (folded-event count)
  - Files: crates/core/src/notification/service.rs
  - Tests: (comment-only; behavior covered by test_groww_reject_open_then_progress_edits_no_second_page occurrence counting)
- [x] Reduce GROWW_REJECT_PAGE_COOLDOWN_SECS 1800 → 60 (mechanism KEPT — transport-failure bound only)
  - Files: crates/app/src/groww_sidecar_supervisor.rs
  - Tests: test_should_page_reject_rising_edge_and_cooldown (≤60 + >0 pins)
- [x] Update the rule file's cooldown contract with a dated 2026-07-14 note
  - Files: .claude/rules/project/feed-stall-watchdog-error-codes.md
  - Tests: (docs; the ≤60 pin above is the mechanical guard)

## Design

The episode (one-live-bubble) machinery is family-generic in its FSM/registry
but was wired to only 3 families (MainFeedWs, OrderUpdateWs, Boot); every
other High/Critical event pages immediately + SMS per event. The persistent
Groww reject storm (RC-C, the 2026-07-10 trigger) therefore paged ~2×/hr
(1800s supervisor cooldown) for a whole reject day. This PR adds the
`GrowwFeed` episode family: `GrowwSidecarRejected` + the Groww
`FeedDown`/`FeedRecovered` arms map to `EpisodeKey{GrowwFeed, conn:0}` — the
FIRST event still pages (+ SMS at ≥High, the preserved bypass), recurrences
become in-place bubble edits (no push, no SMS), `FeedRecovered` flips to
Recovering and the 10s ticker closes green after the 60s stability window.
The only non-generic surface was the renderer (WS literals "DOWN"/"drops"/
"reconnect attempts"/"reconnecting"): six `const fn` phrasing hooks on
`EpisodeFamily` (down_headline/occurrence_noun/counter_noun/action_line/
recovering_line/recovered_verb) supply per-family wording; the WS families'
strings are the exact prior literals so their output is byte-identical
(ratcheted). The supervisor's cross-child page cooldown drops 1800→60s: page
dedup is now owned by the episode fold; the 60s gate remains ONLY to bound
the `SendNewFallback` transport-failure path (first page never landed →
every event is a fresh send) to ≤1 notify()/min. Deferred to PR-B per the
approved staging: `MainFeedPool` + `Health` families. NOT touched:
coalescer, CloudWatch/terraform alarms (FEED-STALL-01/FEED-REJECT-01 stay
the redundant pager), WS endpoints, §28 indicator/strategy, hot path.

## Edge Cases

- Off-hours `FeedDown` is Low: a Low-peak GrowwFeed bubble crossing into an
  in-market High reject re-pages FRESH via the severity-escalation edge
  (episode.rs FSM, pre-existing, family-generic) — SMS never swallowed.
- `FeedDown{operator_initiated:true}` (deliberate toggle-off) fires ONCE
  (edge-latched upstream); the bubble stays on its first page, which carries
  the honest "stays OFF until re-enabled" body — the "retrying" steady line
  only appears if involuntary events RECUR.
- Non-Groww `FeedDown`/`FeedRecovered` (Dhan, future feed #3) keep the
  legacy lane (feed-name compare, case-insensitive) — never wrongly folded.
- Groww BOOT pings (FeedAuthOk/FeedInstrumentsLoaded/FeedConnectedAwaitingTicks)
  keep mapping to the Boot family — distinct variants, no collision.
- Fleet-summary GrowwSidecarRejected folds into the SAME bubble (conn=0).
- Restart mid-incident: GrowwFeed is snapshotted/rehydrated like the WS
  families (label "groww_feed"); a rollback reading the new label drops the
  entry fail-open (one fresh bubble, never a panic).
- Event-less Down bubble stale-expires after 1800s with NO tombstone → the
  next incident opens a fresh page + SMS (restart edge, pre-existing).
- Reason-text change mid-incident (auth → entitlement) does not re-surface
  in edits — the CloudWatch FEED-REJECT-01 signature carries the cause.

## Failure Modes

- Episode first page never lands (Telegram transport down → message_id=None):
  every subsequent reject takes `SendNewFallback` (fresh send). The KEPT 60s
  supervisor cooldown bounds that path to ≤1/min; TELEGRAM-01 error!+counter
  fire per failure (never silent). This is WHY the cooldown is reduced, not
  removed.
- Edit-API flap on the High-peak bubble: the TELEGRAM-03 ladder falls back
  to a FRESH send on the first exhausted transient (duplicate-over-drop,
  pre-existing, family-generic).
- Never-drop pin: `next_episode_action` can never return Ignore for a
  High/Critical Open/Progress (proptest, family-generic — the proptest is
  parameterized on severity/role, covering the new family by construction).
- A future edit restoring the 30-min cooldown gap would starve the bubble's
  occurrence counter → pinned by the ≤60 assert in the supervisor test.
- Deleting any episode mapping silently restores the storm → pinned by
  `episode_runtime_family_wiring_guard.rs`.
- CloudWatch alarm chain untouched: the `error!` emit sites (FEED-REJECT-01,
  FEED-STALL-01) are not modified — the errcode alarms page exactly as
  before even if a bubble edit is swallowed.

## Test Plan

- Scoped per testing-scope.md: `cargo test -p tickvault-core --lib`,
  `cargo test -p tickvault-app --lib`, the episode guard integration tests;
  `cargo check --workspace --tests`; fmt + clippy (core+app).
- Ratchets (6, per design-root.md §3): (1) FSM unit test
  `test_groww_reject_open_then_progress_edits_no_second_page`; (2) render
  regression `test_render_regression_ws_families_byte_identical` (existing
  families byte-identical) + `test_groww_feed_render_phrasing_no_ws_literals`
  (commandments); (3) snapshot round-trip incl. GrowwFeed + exhaustiveness
  pin; (4) wiring guard `episode_runtime_family_wiring_guard.rs` (4 tests);
  (5) Boot-exclusion inverse `test_groww_feed_included_in_tick_scans_and_snapshot`;
  (6) cooldown ≤60 pin in `test_should_page_reject_rising_edge_and_cooldown`.

## Rollback

- Revert the single squash commit → byte-identical prior behavior (the WS
  render regression ratchet proves the render surface was unchanged, so the
  revert surface is exactly the new family + the cooldown constant). No
  schema, no config key, no alarm change. The snapshot file is fail-open:
  a rolled-back binary reading a "groww_feed" entry drops it silently.
- Config kill switch already exists: `[notification] episode_mode = false`
  restores legacy per-event dispatch for ALL families (pre-existing).

## Observability

- No new metric/alarm. Existing episode counters gain the new family's
  traffic: `tv_telegram_episode_events_total{action=open|edit|edit_throttled|
  escalate|close|expired|reopen}`; `tv_groww_reject_page_cooldown_suppressed_total`
  keeps counting supervisor-side suppressions (now at the 60s cadence).
- CloudWatch FEED-REJECT-01 / FEED-STALL-01 error-code alarms are the
  UNCHANGED redundant pager (design risk table: no double-count, no
  regression — this PR touches only the Telegram lane).
- Complexity honesty: episode lookup/edit is O(1) (HashMap on a Copy key);
  `tick()` is O(N) in live episodes (N ≤ a handful) on the 10s cold-path
  drain ticker inside a spawned task — NOT the tick hot path; flagged
  honestly per engineering-execution-standard §4. `episode_key()` stays a
  zero-alloc Copy match (DHAT bypass-arm pin holds).

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Persistent Groww reject day (the 2026-07-09 class) | ONE bubble: first page + SMS at 9:22 AM, occurrence counter edits ≤3/min thereafter, zero further pushes/SMS |
| 2 | Reject recovers (streaming resumes) | FeedRecovered → amber "confirming…" edit → ONE green close after 60s quiet |
| 3 | Telegram transport down at first page | SendNewFallback fresh sends bounded ≤1/min by the kept 60s cooldown; TELEGRAM-01 loud |
| 4 | Off-hours Low FeedDown then in-market High reject | escalation edge re-pages FRESH + SMS |
| 5 | Dhan FeedDown | legacy immediate lane, byte-identical to today |
| 6 | Restart mid-storm | bubble rehydrated from snapshot; edits continue on the same message id |

## Zero-Loss Guarantee Charter check

- Coverage: 6 ratchets + render byte-pins added; no coverage floor lowered.
- No tick/persist/order path touched; no new tick-drop path; hot path
  untouched (cold notify lane only); no new hot-path allocation (const fn
  &'static str phrasing; DHAT dispatcher pin unchanged).
- No WS endpoint / connection change; no §28 indicator/strategy touch.
- Honest envelope: the fold dedups Telegram pages only — the CloudWatch
  errcode alarms and errors.jsonl forensic stream are unchanged, so incident
  detectability is never reduced; the first page + SMS of every incident is
  preserved by the never-drop pin.
