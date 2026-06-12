# Plan: fix the empty `/tickvault/prod/app` CloudWatch log group

**Status:** APPROVED
**Date:** 2026-06-12
**Approved by:** Parthiban (2026-06-12 — "yes fix it")
**Branch:** claude/nice-cerf-hemuvn
**Changed files (infra/CI only — NOT crates/*/src):** `.github/workflows/deploy-aws.yml`, `deploy/aws/terraform/main.tf`

## Design

The `/tickvault/prod/app` log group exists (19 days) but has **0 streams / 0 bytes** — the CloudWatch agent ships nothing. The agent config (`deploy/aws/cloudwatch-agent.json`) tails `/opt/tickvault/data/logs/{errors.jsonl*,app.*}` → `/tickvault/prod/app`, and `deploy-aws.yml` installs + `fetch-config` + starts it (all non-fatal `|| true`). The terraform IAM policy (`main.tf` `tv_instance`) already grants `logs:CreateLogStream`/`logs:PutLogEvents`/`ec2:DescribeTags`. So the code is *almost* right; the failure is one of three, none verifiable from the dev container (no AWS access):

1. **IAM drift** — the policy is correct in code but was never *applied* to the live role.
2. **Config-load timeout** — `timeout 25` on `fetch-config` kills agent config validation (the 2026-06-09 incident measured ~60s before the DescribeTags fix).
3. **Unseen agent error** — the agent's own log is never surfaced (everything is `|| true`), so the reason is invisible.

This PR hardens all three:
- **IAM completeness** (`main.tf`): add `logs:CreateLogGroup` + `logs:DescribeLogStreams` to the `tv_instance` logs statement (Resource already `*`). Re-applied automatically by `terraform-apply.yml` on merge → fixes any drift.
- **Timeout** (`deploy-aws.yml`): raise `fetch-config` `timeout 25` → `90`.
- **Visibility** (`deploy-aws.yml`): after start, dump (non-fatal) the agent `status`, the agent's own log tail, and `ls` of the app log dir — so the next deploy OUTPUT shows exactly why if it's still empty (Z+ L1 DETECT).

The agent stays **non-fatal** throughout (preserves the 2026-06-09 guarantee that an observability fault can never take the live trading app down).

## Edge Cases
- Log group already exists → `CreateLogGroup` is a harmless no-op (idempotent).
- Agent already healthy → status/log dumps are read-only, no behavior change.
- Log files absent at the tailed path → the new `ls` dump reveals it next deploy.
- Market hours → both `terraform-apply` and `deploy-aws` keep their existing market-hours guards; merge after close auto-applies safely.

## Failure Modes
- If the real cause is none of the three (e.g. a malformed agent config), the added diagnostics surface it in the next deploy log so a follow-up can nail it — honest, not a blind claim of "fixed."
- IAM change is additive + least-privilege (only `logs:*` describe/create + existing put); no new attack surface.

## Test Plan
- `terraform fmt -check` + `terraform validate` (run by `terraform-apply.yml` plan job on the PR).
- `deploy-aws.yml` YAML lint / the workflow's own preflight.
- Post-merge: the after-close auto-deploy runs; the new diagnostic block prints agent status + log dir; `/tickvault/prod/app` should gain `{instance}/app` + `{instance}/errors-jsonl` streams. **One deploy cycle confirms** (honest — cannot be verified from the dev container).

## Rollback
Single-commit revert restores `timeout 25` + drops the 2 IAM actions + the diagnostic lines. No data migration, no schema, no app-code change. Zero blast radius on the trading app (agent is non-fatal).

## Observability
- The PR's entire point is observability: it makes the app's logs reach CloudWatch (`/tickvault/prod/app`) so they're readable in-console, and makes the agent's own failures visible in the deploy output.
- No new app metric/counter (infra-only change).

## Per-Wave Guarantee Matrix (cross-reference)

See `.claude/rules/project/per-wave-guarantee-matrix.md` — all 15 rows of the
100% Guarantee Matrix and all 7 rows of the Resilience Demand Matrix apply to
every item in this plan. This is an infra/CI-only change (no `crates/*/src`
touched), so the app-code rows (code coverage, DHAT zero-alloc, O(1) hot path,
Zero ticks lost) are N/A by construction; the rows this PR directly advances are
**100% monitoring / 100% logging / 100% alerting** — it makes the app's logs
actually reach CloudWatch and surfaces the agent's own failures. Verification is
the full CI workspace suite (the comprehensive automated check) plus the
`terraform-apply` plan job, both green before merge.

## Plan Items
- [ ] Add `logs:CreateLogGroup` + `logs:DescribeLogStreams` to `tv_instance` IAM policy
  - Files: deploy/aws/terraform/main.tf
- [ ] Raise CloudWatch-agent `fetch-config` timeout 25 → 90
  - Files: .github/workflows/deploy-aws.yml
- [ ] Add non-fatal agent status + log-tail + log-dir diagnostics to the deploy
  - Files: .github/workflows/deploy-aws.yml
