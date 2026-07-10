# Implementation Plan: Window-gate ws-pool liveness alarms (pre-09:00 deferral false pages)

**Status:** VERIFIED
**Date:** 2026-07-10
**Approved by:** Parthiban (operator) — 2026-07-10 directive: "gate that alarm on the deferral window/market-closed state (edge-triggered, no false-OK masking of REAL pre-open outages — keep the 08:45 readiness pager authoritative)"

> Guarantee matrices: this item cross-references
> `.claude/rules/project/per-wave-guarantee-matrix.md` (the 15-row + 7-row
> matrices apply per that file; this is a terraform+ratchet-only change —
> no hot path, no tick path, no new pub fn, no new QuestDB table).

## The confirmed bug (2026-07-10 incident)

`tv-${env}-ws-pool-all-dead` (app-alarms.tf, metric `tv_websocket_pool_all_dead`)
pages "ALL live market data connections are down" during the BY-DESIGN
pre-09:00 IST Dhan connection deferral: the pool watchdog sets the gauge
unconditionally every 5s (crates/core/src/websocket/pool_watchdog.rs:170)
while the pool deliberately opens ZERO sockets until 09:00 IST
(crates/app/src/main.rs:4836). The alarm is the ONLY liveness-class alarm
NOT in the market-hours window-gate Lambda's ALARM_NAMES list
(market-hours-liveness-alarm.tf), so it is armed 24/7 — false pages every
trading morning ~08:34–09:00 and during overnight catch-up-deploy boots
(observed 2026-07-10 at 02:45, 03:42, 08:34). Sibling
`tv-${env}-ws-failed-connections` (metric
`tv_websocket_failed_connections_count`, written by the same ungated
watchdog tick) has the same false class.

## Design

Keep the gauge honest (no producer change); window-gate the alarm ACTIONS —
the exact house pattern already used by `tick_gap_instruments_silent`,
`aggregator_no_seals`, `realtime_guarantee_critical` and the 3 silent-feed
alarms:

1. `deploy/aws/terraform/app-alarms.tf`: set `actions_enabled = false` on
   BOTH `ws_pool_all_dead` and `ws_failed_connections`, with a dated
   2026-07-10 comment block (incident, deferral rationale, honest coverage
   envelope). `alarm_actions` / `ok_actions` unchanged.
2. `deploy/aws/terraform/market-hours-liveness-alarm.tf`: append both alarm
   name references to the window-gate Lambda's `ALARM_NAMES = join(...)`
   env list, so actions enable at the 09:20 IST open cron and disable at
   the 15:35 IST close cron. The open path additionally does
   `set_alarm_state(OK)` on every gated alarm, so a stale pre-open ALARM
   state is reset at window open (edge-triggered — no false-page
   carry-over into the armed window). No cron or Lambda-code change needed
   (the Lambda iterates the env list). Update the "9 gated alarms"
   watchman comment + description to 11.
3. Ratchet test in the house style
   (`crates/common/tests/cloudwatch_app_alarms_wiring.rs`, reusing
   `alarm_resource_block` + `block_has_attr`):
   `test_ws_pool_alarms_are_window_gated_not_always_armed` — pins (a) both
   alarm resources carry `actions_enabled = false`; (b) both alarm-name
   references appear INSIDE the ALARM_NAMES join body in
   market-hours-liveness-alarm.tf.

## Edge Cases

- **Real pre-09:20 outage:** covered by the boot-heartbeat alarm
  (tv_boot_completed, actions windowed 08:50–09:20 IST per
  boot-heartbeat-alarm.tf) + the 08:45 readiness pager; from 09:20 the
  market-hours-liveness alarm + these two re-armed alarms take over. A
  pool genuinely dead past 09:20 pages within ~2 min of window open
  (ws_pool_all_dead is 2×60s eval after the open-time OK reset).
- **Stale pre-open ALARM state:** the gate Lambda's open mode calls
  `set_alarm_state(OK)` on every ALARM_NAMES member, so a breaching
  pre-09:00 deferral window can never carry a page into the armed window.
- **Weekday NSE holiday:** the gate Lambda's open path checks the
  holiday-stop SSM marker + instance state FIRST and skips enabling —
  these two alarms inherit that existing safety.
- **Gate Lambda failure:** the existing `tv-${env}-market-hours-gate-errors`
  watchman alarm pages on a failed open/close invocation; its
  description/comment counts move 9 → 11 in this PR.
- **15:35 close:** actions disabled again, so the 16:30 IST nightly stop /
  weekend metric staleness never pages (same rationale as the sibling
  gated alarms).

## Failure Modes

- **Gate open never fires (scheduler drop / disabled rule):** the two
  alarms stay actions-disabled for the session — the same accepted
  residual as the other 9 gated alarms, watched by the gate-errors
  watchman + the explicit `state = "ENABLED"` pins on the event rules.
  The market-hours-liveness-missing alarm still owns whole-app death.
- **Terraform drift (someone re-enables always-on actions or drops the
  ALARM_NAMES entries):** the new ratchet test fails the build.
- **False-OK masking:** none introduced — the gauge itself is unchanged
  (still honest in dashboards / /health), only PAGING is windowed; in-window
  sensitivity (metric/threshold/eval periods) is unchanged.

## Test Plan

- `cargo test -p tickvault-common --test cloudwatch_app_alarms_wiring` —
  the new `test_ws_pool_alarms_are_window_gated_not_always_armed` plus all
  existing pins in that file must stay green.
- `cargo test -p tickvault-common --test aws_alarm_semantics_guard` — the
  boot-window handover pins must stay green (unchanged files, sanity).
- `bash .claude/hooks/banned-pattern-scanner.sh` clean;
  `cargo fmt --check` clean on the touched Rust test file.

## Rollback

`git revert` of the single commit restores the always-armed alarms
(actions_enabled default true, ALARM_NAMES list back to 9 entries) and
removes the ratchet test — no state migration; on the next
`terraform apply` the alarms return to 24/7 arming. No app binary change,
no data change.

## Observability

- No metric change: `tv_websocket_pool_all_dead` and
  `tv_websocket_failed_connections_count` keep publishing every scrape
  (gauge stays honest for dashboards, doctor, and the SLO WS_health
  dimension).
- Alarm behaviour observable via the gate Lambda's log lines
  ("enabled/disabled actions for [...]") + the gate-errors watchman alarm.
- The dated tf comments document the 2026-07-10 incident + the honest
  coverage envelope for future sessions.

## Plan Items

- [x] Item 1 — `actions_enabled = false` + dated 2026-07-10 comment blocks on
  `ws_pool_all_dead` + `ws_failed_connections`
  - Files: deploy/aws/terraform/app-alarms.tf
  - Tests: test_ws_pool_alarms_are_window_gated_not_always_armed

- [x] Item 2 — add both alarm names to the window-gate Lambda ALARM_NAMES
  join; update the watchman 9→11 counts + description
  - Files: deploy/aws/terraform/market-hours-liveness-alarm.tf
  - Tests: test_ws_pool_alarms_are_window_gated_not_always_armed

- [x] Item 3 — ratchet test pinning actions_enabled=false + ALARM_NAMES
  membership for both alarms
  - Files: crates/common/tests/cloudwatch_app_alarms_wiring.rs
  - Tests: test_ws_pool_alarms_are_window_gated_not_always_armed

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | 08:34 IST boot, pool deferring until 09:00 (gauge = 1) | No page (actions disabled outside window) |
| 2 | 02:45 IST overnight catch-up-deploy boot | No page |
| 3 | Pool genuinely dead at 09:30 IST | Page within ~2 min (2×60s eval, actions armed since 09:20) |
| 4 | Pool dead at 08:55 IST and STAYS dead | boot-heartbeat/readiness pager pre-09:20; ws-pool page ≤ ~09:22 |
| 5 | Alarm in ALARM at 09:19 from the deferral window | Gate open forces OK at 09:20 — no carry-over page; re-fires only if still breaching in-window |
| 6 | 16:30 IST nightly stop | No page (actions disabled at 15:35) |
