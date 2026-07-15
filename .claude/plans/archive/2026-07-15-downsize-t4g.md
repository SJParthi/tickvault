# Implementation Plan: Prod downsize r8g.large → t4g.medium + QuestDB 1g (guarded workflow)

**Status:** APPROVED
**Date:** 2026-07-15
**Approved by:** Parthiban (operator, 2026-07-15, Quote 8 — "Flip tonight: t4g.medium, QuestDB 1g, automated"; recorded in `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md` §0)

## Plan Items

- [x] Rule-file lock flip FIRST — Quote 8 verbatim + 2026-07-15 approval line + §7 rewrite (t4g.medium spec, INTERIM ~₹1,471/mo bill at 270 hrs with the labelled ~₹1,197 post-recreate and ~₹986/176-hr figures, 4 GiB memory budget with the honest app-RSS FLAG, EBS Rule 3 shrink-impossibility record, Rule 7 workflow tooling)
  - Files: .claude/rules/project/daily-universe-scope-expansion-2026-05-27.md
  - Tests: instance_lock_authoritative_rule_file_pins_t4g_medium, instance_lock_monthly_bill_pinned_to_rupees_1471_interim, instance_lock_schedule_pinned_to_0830_ist_start
- [x] Additive RE-SUPERSEDED banners (historical sections retained verbatim)
  - Files: .claude/rules/project/aws-budget.md, docs/architecture/aws-indices-only-locked-architecture.md
  - Tests: instance_lock_supersession_markers_present_in_aws_budget, instance_lock_supersession_markers_present_in_architecture_doc
- [x] Guard re-pins in lockstep (byte-matched to the edited docs/terraform)
  - Files: crates/storage/tests/instance_type_lock_guard.rs, crates/common/tests/aws_infra_wiring.rs, crates/storage/tests/aws_deploy_safety_guard.rs
  - Tests: instance_lock_authoritative_rule_file_pins_t4g_medium, test_terraform_instance_type_pinned, deploy_ebs_default_is_20gb
- [x] Terraform defaults: instance_type t4g.medium (default + validation), ebs_gp3_size_gb 20 (fresh-provision pre-stage only; live volume untouched via lifecycle.ignore_changes)
  - Files: deploy/aws/terraform/variables.tf
  - Tests: test_terraform_instance_type_pinned, deploy_ebs_default_is_20gb
- [x] Manual-fallback script defaults: FROM r8g.large / TO t4g.medium, allowlist gains t4g.medium + t4g.large (r8g.large kept for the roll-UP), per-target QDB auto-default map (t4g.medium→1g, r8g.large→4g)
  - Files: scripts/aws-upgrade-instance.sh
  - Tests: deploy_upgrade_script_clears_stop_protection_before_stop (unchanged pin — verified green)
- [x] Compose default QDB_MEM_LIMIT 1g + honest .env-precedence note
  - Files: deploy/docker/docker-compose.yml
  - Tests: (no test pins QDB_MEM_LIMIT — verified by grep sweep in the prep dossier)
- [x] The guarded one-shot workflow with review-round-1 fixes folded in (env-mapped inputs, SSM/SNS IAM probes, retune-only continuation mode, verified rollback, always()-power-state restore, honest failure Telegram)
  - Files: .github/workflows/downsize-instance.yml
  - Tests: (workflow YAML — validated by yamllint-equivalent parse + the SSM-quoting rule pattern; not crate-testable)
- [x] Review round 6 hardening: credential_mode input (bootstrap-keys default — the OIDC role has ZERO EC2 mutation grants), probe list matched to the actions actually used (volume-mutation probes removed; DescribeVolumes/Addresses/Snapshots/InstanceStatus + CreateTags added), fail-loud SSM QuestDB-cred fetch (admin/quest fallbacks removed), snapshot AFTER stop with immediate snap_id output + tv-delete-after tag + manual-deletion honesty, MainPID-verified app restart, 240s QuestDB readiness, verified re-stop feeding the success Telegram (failed re-stop fails the run), honest initially-running-left-stopped reporting, rollback stop-wait, status-ok wait loop, post-modify type read-back, SHA-pinned credentials action; honest cost/memory wording sweep (₹986 dual-precondition caveat, ~₹271/mo observability COST NOTES, Rule-2 arithmetic + 4-SID measurement qualifier + ~770-SID formula flag, banner clause (e) supersession bracket, dated r8g drift notes in main.tf/budget.tf/user-data/app-alarms/instance-upgrade runbook/PROC-01 triage)
  - Files: .github/workflows/downsize-instance.yml, scripts/validate-deploy-ssm-quoting.sh, .claude/rules/project/daily-universe-scope-expansion-2026-05-27.md, .claude/rules/project/aws-budget.md, .claude/rules/project/wave-4-error-codes.md, docs/architecture/aws-indices-only-locked-architecture.md, docs/runbooks/instance-upgrade.md, deploy/aws/terraform/main.tf, deploy/aws/terraform/budget.tf, deploy/aws/terraform/user-data.sh.tftpl, deploy/aws/terraform/app-alarms.tf, crates/storage/tests/instance_type_lock_guard.rs, crates/storage/tests/aws_deploy_safety_guard.rs
  - Tests: instance_lock_monthly_bill_pinned_to_rupees_1471_interim, deploy_questdb_mem_limit_pins_1g_for_t4g_medium

## Design

One-shot guarded `workflow_dispatch` workflow (`.github/workflows/downsize-instance.yml`)
executes the flip: confirm-input gate (env-mapped, never template-interpolated into
shell — script-injection class) → AWS creds (deploy-aws.yml parity) → IAM dry-run
probe of EVERY EC2 mutation + real read-probes of ssm:DescribeInstanceInformation +
sns:Publish (preflight ping) → post-market IST guard → bounded wait for in-flight
deploy-aws runs → record state + pick mode (`full` when r8g.large; `retune_only`
when already t4g.medium — a prior partial run continues instead of dead-ending) →
stop (benign/reversible) → snapshot AFTER the stop (clean image; NO auto-delete —
manual cleanup after the rollback week, tagged tv-delete-after; round 6, was
snapshot-first) → modify (post-modify type read-back) → start + status-ok + EIP
identity + SSM-online → SSM retune (idempotent .env sed-upsert to QDB_MEM_LIMIT=1g
on BOTH .env paths + questdb recreate + app restart with a 300s active bound) →
log-ingestion smoke → power-state restore (`if: always()`) → honest Telegram. The
repo lock set (rule file, banners, guards, terraform, script, compose) flips in the
SAME PR so the cross-pinning ratchets stay green atomically. EBS 50→20 is
IMPOSSIBLE in place (gp3 never shrinks; a 50 GB snapshot cannot restore into
20 GB) — tonight keeps the live 50 GB root; terraform default 20 only pre-stages
the operator-window terminate-and-recreate (executor decision, not Quote 8 scope).

## Edge Cases

- Box initially STOPPED (normal post-16:30 IST) vs RUNNING — both tolerated; the
  stop step re-reads LIVE state (a queued deploy dispatch can self-start the box
  mid-run); step 9b restores the initial power state on success AND failure, reads
  back the final LIVE state for the success message, fails the run on a failed
  re-stop, and honestly reports an initially-running box a failure left stopped
  (never auto-restarts post-failure).
- Box already t4g.medium (prior partial run) — retune-only continuation mode, never
  a refusal; the .env upsert is idempotent.
- Queued deploy-aws dispatches (29409568521 / 29411020997) + 16:16/17:01 IST crons —
  step 1b waits out in-flight runs bounded ~45 min, then proceeds loudly.
- Mid-transition instance state (stopping/pending) — refused until settled.
- EIP drift before OR after the flip — hard abort + Critical Telegram (Dhan
  static-IP whitelist).
- Hostile dispatch input — all three inputs flow through step `env:` blocks; a
  crafted input can no longer inject into the runner shell after creds load.
- Legacy on-box `.env` at `/opt/tickvault/deploy/docker/.env` — upserted too.

## Failure Modes

- Missing IAM action — discovered pre-mutation by step 0 (EC2 dry-runs + SSM read
  canary + SNS publish probe); named in the error; nothing changed.
- Snapshot never completes (~60 min bound) — abort BEFORE the type change; the box
  is merely stopped (benign/reversible — step 9b reports/restores power state).
- t4g.medium start failure (capacity) — auto-rollback to r8g.large, then VERIFIED
  (live type + power re-read); Telegram says "rolled back (verified)" or "ROLLBACK
  NOT CONFIRMED + actual state + manual commands" — never an unverified claim.
- Post-flip failures (status-ok timeout / EIP mismatch / SSM offline / retune or
  app-restart failure) — box is t4g.medium possibly WITHOUT the retune (over-commit
  risk with the on-box 4g .env): failure Telegram reports the ACTUAL live type +
  power state and directs re-dispatch (retune-only mode completes the job); step 9b
  re-stops an initially-stopped box even on failure (a stopped box cannot OOM).
- Playbook for every arm is in the workflow header (POST-FLIP FAILURE PLAYBOOK).
- 4 GiB fit risk — honest FLAG in §7 Rule 2: read tv_process_rss_bytes /
  mem_used_percent before + after cutover; t4g.large is the rip-cord; watch
  CPUCreditBalance (burstable).

## Test Plan

- `cargo test -p tickvault-storage -p tickvault-common` — the re-pinned ratchets
  (instance_type_lock_guard, aws_infra_wiring::test_terraform_instance_type_pinned,
  aws_deploy_safety_guard::deploy_ebs_default_is_20gb) must be green against the
  edited docs/terraform in the same diff.
- `cargo fmt --all` clean; pre-commit hook battery (banned patterns, secrets,
  commit-msg format) on commit.
- Workflow: dispatch is operator-gated (exact instance-id confirm); the step-0
  probe run is the live dry-run test; SSM commands block follows the
  validate-deploy-ssm-quoting.sh rule (no interior apostrophes — verified).

## Rollback

- Pre-flip: nothing changed; re-dispatch after fixing the named cause.
- Start-failure: automatic rollback to r8g.large with post-rollback verification.
- Post-flip regret / 4 GiB does not fit: `scripts/aws-upgrade-instance.sh
  --from t4g.medium --to r8g.large --qdb-mem 4g` (allowlist keeps r8g.large; the
  r8g target auto-defaults QDB 4g) — the roll-UP direction.
- Data: pre-downsize snapshot (tagged tv-prod-pre-downsize-rollback, kept ~1 week)
  restores the old root if the erase-window recreate goes wrong.
- Repo: revert this PR (single commit) restores every pin to r8g.large/50 GB/4g.

## Observability

- One Telegram per major step via the existing SNS→Lambda chain: preflight (doubles
  as the sns:Publish probe), snapshot-complete, success (names snapshot id, disk
  honesty line, final power state), failure (ACTUAL live type + power state + next
  action — never a false rollback claim), rollback-verified / rollback-not-confirmed.
- Run log: IAM probe verdict per action, mode decision, SSM StandardOutput/Error
  fetched verbatim, MEMLIMIT docker-inspect line proving the 1g landed, log-ingestion
  smoke against /tickvault/prod/app.
- Post-cutover watch items recorded in §7 Rule 2: tv_process_rss_bytes /
  mem_used_percent / CPUCreditBalance; existing CloudWatch alarms unchanged.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dispatch with wrong confirm input | Refused before creds; nothing runs |
| 2 | Box stopped at start, clean run | flip + retune, box re-stopped post-verify |
| 3 | Box running at start, clean run | flip + retune, box left running (16:30 cron owns it) |
| 4 | t4g capacity failure | verified rollback to r8g.large + honest Telegram |
| 5 | Retune SSM failure post-flip | failure Telegram names t4g.medium + power state; re-dispatch continues retune-only |
| 6 | Re-dispatch on already-t4g box | retune-only mode completes steps 7-10 |

## Per-Item Guarantee Matrix

Per `.claude/rules/project/per-wave-guarantee-matrix.md` — this plan cross-references
the canonical 15-row "100% everything" matrix and the 7-row Resilience matrix in
`operator-charter-forever.md` §C for every item above (docs/infra/workflow scope: the
code-coverage/DHAT/hot-path rows are N/A — no `crates/*/src` production code is
touched, only ratchet TESTS + docs + terraform + script + compose + workflow; the
extreme-check row is satisfied by the re-pinned build-failing ratchets themselves;
the resilience rows are unaffected — no tick path, no WS path, no QuestDB schema
change; QuestDB capacity change is config-only and rollback-able per the Rollback
section). Honest 100% claim: 100% inside the tested envelope, with ratcheted
regression coverage — the instance/EBS/bill/schedule pins are build-failing tests
(`instance_type_lock_guard.rs`, `aws_infra_wiring.rs`, `aws_deploy_safety_guard.rs`);
beyond the envelope, the pre-downsize snapshot + verified-rollback arm catch the
irreversible cases. NOT claimed: that the app fits 4 GiB (FLAGGED, measured live),
that t4g CPU credits suffice for the ~770-SID workload (watched live), or that the
20 GB volume exists tonight (it does not — live root stays 50 GB).
