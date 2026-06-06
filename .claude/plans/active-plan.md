# Implementation Plan: fill the 3 acknowledged doc gaps (kill-switch runbook + GitHub OIDC runbook + override-log link fix)

**Status:** VERIFIED
**Date:** 2026-06-06
**Approved by:** Parthiban — "yes dude go ahead" (write the 3 real docs identified in the doc audit).
**Crate(s) touched:** none (docs + 1 test-file allowlist edit). No production source, no new ErrorCode.

## Context

The doc audit found the codebase's own cross-link guard (`runbook_cross_link_guard.rs`) carries a
shrinking allowlist of acknowledged-missing docs. Three are real gaps for LIVE functionality:
1. `docs/runbooks/kill-switch.md` — kill-switch is live OMS code (`activate/deactivate/
   get_kill_switch_status`) and prod `events.rs` tells the operator "decide go/no-go per kill-switch
   runbook", but the runbook doesn't exist.
2. `docs/runbooks/github-oidc-setup.md` — allowlisted TODO; `aws-deploy.md` step 1 points here for the
   GitHub-Actions→AWS OIDC role (`AWS_ROLE_ARN` secret, `id-token: write`, ap-south-1).
3. `docs/templates/override_log.md` — allowlisted TODO, BUT the file already exists as
   `override-log.md` (hyphen); the reference in `daily-operations.md` is a 1-char typo
   (`override_log` vs `override-log`). Fix the link, don't author a duplicate.

## Design

- **kill-switch.md** — operator runbook from the verified facts: endpoints `POST /v2/killswitch?
  killSwitchStatus=ACTIVATE|DEACTIVATE`, `GET /v2/killswitch`; prerequisite (all positions closed +
  no pending orders before ACTIVATE → else `exit_all_positions()` first); day-scoped; the OMS client
  fns that drive it; when to pull it (the OptionChainStaleHalt go/no-go reference); P&L-exit
  (`/pnlExit`) as the related auto-exit. Ground truth: `docs/dhan-ref/15-traders-control.md`.
- **github-oidc-setup.md** — steps matching the real workflows: create the GitHub OIDC IAM identity
  provider (`token.actions.githubusercontent.com`), an IAM role (`tv-github-deploy-role`) with a trust
  policy scoped to `repo:SJParthi/tickvault:*`, attach least-privilege deploy permissions, set the
  `AWS_ROLE_ARN` repo secret + `AWS_REGION` var; replaces the long-lived-creds temporary workaround.
- **daily-operations.md** — change `docs/templates/override_log.md` → `docs/templates/override-log.md`
  (existing file).
- **runbook_cross_link_guard.rs** — remove the two now-resolved allowlist entries
  (`docs/runbooks/github-oidc-setup.md`, `docs/templates/override_log.md`) so the guard ratchets down.

## Edge Cases

- Removing allowlist entries makes the guard ENFORCE resolution: kill-switch.md + github-oidc-setup.md
  now exist; the override_log reference is fixed to the existing override-log.md → all resolve.
- kill-switch.md is referenced from a phase doc (not a runbook) so it was never allowlisted; creating
  it is purely additive.

## Failure Modes

- Docs + one test-file edit; zero production-behavior impact. The only failure surface is the
  cross-link guard, which I run locally to confirm green after the allowlist shrink.

## Test Plan

- `cargo test -p tickvault-common --test runbook_cross_link_guard` — green after the 2 new docs + the
  link fix + the allowlist shrink.
- `bash .claude/hooks/plan-verify.sh` + plan-gate green.

## Rollback

- `git revert` removes the 2 docs, restores the typo + the 2 allowlist entries. Pure docs revert.

## Observability

- N/A — documentation. The guard test is the mechanical proof the links resolve.

## Plan Items

- [x] Item 1 — write `docs/runbooks/kill-switch.md` (live OMS kill-switch operator runbook)
  - Files: `docs/runbooks/kill-switch.md`
  - Tests: every_repo_path_referenced_in_runbooks_resolves
- [x] Item 2 — write `docs/runbooks/github-oidc-setup.md` + remove its allowlist entry
  - Files: `docs/runbooks/github-oidc-setup.md`, `crates/common/tests/runbook_cross_link_guard.rs`
  - Tests: every_repo_path_referenced_in_runbooks_resolves
- [x] Item 3 — fix `daily-operations.md` override-log link + remove its allowlist entry
  - Files: `docs/runbooks/daily-operations.md`, `crates/common/tests/runbook_cross_link_guard.rs`
  - Tests: every_repo_path_referenced_in_runbooks_resolves

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Operator hits OptionChainStaleHalt Telegram | kill-switch.md tells them go/no-go + how to pull the switch |
| 2 | New deployer sets up CI→AWS | github-oidc-setup.md gives the exact IAM OIDC steps |
| 3 | Operator logs a manual override | daily-operations.md links to the real override-log.md template |
| 4 | Cross-link guard runs | all referenced paths resolve; allowlist shrank by 2 |
