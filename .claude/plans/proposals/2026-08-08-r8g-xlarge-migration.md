# Proposal: r8g.xlarge migration — AZ un-pin, 13 timeframes, tick retention

**Status:** DRAFT — needs operator approval + a dated quote before ANY change lands
**Date:** 2026-08-08
**Approved by:** pending
**Absorbs:** `2026-08-08-az-unpin.md` (the AZ un-pin is Phase 1 here — same instance
replacement does both; that proposal stays as the standalone fallback if only the
outage fix is wanted)

---

## Scope honesty — READ FIRST

This plan delivers **a box that CAN hold** the 25,000-instrument / 13-timeframe /
tick-retaining workload, and fixes the outage that has kept the box dark since
~2026-08-06.

It does **NOT** deliver the data itself. The current runtime is **REST-only, 3–4
hardcoded index SIDs** (`SPOT_1M_REST_INDICES` = 13/25/51 + INDIA VIX). There is
**no live tick feed running at all** — Dhan live WS retired 2026-07-13, Groww live
WS retired 2026-07-15, GDF and TrueData are both DEFAULT-OFF trial-first locks whose
implementation PRs have never started.

**Filling 25,000 instruments with ticks requires a separate, larger effort** — a live
feed trial (GDF or TrueData), the instrument-universe expansion, and its own operator
scope edits to `websocket-connection-scope-lock.md` +
`daily-universe-scope-expansion-2026-05-27.md`. That is deliberately NOT in this plan.

**What Monday looks like if this lands:** the box starts (outage fixed), runs the
existing REST legs on 4 indices, and has 32 GB + 100 GB + 13 defined timeframes
waiting. Not 25,000 instruments streaming.

---

## Design

### Problem 1 — the box cannot start (the live outage)

`main.tf:77` pins the only subnet to a single AZ:

```hcl
availability_zone = "${var.aws_region}a"
```

`ap-south-1a` is out of capacity. Verified: six start attempts 2026-08-07 all
returned `InsufficientInstanceCapacity`; the authorized t4g.medium → t4g.large
flip was refused for the SAME reason and rolled back (workflow run 31148235540).
CloudWatch CPU: Aug 5 = 0h, Aug 7 = 0h, Aug 8 = 0h.

**That the type flip also failed is the proof the constraint is the ZONE, not the
size.** `describe-instance-type-offerings` confirms every candidate type
(t4g.medium, t4g.large, r8g.large, r8g.xlarge, r8gd.large, m8g.large, r8i.large)
is offered in **all three** Mumbai AZs.

**Fix:** provision a subnet in each of 1a/1b/1c; select the instance's subnet via a
new `var.availability_zone`. A future capacity failure becomes a one-variable change
and re-apply, not a re-architecture.

### Problem 2 — capacity for the target workload

| Requirement | Sizing |
|---|---|
| 13 timeframes × 25,000 live candles | 13 × 128 B × 25,000 = **42 MB** |
| Seal ring buffer | 200,000 × 144 B = **29 MB** |
| One day of ticks in RAM (25–80 M) | **2.3 – 7.2 GB** |
| QuestDB | 8 – 16 GB |
| App + OS + page cache | 4 – 8 GB |
| **Total** | **14 – 31 GB → r8g.xlarge (32 GB)** |

r8g = 8 GB RAM per vCPU (memory-optimised), Graviton4, same ARM architecture as
today → **drop-in, no rebuild**. Rejected alternatives: `m8g` (4 GB/vCPU — forces
buying unused CPU to reach 16 GB); `r8gd` (local NVMe is **wiped on stop**, and the
box stops daily); `r8i` (Intel — would force an x86 rebuild of the whole ARM
pipeline incl. lambdas).

### Problem 3 — three timeframes don't exist

Requested: **1s, 5s, 10s, 15s, 30s · 1m, 2m, 3m, 5m, 15m, 30m, 60m · 1d** (13).

`tf_index.rs` has 21 ordinals: M1=0, M3=1, M5=2, M15=3, D1=4, S1=5…S15=19, S30=20.
Present: S1, S5, S10, S15, S30, M1, M3, M5, M15, D1. **Missing: M2, M30, M60.**

**Fix:** append as ordinals 21/22/23, `TF_COUNT` 21 → 24 — the exact pattern the
second-frames already used, chosen so every pre-existing ordinal stays byte-stable
and `SEAL_SPILL_FORMAT_VERSION` stays 1. Never insert mid-list: ordinals are the
array index for `[Mutex<LiveCandleState>; TF_COUNT]`, `[Sender; TF_COUNT]`, and the
spill records already on disk.

### Problem 4 — disk

Current: **30 GB** (verified — Cost Explorer `VolumeUsage.gp3` qty=30.0 at
$2.736/mo; rule file §7 "30 GB LIVE — ACCEPTED"). Already hit 82% pressure on the
3-index workload.

| Layer | Est. rows/day @ 25k | Disk/month |
|---|---|---|
| Ticks | 25 – 80 M | 44 – 141 GB |
| 13 timeframes (sparse) | ~46 M | ~61 GB |

Sparse is confirmed by `live_candle_state.rs:105` — `bucket_start_ist_secs == 0` is
the "slot never opened" sentinel, so an empty bucket emits nothing. Dense would be
808 M rows/day (35,900/sec vs the ~5,000/sec envelope); sparse is ~2,050/sec.

**Fix:** provision **100 GB** on the fresh volume + ~30-day on-disk retention with
S3 archival. gp3 grows online in one command and can NEVER shrink — so start at 100,
not 250. `variables.tf:72` already permits 10–200 GB; 100 needs no validation change.

⚠️ The 25–80 M tick range is **Assumed**. It swings disk 3×. Measure on the first
live day and resize then.

### Problem 5 — EBS is AZ-locked

The 30 GB root cannot move zones. Snapshots are region-scoped and CAN restore into
any AZ. The old box **cannot be started** to take an application-level backup, so
the migration must be snapshot-based.

---

## Plan items

### Phase 1 — Terraform (no live change until applied)

- [ ] **1.1** Multi-AZ subnets: `aws_subnet.public` → `for_each` over `["a","b","c"]`,
      each keeping `map_public_ip_on_launch = true`; route-table associations follow.
  - Files: `deploy/aws/terraform/main.tf`
  - Tests: `terraform validate`, `terraform plan` reviewed before apply

- [ ] **1.2** New `var.availability_zone` (default `"b"`, validation `a|b|c`);
      instance takes `aws_subnet.public[var.availability_zone].id`.
  - Files: `deploy/aws/terraform/variables.tf`, `main.tf`
  - Tests: `terraform validate`

- [ ] **1.3** `var.instance_type` default + validation `t4g.large` → `r8g.xlarge`;
      rewrite the description with the 2026-08-08 rationale.
  - Files: `deploy/aws/terraform/variables.tf:27-36`
  - Tests: `terraform validate`

- [ ] **1.4** `var.ebs_gp3_size_gb` default 20 → 100 (validation already allows ≤200).
  - Files: `deploy/aws/terraform/variables.tf:66-74`
  - Tests: `terraform validate`

- [ ] **1.5** Rule-file edits per §7 Mechanical Rule 1 — operator dated quote,
      instance row, bill table, memory map (Rule 2 FLAG retires at 32 GB), EBS row
      (Rule 3), and the sub-₹1,000 target breach recorded honestly.
  - Files: `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`,
    `.claude/rules/project/aws-budget.md`,
    `docs/architecture/aws-indices-only-locked-architecture.md` §5
  - Tests: `instance_type_lock_guard.rs` (updated in 1.6)

- [ ] **1.6** Update the ratchet to pin `r8g.xlarge`; rename
      `instance_lock_authoritative_rule_file_pins_t4g_large` accordingly.
  - Files: `crates/storage/tests/instance_type_lock_guard.rs`
  - Tests: `cargo test -p tickvault-storage --test instance_type_lock_guard`

- [ ] **1.7** `scripts/aws-upgrade-instance.sh` `FROM_TYPE`/`TO_TYPE` defaults.
  - Files: `scripts/aws-upgrade-instance.sh`
  - Tests: `bash -n`; `--help` output reviewed

- [ ] **1.8** EIP decision. Operator Quote 10 (2026-07-19) already approves release
      for the no-live-orders period, and a bundled recreate is the sanctioned path
      (`docs/runbooks/eip-release.md`). **Verify the fresh ENI mints an ephemeral IP
      BEFORE merging `enable_eip=false`** — that ordering is non-negotiable.
  - Files: `deploy/aws/terraform/variables.tf` (`enable_eip`), runbook
  - Tests: live `describe-instances` shows a `PublicIpAddress` on the new box

### Phase 2 — Migration (operator-scheduled window, out of market hours)

- [ ] **2.1** Snapshot `vol-073ccaa417a0f344b`; record the snapshot id; wait for
      `completed`. Works on the stopped volume — no start needed.
- [ ] **2.2** Record pre-migration QuestDB row counts per table from the last known
      good state, for the 2.6 comparison.
- [ ] **2.3** `terraform apply` — new subnets + new r8g.xlarge in 1b with a fresh
      100 GB root. Old instance and volume left INTACT.
- [ ] **2.4** Create a volume from the 2.1 snapshot **in the new AZ**; attach to the
      new box as a secondary device.
- [ ] **2.5** Copy the QuestDB data directory across; detach the secondary volume.
- [ ] **2.6** Verify: row counts match 2.2 for every table, `make doctor` 7-section
      green, `/health` 200, REST legs firing on the next minute boundary.
- [ ] **2.7** Rotate the `EC2_INSTANCE_ID` GitHub secret + any EIP literal consumers
      (`downsize-instance.yml` `EXPECTED_EIP`, SSM `/tickvault/<env>/network/static-ip`).
- [ ] **2.8** After operator sign-off ONLY: delete the old volume, old instance, and
      the stale snapshots currently billing (`EBS:SnapshotUsage` $0.78 Jul / $0.35 Aug).

### Phase 3 — Timeframes (code)

- [ ] **3.1** Append `M2 = 21`, `M30 = 22`, `M60 = 23`; `TF_COUNT` 21 → 24; extend
      `ALL`, `bucket_secs`, name/parse maps, and the `[&str; TF_COUNT]` builder.
  - Files: `crates/trading/src/candles/tf_index.rs`
  - Tests: `tf_index` unit tests — ordinal stability, round-trip, bucket seconds

- [ ] **3.2** Extend the fixed-size consumers.
  - Files: `crates/app/src/rest_candle_fold.rs:399,413`,
    `crates/storage/src/shadow_persistence.rs:129-130`
  - Tests: `cargo test -p tickvault-app -p tickvault-storage`

- [ ] **3.3** Confirm `SEAL_SPILL_FORMAT_VERSION` stays 1 — append-only means every
      existing on-disk ordinal keeps its meaning. Add a regression test that an
      ordinal-0..=20 spill record still loads after the change.
  - Files: `crates/storage/src/seal_spill.rs` (test only)
  - Tests: `seal_spill` round-trip + backward-compat test

- [ ] **3.4** QuestDB DDL for the 3 new candle tables, with `feed` in the DEDUP key
      per I-P1-11 + feed-in-key.
  - Files: storage DDL + `shadow_persistence.rs`
  - Tests: `dedup_segment_meta_guard.rs`, `questdb_init_script_guard.rs`

- [ ] **3.5** Adversarial 3-agent review on the diff (hot-path, security, hostile).

---

## Edge Cases

| # | Case | Handling |
|---|---|---|
| E1 | 1b ALSO out of capacity at apply time | `var.availability_zone` → `"c"`, re-apply. All 3 offer every type. |
| E2 | Snapshot restore lands a 30 GB filesystem on a 100 GB volume | Secondary-volume copy (2.4/2.5), not root-restore — the root is fresh from AMI at 100 GB. |
| E3 | `instance_type` / `volume_size` sit in `lifecycle.ignore_changes` | Those suppress in-place updates only; a REPLACEMENT uses the new values. Confirm in `terraform plan` output before apply — do not assume. |
| E4 | EIP released but fresh ENI gets no public IP | 1.8 verifies BEFORE the release merges. Subnet already carries `map_public_ip_on_launch = true`. |
| E5 | Ordinal drift breaks on-disk spill records | Append-only; 3.3 regression test proves old records still load. |
| E6 | Real tick volume ≫ estimate; 100 GB fills fast | gp3 grows online, one command, no downtime. Disk-pressure watcher already alarms. |
| E7 | 25k × 13 TF exceeds the ~5,000/sec ingest envelope | Out of scope here (no feed yet). Flagged for the feed-trial plan: restrict second-frames to liquid instruments. |
| E8 | Rule-file edit lands without the operator quote | §7 Rule 1 REJECT — 1.5 is blocked until the quote exists. |

## Failure Modes

| # | Failure | Detection | Response |
|---|---|---|---|
| F1 | `terraform apply` fails mid-way | apply output; state file | Old box + volume untouched. Re-run or `terraform destroy` the new resources. |
| F2 | Data copy incomplete | 2.6 row-count mismatch vs 2.2 | Do NOT sign off. Old volume still exists — retry or restore. |
| F3 | New box boots but app fails | `make doctor`, `/health`, boot-heartbeat alarm | Roll back per Rollback below. |
| F4 | Old snapshot deleted too early | — | 2.8 gated on explicit operator sign-off. Never automatic. |
| F5 | Cost overshoot | `tv-prod-monthly-budget-v2`, currently $35 ceiling | ⚠️ **Ceiling must rise before apply** — see Cost. |
| F6 | Rust change breaks a `[_; 21]` consumer | `cargo test --workspace` | Compile-time — arrays are fixed-size, so this fails the build, not production. |

## Test Plan

| Layer | What |
|---|---|
| Terraform | `validate` + `plan` reviewed line-by-line before any apply |
| Unit | `tf_index` ordinals/round-trip/bucket-secs; `seal_spill` backward-compat |
| Integration | `cargo test --workspace` (TF_COUNT touches `common` → workspace scope per testing-scope.md) |
| Ratchet | `instance_type_lock_guard`, `dedup_segment_meta_guard`, `questdb_init_script_guard`, `claude_md_codebase_map_guard` |
| Live | Row-count parity (2.6), `make doctor` 7-section, `/health`, REST legs firing, `tv_process_rss_bytes` + `CPUCreditBalance` on the first session |
| Adversarial | 3-agent review before and after implementation |

## Rollback

| Phase | Rollback |
|---|---|
| 1 (terraform) | Nothing applied — revert the PR. |
| 2 (migration) | Old instance + 30 GB volume + snapshot all INTACT until 2.8. Revert `var.availability_zone`/`instance_type` and re-apply, or restore the snapshot. **The old box still cannot start in 1a — so rollback means rolling back to 1b/1c with the old type, not back to 1a.** |
| 3 (code) | Revert the PR. Append-only ordinals mean no on-disk migration to undo. |

Rollback window: from apply until operator sign-off at 2.8. Snapshot retained ≥7 days.

## Observability

| Signal | Where |
|---|---|
| Instance start success/failure | EventBridge + `tv-<env>-boot-heartbeat-missing` |
| Memory headroom (the Rule 2 FLAG) | `tv_process_rss_bytes`, `mem_used_percent` — **read on the first live session** |
| Disk fill rate vs the 100 GB estimate | `disk_health_watcher`, `disk_used_percent` |
| QuestDB ingest vs the ~5,000/sec envelope | `questdb_health`, WAL suspension watcher |
| Cost | `tv-prod-monthly-budget-v2` (ceiling must rise — see below) |
| New timeframes producing rows | `candles_named` console view; per-TF row counts |

---

## Cost

**r8g.xlarge · 100 GB · weekdays 08:00–17:00 (~210 hrs) · ticks retained**

| Line | Low | High |
|---|---|---|
| r8g.xlarge × 210 hrs | $34.86 | $50.40 |
| EBS gp3 100 GB | $9.12 | $9.12 |
| S3 cold archive | $7.50 | $7.50 |
| CloudWatch + SMS | $2.98 | $2.98 |
| Elastic IP (if kept) | $3.60 | $3.60 |
| **Subtotal** | **$58.06** | **$73.60** |
| **₹ incl. 18% GST** | **₹5,824** | **₹7,382** |

Rate derived from the recorded r8g.large bill ($0.083/hr → xlarge $0.166/hr); AWS
list may reach ~$0.24/hr, hence the range. Releasing the EIP saves ~₹360/mo.

⚠️ **This breaches the Quote 9 sub-₹1,000/month target by ~6×** and exceeds the live
**$35** budget kill-ceiling — whose `STOP_EC2_INSTANCES` actions were already stuck in
`EXECUTION_FAILURE` on 2026-07-31. **The ceiling must be raised in the same change**,
or the budget action will try to stop the box mid-session. Both need the operator's
dated quote.

---

## Guarantee matrices

Per `per-wave-guarantee-matrix.md` — the 15-row and 7-row matrices apply. Notes
specific to this plan:

- **Zero data loss:** no new tick-drop path. Old volume retained until sign-off.
  Ordinal append-only ⇒ no spill-record migration.
- **O(1):** unchanged. TF_COUNT 21→24 keeps fixed-size array indexing; no new
  hot-path allocation. Honest flag: the RAM decision read stays **O(log n)**
  (`spot_bar_store.rs`) — unchanged by this plan, still not O(1).
- **Uniqueness/dedup:** the 3 new candle tables carry
  `(security_id, exchange_segment, feed)` per I-P1-11 + feed-in-key.
- **Coverage / testing / review:** per Test Plan above; 3-agent adversarial pass
  before and after.

## Honest envelope

> 100% inside the tested envelope, with ratcheted regression coverage: terraform
> changes are `plan`-reviewed before apply; the old instance, volume and snapshot are
> retained until explicit operator sign-off; timeframe ordinals are append-only so
> every existing on-disk seal record keeps its meaning (regression-tested);
> `TF_COUNT` is a compile-time array bound, so a missed consumer fails the build, not
> production.
> **NOT claimed:** (a) that 25,000 instruments will flow — no live feed exists; this
> plan builds capacity, not data; (b) the real tick volume — 25–80 M/day is Assumed
> and swings disk 3×, measured on the first live day; (c) that r8g.xlarge is capacity-
> available in 1b at apply time — AWS capacity is not queryable in advance, which is
> exactly why the AZ becomes a variable; (d) live behaviour of the data copy — verified
> by row-count parity at 2.6, not asserted in advance; (e) the exact hourly rate —
> derived from the recorded bill, range shown, confirm on the first invoice.

## Auto-driver explanation

> Sir, our shop is in a market lane that has run out of space — the landlord cannot
> give us our stall, so the shop has been shut three days. So: we take a stall in the
> lane next door (any of the three lanes, whichever has room), and while moving we
> take a bigger table (32 GB instead of 4) and a bigger cupboard (100 GB instead of
> 30) — because you want to track twenty-five thousand items across thirteen different
> clocks instead of four items. We photograph the old cupboard first, keep the old
> stall exactly as it is until you personally confirm the new one works, and only then
> hand back the keys. Two honest words: the new shop costs about five times the old
> one, and on day one it will still only be selling the four items — filling it with
> twenty-five thousand is a separate job that needs a supplier we have not signed yet.
