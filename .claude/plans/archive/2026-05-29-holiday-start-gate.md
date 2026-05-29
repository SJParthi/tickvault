# Implementation Plan: Holiday-aware EC2 start gate (in-boot self-stop)

**Status:** VERIFIED
**Date:** 2026-05-29
**Approved by:** Parthiban (selected "Holiday-aware start gate" via AskUserQuestion 2026-05-29)

## Problem

EventBridge cron starts the m8g.large box `cron(0 3 ? * MON-FRI *)` (08:30 IST)
EVERY weekday — including NSE weekday holidays (Republic Day, Independence Day,
Diwali, etc.). On a holiday the box runs ~8h for a no-op boot (the in-app §22
orchestrator already skips the CSV retry loop, but the instance still bills +
emits boot noise). During the 3-month data-pull this wastes cost + adds noise.

## Design — Architecture B: in-boot self-stop (single source of truth)

A dedicated oneshot systemd unit runs on every boot BEFORE `tickvault.service`.
It asks the app's OWN binary `--check-trading-day` (which reads the SAME
`config/base.toml` NSE holiday list via `TradingCalendar` — ZERO duplication),
and on a DEFINITIVE non-trading verdict it self-stops the instance. FAIL-OPEN
on every uncertainty (binary missing on first boot, config error, IMDS error,
unknown exit code) so it can NEVER accidentally kill a real trading day.

Override: operator touches `/opt/tickvault/ALLOW_HOLIDAY_RUN` to work on a
holiday without the auto-stop (manual runs per operator's "I start manually").

Exit-code contract (no stdout/stderr — clippy bans print_*; shell reads `$?`):
- `0`  = trading day → let app start
- `75` = non-trading day (weekend/holiday) → gate self-stops
- `70` = config/calendar load error → FAIL-OPEN (treated as 0 by the gate)

## Plan Items

- [x] Item 1 — `--check-trading-day` CLI short-circuit in `main()`
  - Files: crates/app/src/main.rs
  - Pure helper `trading_day_gate_exit_code(is_trading_day: bool) -> i32`
  - Tests: test_trading_day_gate_exit_code_trading, test_trading_day_gate_exit_code_non_trading

- [x] Item 2 — holiday-gate shell script (override + binary check + self-stop)
  - Files: deploy/aws/holiday-gate.sh
  - IMDSv2 token → instance-id + region → `aws ec2 stop-instances`; fail-open

- [x] Item 3 — dedicated oneshot systemd unit (Before=tickvault.service)
  - Files: deploy/systemd/tickvault-holiday-gate.service

- [x] Item 4 — install + enable the gate unit in first-boot user-data
  - Files: deploy/aws/terraform/user-data.sh.tftpl

- [x] Item 5 — Terraform IAM: ec2:StopInstances scoped to tv_app on instance role
  - Files: deploy/aws/terraform/main.tf

- [x] Item 6 — Z+ source-scan ratchet pinning gate wiring + IAM + fail-open
  - Files: crates/storage/tests/aws_deploy_safety_guard.rs
  - Tests: holiday_gate_is_wired_and_fail_open

- [x] Item 7 — runbook section: holiday gate + override marker
  - Files: docs/runbooks/may31-inplace-upgrade-and-access.md

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Trading weekday | `--check-trading-day` exit 0 → app starts |
| 2 | NSE weekday holiday | exit 75 → gate stops instance, app does not run |
| 3 | Weekend (manual start) | exit 75 → gate stops UNLESS override marker present |
| 4 | Operator holiday work | `touch ALLOW_HOLIDAY_RUN` → gate exits 0, app runs |
| 5 | First boot (binary absent) | gate fail-open → exits 0, normal first-boot flow |
| 6 | Config load error | exit 70 → gate fail-open → exits 0 (app surfaces error) |
| 7 | IMDS/aws unreachable | gate logs + fail-open → does NOT loop-stop |

## Z+ 7-row resilience matrix

| Demand | This change |
|---|---|
| Zero ticks lost | no tick-path change |
| WS never disconnects | untouched |
| Never slow/locked/hanged | oneshot gate, no restart loop (separate unit, not ExecStartPre) |
| QuestDB never fails | untouched |
| O(1) latency | gate is cold-path boot-only; one calendar lookup |
| Uniqueness + dedup | n/a |
| Real-time proof | journald log of the verdict → CloudWatch; stop visible in console |

## Honest 100% claim

100% inside the tested envelope: the gate stops the instance ONLY on a
definitive non-trading exit code (75) from the app's own calendar; every other
outcome (config error, missing binary, IMDS failure, unknown code, override
marker present) FAILS OPEN so a real trading day is never killed. Source-scan
ratchet pins the wiring + the fail-open default + the scoped IAM statement.
