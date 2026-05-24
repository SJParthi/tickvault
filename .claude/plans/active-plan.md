# Implementation Plan: PR #772 — auto-trigger deploy-aws on push to main

**Status:** VERIFIED
**Date:** 2026-05-24
**Approved by:** Parthiban (5-PR roadmap confirmed via AskUserQuestion 2026-05-24)
**Branch:** `claude/confident-brahmagupta-aqN8R`
**Position in roadmap:** PR #772 of 5 (after #770 + #771 merged)

## Honest re-scope (from original "blue/green ~200ms downtime")

Original plan promised blue/green with ~200ms downtime. After investigation:

| Was promised | Reality |
|---|---|
| Blue/green ~200ms downtime | Single-instance ~3-5s downtime today (systemctl restart). True 0ms requires socket activation in Rust app code OR dual-instance ₹2,044/mo (out of charter budget). |
| Downtime kills ticks | Existing `guard_market_hours` step blocks deploys 09:15-15:30 IST → **zero ticks lost during trading** by construction. |
| `git push` deploys | Today `deploy-aws.yml` triggers ONLY on tag `v*.*.*` or manual workflow_dispatch — operator must manually click "Run workflow" |

**Reframed #772 scope (tight + honest):** add the missing `push: branches: [main]` trigger to `deploy-aws.yml` so any merge to main with Rust changes auto-deploys outside market hours. Apply the same preflight gate as #771 so the workflow skips cleanly when AWS bootstrap secrets are absent.

True 0ms downtime via socket activation = **deferred to #772b** as a follow-up if operator wants it.

## Plan Items

- [x] **Item 1 — Add push-to-main trigger to deploy-aws.yml**
  - Files: `.github/workflows/deploy-aws.yml`
  - Behavior: on push to `main` with paths matching `crates/**` OR `Cargo.toml` OR `Cargo.lock` OR `deploy/systemd/**` → workflow auto-fires
  - Existing tag trigger preserved, existing workflow_dispatch preserved
  - Validation: push of unrelated change (e.g. docs only) does NOT trigger; push of Rust change DOES trigger
  - Tests: r11_deploy_aws_has_push_to_main_trigger_with_rust_paths

- [x] **Item 2 — Add preflight job to deploy-aws.yml (mirror #771 pattern)**
  - Files: `.github/workflows/deploy-aws.yml`
  - Behavior: new `preflight` job runs first; checks if `AWS_ROLE_ARN` OR `AWS_ACCESS_KEY_ID` secret is set. If neither, prints friendly notice + downstream jobs skip. If yes, downstream proceeds.
  - Why: same chicken-and-egg as #771 — without bootstrap, every contributor PR would fail the deploy check
  - Validation: workflow runs end-to-end without secrets → preflight = success, build/guard/deploy = skipped
  - Tests: r12_deploy_aws_has_preflight_gate

- [x] **Item 3 — Extend github_workflow_guard.rs ratchet to cover deploy-aws.yml**
  - Files: `crates/common/tests/github_workflow_guard.rs`
  - Behavior: add 2 new tests covering deploy-aws.yml — auto-trigger present, preflight gate present
  - Validation: `cargo test -p tickvault-common --test github_workflow_guard` — all 13 pass
  - Tests: r11_deploy_aws_has_push_to_main_trigger_with_rust_paths, r12_deploy_aws_has_preflight_gate

- [x] **Item 4 — Update deploy-aws.yml header comment to reflect new triggers**
  - Files: `.github/workflows/deploy-aws.yml`
  - Behavior: update the docstring at the top of the file to mention auto-trigger on `push to main with Rust paths`
  - Validation: header comment lists all 3 triggers (push, tag, dispatch)
  - Tests: r13_deploy_aws_header_documents_push_trigger

## Scenarios covered

| # | Scenario | Expected behavior |
|---|----------|-------------------|
| 1 | Operator pushes Rust change to main outside market hours | deploy-aws.yml auto-fires → build → guard (off-hours) → deploy → Telegram |
| 2 | Operator pushes Rust change to main during market hours (10:00 IST) | deploy-aws.yml auto-fires → build → guard BLOCKS → workflow fails with clear message; no deploy |
| 3 | Operator pushes docs-only change | deploy-aws.yml does NOT trigger (path filter excludes docs) |
| 4 | Operator pushes Rust change BEFORE bootstrap | preflight detects no secrets → prints notice → downstream jobs skip cleanly; PR check stays green |
| 5 | Tag `v1.2.3` pushed | Existing tag trigger fires (unchanged behavior) |
| 6 | Operator clicks workflow_dispatch with confirm_market_hours=yes during market hours | Existing override path still works (unchanged) |

## Per-item Z+ Defense Matrix

| Layer | Item 1 (trigger) | Item 2 (preflight) | Item 3 (ratchet) | Item 4 (docs) |
|---|---|---|---|---|
| L1 DETECT | workflow log on push | preflight log | cargo test | grep header |
| L2 VERIFY | path filter matches | secret presence check | source-scan asserts | source-scan asserts |
| L3 RECONCILE | per-push | per-run | per-PR | per-PR |
| L4 PREVENT | path filter limits scope | gate downstream jobs | banned regression | docs sync |
| L5 AUDIT | GH Actions log | CloudTrail | ratchet test | git history |
| L6 RECOVER | manual workflow_dispatch | provide secrets later | revert PR | revert PR |
| L7 COOLDOWN | concurrency.group | n/a | n/a | n/a |

## Z+ 15-row "100% Everything" Matrix

| Demand | Mechanical proof for THIS PR |
|---|---|
| Code coverage | github_workflow_guard.rs extended to 13 tests |
| Audit coverage | GH Actions log preserves every deploy invocation |
| Testing coverage | Rust ratchet test + YAML lint + manual scenario walk |
| Code checks | Pre-commit gate (fmt, secret-scan, conv-commit, S6 invariants) |
| Code performance | N/A — workflow YAML, not hot path |
| Monitoring | GH Actions + SNS notify (already wired) |
| Logging | GH Actions stdout, preserves workflow run history |
| Alerting | SNS publish to tv-prod-alerts on success/failure (already wired) |
| Security | OIDC preferred via existing AWS_ROLE_ARN; no new secrets introduced |
| Security hardening | Path filter limits trigger scope (no accidental deploy on doc changes) |
| Bug fixing | Closes the missing-auto-trigger gap — operator no longer types "Run workflow" |
| Scenarios covering | 6 scenarios above |
| Functionalities covering | Every main-merge with Rust changes auto-deploys outside market hours |
| Code review | Self-review per plan + Z+ matrix |
| Extreme check | github_workflow_guard.rs ratchets the new contract |

## Z+ 7-row "Resilience" Matrix

| Demand | Honest envelope |
|---|---|
| Zero ticks lost | Market-hours guard (existing) prevents deploy during 09:15-15:30 IST → zero ticks lost in trading window |
| WS never disconnects | Off-hours deploy = WS already in sleep-until-open dormant state per Wave-2-A — no live disconnects |
| Never slow/locked/hanged | Workflow 30 min timeout; existing rollback path on failed smoke test |
| QuestDB never fails | N/A — workflow only deploys the Rust binary |
| O(1) latency | N/A — provisioning is one-shot |
| Uniqueness + dedup | `concurrency.group: deploy-aws-prod` prevents simultaneous deploys (already wired) |
| Real-time proof | Workflow logs visible in GH Actions UI; SNS Telegram message confirms deploy outcomes |

## Honest 100% claim (envelope-qualified per operator-charter-forever.md §F)

> 100% inside the tested envelope: after this PR merges + the one-time AWS bootstrap (from #771's BOOTSTRAP-ONE-TIME.md), every push to main that touches `crates/**`, `Cargo.toml`, `Cargo.lock`, or `deploy/systemd/**` automatically triggers binary build + market-hours guard + SSM deploy + 60s post-deploy monitor + Telegram notification. Market-hours guard blocks the deploy during 09:15-15:30 IST Mon-Fri unless workflow_dispatch with `confirm_market_hours=yes`. Effective tick loss during deploys: **zero** (market-hours guard + off-hours dormant WS pattern). Deploy downtime when it does happen (off-hours): ~3-5s during `systemctl restart`. True 0ms downtime requires socket activation in the Rust app (deferred to #772b follow-up) OR dual-instance (₹+1,022/mo, breaches charter budget).

## Post-merge effective behavior

| Trigger | What happens |
|---|---|
| Operator merges Rust change to main, 10pm IST | Auto-deploy fires within ~5 min → live binary updated → Telegram message arrives |
| Operator merges Rust change to main, 11am IST | Auto-deploy fires → guard blocks (market hours) → workflow fails → Telegram alert → operator sees they tried to deploy in market hours |
| Operator merges docs change to main | No deploy trigger; only CI runs |
| Operator merges Terraform change to main | terraform-apply.yml fires (from #771); deploy-aws.yml does NOT fire |
| Operator tags v1.2.3 | Existing tag trigger fires (unchanged) |
