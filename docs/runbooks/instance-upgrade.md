# Runbook — One-command instance upgrade (type + EBS + QuestDB retune)

> **Tool:** `scripts/aws-upgrade-instance.sh`
> **Authority:** `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`
> §7 Mechanical Rule 1 (instance-type lock) + Rule 7 (in-place upgrade).
> **Status:** READY-TO-FIRE TOOLING. The instance-type LOCK is still
> `m8g.large` everywhere. This runbook + script let the operator EXECUTE a
> bigger type safely **once the lock flip is authorized** — they do NOT flip
> the lock themselves.

---

## TL;DR — the one command

After the dated-quote + 4-file lock flip is done (see "Lock flip" below), a
full upgrade to r8g.large with a 30→60 GB disk and QuestDB 2g→4g is:

```bash
./scripts/aws-upgrade-instance.sh \
  --from m8g.large --to r8g.large \
  --ebs-size 60 --ebs-iops 4000 --ebs-throughput 250 \
  --qdb-mem 4g
```

Then run the two printed in-guest commands on the box (the script never SSHes).
Dry-run first to see every action without changing anything:

```bash
./scripts/aws-upgrade-instance.sh --from m8g.large --to r8g.large \
  --ebs-size 60 --qdb-mem 4g --dry-run
```

---

## Design: auto-DETECT, not auto-SPEND

| Piece | What it does | Spends money? |
|---|---|---|
| `tv-prod-mem-used-high` CloudWatch alarm | Pages the operator when host memory > 80% for 15 min (3×5m) | No — it only tells you WHEN to upgrade |
| `tv-prod-disk-used-high` alarm (pre-existing) | Pages when root volume > 75% | No |
| `scripts/aws-upgrade-instance.sh` | The operator runs it MANUALLY to apply an upgrade | Yes — but only when invoked |

The system **detects** capacity pressure and tells the operator; it **never
auto-upgrades**. The decision (and the cost) stays with a human, consistent
with `aws-budget.md`.

---

## Prerequisites

1. `aws` CLI installed + `aws configure` done (the script uses `~/.aws` creds).
2. Run from the operator's Mac, on a **Sunday / off-market window**. The script
   refuses the instance-type flip during 09:00–15:30 IST Mon–Fri (a stop would
   interrupt the live session) unless you pass `--force`.
3. **The lock flip must already be done** if `--to` is anything other than the
   current locked `m8g.large` (see below). The script's `--to` allowlist is
   `{m8g.large, r8g.large, m8g.xlarge, r8g.xlarge}` — but allow-listing a type
   is NOT the same as flipping the production lock.

---

## What each step does (and which ones stop the box)

| Step | Online? | Trigger |
|---|---|---|
| 1. Validate args + discover instance by `Name=tv-prod-app` | yes | always |
| 2. Online EBS resize (`aws ec2 modify-volume`) | **online, no stop** | only if `--ebs-size` / `--ebs-iops` / `--ebs-throughput` given |
| 3. Instance-type flip: stop → modify → start → wait | **STOPS the box (~3 min)** | only if current type != `--to` |
| 4. Verify new type + EIP preserved | yes | after a flip |
| 5. QuestDB mem_limit retune | **online, restarts only the questdb container** | only if `--qdb-mem` given |
| 6. Print in-guest `growpart` + `xfs_growfs`/`resize2fs` commands | print only | only if `--ebs-size` given |

If you pass ONLY `--ebs-size` and/or `--qdb-mem` (no `--to` change), the box is
**never stopped** — these are online operations and the market-hours guard does
not block them.

---

## The in-guest filesystem grow (you run these on the box)

AWS grows the EBS volume online, but the OS partition + filesystem must be
grown **inside the guest** to use the new space. The script prints these; run
them over SSM / SSH after the resize reaches `optimizing|completed`:

```bash
lsblk                              # find the root partition (e.g. nvme0n1p1)
sudo growpart /dev/nvme0n1 1       # NOTE the space before "1"
sudo xfs_growfs -d /               # AL2023 default is xfs; for ext4: sudo resize2fs /dev/nvme0n1p1
df -h /                            # confirm the new size
```

Until you grow the filesystem in-guest, the OS still sees the OLD size.

---

## Rollback (automatic on a failed start)

If the box fails to come back up on the new type (the classic
`InsufficientInstanceCapacity` for the target type in `ap-south-1`), the script
**automatically rolls back**: it re-sets the instance type to `--from`, starts
the box, verifies it is running, and exits non-zero with a clear message. The
EBS volume + all QuestDB data survive (`delete_on_termination = false`), so a
failed type flip never loses data. If the rollback ALSO fails to start the box,
the script stops and tells you to intervene manually (`aws ec2
describe-instances --instance-ids <id>`).

EBS resize and QuestDB retune have no rollback — they are non-destructive grows
(you can shrink IOPS/throughput back via another `modify-volume`; gp3 size can
only grow).

---

## QuestDB mem recommendation for 2K-both-feeds

When both feeds run at ~2K SIDs each, QuestDB sees materially more write/read
pressure. Recommended retune on a bigger box:

| Setting | Today (m8g.large, 8 GiB) | 2K-both-feeds target |
|---|---|---|
| QuestDB `mem_limit` | `2g` | **`4g`** (`--qdb-mem 4g`) |
| gp3 IOPS | 3000 (baseline) | **4000** (`--ebs-iops 4000`) |
| gp3 throughput | 125 MiB/s (baseline) | **250 MiB/s** (`--ebs-throughput 250`) |
| Disk size | 30 GB | **60 GB** (`--ebs-size 60`) — more hot window |

The compose file reads `mem_limit: ${QDB_MEM_LIMIT:-2g}`, so `--qdb-mem 4g`
writes `QDB_MEM_LIMIT=4g` into the compose env file (the `.env` next to the
compose file, on the box — a runtime-generated, gitignored file) and recreates
only the `tv-questdb` container. Default stays `2g` when unset (current
behaviour unchanged). Pass `--apply-ssm` to send the on-box command via SSM
RunShellScript automatically, or run the printed command yourself.

The Terraform `ebs_gp3_iops` / `ebs_gp3_throughput` variables exist so a later
`terraform apply` keeps in sync with an online bump (it will not revert the
resize you did with the script).

---

## Lock flip — the 4-file + ratchet update (DO THIS FIRST for a new type)

This tool is intentionally decoupled from the production instance-type LOCK.
Flipping the locked type (e.g. `m8g.large` → `r8g.large`) is a **separate,
deliberate act** that requires, per
`daily-universe-scope-expansion-2026-05-27.md` §7 Mechanical Rule 1:

1. A **dated operator quote** authorizing the new type.
2. Update `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md` §7.
3. Update `docs/architecture/aws-indices-only-locked-architecture.md` §5.
4. Update `.claude/rules/project/aws-budget.md` (marked SUPERSEDED).
5. Update `crates/storage/tests/instance_type_lock_guard.rs` to pin the new type,
   AND `deploy/aws/terraform/variables.tf` `instance_type` validation.

**This PR does NONE of those** — it ships ready-to-fire tooling only, so the
`instance_type_lock_guard.rs` ratchet stays green. Run the upgrade script for a
new `--to` type ONLY after the 5 items above land.

---

## Verify after upgrade

1. `make doctor` / CloudWatch `tv_boot_completed` — app rebooted cleanly.
2. Ticks flowing (CloudWatch / Telegram streaming confirmation).
3. EIP unchanged (the script verifies this and refuses if it changed — Dhan
   static-IP whitelist depends on it).
4. If you grew the disk: `df -h /` on the box shows the new size (after the
   in-guest grow).

## See also

- `docs/runbooks/may31-inplace-upgrade-and-access.md` — the original manual
  upgrade + access notes.
- `docs/runbooks/aws-capacity-error.md` — `InsufficientInstanceCapacity` fallback.
