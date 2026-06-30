# Implementation Plan: AWS cost-hardening / never-cross (budget/stop/Telegram only — EIP deferred)

**Status:** VERIFIED
**Date:** 2026-06-30
**Approved by:** Parthiban (operator) — directive given this session 2026-06-30 ("build the AWS cost-hardening / never-cross PR including the corrected EIP approach: enable auto-assign-public-IP + REMOVE the EIP since the EIP is only needed for live orders which are OFF").

> **SCOPE REVISION 2026-06-30 (post adversarial-review CRITICAL):** the
> EIP-off + auto-assign-public-IP + `ip_verification_enabled=false` changes
> (originally Item 2) are **DEFERRED to the future r8g relaunch PR** and have
> been REVERTED out of this PR. Reason: `terraform-apply.yml` runs
> `terraform apply -auto-approve` on infra changes when this branch merges to
> `main`. Flipping `enable_eip=false` on merge would DISASSOCIATE + RELEASE the
> EIP currently attached to the running box `i-0b956d0209231a48b` — whose ENI
> has NO dynamic public-IP association — STRANDING the box (no public IP → no
> SSM → no Dhan → no internet), the exact failure the 2026-05-31 flip
> documented. `associate_public_ip_address` is CREATE-ONLY, so it cannot rescue
> a running box; only a RELAUNCH (the r8g upgrade) safely re-provisions an ENI
> that auto-assigns a public IP. Therefore the EIP removal is a relaunch-time
> change and belongs in the r8g PR, NOT here. **This PR is now budget / stop /
> Telegram hardening ONLY** — every change in it is safe to `terraform apply`
> against the running box. `enable_eip` default stays `true`,
> `ip_verification_enabled` stays `true`, no networking attribute changes.

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
5. **Corrected EIP approach — DEFERRED to the r8g relaunch PR (NOT in this PR).**
   The EIP-off + auto-assign-public-IP + `ip_verification_enabled=false` set is a
   RELAUNCH-time change: `terraform-apply.yml` auto-applies on merge, and
   flipping `enable_eip=false` then would release the in-use EIP and strand the
   running box (its ENI has no dynamic public-IP association;
   `associate_public_ip_address` is create-only and cannot rescue a running
   box). The safe moment to drop the EIP is the r8g relaunch, which provisions a
   fresh ENI that auto-assigns a public IP. So this networking work moves WHOLE
   to the r8g PR. `enable_eip` default stays `true`,
   `ip_verification_enabled` stays `true` here.
6. **Cosmetic correctness.** Flip the telegram-webhook SSM-param defaults
   `/tickvault/staging/telegram/*` → `/tickvault/prod/telegram/*` (verified the
   prod params exist, staging is empty) + fix the stale "TV_ENVIRONMENT=staging"
   comment.

## Edge Cases

- **Removing the EIP on the LIVE box would strand it** (no public IP → no SSM
  → no Dhan → no internet) because the existing primary ENI (attached
  2026-05-24) has no dynamic public-IP association of its own and
  `map_public_ip_on_launch` only applies at original launch, and
  `terraform-apply.yml` auto-applies on merge. **DECISION: defer ALL EIP /
  auto-assign / `ip_verification` changes to the r8g relaunch PR** — the
  relaunch provisions a fresh ENI that auto-assigns a public IP, so the EIP can
  be dropped safely THEN. This PR makes ZERO networking changes; `enable_eip`
  stays `true`, so a merge-triggered `terraform apply` from this branch never
  touches/releases the EIP and never replaces the instance.
- **Budget filter removal** widens scope to total account; acceptable because
  this account hosts only tickvault. Documented.
- **Hourly hard-stop must NOT stop during market hours** — the out-of-window
  check force-stops ONLY outside Mon-Fri 08:30-16:30 IST; inside the window it
  is a strict no-op.
- **`aws_deploy_safety_guard.rs` EIP guard** is UNCHANGED from `origin/main`
  (`deploy_eip_is_enabled_by_default` asserts `default = true`) since this PR no
  longer flips the EIP. The updated false-asserting guard moves to the r8g PR.

## Failure Modes

- Native Budget Action fails to provision → killswitch Lambda (kept as backup)
  + email still fire; not a regression.
- Hourly hard-stop Lambda errors → existing Lambda self-error alarm pages.
- Telegram param flip wrong → verified prod params exist before flipping; the
  staging path was actively broken (ParameterNotFound), so this only fixes.
- Deploy self-stop fires during window → guarded by the same IST window check
  the self-start uses; `if: always()` only stops a box THIS run started.
- **EIP/networking auto-apply strand (ELIMINATED in this PR)** — by deferring
  the `enable_eip=false` flip to the r8g relaunch PR, a merge-triggered
  `terraform apply` from THIS branch makes no networking change, so the running
  box's EIP/internet path is untouched.

## Test Plan

- `cargo test -p tickvault-storage --test aws_deploy_safety_guard` (UNCHANGED
  from origin/main — EIP guard stays `default=true`; schedule/EBS/upgrade guards
  green).
- `cargo test -p tickvault-common --test aws_infra_wiring --test production_config_wiring`
  (dry_run lock + schedule + production.toml asserts unaffected — production.toml
  is now identical to origin/main).
- `terraform fmt -check` + `terraform validate` (if terraform available).
- `bash .claude/hooks/banned-pattern-scanner.sh` + `plan-verify.sh` + plan-gate.

## Rollback

- Every change is a small, reversible IaC/workflow/config edit. The budget
  filter, limit, Budget Action, hourly cron, and deploy auto-stop each revert by
  reverting the single commit. No networking change, no EIP change, no data, no
  schema, no instance replacement — so even a merge-triggered `terraform apply`
  is non-destructive to the running box.

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
  — **DEFERRED to the r8g relaunch PR (REVERTED out of this PR — strand-safe).**
  The EIP-off + auto-assign-public-IP + `ip_verification_enabled=false` changes
  to `main.tf` / `production.toml` / `aws_deploy_safety_guard.rs` were reverted
  to `origin/main`; `variables.tf` `enable_eip` default restored to `true`. Only
  the relaunch (r8g) can safely drop the EIP without stranding the running box.
  - Files (in r8g PR): deploy/aws/terraform/variables.tf, deploy/aws/terraform/main.tf, config/production.toml, crates/storage/tests/aws_deploy_safety_guard.rs
  - Tests (in r8g PR): deploy_eip_is_disabled_by_default
- [x] Item 3 — Hourly out-of-window hard-stop + deploy auto-stop + hourly running digest
  - Files: deploy/aws/terraform/budget-guards.tf, .github/workflows/deploy-aws.yml
  - Tests: cargo test -p tickvault-storage --test aws_deploy_safety_guard
- [x] Item 4 — Cosmetic: telegram SSM-param prod defaults + stale comment fix
  - Files: deploy/aws/terraform/variables.tf (the webhook Lambda reads these
    `telegram_*_ssm_param` defaults; staging path is now empty → 404, prod exists)
  - Tests: (none — string defaults; verified prod SSM params exist)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Spend crosses $55 ACTUAL | Native Budget Action stops the box; killswitch Lambda + email also fire |
| 2 | Box still running at 18:00 IST (EventBridge 16:30 stop missed) | Hourly hard-stop force-stops it + Telegram |
| 3 | Deploy self-starts box outside window for an after-close run | `if: always()` step stops it again when outside window |
| 4 | Merge-triggered `terraform apply` from this branch | NO networking change (enable_eip stays true) → EIP untouched, running box not stranded, instance not replaced |
| 5 | dry_run flipped to false | production_config_wiring + aws_infra_wiring FAIL the build (unchanged guard) |
