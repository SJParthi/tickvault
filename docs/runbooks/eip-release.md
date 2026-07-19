# EIP Release — Bundled Erase-Window Recreate Runbook (2026-07-19)

> **Authority:** operator ruling 2026-07-19 (verbatim, typos included):
> *"until or unless we flip the real orders static ip is not needed due okay?"*
> — recorded in `.claude/rules/project/aws-budget.md` "OPERATOR RULING
> 2026-07-19 (SECOND, same day)", `daily-universe-scope-expansion-2026-05-27.md`
> §0 Quote 10 + §7 EIP row, and `websocket-connection-scope-lock.md`
> "2026-07-19 — Static IP / EIP ruling".
> **Safety order (operator-mandated):** VERIFY outbound-without-EIP FIRST,
> release SECOND. This runbook encodes that order mechanically.
> **Who runs what:** every `aws` CLI step needs credentialed access — the
> coordinator session or the operator. The dev sandbox has NO AWS credentials
> (verified 2026-07-18/19). The terraform release step is a merge-triggered
> auto-apply — no manual `terraform` typing.

---

## §0. Why the release CANNOT be standalone (live verification, 2026-07-19)

Live describe evidence (coordinator session, 2026-07-19):

| Fact | Value |
|---|---|
| Instance | `i-0b956d0209231a48b` (tv-prod-app), eth0 = `eni-01fdeec2412f55587` |
| Current public IP | the EIP `13.234.145.177` (`eipalloc-01d43d4debab9217b`, the account's ONLY EIP) |
| Subnet | `subnet-00c8d06903d1482ea`, `MapPublicIpOnLaunch=true`, route `0.0.0.0/0 → igw-00469f8a48d456a9c` (public, no NAT) |
| The blocker | ephemeral-public-IP assignment is a **LAUNCH-TIME ENI attribute**. This ENI launched 2026-05-24, BEFORE the subnet flag landed (~2026-05-29, `main.tf:74-88`), and AWS cannot enable the attribute post-launch. |
| Verdict | Releasing the EIP today (running OR after stop/start) leaves the box with **NO public IPv4** → no SSM, no REST pulls, no deploys. Matches the 2026-05-31 `variables.tf:61` observation and daily-universe §7. |

**Therefore:** the release lands ONLY bundled with the erase-window instance
RECREATE — a FRESH launch inherits the subnet's auto-assign and mints an
ephemeral public IP on every start. The 20 GB fresh-volume pre-stage
(`variables.tf ebs_gp3_size_gb = 20`) rides the same recreate: −₹361 (EIP)
and −₹92 (EBS) land in ONE window.

**Why an ephemeral IP is enough right now:**
- SSM agent + deploys: OUTBOUND-only to SSM endpoints — any public IP works;
  no inbound static address needed (`main.tf:314-328` SSM core attachment;
  deploys are `ssm send-command`, never SSH-to-IP).
- Market data: the Dhan §8 + Groww §9 per-minute REST pulls have NO static-IP
  requirement — only Dhan ORDER APIs mandate a whitelisted static IP, and
  live orders are OFF (`dry_run=true`, all order gates locked).
- App boot: the Step 6a static-IP gate + Step 5.5 IP verification **already
  retired** with the Dhan live-WS lane (PR-C2, 2026-07-13). `crates/core/src/
  network/ip_verifier.rs` has ZERO production callers (Verified by source
  scan 2026-07-19) — the app boots identically on an ephemeral IP. **No code
  change, no config flag needed.**

---

## §1. Pre-window prep (any day before the window)

1. **Lever 1 first (independent):** after Mon 2026-07-22, delete the rollback
   snapshot `snap-090ed9c4f3df0ca61` (see aws-budget.md Lever 1 — sanctioned,
   schedule-only).
2. Pick the erase window: a post-market evening (box auto-stops 16:30 IST;
   the market-hours guards in `terraform-apply.yml` block in-session applies
   anyway). The window IS the operator's planned data-erase window — the
   fresh 20 GB volume deliberately starts empty; SEBI history lives in the S3
   cold bucket (`tv-prod-cold`, 5y lifecycle) and is NOT touched.
3. Confirm nothing in-flight needs the old root volume (the 2026-07-15
   snapshot is deleted by then; the recreate is one-way).

## §2. The window — RECREATE → VERIFY → RELEASE (single evening)

### Step 2a. Recreate the instance (coordinator/operator, credentialed CLI)

The merge-triggered terraform lane cannot express `-replace`, and
`disable_api_termination = true` blocks destroy (`main.tf:344-360`), so the
recreate is a deliberate two-step manual action:

```bash
# 1. allow termination (the documented two-step destroy defense)
aws ec2 modify-instance-attribute --instance-id i-0b956d0209231a48b \
  --no-disable-api-termination --region ap-south-1
# 2. targeted replace against the S3-backed state (same backend-config as
#    terraform-apply.yml; run outside market hours)
terraform -chdir=deploy/aws/terraform init -backend-config=... # (workflow values)
terraform -chdir=deploy/aws/terraform apply -replace=aws_instance.tv_app
```

Notes:
- The fresh launch uses the AL2023 arm64 AMI + `user-data.sh.tftpl` full
  cattle bootstrap (`main.tf:384-389`) and the `ebs_gp3_size_gb=20` default
  (the pre-staged fresh-provision intent).
- `enable_eip` is STILL true at this point — `aws_eip.tv_app` re-associates
  the EIP to the new instance automatically (`main.tf:431-439`). Nothing is
  released yet (verify-first order).
- Alternative: a guarded one-shot `workflow_dispatch` workflow (the
  `downsize-instance.yml` pattern) can wrap this — flagged follow-up, not
  required for a one-time window.

### Step 2b. Rotate identities (immediately after the new instance is running)

1. GitHub secret `EC2_INSTANCE_ID` → the NEW instance id (daily-universe §7
   Rule 3 requirement).
2. Verify SSM sees the new box:
   `aws ssm describe-instance-information --region ap-south-1` lists the new
   id (the deploy lane depends on this).
3. The EventBridge start/stop targets reference `aws_instance.tv_app.id` via
   terraform — the `-replace` apply already re-wired them.

### Step 2c. VERIFY the fresh ENI mints an ephemeral IP (the operator's verify-first step)

```bash
# reversible probe: DISASSOCIATE (not release) the EIP
aws ec2 disassociate-address --association-id <from describe-addresses> --region ap-south-1
# wait ~2-5 min, then:
aws ec2 describe-instances --instance-ids <NEW_ID> --region ap-south-1 \
  --query 'Reservations[0].Instances[0].PublicIpAddress'
```

- **PASS** = a fresh ephemeral public IP appears (the launch-time attribute is
  ON for the new ENI). Also verify SSM ping + one `ssm send-command` echo.
- **FAIL** = no IP within ~5 min → re-associate the EIP
  (`aws ec2 associate-address`) — fully reversible, box back to today's
  state; investigate before any release. **Do NOT proceed.**
- Optional belt+suspenders: one manual stop→start and re-check the IP
  (a NEW ephemeral address each start is EXPECTED and fine).

### Step 2d. RELEASE (the terraform PR — the sanctioned mechanism)

Only after 2c PASSES: open + merge the `enable_eip = false` PR
(`deploy/aws/terraform/variables.tf` default flip + a rewritten `enable_eip`
description + dated notes). The path-filtered
`.github/workflows/terraform-apply.yml` lane auto-applies on the main push
(plan on PR, apply outside market hours, 3 post-close cron retries) —
`aws_eip.tv_app` count drops to 0 → the EIP is disassociated + released.
That PR must ALSO update, in lockstep (each currently pins/uses the literal
or the EIP concept):

| Consumer | File:line | Action at release |
|---|---|---|
| Downsize workflow abort guard | `.github/workflows/downsize-instance.yml:146` (`EXPECTED_EIP: 13.234.145.177`) | retire the EIP identity check (dated note) — the workflow is one-shot-complete anyway |
| SSM static-ip param | `scripts/aws-seed-ssm-parameters.sh:206` (`/tickvault/<env>/network/static-ip`) | mark n/a for the no-EIP period (dated note); the param's only code reader (`ip_verifier`) is dormant |
| Autopilot EIP self-heal | `scripts/aws-autopilot.sh:168-183` | passes naturally (a public IP IS present); the re-associate arm goes dead — dated note |
| aws-control rotate-ip job | `.github/workflows/aws-control.yml:332-373` | obsolete during the no-EIP period — dated note |
| Upgrade script EIP checks | `scripts/aws-upgrade-instance.sh:275-280,415-420` | already tolerant (warns when no EIP associated) — verify wording |
| `variables.tf enable_eip` description | `deploy/aws/terraform/variables.tf:60-64` | rewrite: the 2026-05-31 "mandatory" rationale is superseded for the fresh-launch instance |

## §3. Post-release verification (same evening + next trading morning)

1. `aws ssm describe-instance-information` — the box stays a managed
   instance on its ephemeral IP.
2. Trigger one deploy-lane smoke (`deploy-aws.yml` dispatch or wait for the
   next merge): `ssm send-command` path green.
3. Next trading morning (08:30 auto-start): boot Telegram arrives; spot-1m +
   option-chain rows land (`spot_1m_rest` / `option_chain_1m` counts
   advancing); budget digest arrives.
4. Confirm the ephemeral IP CHANGED across the stop/start (expected) and
   nothing paged about it (no IP-pinned monitor remains — §2d table).
5. Bill watch: EIP line disappears from the next Cost Explorer/digest cycle
   (−$3.60/mo pre-GST) + EBS line at 20 GB (−$0.92/mo).

## §4. RE-ENABLE PROTOCOL — live-trading return (plan ≥7 days before go-live)

Dhan's static-IP whitelist has a **7-day modify cooldown** — this sequence
starts AT LEAST 7 days before the first live order:

1. Fresh dated operator quote editing `websocket-connection-scope-lock.md`
   ("2026-07-19 — Static IP / EIP ruling" section) + daily-universe §7.
2. `enable_eip = true` terraform PR → auto-apply allocates + associates a
   **NEW address** (the old `13.234.145.177` is gone forever — never assume
   it returns).
3. Register with Dhan: `POST https://api.dhan.co/v2/ip/setIP`
   (`{"dhanClientId":"…","ip":"<NEW_EIP>","ipFlag":"PRIMARY"}`) — get it
   right the first time (7-day cooldown on modify); verify with
   `GET /v2/ip/getIP` = MATCH.
4. Lockstep updates: `downsize-instance.yml` `EXPECTED_EIP` (if that guard is
   revived), SSM `/tickvault/prod/network/static-ip`, and RE-WIRE the boot
   static-IP gate (`ip_verifier::verify_static_ip_at_boot` — dormant module,
   restore a call site) before the first live order.
5. Only then may the live-fire gates (dry_run flip etc., their own rule
   protocols) proceed.

## §5. Bill effect (arithmetic — see aws-budget.md second-ruling section)

- Interim (pre-window): ~₹1,473 − 125 (snapshot) − up to 271 (alarm trims) −
  28 (SMS) = **~₹1,049** floor.
- Post-window (bundle lands): **~₹596–717/mo** (~176-hr basis; full vs
  conservative trims) / **~₹808–929** at the 270-hr ceiling — under the
  < ₹1,000 target at BOTH bases.

## §6. Honest envelope

- The §0 verdict is live-verified for the CURRENT ENI; the §2c probe is the
  mechanical proof for the NEW one — the release never fires before it passes.
- The recreate erases the hot QuestDB volume BY DESIGN (the operator's erase
  window); S3 cold retention (SEBI 5y) is untouched. Anything not yet
  archived to S3 on the old 30 GB root is lost at terminate — the pre-window
  check in §1.3 exists for exactly this.
- An ephemeral IP changes on every stop/start: fine for outbound-only
  (SSM/REST), structurally incompatible with the Dhan ORDER whitelist —
  which is the entire reason §4 exists and why this release is legal ONLY
  while real orders are off.
