# Implementation Plan: Flip instance lock m8g.large → r8g.large (16 GiB) per operator 2026-06-30

**Status:** APPROVED
**Date:** 2026-06-30
**Approved by:** Parthiban (operator) — dated directive this session 2026-06-30: "just upgrade the instance to r8 large + everything related to the infra"

> Guarantee matrices: this item carries the 15-row + 7-row matrices by
> cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`
> (mandatory per `per-item-guarantee-check.sh`). The dominant guarantee for
> THIS change is the §7 Mechanical Rule 1 four-file instance lock: the new
> r8g.large/16-GiB type is pinned CONSISTENTLY across the rule file, the
> superseded budget doc, the architecture doc, the ratchet test, Terraform
> validation, the upgrade script, and the Docker compose mem default. No
> path flips `dry_run` (stays `true`) and no path changes the universe cap
> (`MAX_DAILY_UNIVERSE_SIZE`) or `SEAL_BUFFER_CAPACITY` — the 2K universe
> expansion is a SEPARATE later PR gated on a memory measurement.

## Design

The §7 instance lock in `daily-universe-scope-expansion-2026-05-27.md`
currently pins **m8g.large** (Graviton4, 2 vCPU / 8 GiB) per operator Quote 5
(2026-05-29). The operator authorized a hardware upgrade on 2026-06-30 to
**r8g.large** (Graviton4, 2 vCPU / **16 GiB**, memory-optimized r-family),
doubling RAM headroom for the upcoming both-feeds + larger-universe workload.

§7 Mechanical Rule 1 requires that ANY instance-type change be applied across
FOUR authority files + the ratchet test + Terraform + the upgrade script. This
PR does exactly that hardware lock-flip and nothing else:

- `m8g.large` → `r8g.large` everywhere the type is the LOCKED value.
- `8 GiB` → `16 GiB` everywhere the RAM is the locked value.
- §7 Rule 2 memory budget recomputed for a 16-GiB host at the CURRENT
  universe (~250–1000 SIDs across 21 TFs), NOT the deferred 2K expansion.
- §7 cost line recomputed: r8g.large = $0.08258/hr (verified ap-south-1),
  weekday 08:30–16:30 IST schedule (270-hr/mo ceiling), + EIP (kept), + 18%
  GST → ~₹2,919/mo (inside the operator's ~₹2,500–3,200 envelope).
- Docker QuestDB `QDB_MEM_LIMIT` default raised 2g → 4g (the §7 "both feeds
  ~2K → --qdb-mem 4g" note; 4g fits comfortably in 16 GiB with ~10+ GiB free).

KEEP (explicitly UNCHANGED by this PR): the Elastic IP (`enable_eip = true`,
a separately-verified networking decision), `dry_run = true`, the
`MAX_DAILY_UNIVERSE_SIZE` / `SEAL_BUFFER_CAPACITY` constants, the 2-WebSocket
lock, and the 08:30–16:30 IST weekday schedule.

## Edge Cases

- **Ratchet test text-coupling:** `instance_type_lock_guard.rs` asserts the
  EXACT bold-pinned string (`**r8g.large**`), the RAM string (`16 GiB RAM`),
  and the recomputed ₹ bill. All three rule-file edits must match the test's
  new expectations verbatim or the build fails (intended — the test IS the
  lock).
- **Superseded-marker preservation:** the test also asserts the 4 superseded
  files still carry their `SUPERSEDED 2026-05-27` markers + the link to the
  authoritative rule file. Those markers are NOT removed — only the instance
  string inside them (where present) is updated.
- **Historical mentions:** `m8g.large` / `8 GiB` may remain ONLY as historical
  / superseded context (e.g. "supersedes the 2026-05-29 m8g.large lock"), never
  as the current LOCKED value. A grep guard confirms this.
- **Upgrade-script default flip:** `FROM_TYPE` moves `t4g.medium` → `m8g.large`
  so the now-canonical flip command is `--from m8g.large --to r8g.large`.
  `r8g.large` is ALREADY in the `--to` allowlist (no allowlist change needed).
- **Terraform default:** `instance_type` default + validation condition both
  move to `r8g.large` so a fresh `terraform apply` provisions the new type.

## Failure Modes

- **Inconsistent lock (one file misses the flip):** the grep guard + the
  ratchet test catch any file still pinning m8g.large as the live value.
- **Cost/RAM miscompute pinned wrongly:** the `instance_lock_monthly_bill_pinned_to_rupees_*`
  ratchet pins the recomputed bill; a typo'd figure fails the test.
- **Accidental scope creep:** if a diff hunk touched `dry_run`,
  `MAX_DAILY_UNIVERSE_SIZE`, or `SEAL_BUFFER_CAPACITY`, self-review + grep
  would flag it. This PR touches none of them.

## Test Plan

- `cargo test -p tickvault-storage --test instance_type_lock_guard` — the
  rewritten ratchet (all tests) must pass against the new rule-file text.
- `grep` to confirm no stray `m8g.large` / `8 GiB` remains as the LOCKED value
  (only historical/superseded mentions allowed).
- `.claude/hooks/banned-pattern-scanner.sh` clean.
- `.claude/hooks/plan-verify.sh` + design-first `plan-gate.sh` (this plan,
  6 sections, APPROVED, references `tickvault-storage`).
- `terraform fmt -check` on the variables.tf edit (if terraform available).

## Rollback

Single revert of the PR commit restores the m8g.large lock everywhere
atomically (rule files + ratchet + Terraform + script + compose move together
in one commit). No data migration, no runtime state — this is a documentation
+ infra-config + ratchet-text change only. The live instance is NOT touched by
this PR (the actual `aws ec2 modify-instance-attribute` flip is a separate
operator-run step via `scripts/aws-upgrade-instance.sh`).

## Observability

No new runtime code, no new ErrorCode, no new counter. The lock-flip is a
documentation + config + ratchet change. The existing `instance_type_lock_guard.rs`
ratchet is the observability layer — it FAILS THE BUILD on any future drift
away from the r8g.large/16-GiB lock. The recomputed cost line documents the new
CloudWatch budget-alarm ceiling for the operator.

## Plan Items

- [x] §7 of `daily-universe-scope-expansion-2026-05-27.md`: instance lock
  m8g.large → r8g.large; dated 2026-06-30 operator-quote block; §7 Rule 2
  memory budget recomputed for 16 GiB at current universe; cost line recomputed.
  - Files: .claude/rules/project/daily-universe-scope-expansion-2026-05-27.md
- [x] `aws-budget.md`: update superseded instance ref + add "superseded → r8g.large
  2026-06-30" one-line marker.
  - Files: .claude/rules/project/aws-budget.md
- [x] `aws-indices-only-locked-architecture.md` §5: same instance/memory/cost
  update to r8g.large / 16 GiB in the §5 supersession marker.
  - Files: docs/architecture/aws-indices-only-locked-architecture.md
- [x] Rewrite ratchet `instance_type_lock_guard.rs`: m8g.large→r8g.large,
  8 GiB→16 GiB, re-pin the recomputed ₹ bill.
  - Files: crates/storage/tests/instance_type_lock_guard.rs
  - Tests: instance_lock_authoritative_rule_file_pins_r8g_large, instance_lock_monthly_bill_pinned_to_rupees
- [x] Terraform `variables.tf`: `instance_type` validation + default → r8g.large.
  - Files: deploy/aws/terraform/variables.tf
- [x] `aws-upgrade-instance.sh`: `FROM_TYPE` default t4g.medium → m8g.large;
  confirm r8g.large in `--to` allowlist (already present).
  - Files: scripts/aws-upgrade-instance.sh
- [x] `docker-compose.yml`: QuestDB `QDB_MEM_LIMIT` default 2g → 4g for 16 GiB;
  update the host-spec comment.
  - Files: deploy/docker/docker-compose.yml

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | `cargo test -p tickvault-storage --test instance_type_lock_guard` | all pass against new r8g.large/16-GiB text |
| 2 | grep `m8g.large` as a LOCKED value | only historical/superseded mentions remain |
| 3 | fresh `terraform apply` | provisions r8g.large (validation passes) |
| 4 | `aws-upgrade-instance.sh --from m8g.large --to r8g.large` | accepted (allowlist + FROM default) |
| 5 | `dry_run` / `MAX_DAILY_UNIVERSE_SIZE` / `SEAL_BUFFER_CAPACITY` | UNCHANGED |
