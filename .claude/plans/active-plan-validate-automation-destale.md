# Implementation Plan: De-stale scripts/validate-automation.sh (CloudWatch-only reality)

**Status:** APPROVED
**Date:** 2026-06-14
**Approved by:** Parthiban (2026-06-14, verbatim: "yes dude fix and solve everything dude okay")
**Branch:** `claude/upbeat-hypatia-6trzk3`
**Authority:** CLAUDE.md > operator-charter-forever.md > design-first-wall.md > this file
**Scope guard:** `scripts/` + `.claude/plans/` only — NO `crates/*/src`. Plan-gate exempt.

## Design

`scripts/validate-automation.sh` still runs 4 checks against artifacts deliberately DELETED in the CloudWatch-only migration (#O1/#O2/#O3) + the 2026-06-10 depth-script retirement, plus a schema-self-heal grep hardcoded to a file the pattern moved out of. Result: `make validate-automation` exits 1 with 5 false FAILs even though the automation is intact. Verified this session: the 3 guard test files are gone, `auto-fix-restart-depth.sh` is gone, and `ADD COLUMN IF NOT EXISTS` now lives in 4 other storage files. Fix = remove the retired checks + repoint the self-heal grep so the sweep reflects reality and goes green honestly.

## Plan Items

- [x] Item 1 — Remove the 3 deleted-guard checks (`operator_health_dashboard_guard`, `resilience_sla_alert_guard`, `recording_rules_guard`) from `scripts/validate-automation.sh`.
  - Files: `scripts/validate-automation.sh`
- [x] Item 2 — Remove the retired `auto-fix restart-depth executable` check.
  - Files: `scripts/validate-automation.sh`
- [x] Item 3 — Repoint the schema-self-heal grep from the single hardcoded `instrument_persistence.rs` to a recursive grep over `crates/storage/src/` (robust to the pattern's location), + add a dated de-stale comment.
  - Files: `scripts/validate-automation.sh`
- [x] Item 4 — Sweep the repo for any OTHER stale references to the same retired artifacts (Makefile etc.) and confirm none remain (or fix).
  - Files: `scripts/validate-automation.sh` (+ any found)

## Edge Cases
- Don't remove still-live guards (loki_alloy_profile_guard, tickvault_logs_mcp_guard, etc. all PASS — keep them).
- Recursive grep must be scoped to `crates/storage/src/` so it can't false-pass on a test fixture.

## Failure Modes
- If a removed check protected something still live → caught by the post-edit full sweep going green only when the real guards pass.
- Zero runtime impact: this is a CI/operator validator script, not app code.

## Test Plan
- `bash scripts/validate-automation.sh` → exits 0, board shows only real PASSes, no false FAILs.
- `bash -n scripts/validate-automation.sh` parses clean.
- Pre-push gates: banned-pattern, plan-verify, fmt.

## Rollback
- Single shell script; `git revert <sha>` restores it verbatim. No runtime/schema/deploy impact.

## Observability
- This IS an observability-integrity fix: it restores the truthfulness of the `make validate-automation` signal so a green board means green.

## Scenarios
| # | Scenario | Expected |
|---|----------|----------|
| 1 | Operator runs `make validate-automation` | Green board, exit 0, no phantom Grafana/Prometheus/depth FAILs |
| 2 | A real guard regresses later | Sweep FAILs on the real check (signal preserved) |
| 3 | Someone re-adds a retired guard check | Visible in review; not reintroduced silently |
