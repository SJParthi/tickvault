# AWS Disaster Recovery Runbook

> **Scope:** Post-go-live emergency procedures. Use when something is
> actively broken — DO NOT use this as a first-time setup guide
> (see `aws-deploy.md` for that).

## Quick reference — What's broken?

| Symptom | Jump to |
|---|---|
| Telegram alert: "QuestDB disconnected > 30s" | [§1. QuestDB outage](#1-questdb-outage) |
| Telegram alert: "Pool watchdog HALT" | [§2. WebSocket pool halted](#2-websocket-pool-halted) |
| Telegram alert: "Order update WebSocket down > 60s" | [§3. Order update WS down](#3-order-update-ws-down) |
| systemctl status tickvault: inactive (crashed) | [§4. App crash, no auto-restart](#4-app-crash-no-auto-restart) |
| EC2 instance is unreachable (SSH + SSM both fail) | [§5. Instance unreachable](#5-instance-unreachable) |
| Dhan 807/DH-901 authentication errors | [§6. Token expired](#6-token-expired) |
| Dhan 805 "too many requests" | [§7. Rate-limited by Dhan](#7-rate-limited-by-dhan) |
| Disk full on /opt/tickvault/data/spill | [§8. Spill disk full](#8-spill-disk-full) |
| Running instance unexpectedly during 15:30-08:00 IST (off-hours) | [§9. Unexpected running instance](#9-unexpected-running-instance) |
| 8+ Dependabot CVEs flagged on branch | [§10. Dependabot CVE surge](#10-dependabot-cve-surge) |
| Need to roll back a bad deploy mid-day | [§11. Emergency rollback](#11-emergency-rollback) |

---

## §1. QuestDB outage

**Symptom:** Telegram `tv-questdb-disconnected` alert firing. Ticks are
still being ingested (ring buffer + spill absorbs them), but the
`ticks` table is not receiving writes.

**Immediate action:**

```bash
# 1. Open SSM session
aws ssm start-session --region ap-south-1 --target $INSTANCE_ID

# 2. Check docker
sudo -i
docker ps | grep tv-questdb
docker logs --tail 100 tv-questdb

# 3. If QuestDB is unresponsive, restart it
docker restart tv-questdb
sleep 10
docker exec tv-questdb curl -s localhost:9000/exec?query=SELECT+1

# 4. Watch the writer reconnect
journalctl -u tickvault -f | grep -E 'questdb|QuestDB'
# Expected: 'S3-1: QuestDB reconnected' within 2s of docker restart
```

**What the app is doing while QDB is down** (verified by
`chaos_questdb_full_session_zero_tick_loss`):
- Every incoming tick lands in the ring buffer (600,000 capacity)
- Overflow spills to `/opt/tickvault/data/spill/ticks-YYYYMMDD.bin`
- Zero ticks dropped up to ~1 million ticks
- On reconnect, ring buffer drains first (oldest-first), then spill file replay

**Maximum survivable outage:**
- At 1000 ticks/sec sustained: 600K/1000 = 10 minutes before spill starts
- After spill starts: bounded only by disk space (EBS 100GB = ~20 hours at 5KB/tick)
- **So: QDB can be down for 20 hours and we still lose zero ticks.**

**Do NOT:**
- `docker kill` tv-questdb (use `restart` — kill skips the WAL flush)
- Delete spill files manually — the app replays them on reconnect
- Restart tickvault just because QDB is down — the writer handles it itself

---

## §2. WebSocket pool halted

**Symptom:** Telegram alert "S4-T1a FATAL: pool watchdog fired Halt
verdict". The app exited with status 2. systemd should auto-restart
within 3 seconds per `deploy/systemd/tickvault.service:Restart=always`.

**Root causes:**
1. All 5 Dhan WebSocket connections failed repeatedly for >300 seconds
2. Token expired AND token renewal failed (see §6)
3. Dhan side outage (check Dhan status page)

**Immediate action:**

```bash
# 1. Verify systemd restarted the app
aws ssm start-session --region ap-south-1 --target $INSTANCE_ID
systemctl status tickvault
# Expected: active (running)

# 2. Check how many times it's been restarted today
journalctl -u tickvault --since today | grep 'pool watchdog fired Halt' | wc -l

# 3. If restart loop > 3 in 10 minutes, halt the app to prevent thrashing
systemctl stop tickvault

# 4. Investigate
journalctl -u tickvault --since '15 minutes ago' | grep -E 'ERROR|FATAL' | tail -50
```

**Common causes + fixes:**
- **Token expired:** see §6
- **Dhan outage:** check https://dhanhq.co/status and wait
- **Static IP rejected:** Dhan's static IP cooldown hit (7 days). Verify
  `ordersAllowed: true` via `GET /v2/ip/getIP`

---

## §3. Order update WS down

**Symptom:** Telegram alert `tv-order-update-ws-down` (> 60 seconds
disconnected). **Fills may be silently lost.**

This is more serious than §2 because fill confirmations arrive on this
WS. If we miss a fill, the OMS state machine gets out of sync with Dhan.

**Immediate action:**

```bash
# 1. Check OMS position drift
aws ssm start-session --region ap-south-1 --target $INSTANCE_ID
# In the session:
curl http://localhost:3001/api/stats | jq '.oms_positions_count'

# 2. Reconcile with Dhan
curl -H "access-token: $TOKEN" https://api.dhan.co/v2/orders | jq '.[] | select(.orderStatus != "TRADED")'

# 3. If drift detected, halt trading immediately
curl -X POST "https://api.dhan.co/v2/killswitch?killSwitchStatus=ACTIVATE" \
  -H "access-token: $TOKEN"
```

---

## §4. App crash, no auto-restart

**Symptom:** `systemctl status tickvault` shows `inactive (dead)` and
Restart counter is high or hit the burst limit.

```bash
# 1. Clear the restart burst counter
systemctl reset-failed tickvault

# 2. Start manually
systemctl start tickvault

# 3. Watch the first 60 seconds
journalctl -u tickvault -f
```

If it immediately crashes again, the issue is a code or config bug —
see §11 for rollback.

---

## §5. Instance unreachable

**Symptom:** Both `aws ssm start-session` and SSH time out. Grafana also
unreachable (port-forward fails).

**Likely causes:** kernel panic, network layer issue, EC2 hardware fault.

```bash
# 1. Check instance status from AWS API
aws ec2 describe-instance-status --region ap-south-1 --instance-ids $INSTANCE_ID

# 2. If System Check failed, force-reboot via the EC2 console:
aws ec2 reboot-instances --region ap-south-1 --instance-ids $INSTANCE_ID

# 3. If still unreachable after 5 minutes, stop + start (moves to new host):
aws ec2 stop-instances --region ap-south-1 --instance-ids $INSTANCE_ID
aws ec2 wait instance-stopped --region ap-south-1 --instance-ids $INSTANCE_ID
aws ec2 start-instances --region ap-south-1 --instance-ids $INSTANCE_ID
```

**NOTE:** `stop-instances` does NOT release the Elastic IP (it stays
bound while stopped for EIPs, though you pay Rs 0.005/hr when the
instance is stopped). After `start-instances` the same IP returns —
Dhan's static-IP whitelist stays valid.

---

## §6. Token expired

**Symptom:** Dhan 807 / DH-901 errors in app logs.

```bash
# 1. Generate a fresh token manually (needs TOTP)
TOTP=$(oathtool --totp -b "$DHAN_TOTP_SECRET")
curl -X POST "https://auth.dhan.co/app/generateAccessToken?dhanClientId=$CID&pin=$PIN&totp=$TOTP"

# 2. Update SSM parameter
aws ssm put-parameter \
  --region ap-south-1 \
  --name /tickvault/prod/dhan/access-token \
  --type SecureString \
  --value "$NEW_TOKEN" --overwrite

# 3. Signal the app to reload
systemctl restart tickvault
```

---

## §7. Rate-limited by Dhan

**Symptom:** 805 or 904 errors from Dhan. App logs show `DH-904 rate limit`.

**Automatic handling:** the app already backs off exponentially (10s,
20s, 40s, 80s) and the backfill worker has a 5/sec governor rate limiter
(S5-A3). If you're seeing 805 despite that, something is wrong — check:

```bash
journalctl -u tickvault --since '10 minutes ago' | grep -E 'DH-904|805'
```

If 805 persists:
1. **Stop all requests for 60 seconds** (Dhan's mandate after 805)
2. **Halt the app**: `systemctl stop tickvault`
3. **Wait 60 seconds**: `sleep 60`
4. **Audit connection count**: `GET /v2/ip/getIP` should show 0 active connections
5. **Restart**: `systemctl start tickvault`

---

## §8. Spill disk full

**Symptom:** Telegram alert `tv-spill-disk-low-critical` (< 100MB free).

```bash
# 1. Check disk usage
df -h /opt/tickvault/data/spill

# 2. Find oldest spill files
ls -lht /opt/tickvault/data/spill/ticks-*.bin | tail -10

# 3. Archive oldest to S3 cold bucket
for f in /opt/tickvault/data/spill/ticks-*.bin; do
  age_days=$(( ( $(date +%s) - $(stat -c%Y "$f") ) / 86400 ))
  if [ "$age_days" -gt 7 ]; then
    aws s3 cp "$f" s3://tv-prod-cold/spill-archive/ --region ap-south-1
    rm "$f"
  fi
done
```

**Long-term fix:** add a systemd timer that runs this archival daily.

---

## §9. Unexpected running instance

**Symptom:** Billing alert at 50% mid-month. `aws ec2 describe-instances`
shows the instance running at 02:00 IST (outside the schedule).

```bash
# 1. Force-stop
aws ec2 stop-instances --region ap-south-1 --instance-ids $INSTANCE_ID

# 2. Check which EventBridge rule failed
aws events describe-rule --region ap-south-1 --name tv-prod-weekday-stop
# Expected: ScheduleExpression = cron(30 11 ? * MON-FRI *)

# 3. Review CloudTrail for who started it
aws cloudtrail lookup-events \
  --region ap-south-1 \
  --lookup-attributes AttributeKey=EventName,AttributeValue=StartInstances \
  --start-time $(date -d '24 hours ago' -u +%Y-%m-%dT%H:%M:%SZ)
```

---

## §10. Dependabot CVE surge

**Symptom:** Repo header shows 10+ vulnerabilities.

1. Run the dep freshness scanner:
   ```bash
   bash .claude/hooks/dep-freshness-scanner.sh
   ```
2. If PATCH drifts: auto-bump in a PR (Option A nightly action does this).
3. If MINOR drifts: test manually, then commit.
4. If transitive CVE pulled by AWS SDK / third-party: add `[patch.crates-io]`
   override temporarily, open an upstream issue.

---

## §11. Emergency rollback

**Symptom:** Just deployed, the new binary is broken, need the previous
version back immediately.

```bash
# 1. SSH / SSM to the instance
aws ssm start-session --region ap-south-1 --target $INSTANCE_ID
sudo -i

# 2. Swap the backup binary back
cd /opt/dlt
if [ -f bin/tickvault.backup ]; then
  mv bin/tickvault.backup bin/tickvault
  systemctl restart tickvault
else
  # No backup — download the previous release from S3
  aws s3 cp s3://tv-prod-cold/deploys/<previous-sha>/tickvault bin/tickvault
  chmod +x bin/tickvault
  systemctl restart tickvault
fi
```

The GitHub Actions deploy workflow creates `.backup` automatically before
each swap — see `.github/workflows/deploy-aws.yml` step 3.

---

## General principles

1. **Telegram alerts fire before you notice** — trust the alert, not the
   absence of one. If you're seeing "QDB down" but tests show green,
   investigate the false-positive path.
2. **Zero tick loss is guaranteed for the tick path** — even if you
   panic and kill the app with SIGKILL, spill files persist and replay
   on restart. Check `spill_*.bin` age before assuming data is lost.
3. **Static IP has a 7-day cooldown on modify.** Never destroy the
   Terraform EIP unless you have a 7-day plan.
4. **The kill switch is your friend.** `POST /v2/killswitch?killSwitchStatus=ACTIVATE`
   halts ALL trading for the day, even on successful reconnects. Use it
   any time you're unsure about OMS state.
5. **`systemctl stop tickvault` is safe.** The graceful shutdown handler
   sends `RequestCode 12` to Dhan (S4-T1b), flushes the ring buffer,
   and exits cleanly. Use it over `kill` or `pkill`.
