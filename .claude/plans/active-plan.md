# Implementation Plan: Collapse dev/staging into a single `prod` environment (dry_run=true LOCKED)

**Status:** VERIFIED
**Date:** 2026-06-30
**Approved by:** Parthiban (operator) — directive given this session 2026-06-30 ("collapse dev/staging/prod into a single prod env across the entire workspace; preserve dry_run=true / NO real orders")

> Guarantee matrices: this item carries the 15-row + 7-row matrices by
> cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`
> (mandatory per `per-item-guarantee-check.sh`). The dominant guarantee for
> THIS change is the order-safety invariant: `dry_run=true` is preserved in
> every active config; no path reaches `api_client.place_order`.

## Design

The workspace currently models three environments — `dev`, `staging`, `prod`.
`staging` was the 3-month data-pull env (sandbox/dry_run, no real orders);
`dev` was local. The operator directive collapses these into a SINGLE real env,
`prod`, used everywhere — config, systemd, deploy workflows, terraform IAM,
helper-script defaults, and BOTH feeds (Dhan + Groww). `prod` becomes the only
real environment while PRESERVING the order-safety lock.

Hard safety constraint (non-negotiable): `dry_run=true` (NO real orders) MUST be
preserved. This is still the data-pull phase. `config/production.toml` currently
sets `dry_run = false` — it is changed to `dry_run = true` with a LOUD comment,
plus a far-future `sandbox_only_until` belt-and-suspenders (mirroring the deleted
staging.toml) so even a future misedit flipping `mode=live` is mechanically
blocked at boot by `StrategyConfig::check_sandbox_window`.

Semantic (not blind find-replace): only the env-SELECTION strings/defaults change.
`dev`/`staging` config branches are retired; everything repoints to `prod`.
`config/base.toml` + `config/production.toml` become the only env configs.

Changed crates: **tickvault-common** (the build-blocking `aws_infra_wiring.rs`
test must be rewritten to assert `prod` + the new `dry_run=true` invariant, plus
the `DEFAULT_SSM_ENVIRONMENT` constant) and **tickvault-core**/**tickvault-app**
(env default `dev`→`prod` in `secret_manager::resolve_environment` +
`boot_helpers::resolve_config_env`). Non-crate files: systemd unit, terraform
`main.tf` IAM grant, deploy/autopilot workflows, helper scripts, config TOMLs.

## Plan Items

- [x] `config/production.toml`: `dry_run = true` (LOUD locked comment); `[feeds] dhan_enabled=true` + `groww_enabled=true`; far-future `sandbox_only_until`; keep `ip_verification_enabled=true`, `sns_enabled`.
  - Files: config/production.toml
  - Tests: aws_infra_wiring::test_production_toml_locks_dry_run_true_no_real_orders (new)
- [x] Delete `config/staging.toml`; keep `config/local.toml` (Mac host-dev, still referenced by `CONFIG_LOCAL_PATH`).
  - Files: config/staging.toml (deleted)
- [x] Default env → `prod`: `resolve_config_env` (boot_helpers), `resolve_environment` (secret_manager), `DEFAULT_SSM_ENVIRONMENT` const (common/constants); helper-script defaults `staging`→`prod` (aws-autopilot.sh, ensure-questdb.sh, aws-autopilot.yml).
  - Files: crates/app/src/boot_helpers.rs, crates/core/src/auth/secret_manager.rs, crates/common/src/constants.rs, scripts/aws-autopilot.sh, scripts/ensure-questdb.sh, .github/workflows/aws-autopilot.yml
  - Tests: boot_helpers config_env_path tests; secret_manager resolve/validate tests (updated)
- [x] `deploy/systemd/tickvault.service`: `TV_ENVIRONMENT=staging`→`prod` + comment block rewrite.
  - Files: deploy/systemd/tickvault.service
- [x] `deploy/aws/terraform/main.tf`: drop the redundant `/tickvault/staging/*` IAM grant (already granted via `var.environment`=prod), rewrite comment.
  - Files: deploy/aws/terraform/main.tf
- [x] `.github/workflows/deploy-aws.yml`: retire the auto-seed-to-staging step (operator populates `/tickvault/prod/*` manually); repoint the config-diff + QuestDB SSM reads to prod; rewrite comments.
  - Files: .github/workflows/deploy-aws.yml
- [x] Delete the now-dead `.github/workflows/seed-staging-ssm.yml` auto-seed workflow; drop its stale reference in aws-autopilot.sh.
  - Files: .github/workflows/seed-staging-ssm.yml (deleted), scripts/aws-autopilot.sh
- [x] Rewrite `crates/common/tests/aws_infra_wiring.rs` env-string tests to assert `prod` + the new invariants; drop the two seed-staging-ssm tests.
  - Files: crates/common/tests/aws_infra_wiring.rs
  - Tests: test_terraform_instance_iam_allows_prod_ssm_prefix, test_production_toml_locks_dry_run_true_no_real_orders, test_deploy_aws_workflow_refreshes_repo_and_systemd_unit

## Edge Cases

- A box that already exported `TV_ENVIRONMENT=staging` (stale systemd unit) — the deploy refreshes the systemd unit from the repo, so after this lands the box runs `TV_ENVIRONMENT=prod` and reads `/tickvault/prod/*` (which the operator populates manually + the IAM `var.environment` grant already covers).
- `config_env_path("prod")` already maps to `production.toml` (pre-existing alias) — no new path logic needed; only the DEFAULT when the env var is unset changes.
- Path-traversal hostile env var: the strict `[a-z0-9-]` allowlist in `config_env_path` + `validate_environment` is unchanged, so the safety property is preserved.
- `local.toml`: still selected by `config_env_path` returning `None` for `local`/`dev` and merged separately via `CONFIG_LOCAL_PATH`; left intact for Mac host-dev.

## Failure Modes

- **Real orders accidentally enabled** — the whole point of the lock. Mitigated by: `production.toml dry_run=true`, `base.toml dry_run=true`, `default_dry_run()=true`, far-future `sandbox_only_until`, and the engine.rs dry_run gate that returns before any HTTP. Proven by grep + order-path trace in the PR body.
- **Box IAM-denied reading its secrets** — `var.environment`=prod already grants `/tickvault/prod/*`; removing the redundant staging grant cannot deny prod.
- **Deploy FATAL on missing `config/staging.toml`** — the deploy config-diff is repointed to `production.toml`, so deleting staging.toml does not break the deploy verification step.
- **Build break from the build-blocking `aws_infra_wiring.rs`** — rewritten in the same change so the workspace builds green.

## Test Plan

- `grep -rn 'dry_run' config/` → production.toml = true, no active `false`.
- `cargo build --workspace` → 0 errors.
- `cargo test -p tickvault-common -p tickvault-app -p tickvault-core -p tickvault-trading` (env/secret/oms/aws_infra_wiring/dry_run paths) → green; paste result lines.
- `cargo fmt --check`, banned-pattern-scanner, plan-verify, plan-gate.
- Adversarial order-safety trace: with `dry_run=true`, no config/code path under `prod` reaches `api_client.place_order`.

## Rollback

Single squash-merge PR. Revert the merge commit to restore the three-env model
(`config/staging.toml`, the seed-staging-ssm workflow, the `dev`/`staging`
defaults, and the original `production.toml dry_run=false`). No data migration,
no schema change, no stored-state dependency — pure config/deploy/test edits.

## Observability

No new runtime telemetry is required — this is a config/deploy consolidation.
Existing signals remain: `resolve_environment()` warns once if `TV_ENVIRONMENT`
is unset (now defaulting to `prod`); the OMS engine logs `dry_run=...` at daily
reset; deploy workflow steps print the deployed config diff. The order-safety
invariant is enforced mechanically by the rewritten `aws_infra_wiring.rs` test
(`test_production_toml_locks_dry_run_true_no_real_orders`) which fails the build
if `production.toml` ever sets `dry_run = false` again.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | `TV_ENVIRONMENT` unset at boot | defaults to `prod`, reads `/tickvault/prod/*`, loads `production.toml` |
| 2 | Stale box with `TV_ENVIRONMENT=staging` unit | deploy refreshes unit → `prod` |
| 3 | Future misedit flips `mode=live` in production.toml | boot panics via `check_sandbox_window` (far-future `sandbox_only_until`) |
| 4 | Operator has not yet populated `/tickvault/prod/*` | boot halts with SecretRetrieval (honest, no silent fallback to dev) — operator is populating manually |
