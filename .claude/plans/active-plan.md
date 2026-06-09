# Implementation Plan: Permanent AUTO-START guarantee — failed deploy must not disable the unit

**Status:** VERIFIED
**Date:** 2026-06-09
**Approved by:** Parthiban ("yes dude yes" — 2026-06-09)
**Crate(s) touched:** `tickvault-storage` (test only) + `.github/workflows/deploy-aws.yml` (infra)

## Context

Operator question 2026-06-09: *"will this auto start tomorrow as per the process
automatically can you guarantee me?"* Investigation found `deploy-aws.yml`'s
deploy-failure path ran `systemctl stop tickvault` **and** `systemctl disable
tickvault`. The `disable` is the auto-start killer: a disabled unit does not
start at the next 08:30 IST boot, AND `scripts/aws-autopilot.sh` treats a
disabled unit as an intentional kill-switch and refuses to self-heal it. So one
failed evening deploy silently removed the next morning's auto-start with almost
no signal. This is the permanent repo fix the operator asked for.

## Design

Keep the `systemctl stop` (it alone breaks the systemd `Restart=always`
"Auth OK" Telegram-per-minute spam loop for the current boot — an explicit stop
suppresses `Restart=always` until the next boot). DROP the `systemctl disable`.
The unit stays ENABLED on a failed deploy, so:
- systemd auto-starts it at the next 08:30 IST boot;
- `aws-autopilot.sh` restarts an enabled-but-inactive unit (existing self-heal);
- a genuinely-bad binary is replaced by the morning redeploy (~08:45 IST).
The intentional kill-switch (disable via AWS Control) stays the ONLY source of a
disabled unit, so the autopilot's `disabled == intentional` semantics remain
correct. No new Lambda / IAM / SSM machinery — the fix re-enables the existing
self-heal by removing the one line that defeated it.

## Edge Cases

- **Binary crash-loops after a passed smoke-test:** unit stays enabled → next
  boot crash-loops until the ~08:45 IST redeploy swaps a good binary. Bounded,
  self-healing, operator already paged by the failure Telegram. Acceptable vs.
  the old behaviour (silently disabled forever).
- **Intentional kill-switch (AWS Control disable):** unaffected — still the only
  path that disables; autopilot still respects it.
- **Failure before AWS creds configured:** the SSM send-command no-ops via
  `|| true` (unchanged).

## Failure Modes

- Guard false-match on a comment string: avoided — the workflow no longer
  contains the literal `disable tickvault` anywhere (the new comment uses
  `systemctl disable` without `tickvault`).
- Autopilot self-heal deleted in a future edit: pinned by Section C of the guard.

## Test Plan

- `crates/storage/tests/deploy_no_disable_on_failure_guard.rs`:
  - `deploy_failure_path_never_disables_tickvault_unit`
  - `deploy_failure_path_still_stops_to_break_restart_spam_loop`
  - `autopilot_still_self_heals_enabled_but_inactive_unit`
- `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/deploy-aws.yml'))"` parse check.

## Rollback

Pure infra (YAML + a test + this plan). Revert the single commit to restore the
prior `stop`+`disable` failure path. No data migration, no binary behaviour
change.

## Observability

No new metric needed — the existing deploy-failure Telegram fires unchanged
(wording updated to "stays set to auto-start at the next boot/deploy"). The
`aws-autopilot.sh` self-heal already emits its `note_ok` / restart logs. The
15-row + 7-row guarantee + resilience matrices are cross-referenced from
`.claude/rules/project/per-wave-guarantee-matrix.md`; the applicable rows for
this infra-only change are Recovery validation + Scenario coverage, both proven
by the source-scan guard above.

## Plan Items

- [x] Drop `systemctl disable tickvault` from the deploy-failure SSM command; keep `stop`
  - Files: .github/workflows/deploy-aws.yml
  - Tests: deploy_failure_path_never_disables_tickvault_unit, deploy_failure_path_still_stops_to_break_restart_spam_loop
- [x] Update the failure comment block + failure Telegram wording to reflect "unit stays enabled"
  - Files: .github/workflows/deploy-aws.yml
- [x] Add source-scan ratchet pinning the fix + the autopilot self-heal dependency
  - Files: crates/storage/tests/deploy_no_disable_on_failure_guard.rs
  - Tests: autopilot_still_self_heals_enabled_but_inactive_unit

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Evening deploy fails (smoke pass, runtime fail) | Unit STOPPED but ENABLED → next 08:30 boot auto-starts |
| 2 | Next morning boot with enabled-but-inactive unit | systemd starts it; autopilot restarts within 15 min if not |
| 3 | Operator intentionally disables via AWS Control | Autopilot respects kill-switch (unchanged) |
| 4 | Future edit re-adds `disable tickvault` | Build fails on the ratchet guard |
