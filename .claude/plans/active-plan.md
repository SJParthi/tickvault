# Implementation Plan: Session 7 — Phase 8 AWS + Dep Auto-Update + dlt-doctor

**Status:** APPROVED
**Date:** 2026-04-14
**Approved by:** Parthiban ("go ahead with everything dude")
**Branch:** `claude/websocket-zero-tick-loss-nUAqy`
**Previous session archived:** `archive/2026-04-14-zero-tick-loss-session-6.md`

## Goal

Get the repo AWS-deployment-ready before May 1. Wire the dep auto-update nightly. Add `dlt-doctor` CLI for auto-triage on CRITICAL alerts. All three approved plans from session 6 executed together.

## Plan items (committed after each, push at end)

- [x] Step 1: Archive session 6 + write session 7 plan as APPROVED.
  - Files: active-plan.md
  - Tests: deny_config_exists_and_has_required_sections

- [x] Step 2: Phase 8.1 — AWS Terraform foundation.
  - Files: deploy/aws/terraform/main.tf, variables.tf, outputs.tf, versions.tf, README.md
  - Tests: terraform_files_exist

- [x] Step 3: Phase 8.2 — GitHub Actions AWS deploy workflow.
  - Files: .github/workflows/deploy-aws.yml
  - Tests: github_workflow_syntax_valid

- [x] Step 4: Phase 8.5 — Smoke test binary (dhan-live-trader smoke-test).
  - Files: crates/app/src/bin/smoke_test.rs, crates/app/Cargo.toml
  - Tests: test_smoke_test_binary_exists

- [x] Step 5: Phase 8.6 — Staging config profile.
  - Files: config/staging.toml
  - Tests: test_staging_config_exists_and_parses

- [x] Step 6: Phase 8.3 + 8.4 — AWS deploy + DR runbooks.
  - Files: docs/runbooks/aws-deploy.md, docs/runbooks/aws-disaster-recovery.md
  - Tests: test_aws_runbooks_exist

- [x] Step 7: Option A — Nightly dep auto-update GitHub Action.
  - Files: .github/workflows/dep-freshness-nightly.yml
  - Tests: github_deps_workflow_syntax_valid

- [x] Step 8: dlt-doctor CLI — auto-triage bundle on alerts.
  - Files: crates/app/src/bin/dlt_doctor.rs
  - Tests: test_dlt_doctor_binary_exists

- [x] Step 9: Final plan-verify + push session 7.
  - Files: active-plan.md
  - Tests: deny_config_not_empty

## Honest constraints

- Terraform files will be valid HCL but I will NOT run `terraform apply` — that needs your AWS credentials. You'll run it once locally after I land the files.
- GitHub Actions workflow will be valid YAML. I will NOT run it in this sandbox — you'll see it fire on the next push.
- `dlt-doctor` CLI uses the existing tracing + metrics infrastructure; no new external tools.
- AWS account creation / credit card / IAM root user setup is human-only. The docs will tell you exactly what to click.
