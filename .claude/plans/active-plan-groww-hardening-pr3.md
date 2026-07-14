# Implementation Plan: Groww Lossless-Reconnect Hardening PR-3 — Feed Gap-Episode Audit

**Status:** APPROVED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) via coordinator session, 2026-07-14 — lossless-reconnect build PR-3 authorized; FEED-GAP-01 Telegram-episode-only default approved

## Design

Every Groww feed gap becomes a named, durable, Telegram-visible episode. A pure
gap-episode tracker in `crates/app/src/groww_bridge.rs` rides the bridge's
EXISTING periodic liveness poll (never the per-tick hot path): when the
feed-level last-tick age crosses `FEED_GAP_EPISODE_THRESHOLD_SECS` (= 10)
in-session, an OPEN row is best-effort appended to the NEW `feed_gap_audit`
QuestDB table (`crates/storage/src/feed_gap_audit_persistence.rs`,
ILP-over-HTTP, DEDUP UPSERT KEYS `(ts, trading_date_ist, feed, start_ts,
outcome)` — outcome in-key so OPEN and CLOSE rows both survive) plus ONE
edge-latched Telegram bubble (`FeedGapEpisodeOpened`, High). When liveness
advances again, a CLOSE row carries the measured `gap_secs`, `kill_count`
(`-1` sentinel when not cheaply measurable), and the named partial 1-minute
buckets the gap overlapped, plus the `FeedGapEpisodeClosed` Info Telegram.
The unconditional counter `tv_feed_gap_seconds_total` accumulates ALL
measured gaps regardless of threshold. A new `ErrorCode::FeedGap01EpisodeDegraded`
("FEED-GAP-01", Medium, auto-triage-safe) types every machinery-degrade arm
per `.claude/rules/project/feed-gap-error-codes.md`. The 15:45 IST scoreboard
(`crates/app/src/feed_scoreboard_boot.rs`) closes dangling OPEN episodes with
`dangling_closed` rows carrying `-1` sentinels. Changed paths:
`crates/storage/src/feed_gap_audit_persistence.rs` (new) +
`crates/storage/src/lib.rs`, `crates/common/src/error_code.rs`,
`crates/core/src/notification/events.rs`, `crates/app/src/groww_bridge.rs`,
`crates/app/src/feed_scoreboard_boot.rs`.

## Edge Cases

Sub-10s micro-gaps: no episode, but `tv_feed_gap_seconds_total` still
accumulates the measured gap. Gap spanning many minutes: `partial_minutes`
bounded to 10 bucket labels + a trailing ellipsis marker. Gap opening
pre-session or after close: the tracker is market-hours-gated, no episode.
Feed never streamed this session (no known last-tick): no episode — the
never-streamed arm belongs to FEED-STALL-01, not a gap. Two rapid gaps: each
is its own episode (open→close→open→close), edge-latched so no per-poll spam.
Recovery in the same poll tick as detection: OPEN and CLOSE both emit with
distinct `outcome`, both survive DEDUP. Dry-run mode: the opened Telegram
body carries the "(paper mode — no live orders exist)" clause.

## Failure Modes

QuestDB ILP/HTTP down at an edge: the row is lost for that edge, the
Telegram bubble still fires, `error!(code = FEED-GAP-01, stage = append|flush)`
+ `tv_feed_gap_audit_write_errors_total` make it loud — never a silent hole,
never a blocked recovery path. Ensure-DDL failure at boot: FEED-GAP-01
`stage=ensure_*` with the standard duplicate-row-window consequence until a
later boot re-runs the idempotent DDL. Scoreboard dangling-close read/write
failure: FEED-GAP-01 `stage=dangling_close`, the day's dangling OPEN rows
remain open (honest, visible via SQL). Process death mid-gap: the OPEN row is
already durable; the 15:45 sweep (or the next day's inspection) names it
`dangling_closed` with `-1` sentinels — never fabricated measurements.

## Test Plan

Unit tests, block-scoped per `testing-scope.md`: pure tracker transitions
(no-open below 10s, open at >10s, exactly one OPEN per episode, close on
liveness advance, counter accumulation on sub-threshold gaps), the
`FEED_GAP_EPISODE_THRESHOLD_SECS == 10` pin, partial-minute bucket
computation (bounded list + ellipsis), storage pure builders (DDL string
contains the DEDUP key clause; row build for open/closed/dangling_closed),
error-code catalogue tests (`cargo test -p tickvault-common` — crossref +
tag guards pick up the new variant + rule file), notification wording tests
per the events.rs house style. `cargo test -p tickvault-app`,
`-p tickvault-storage`, `-p tickvault-common`, `-p tickvault-core
notification` all green before the PR opens.

## Rollback

The subsystem is annotation-only and additive: reverting the PR (single
`git revert`) removes the tracker, the table writer, the events, and the
error code with zero impact on capture, candles, orders, or the reconnect
machinery — no data migration needed (the `feed_gap_audit` table simply
stops receiving rows; QuestDB tables are never dropped). No config flag is
required because the code path is best-effort and off the hot path; the
threshold constant is the single tuning knob.

## Observability

New counter `tv_feed_gap_seconds_total` (unconditional gap accumulation) and
`tv_feed_gap_audit_write_errors_total` (write-degrade). Typed Telegram
events `FeedGapEpisodeOpened` (High — one page per episode) /
`FeedGapEpisodeClosed` (Info). Durable forensic rows in `feed_gap_audit`
(open/closed/dangling_closed lifecycle chain, SEBI-style never-delete).
`error!` lines carry `code = ErrorCode::FeedGap01EpisodeDegraded.code_str()`
+ a `stage` field per the tag-guard convention. Delivery boundary is
Telegram-episode-only per the operator-approved 2026-07-14 default — NO
CloudWatch metric filter (documented in the rule file §1).

## Plan Items

- [x] FEED-GAP-01 rule file
  - Files: .claude/rules/project/feed-gap-error-codes.md
  - Tests: error_code_rule_file_crossref
- [ ] ErrorCode variant FeedGap01EpisodeDegraded
  - Files: crates/common/src/error_code.rs
  - Tests: test_all_variants_have_unique_code_str
- [ ] feed_gap_audit persistence
  - Files: crates/storage/src/feed_gap_audit_persistence.rs, crates/storage/src/lib.rs
  - Tests: feed_gap_audit DDL/row builder unit tests
- [ ] NotificationEvent variants + bridge gap-episode tracker
  - Files: crates/core/src/notification/events.rs, crates/app/src/groww_bridge.rs
  - Tests: tracker transition tests, threshold pin, partial-minute tests
- [ ] Scoreboard dangling-close sweep (or honest flagged deferral)
  - Files: crates/app/src/feed_scoreboard_boot.rs
  - Tests: dangling-close pure helper test (if not deferred)
