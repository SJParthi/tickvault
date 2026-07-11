# Implementation Plan: Close the 09:10–09:20 IST Alarm Seam Spanning Market Open

**Status:** APPROVED
**Date:** 2026-07-09
**Approved by:** Parthiban (operator) — PR #3 of the 5-PR serial sequence, 2026-07-09 audit directive ("a process crash/death between ~09:10 and 09:20 IST — exactly spanning the market open — pages NOBODY")

## Plan Items

- [x] Move the boot-heartbeat alarm-action window close 09:10 → 09:20 IST so liveness coverage hands over to the market-hours window with no seam
  - Files: deploy/aws/terraform/boot-heartbeat-alarm.tf (close cron `cron(40 3 ? * MON-FRI *)` → `cron(50 3 ? * MON-FRI *)`, dated 2026-07-09 seam-closure notes, `state = "ENABLED"` pin on the close rule)
  - Tests: test_boot_heartbeat_window_hands_over_to_market_hours_window
- [x] Update the stale window references in market-hours-liveness-alarm.tf (comments + output description only)
  - Files: deploy/aws/terraform/market-hours-liveness-alarm.tf
  - Tests: (comment-only — covered by existing test_gate_lambda_open_* pins staying green)
- [x] Update the boot-heartbeat window doc comment in crates/app/src/main.rs (`emit_boot_completed` rustdoc, 08:50–09:10 → 08:50–09:20; comment-only, zero logic change)
  - Files: crates/app/src/main.rs
  - Tests: (no behavior change — boot_completed_metric_guard stays green)
- [x] Add the handover ratchet test pinning both crons + the close-rule state so the seam can never silently reopen
  - Files: crates/common/tests/aws_alarm_semantics_guard.rs
  - Tests: test_boot_heartbeat_window_hands_over_to_market_hours_window
- [x] Append the dated 2026-07-09 seam-closure note to the boot-heartbeat contract docs (§19)
  - Files: .claude/rules/project/daily-universe-scope-expansion-2026-05-27.md
  - Tests: (docs)

## Design

The boot-heartbeat alarm (`tv-<env>-boot-heartbeat-missing`, `tv_boot_completed` MISSING with `treat_missing_data=breaching`, 2×60s evaluation) had its actions gated to 08:50–09:10 IST by the boot-window Lambda. The market-hours liveness window only opens at 09:20 IST, and its open-mode Lambda resets its 9 alarms to OK — with a 5-period missing-data evaluation its first possible page is ~09:25-09:26. Net: a process death anywhere in [09:10, 09:20) IST — spanning the 09:15 NSE open — pages nobody inside the seam and at best ~09:25 (up to ~15 min blind). Fix: move ONLY the boot-window CLOSE cron to `cron(50 3 ? * MON-FRI *)` (09:20 IST), the exact minute the market-hours window opens, so coverage hands over seamlessly. `tv_boot_completed` is the correct signal for the extension: metrics-exporter-prometheus re-renders the gauge on every 60s CW-agent scrape while the process is alive (verified: `scrape_interval: 60s` + the metric_selectors allowlist in user-data.sh.tftpl), so a healthy app publishes a datapoint every alarm period through 09:10–09:20 (no false page) and a dead one goes missing → page within ~2-5 min. Cost: 0 new alarms, 0 new metrics, 0 new Lambdas — a cron-schedule change. The rejected alternative (opening the market-hours window at 09:10) would arm 9 alarms whose signals are deliberately invalid pre-09:20 (SLO tick-freshness pre-open pin, 9-of-15 degraded lookback) — re-opening the pre-open false-page class the 09:20 gate exists to prevent.

## Edge Cases

- Weekends: both crons are MON-FRI — window never opens; unchanged.
- Weekday NSE holidays: box self-stops ~08:32 (holiday-gate.sh); the boot gate is holiday-blind (pre-existing) and the alarm latches ALARM ~08:52 with one page — keeping actions enabled to 09:20 emits NO additional SNS (CloudWatch notifies only on state transitions; `disable_alarm_actions` at close emits nothing). No new holiday page class.
- 16:30 IST intentional stop: actions disabled since 09:20 — cannot page. The new `state = "ENABLED"` pin on the close rule guards against a once-disabled close rule silently leaving actions armed (the #1404 provider-stops-managing-state lesson).
- Same-minute 09:20 crons: the boot-close Lambda disables ONLY `boot_heartbeat_missing`; the market-open Lambda manages a disjoint 9-alarm set — no race.
- Death in ~[09:16, 09:20): honest residual — the boot alarm may not complete its evaluation (missing-data evaluation-range padding) before close; the market-hours alarm pages ~09:25-09:26 (worst ~9-10 min, inside the ≤10 min envelope).
- Double-page for one death (boot ~09:14-16 + market ~09:25-26): pre-existing class for pre-09:10 deaths, merely extended; two edge-triggered transitions, not a storm.

## Failure Modes

- Close Lambda invocation fails at 09:20 → actions stay armed → the 16:30 stop would page once; the existing gate-error watchman pattern + the boot gate Lambda's own log group surface it. Bounded to one false page per failure.
- EventBridge drops the close cron slot → same as above; `state = "ENABLED"` ensures terraform keeps the rule managed.
- CW-agent scrape lag delays the missing-data verdict by 1-2 periods → detection ~09:15-09:16 for a 09:12 death instead of ~09:14 — still inside the envelope.
- App healthy but `tv_boot_completed` stops being scraped (agent death) → alarm pages: honest, because a dead CW agent during market open IS an operator-actionable monitoring outage.

## Test Plan

- `cargo test -p tickvault-common --test aws_alarm_semantics_guard` — the new handover ratchet (full `schedule_expression` attribute-line pins + `state = "ENABLED"` pin + `!cron(40 3` regression pin) plus the 4 pre-existing pins.
- `cargo test -p tickvault-app --test boot_completed_metric_guard --test cloudwatch_agent_glob_guard` — the boot-metric + gate-lambda holiday-safety pins stay green.
- `cargo test -p tickvault-common --test cloudwatch_app_alarms_wiring` — alarm wiring pins stay green.
- `terraform validate` runs in Deploy Lint CI (terraform not installed locally — stated honestly).
- `bash .claude/hooks/banned-pattern-scanner.sh` clean.

## Rollback

Single-commit revert restores the 09:10 close (`cron(40 3 ? * MON-FRI *)`) and the prior comments; the guard test travels in the same commit so a revert removes the pin with the cron (no orphaned red test). No state migration: EventBridge rules/Lambda are terraform-managed in-place updates — `terraform apply` of the revert restores the previous schedule. The alarm itself, its metric, and both Lambdas are untouched in behavior, so rollback risk is limited to the window returning to its previous (seam-open) shape.

## Observability

- The page path is the EXISTING chain: `tv-<env>-boot-heartbeat-missing` → SNS `tv_alerts` → Telegram webhook Lambda. No new metric, no new alarm, no new Lambda (cost delta ₹0 / $0 vs aws-budget.md — a cron-schedule string change).
- Seam-closure proof: a dead app at 09:12 IST now transitions the boot alarm OK→ALARM at ~09:14-09:16 with actions enabled → one page; previously nothing could page until ~09:25-09:26.
- Ratchet: `crates/common/tests/aws_alarm_semantics_guard.rs::test_boot_heartbeat_window_hands_over_to_market_hours_window` fails the build if either cron or the close-rule state pin drifts.
- Docs: dated 2026-07-09 note appended to `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md` §19 (the boot-heartbeat contract home).

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | App dies 09:12 IST trading day | Boot alarm ALARM ~09:14-16, SNS page (was: nothing until ~09:25) |
| 2 | Healthy app 09:10–09:20 | `tv_boot_completed` datapoint every 60s period → alarm stays OK, no page |
| 3 | Weekday NSE holiday (box stopped 08:32) | One pre-existing page ~08:52; NO additional page from the 10-min extension |
| 4 | 16:30 IST intentional stop | Actions disabled since 09:20 → no page |
| 5 | Death ~09:18 | Boot alarm may miss before close; market alarm pages ~09:25-26 (documented residual) |
| 6 | Future PR moves either 09:20 cron alone | Ratchet test fails the build |

## Per-Item Guarantee Matrix

Cross-reference: `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row matrices apply as written). Item-specific deltas: no hot-path code touched (terraform + comments + one rustdoc); no new tick-drop path; no new pub fn; DEDUP/uniqueness untouched; coverage delta ≥ 0 (one new ratchet test); honest 100% claim carried in the PR body with the envelope qualifier.
