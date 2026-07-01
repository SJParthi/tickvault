# Implementation Plan: BP-09 — autopilot recovers a systemd `failed` crash-loop

**Status:** VERIFIED
**Date:** 2026-07-01
**Approved by:** Parthiban (operator) — grounded directive, this session (continue the permutation-coverage audit BP-09 fix; the prod box flapped start/stop this morning)

> **Guarantee matrices:** carried by cross-reference to
> `.claude/rules/project/per-wave-guarantee-matrix.md` (the 15-row 100% matrix +
> the 7-row resilience matrix) and `operator-charter-forever.md` §C — all 15+7
> rows apply to this item. This is a bash + docs + source-scan-guard change (no
> Rust hot path, no new tick-drop path, no new WS/QuestDB path, no new DEDUP
> table), so the hot-path/DHAT/bench/DEDUP rows resolve `N/A — not a hot path`
> and the tick-loss/WS/O(1) rows resolve `N/A — infra self-heal script, no tick
> path touched`; the coverage/audit-of-fix/logging/functionality/extreme-check
> rows are satisfied by the source-scan guard test below.

## Design

BP-09 (permutation-coverage audit §169, gaps-matrix row): `scripts/aws-autopilot.sh`
self-heals an **enabled-but-inactive** `tickvault` unit by running
`systemctl restart tickvault` (line 205). But `deploy/systemd/tickvault.service`
sets `StartLimitIntervalSec=600` + `StartLimitBurst=8`: after **> 8 restarts in
10 min** systemd transitions the unit to `failed` and refuses further
(re)starts — a plain `restart`/`start` then returns *"Start request repeated too
quickly"* and is a **no-op**. systemd requires `systemctl reset-failed` FIRST to
clear the start-limit counter before it will start the unit again (the service
file's own comment, line 93, documents the manual recovery
`systemctl reset-failed tickvault && systemctl start tickvault`). Today the box
stays down until a human runs `reset-failed`; autopilot correctly *pages* but
cannot *recover* — exactly the BP-09 gap, and consistent with the prod box
flap-loop observed this morning (team memory `dhan-feed-reset-root-cause-2026-06-30`
— a Dhan-RST-driven restart storm is the crash-loop that trips StartLimitBurst).

**Fix (minimal, correct, idempotent):** in the enabled-but-inactive branch,
replace the single `systemctl restart tickvault` with
`systemctl reset-failed tickvault` (clears any start-limit `failed` state — a
safe no-op on a non-failed unit) **then** `systemctl start tickvault`. This
recovers BOTH cases with one code path:
- plain enabled-but-inactive (not failed): `reset-failed` is a harmless no-op,
  `start` brings it up (equivalent to the old `restart` for this case);
- StartLimit-`failed` crash-loop: `reset-failed` clears the counter so `start`
  actually launches the unit — the case the old `restart` could not recover.

`start` (not `restart`) is correct after `reset-failed`: the unit is inactive,
so `start` launches it; `restart` would also work but `start` is the precise
intent. The kill-switch (`disabled` unit) branch is UNCHANGED — a disabled unit
is still treated as an intentional stop and never auto-recovered.

Two companion corrections in the same PR:
1. **Runbook doc-drift** — `docs/runbooks/aws-tickvault-exit-loop.md` says
   "3 retries" / "3rd systemd restart" while the service file is
   `StartLimitBurst=8` / `StartLimitIntervalSec=600`. Correct to
   "8 restarts in 10 min" and add the `reset-failed`-then-`start` recovery
   command that autopilot now performs (so operator + autopilot agree).
2. **Guard test** — extend the existing autopilot self-heal ratchet
   (`crates/storage/tests/deploy_no_disable_on_failure_guard.rs` Section C
   already pins the restart mechanism) with a test asserting autopilot invokes
   `reset-failed tickvault` before the start, so this recovery can never
   silently regress to a bare `restart` (which is the no-op that caused BP-09).

## Edge Cases

- **Unit not failed, just inactive:** `reset-failed` on a healthy/inactive unit
  is a documented no-op (exit 0, nothing to clear) — `start` then launches it.
  No behavioural regression vs the old `restart` for this common case.
- **Unit disabled (kill-switch):** untouched — the `ENABLED == disabled` branch
  short-circuits before reaching the restart path, so a 429-cooldown intentional
  stop is still never auto-recovered.
- **reset-failed itself fails / SSM hiccup:** the `2>/dev/null` + subsequent
  `is-active` re-check still classifies the outcome; a failed recovery routes to
  `note_issue` ("not active after restart") exactly as before — autopilot still
  pages, never silently claims success (audit Rule 11, no false-OK).
- **Repeated crash-loop that re-trips StartLimit after recovery:** autopilot runs
  every 15 min; each run clears + restarts once. If the app immediately re-loops
  to `failed`, the next run's `is-active` re-check reports inactive -> `note_issue`
  pages. autopilot does not mask a persistent crash — it recovers a *stuck*
  `failed` state (the BP-09 harm) without hiding a *genuine* repeating crash.
- **Guard test running with no repo file present:** the guard reads the script
  from `repo_root()` (same helper the file already uses); a missing file panics
  the test loudly (correct — the script must exist).

## Failure Modes

- If Dhan keeps RST-storming the feed (team memory 2026-06-30), the app can
  crash-loop -> StartLimit `failed` again; autopilot now un-sticks it every 15
  min instead of leaving it dead all day. This does NOT fix the upstream RST
  cause (a separate DHAN-LANE / WS-GAP-09 concern) — it removes the
  self-inflicted "box stays down until a human runs reset-failed" amplifier.
  Honest envelope: recovery of the systemd `failed` state, not prevention of the
  crash that caused it.
- No data path touched: zero tick-loss / WS / QuestDB / DEDUP exposure. This is
  a control-plane heal script.

## Test Plan

- `crates/storage/tests/deploy_no_disable_on_failure_guard.rs` — NEW Section D
  test `autopilot_resets_failed_before_start_to_recover_crash_loop`: source-scan
  asserts `scripts/aws-autopilot.sh` contains `reset-failed tickvault` AND that
  the recovery path launches the unit (`systemctl start tickvault` present).
  Fails the build if the fix regresses to a bare `restart`.
- Existing Section C test (`autopilot_still_self_heals_enabled_but_inactive_unit`)
  is UPDATED: it asserted `systemctl restart tickvault`; the recovery now uses
  `reset-failed` + `start`, so the assertion is retargeted to `systemctl start
  tickvault` (the mechanism the auto-start guarantee now depends on) — keeping
  the ratchet intact against silent deletion.
- `cargo test -p tickvault-storage --test deploy_no_disable_on_failure_guard`
  green.
- `bash -n scripts/aws-autopilot.sh` — script parses.
- Guard hooks: `per-item-guarantee-check.sh` (active-plan.md PASS),
  `plan-verify.sh`, `banned-pattern-scanner.sh`, `pub-fn-test-guard.sh`,
  `pub-fn-wiring-guard.sh`.

## Rollback

Single squash commit; `git revert <sha>` restores the prior autopilot
`restart`, the prior runbook text, and drops the new guard test. No schema, no
migration, no data, no config-flag state — a pure revert is complete and safe.

## Observability

No new metric/ErrorCode needed — autopilot already emits `note_heal` /
`note_issue` -> GitHub Step Summary + Telegram via SNS (`tv-prod-alerts`). The
heal message ("restarted app (tickvault) — was inactive") continues to fire on
success and the issue message on failure, so the operator sees the recovery (or
its failure) exactly as before, now for the `failed`-crash-loop case too. The
source-scan guard test is the regression-observability for the fix itself.

## Plan Items

- [x] Item 1 — autopilot `reset-failed` + `start` recovery
  - Files: scripts/aws-autopilot.sh
  - Tests: autopilot_resets_failed_before_start_to_recover_crash_loop
- [x] Item 2 — runbook doc-drift fix (3->8 retries, add reset-failed recovery)
  - Files: docs/runbooks/aws-tickvault-exit-loop.md
  - Tests: (docs; covered by guard test's script assertion + manual read)
- [x] Item 3 — extend/retarget the autopilot self-heal guard
  - Files: crates/storage/tests/deploy_no_disable_on_failure_guard.rs
  - Tests: autopilot_resets_failed_before_start_to_recover_crash_loop, autopilot_still_self_heals_enabled_but_inactive_unit

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Unit in StartLimit `failed` (>8 restarts/10min), autopilot runs | reset-failed clears counter, start relaunches, note_heal; box recovers without a human |
| 2 | Unit enabled-but-inactive (not failed) | reset-failed is a no-op, start launches, note_heal (unchanged behaviour) |
| 3 | Unit disabled (kill-switch) | untouched — note_ok "intentionally stopped"; never auto-recovered |
| 4 | App re-loops to `failed` immediately after recovery | next run's is-active re-check -> note_issue pages; no false-OK |
| 5 | Guard test after a future edit reverts to bare `restart` | build fails (reset-failed assertion) |
