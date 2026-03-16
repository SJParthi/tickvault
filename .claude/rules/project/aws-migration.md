---
paths:
  - "deploy/**"
---

# AWS Migration — Local Mac Cleanup Enforcement

## Purpose

When dhan-live-trader moves to AWS, ALL local Mac infrastructure must be deleted.
Enforced mechanically at 4 layers — **zero sentinel files, zero human memory required**.

## What Must Be Deleted on AWS Deploy

1. **`deploy/launchd/`** — entire directory
   - `DLT-Run.command` (Desktop one-click launcher)
   - `trading-launcher.sh` (Mac-specific: Docker Desktop, DNS flush, launchd)
   - `co.dhan.live-trader.plist` (launchd agent config)
   - `setup-power-management.sh` (Mac power/sleep settings)

2. **`scripts/smoke-test.sh`** — Mac-specific health checks (Docker Desktop, nc probes)

3. **Mac-specific code in `crates/app/src/infra.rs`**
   - `df -k .` disk check — replace with Linux `/proc` or cloud health check
   - Any `#[cfg(target_os = "macos")]` blocks

## How You Get Reminded (Fully Automatic)

**Trigger: touching ANY file in `deploy/aws/`**. No sentinel. No manual step.

### Layer 1: `ec2-setup.sh` self-guards
The AWS deploy script itself checks for `deploy/launchd/` and **refuses to run**
if Mac infra still exists. You literally cannot deploy to AWS without cleaning up.

### Layer 2: Pre-Push Gate 15/15 (`.claude/hooks/pre-push-gate.sh`)
Detects if your changeset includes `deploy/aws/` files. If yes AND `deploy/launchd/`
exists → **BLOCKS push**. Lists exactly what needs to be deleted.

### Layer 3: CI Gate (`.github/workflows/ci.yml`)
Stage 0 in Build & Verify. Diffs the PR for `deploy/aws/` changes. If found AND
`deploy/launchd/` exists → **BLOCKS merge**. Cannot reach main.

### Layer 4: Claude Code Rule (this file)
Auto-loaded when editing any file in `deploy/`. Claude Code must:
- Check if `deploy/launchd/` still exists when working on `deploy/aws/`
- If yes, raise it immediately — do not proceed until cleanup is confirmed

### Layer 5: Standalone Script (`scripts/aws-migration-guard.sh`)
Run manually anytime: `bash scripts/aws-migration-guard.sh`
Or full scan: `bash scripts/aws-migration-guard.sh . --full`

## Runtime Enforcement (prevents duplicate trading — FULLY AUTOMATIC)

The installed launchd agent on the Mac fires at 8:15 AM every day. If AWS is also
running, you get duplicate orders. This is prevented automatically:

### What happens when you run `ec2-setup.sh` on AWS:

1. **Step 8:** Sets SSM parameter `primary-host=aws` (safety net)
2. **Step 9:** SSHes into Mac automatically and runs `decommission-mac.sh`
3. `decommission-mac.sh` **DELETES EVERYTHING** from the Mac:
   - Kills running binary + containers
   - Unloads launchd + deletes plist (8:15 AM trigger gone)
   - **Deletes `/Users/Shared/dhan-live-trader/` entirely** (scripts, binary, config, all of it)
   - Removes all dhan Docker containers, images, and volumes
   - Deletes Desktop shortcut (`DLT-Run.command`)
   - Reverts power management
   - Sends Telegram confirmation

**One command on AWS. Mac is wiped. No auto-start. No manual start. Nothing to start.**

### If SSH to Mac fails (network issue):
- SSM safety net is active
- Next 8:15 AM → `trading-launcher.sh` checks SSM → sees `aws` → refuses to start,
  auto-creates local decommission file, sends Telegram warning
- Mac is blocked from trading, but files still exist until SSH works

### Three independent safety layers:
1. `decommission-mac.sh` via SSH (deletes everything — primary path)
2. SSM parameter check (blocks launcher if SSH failed — safety net)
3. `~/.dhan-aws-is-primary` flag (instant exit if files somehow remain)

## Migration Checklist (copy into AWS PR description)

### On AWS (run ec2-setup.sh — handles steps 1-2 automatically):
- [ ] Run `sudo bash deploy/aws/ec2-setup.sh`
- [ ] Verify SSM parameter: `aws ssm get-parameter --name /dhan-live-trader/deployment/primary-host`

### On Mac (run decommission-mac.sh — handles steps 3-7 automatically):
- [ ] Run `bash deploy/launchd/decommission-mac.sh`
- [ ] Verify: `launchctl list | grep dhan` (should return nothing)
- [ ] Verify: `ls ~/Library/LaunchAgents/co.dhan*` (should not exist)
- [ ] Verify: `cat ~/.dhan-aws-is-primary` (should exist)

### In repo (done via PR):
- [ ] Delete `deploy/launchd/` entirely
- [ ] Delete `scripts/smoke-test.sh`
- [ ] Replace Mac disk check in `infra.rs` with Linux equivalent
- [ ] Remove any `#[cfg(target_os = "macos")]` blocks

## What This Prevents

- Stale Mac scripts lingering in repo after AWS migration
- Confusion about which deploy path is active
- Mac-specific code running on Linux AWS instances
- Forgotten launchd agent still trying to start on Mac after migration
