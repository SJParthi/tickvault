# May 31 Deploy — In-Place m8g.large Upgrade + 3-Month Data-Pull + Access Guide

> **Authority:** `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md` §7 (operator lock 2026-05-29, Quotes 5+6) > `aws-budget.md` (superseded) > this runbook.
> **Scope:** The ONE-TIME upgrade of the **already-running** `t4g.medium` box
> (`i-0b956d0209231a48b`, `tv-prod-app`) to **`m8g.large`**, plus the 3-month
> **data-pull-only** config (NO real orders until ~Sep 1), plus how to reach the
> box from Claude Code / Chat / Cowork / IntelliJ / GitHub.
> **Target:** running before **Sunday evening, 2026-05-31**.
> **Companion:** `aws-deploy.md` (first-time account setup), `aws-deployment.md`
> (day-2 ops reference), `aws-disaster-recovery.md`.

---

## 0. The one-paragraph picture (auto-driver)

> Sir, the shop (`t4g.medium`) is already open and running. We are NOT building
> a new shop — we just **swap in a bigger fridge** (`m8g.large`, 8 GB). We do
> the swap on **Sunday when the market is closed**, so no customer (tick) is
> lost. For the next **3 months we only WATCH and RECORD** — we write down every
> time the phone line drops, every time a price is late, how fast everything is.
> **We place ZERO real orders.** After 3 months of clean recordings, THEN we
> turn on real ordering.

```
ALREADY RUNNING (t4g.medium, EIP 52.66.22.195)
        │
   Sunday off-market (after 3:30 PM IST / weekend)
        │
   STEP 1  terraform apply ──► reconciles infra ONLY
        │                      (weekday schedule, EBS→30GB, EIP off, alarms)
        │                      NEVER touches instance type or wipes data
        │
   STEP 2  aws-upgrade-instance.sh ──► stop → modify → start
        │                              t4g.medium → m8g.large IN PLACE
        │                              (EBS + QuestDB data preserved)
        │
   STEP 3  SSM in → git pull && docker compose up ──► latest code live
        │
   3 MONTHS: data-pull only, dry_run=true, monitor disconnect/latency/gaps
        │
   ~Sep 1: flip enable_eip=true + register IP with Dhan + dry_run=false
```

---

## 1. Pre-flight (do this any time before Sunday)

| # | Check | Command / where | Pass = |
|---|---|---|---|
| 1 | Both fixes are on `main` | `git log --oneline -5 origin/main` | #867 (terraform fmt+guard) + #868 (index-expiry noise) present |
| 2 | Terraform is `fmt`-clean | `terraform -chdir=deploy/aws/terraform fmt -check -recursive` | exit 0 |
| 3 | The running box is healthy | EC2 console → `i-0b956d0209231a48b` | `Running`, 3/3 checks |
| 4 | You have the SSH key OR SSM works | see §4 | one of them reachable |
| 5 | `dry_run = true` in config | `grep dry_run config/base.toml` | `true` (PAPER — no orders) |

> **Why no rush on the instance swap:** the box is already serving. The swap is
> a ~3-minute stop/start. Doing it off-market means zero tick loss.

---

## 2. STEP 1 — `terraform apply` (infra only, off-market)

**When:** any weekday after **3:30 PM IST**, or any time on the weekend. The
GitHub workflow's market-hours guard blocks apply inside 09:15–15:30 IST Mon–Fri
by design, so you literally cannot fat-finger it during the session.

**How (pick one):**

- **Automated (preferred):** the `terraform-apply` workflow fires on push-to-main
  for `deploy/aws/terraform/**`. Since #867 already merged, trigger a fresh run
  **off-market** via GitHub → Actions → `terraform-apply` → **Run workflow**
  (`workflow_dispatch`), leave `skip_apply=no`, `confirm_market_hours=no`.
- **Manual from your Mac:**
  ```bash
  cd deploy/aws/terraform
  terraform init   # uses the S3 backend
  terraform plan   # READ THIS FIRST — see the safety note below
  terraform apply
  ```

**What the plan SHOULD show (and must NOT show):**

| Expect ✅ | Must NOT see 🔴 |
|---|---|
| `aws_eip.tv_app[0]` **destroy** (EIP off for data-pull — intended, saves ~₹430/mo) | `aws_instance.tv_app` **must change / replace** (guarded by `ignore_changes`) |
| EventBridge crons updated to **MON-FRI** 03:00/11:00 UTC (08:30/16:30 IST) | root volume / `delete_on_termination` change |
| Root EBS `volume_size 10 → 30` (online grow, no data loss) | any `instance_type` line in the diff (it's ignored) |
| `disable_api_stop true → false` | `user_data` forcing replacement |

> **🔴 STOP if the plan shows the instance being replaced/recreated.** With the
> #867 `lifecycle { ignore_changes = [ami, instance_type, user_data] }` guard it
> should not — but if it does, do NOT apply; ping me. (Losing 52.66.22.195 EIP is
> expected and fine; losing the *instance* is not.)

> **Note on the EIP:** the box currently has `tv-prod-eip` (52.66.22.195).
> `enable_eip=false` releases it — **correct for the no-orders window** (Dhan
> static-IP whitelist isn't needed without orders). After the apply the box keeps
> a normal public IP (`map_public_ip_on_launch=true`); that IP changes on each
> stop/start, which is fine for data-pull. We re-enable a stable EIP at ~Sep 1.

---

## 2.1 — ⚠ Grow the filesystem after the EBS resize (MANDATORY)

> Sir, the resize made the **fridge** bigger (block device 10 → 30 GB) — but the
> **shelves inside** are still built for 10 GB. The OS partition + filesystem do
> NOT auto-expand. Until you push the shelves out, QuestDB still hits "disk full"
> at 10 GB even though the console shows 30. **One command fixes it. Skipping
> this is the #1 silent way the 3-month run dies.**

After STEP 1's `terraform apply` resizes the volume, SSM into the box and run:

```bash
# AL2023 arm64 / Nitro: root device is nvme0n1, partition 1, filesystem xfs.
sudo growpart /dev/nvme0n1 1     # expand the partition to fill the volume
sudo xfs_growfs /                # expand the xfs filesystem to fill the partition
df -h /                          # confirm: Size now ~30G, not 10G
```

(If `lsblk` shows the root device is not `nvme0n1` or `df -T /` shows `ext4`
instead of `xfs`, adjust: `resize2fs /dev/<dev>` for ext4. AL2023 arm64 default
is `nvme0n1` + `xfs`.) `growpart`/`xfs_growfs` are **idempotent** — safe to
re-run; they no-op if already grown.

### Disk capacity over the 3 months — the "grow online when the alarm fires" plan

**Why this matters:** retention is **90 days** (`config/base.toml:165`) and a
3-month run is ~90 days — so **no partition ever ages out → zero auto-eviction →
the 30 GB only fills, never drains.** Operator decision (2026-05-29): **start
30 GB, grow online when the alarm trips** (gp3 grows with NO downtime, NO data
loss).

| Trigger | Action (zero downtime) |
|---|---|
| CloudWatch alarm **`tv-prod-disk-used-high`** fires at **>75%** (added in the deploy-hardening PR) — pages via SNS/Telegram | 1. Bump `ebs_gp3_size_gb` (e.g. `30 → 50`) in `variables.tf` → `terraform apply`. 2. On the box: `sudo growpart /dev/nvme0n1 1 && sudo xfs_growfs /`. |
| Belt-and-suspenders | Weekly `df -h /` over SSM / `mcp__tickvault-logs__docker_status` during your 3-month watch. |

> 50 GB gp3 ≈ ₹155/mo more than 30 GB — only paid **if** you actually grow, and
> only from the grow date. Until then the bill stays at the locked ~₹2,058/mo.

---

## 3. STEP 2 — In-place `t4g.medium → m8g.large` (off-market)

The instance type is **NOT** managed by Terraform (it's in `ignore_changes`).
The swap is done by the dedicated script so it's controlled and reversible:

```bash
# Dry-run first — prints exactly what it WILL do, changes nothing:
bash scripts/aws-upgrade-instance.sh --dry-run

# Then the real swap (off-market only — the script refuses 09:00–15:30 IST
# Mon–Fri without --force):
bash scripts/aws-upgrade-instance.sh
```

**What it does:** `aws ec2 stop-instances` → wait stopped →
`modify-instance-attribute --instance-type m8g.large` → `start-instances` →
wait running. **EBS root volume (all QuestDB data) is preserved** across a stop.
Downtime ≈ 3 minutes. The script verifies the EIP/association survives and
aborts if anything it didn't expect changed.

**Verify after:**
```bash
aws ec2 describe-instances --instance-ids i-0b956d0209231a48b \
  --query 'Reservations[].Instances[].InstanceType' --output text   # → m8g.large
```

---

## 4. STEP 3 — Deploy the latest code (SSM, off-market)

Code deploys are **over SSM**, never via Terraform/user_data (that's why
`user_data` is in `ignore_changes`). On the box:

```bash
cd /opt/tickvault        # or the clone path in user-data.sh.tftpl
git pull origin main
docker compose -f deploy/docker/docker-compose.yml up -d --build
make doctor              # 7-section health
```

The app boots `dry_run=true` → **PAPER trading, zero real orders**. Confirm in
the boot log: `dry_run:true` and `AGGREGATOR-HB-01` heartbeat showing
`seals_emitted` ≈ `close_pct_nonzero` (the % column is populating).

---

## 5. The 3-month monitoring goal (what to actually watch)

You wanted, verbatim: *"how many times websocket gets reconnected or
disconnected … ticks missing … latency … monitored at least for three months
along with data pull precisely."* Here is where each lives — **no manual log
grepping needed:**

| You want to know… | Signal (already wired) | How to read it |
|---|---|---|
| WS disconnect/reconnect count | `tv_websocket_connections_active` gauge + `WebSocketDisconnected*` events | CloudWatch metric / `mcp__tickvault-logs__prometheus_query` |
| Ticks missing / silent feed | **WS-GAP-06** tick-gap detector (coalesced 60s summary) | `mcp__tickvault-logs__tail_errors` / CloudWatch logs |
| Overall "is it healthy right now" | **SLO-02** composite score (10s) | log-only, never pages; `prometheus_query "tv_slo_*"` |
| Latency (wire→done) | `tv_wire_to_done_duration_ns`, `tv_tick_processing_duration_ns` | CloudWatch metric |
| Daily proof it's alive | `AGGREGATOR-HB-01` per-minute heartbeat | log / CloudWatch |

> After #868, the **~120 bogus "derivative expiry failed to parse" errors per
> boot are gone** — so the disconnect/latency signals above are no longer buried
> in index-row noise.

**Pulling logs/screenshots (your "pull the logs screenshots everything"):**
- CloudWatch Logs group `/tickvault/prod/app` (14-day hot retention) → export.
- `mcp__tickvault-logs__summary_snapshot` / `app_log_tail` from any Claude session.
- QuestDB Web Console for tick/candle counts (operator-only, not 24/7).

---

## 6. ACCESS — reach the box from every surface

> **Golden rule:** there is **no SSH port-22 dependency** for normal ops — use
> **SSM Session Manager** (no inbound port, IAM-gated, audited). SSH stays as a
> break-glass fallback only.

| Surface | How you access the AWS box | Notes |
|---|---|---|
| **Claude Code (CLI/web/Cowork)** | The **`tickvault-logs` MCP** (auto-loaded from `.mcp.json`) — `mcp__tickvault-logs__run_doctor`, `tail_errors`, `prometheus_query`, `questdb_sql`, `app_log_tail`, `docker_status`. Plus `aws ssm start-session` from a Bash tool when creds are present. | Zero manual setup per session — same tools local or AWS. This is the primary path. |
| **Claude Chat** | Ask it to read this repo + the CloudWatch/QuestDB outputs you paste, OR drive the `tickvault-logs` MCP if connected. | Best for reasoning over pasted logs/screenshots. |
| **Claude Cowork** | Same `.mcp.json` → same `mcp__tickvault-logs__*` tools surface automatically. | Identical to Claude Code. |
| **IntelliJ** | (a) AWS Toolkit plugin → **Session Manager** shell into `i-0b956d0209231a48b`; (b) Remote-SSH over the SSM tunnel; (c) plain Git/terminal for the repo. | No port-22 exposure needed — SSM tunnel. |
| **GitHub** | All infra + code changes go through PRs; `terraform-apply` + CloudWatch alarms + Telegram run from Actions. You never type `terraform apply` on prod by hand. | This is the "no manual inputs" path. |

**One-time access setup (if not already done):**
```bash
# SSM Session Manager — no SSH key, no open port:
aws ssm start-session --target i-0b956d0209231a48b --region ap-south-1
# requires: SSM agent on the box (AL2023 ships it) + the instance IAM role
# (already grants SSM) + your IAM user having ssm:StartSession.
```

---

## 7. ~Sep 1 — flip to LIVE (out of scope for May 31, noted for completeness)

When the 3-month data-pull proves clean and you're ready for real orders:

1. `enable_eip = true` in Terraform → `terraform apply` (off-market) → get a
   stable EIP.
2. Register that EIP with Dhan (`POST /v2/ip/setIP`) — **7-day modify cooldown**,
   so do it ≥ 7 days before first order.
3. Flip `dry_run = false` in config only after Phase-1 promotion criteria
   (`phase-0-architecture.md` §"Phase 1 promotion criteria") are all green.

---

## 8. Rollback (if the upgrade misbehaves)

| Problem | Action |
|---|---|
| Box won't start as m8g.large (rare capacity error) | `aws-upgrade-instance.sh` is idempotent; re-run, or temporarily set `TO_TYPE=t4g.medium` to revert in place. See `aws-capacity-error.md`. |
| Code deploy broke the app | `git checkout <prev-sha> && docker compose up -d` on the box; data is intact. |
| Terraform tried to replace the instance | Do NOT apply. The `ignore_changes` guard should prevent this; if the plan shows it, stop and investigate state drift. |
| Cost creeping up | Budget alarm at $25/mo (pre-GST) → SNS → kill-switch Lambda stops the box + pages Telegram. |

---

## 9. Honest envelope

100% inside the tested envelope: the instance swap is a stop/modify/start with
EBS preserved (no data loss across a stop); Terraform is guarded so apply can
never replace/wipe the box (`aws_deploy_safety_guard.rs`, 8 tests); the weekday
schedule + 30 GB EBS + EIP-off config holds the bill at ~₹2,058/mo incl. GST at
270 running hrs. Beyond the envelope (AWS capacity outage in the swap window,
Dhan-side feed outage) the rollback table §8 + `aws-disaster-recovery.md` apply.
This runbook does **not** promise "never disconnects" — the whole point of the
3-month window is to MEASURE disconnect/latency honestly before going live.
