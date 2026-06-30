# Implementation Plan: AWS cost-hardening / never-cross + corrected EIP approach

**Status:** VERIFIED
**Date:** 2026-06-30
**Approved by:** Parthiban (operator) — directive given this session 2026-06-30 ("build the AWS cost-hardening / never-cross PR including the corrected EIP approach: enable auto-assign-public-IP + REMOVE the EIP since the EIP is only needed for live orders which are OFF").

> Guarantee matrices: this item carries the 15-row + 7-row matrices by
> cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`.
> The dominant guarantees for THIS change are (a) the order-safety invariant
> (`dry_run=true` UNCHANGED, no real orders) and (b) the budget-never-cross
> invariant (total-account budget filter + native AWS Budget Action +
> hourly out-of-window hard-stop).

## Design

The AWS box (`i-0b956d0209231a48b`, m8g.large, ap-south-1) runs a 3-month
no-orders data-pull (`dry_run=true`). Two cost-control gaps make the budget
"never-cross" guarantee currently HOLLOW, plus the operator's corrected
networking approach removes the ~₹300/mo Elastic IP cost:

1. **Budget is blind.** `budget.tf` filters cost by the `Project=tickvault`
   tag, which is NOT applied to the actual spend (EC2/EBS/EIP), so the budget
   measures ~$0 vs the real ~$35 → the killswitch never fires. Fix: drop the
   `cost_filter` so the budget measures TOTAL account spend.
2. **Ceiling too low / un-synced.** Raise $25 → $55 in BOTH synced places
   (`budget.tf` `limit_amount` + the `BUDGET_USD` env in the digest Lambda).
3. **No native AWS stop on breach.** Add an `aws_budgets_budget_action`
   (`RUN_SSM_DOCUMENTS` `AWS-StopEC2Instance`) at 100% + 90% ACTUAL — a native
   AWS guarantee independent of the killswitch Lambda.
4. **Out-of-window run can bill a full overnight.** Deploy self-start +
   hourly hard-stop currently only stop once/day. Make the hard-stop guard
   HOURLY with an out-of-window IST time check; make the deploy self-stop the
   box it started when now outside the 08:30-16:30 IST Mon-Fri window.
5. **Corrected EIP approach.** The 2026-05-31 EIP-flip note shows the running
   ENI does NOT auto-assign a dynamic public IP on stop/start. The operator's
   corrected approach: enable subnet auto-assign + instance
   `associate_public_ip_address=true` (so a FRESH launch gets a dynamic IP),
   then count-gate the EIP OFF (`enable_eip=false`). The EIP is only needed for
   live orders (Dhan static-IP whitelist) which are OFF. Document the
   operator-side ENI re-association for the *existing* running box + the
   orphan EIP release. Set `ip_verification_enabled=false` (no orders → no
   static-IP gate); the real gate is `strategy.mode`/`dry_run` (UNCHANGED).
6. **Cosmetic correctness.** Flip the telegram-webhook SSM-param defaults
   `/tickvault/staging/telegram/*` → `/tickvault/prod/telegram/*` (verified the
   prod params exist, staging is empty) + fix the stale "TV_ENVIRONMENT=staging"
   comment.

## Edge Cases

- **Removing the EIP on the LIVE box would strand it** (no public IP → no SSM
  → no Dhan → no internet) because the existing primary ENI (attached
  2026-05-24) has no dynamic public-IP association of its own and
  `map_public_ip_on_launch` only applies at original launch. MITIGATION: the
  IaC change is safe for a FRESH provision; the running box requires an
  operator-side step (re-create the ENI public-IP association at next start, or
  relaunch). Captured as a prominent OPERATOR ACTION note in terraform + PR; PR
  stays DRAFT so the operator decides sequencing. The EIP resource is
  count-gated, so `enable_eip=false` is a reversible one-line flip.
- **Budget filter removal** widens scope to total account; acceptable because
  this account hosts only tickvault. Documented.
- **Hourly hard-stop must NOT stop during market hours** — the out-of-window
  check force-stops ONLY outside Mon-Fri 08:30-16:30 IST; inside the window it
  is a strict no-op.
- **`ip_verification_enabled` is an unconsumed key** (not in `NetworkConfig`);
  setting it false changes no Rust behaviour today but records intent + avoids
  a future wiring landmine. Commented to that effect.
- **`enable_eip` guard test** currently asserts default=true; updated to assert
  default=false + that the subnet keeps `map_public_ip_on_launch=true` and the
  instance carries `associate_public_ip_address=true`.

## Failure Modes

- Native Budget Action fails to provision → killswitch Lambda (kept as backup)
  + email still fire; not a regression.
- Hourly hard-stop Lambda errors → existing Lambda self-error alarm pages.
- Telegram param flip wrong → verified prod params exist before flipping; the
  staging path was actively broken (ParameterNotFound), so this only fixes.
- Deploy self-stop fires during window → guarded by the same IST window check
  the self-start uses; `if: always()` only stops a box THIS run started.

## Test Plan

- `cargo test -p tickvault-storage --test aws_deploy_safety_guard` (updated EIP
  guard + existing schedule/EBS/upgrade guards green).
- `cargo test -p tickvault-common --test aws_infra_wiring --test production_config_wiring`
  (dry_run lock + schedule asserts unaffected).
- `terraform fmt -check` + `terraform validate` (if terraform available).
- `bash .claude/hooks/banned-pattern-scanner.sh` + `plan-verify.sh` + plan-gate.

## Rollback

- Every change is a small, reversible IaC/workflow/config edit. `enable_eip` is
  a one-line flip back to `true`; the EIP resource is count-gated (re-applying
  `enable_eip=true` re-provisions + re-associates an EIP). The budget filter,
  limit, Budget Action, hourly cron, and deploy auto-stop each revert by
  reverting the single commit. No data, no schema, no instance replacement.

## Observability

- Budget Action + killswitch + email = 3 independent breach signals.
- Hourly hard-stop publishes a Telegram message only when it actually stops.
- New hourly "box still running — Xh today, ~$Y MTD" Telegram while running.
- Daily digest Lambda `% used` now matches the new $55 ceiling (synced).
- All stop/start paths are EC2/SSM by instance-id (CloudWatch + SSM history).

## Plan Items

- [x] Item 1 — Budget un-blind + ceiling + native Budget Action
  - Files: deploy/aws/terraform/budget.tf, deploy/aws/terraform/budget-guards.tf
  - Tests: cargo test -p tickvault-common --test aws_infra_wiring
- [x] Item 2 — Corrected EIP approach (auto-assign on + EIP off) + ip_verification=false
  - Files: deploy/aws/terraform/variables.tf, deploy/aws/terraform/main.tf, config/production.toml, crates/storage/tests/aws_deploy_safety_guard.rs
  - Tests: deploy_eip_is_enabled_by_default (renamed/updated)
- [x] Item 3 — Hourly out-of-window hard-stop + deploy auto-stop + hourly running digest
  - Files: deploy/aws/terraform/budget-guards.tf, .github/workflows/deploy-aws.yml
  - Tests: cargo test -p tickvault-storage --test aws_deploy_safety_guard
- [x] Item 4 — Cosmetic: telegram SSM-param prod defaults + stale comment fix
  - Files: deploy/aws/terraform/variables.tf, deploy/aws/terraform/telegram-webhook-lambda.tf
  - Tests: (none — string defaults; verified prod SSM params exist)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Spend crosses $55 ACTUAL | Native Budget Action stops the box; killswitch Lambda + email also fire |
| 2 | Box still running at 18:00 IST (EventBridge 16:30 stop missed) | Hourly hard-stop force-stops it + Telegram |
| 3 | Deploy self-starts box outside window for an after-close run | `if: always()` step stops it again when outside window |
| 4 | Fresh `terraform apply` with enable_eip=false | Instance gets a dynamic public IP via subnet auto-assign + associate_public_ip_address |
| 5 | dry_run flipped to false | production_config_wiring + aws_infra_wiring FAIL the build (unchanged guard) |
