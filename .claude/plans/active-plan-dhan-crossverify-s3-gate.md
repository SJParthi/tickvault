# Implementation Plan: 1-minute cross-verification restored + S3 archive waits for it

**Status:** APPROVED
**Date:** 2026-09-24
**Approved by:** Parthiban (operator) — "Bro use the 1 min cross verification alone as whatever I have discussed it to you as the requirement provide it dude okay? ... then go ahead with this S3 also dude okay? Fix and resoleve everything dude and then merge and deploy it dude okay?"
**Authority:** `.claude/rules/project/no-rest-except-live-feed-2026-06-27.md` §12.15; `.claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md` §2.5
**Crates:** app (`crates/app/src/dhan_live_crossverify.rs`, `crates/app/src/dhan_live_crossverify_boot.rs`, `crates/app/src/daily_archive_boot.rs`, `crates/app/src/main.rs`), storage (`crates/storage/src/partition_archive.rs`, `crates/storage/src/dhan_live_crossverify_persistence.rs`), aws-lambdas (`crates/aws-lambdas/src/telegram_webhook.rs`), common (`crates/common/src/error_code.rs`, `crates/common/src/constants.rs`)

## Design

- The comparator module is restored from before its 2026-09-17 deletion. It
  compares our `candles_1m` (`feed='dhan'`) with Dhan's own 1-minute tape
  (`POST /v2/charts/intraday`, interval "1"). Prices are compared as integer
  paise. Only IDX_I and NSE_EQ/BSE_EQ targets are compared. F&O is skipped and
  counted, because its vendor `instrument` string cannot be derived from the
  segment.
- `dhan_live_crossverify_boot.rs` is its own task. It runs once per trading day
  at 15:41 IST, or at boot as a catch-up when the process starts later than
  that and no marker exists yet. The live lane never waits for it, and the old
  socket boot floor stays deleted.
- The day marker `data/state/daily/dhan_live_crossverify-YYYY-MM-DD.marker` is
  written only when three things are true: the persist succeeded, the run
  compared at least one minute, and the verdict was measured.
- storage: `VerifiedDayGate` plus `crossverify_hold_decision` hold the DAILY
  archive leg's partitions for day D until D's marker exists. The hold expires
  after `MAX_CROSSVERIFY_HOLD_DAYS` (3) with a loud coded error. A non-trading
  day counts as verified. The disk-pressure leg is not gated.
- The three restored log-filter alarms are `xverify_{vacuous,failed,diverged}`.
  Their Telegram phrases are restored with them.

## Edge Cases

- Box starts after 15:41 → catch-up runs at boot. Box starts after 15:41 and a
  marker exists → the catch-up is skipped.
- Weekend or holiday → the check does not run, and S3 treats the day as
  verified.
- Vendor returns nothing for every target → verdict is vacuous, no marker is
  written, the day is held, and the vacuous alarm pages.
- A security_id outside u32 → it is skipped and counted, never zeroed.
- Dhan disabled (`dhan_enabled=false`) → the task is not spawned. The archive
  hold then expires after 3 days with a loud error rather than filling the
  disk.

## Failure Modes

- Token or HTTP failure → coded `error!` with `source=xverify_failed`, no
  marker, and a page. S3 waits for up to 3 days, then archives anyway with a
  coded error.
- QuestDB persist fails → no marker, so the day is held and the failure is
  logged. The comparison result is not claimed.
- More than half of the compared price fields disagree → the diverged alarm
  pages. A marker is still written, because the check did run and measured.
- Clock skew near 15:41 → the wait uses the IST wall clock. The catch-up covers
  a missed fire.

## Test Plan

- app: `dhan_live_crossverify` (75 unit tests). `dhan_live_crossverify_boot`
  (10 tests), including
  `test_every_xverify_alarm_source_has_a_live_error_emit`, which is bite-proven:
  it fails when an emit is changed to `warn!`.
- storage: 6 `crossverify_hold_decision` / gate tests in `partition_archive.rs`.
- aws-lambdas: `alarm_phrase_coverage_guard` floor raised from 18 to 21, and
  all 19 test binaries green.
- common: `cloudwatch_app_alarms_wiring` (WS-GAP-03 patterns ≥ 4) and
  `error_code_paging_filter_drift_guard` both green.
- clippy `-D warnings` is clean on app, storage and aws-lambdas, and fmt is
  clean.

## Rollback

Revert the PR. The daily archive then runs ungated again. The alarms and
phrases are removed in the same revert, and the vendor-tape and audit tables
keep their rows. No schema is dropped.

## Observability

- Coded `error!` with `code=WS-GAP-03` and
  `source=xverify_{failed,vacuous,diverged}` feeds three CloudWatch log-filter
  alarms that page via Telegram.
- `held_unverified` appears in the daily archive summary. A hold that expires
  past its limit logs one error per pass.
- `dhan_live_crossverify_daily` / `_cell_audit` / `dhan_rest_1m_tape` are
  written every run.

## Per-Item Guarantee Matrix

See `per-wave-guarantee-matrix.md`. All 15 rows of the 100% Guarantee Matrix
(100% code coverage … 100% extreme check) and all 7 rows of the Resilience
Demand Matrix (Zero ticks lost … Real-time proof) apply to every item in this
plan. Item-specific evidence:

| Row | This plan |
|---|---|
| Testing | 75 + 10 app, 6 storage, lambdas guard 18→21, common wiring guards |
| Code checks | plan-gate, banned-pattern, secret, pub-fn test, pub-fn wiring |
| Performance | cold path, once a day; no hot-path change |
| Zero ticks lost | no new drop path; the S3 hold only delays archiving, never deletes |
| WS disconnects | the live lane never waits for the check; no boot floor |
| Uniqueness | audit DEDUP keys include `(security_id, segment, feed)` |
| Honest limit | F&O is not compared; the vendor tape is an independent record, not ground truth |

## Plan Items

- [x] Rule sections §12.15 and §2.5
- [x] Comparator + persistence restored
- [x] Boot task, marker, catch-up, spawn gated on `dhan_enabled`
- [x] storage `VerifiedDayGate` + hold decision + daily archive wiring
- [x] Terraform alarms + Telegram phrases + guard floors
- [x] tests / clippy / fmt green
- [ ] PR All Green; merge; deploy after 15:45 IST
