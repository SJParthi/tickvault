# Runbook — cloud-init / user-data Failure (Case D)

> **Severity:** Critical. EC2 is running but app never starts.
> **Detection latency:** ≤T+240s (60s after the T+180s `tv_boot_complete` alarm deadline).
> **Companion:** `docs/architecture/aws-daily-lifecycle.md` §3 Case D.

## Symptom

- AWS console: instance is `running`, status checks 2/2.
- SSH works.
- But: `systemctl status tickvault` shows `inactive` or `failed`.
- CloudWatch alarm `tv-prod-boot-not-complete` firing.
- SNS fan-out delivered alert.

## Likely causes

| # | Cause | Where to look |
|---|---|---|
| 1 | git clone failed (network, missing SSH key) | `/var/log/cloud-init-output.log` |
| 2 | docker install failed | same |
| 3 | systemd unit not enabled | `systemctl is-enabled tickvault` |
| 4 | user-data script syntax error | `/var/log/cloud-init.log` |
| 5 | SSM parameter store fetch failed (token, secrets) | `journalctl | grep bootstrap` |

## Immediate actions

```bash
# 1. SSM session
INSTANCE_ID=...
aws ssm start-session --region ap-south-1 --target $INSTANCE_ID

# 2. Inspect cloud-init logs (operator-charter §G — read these FIRST)
sudo tail -200 /var/log/cloud-init-output.log
sudo tail -200 /var/log/cloud-init.log

# 3. Inspect systemd unit
sudo systemctl status tickvault
sudo journalctl -u tickvault -n 200

# 4. If unit is loaded but not started — start manually
sudo systemctl start tickvault
sudo journalctl -u tickvault -f
```

## If user-data ran once and failed irrecoverably

cloud-init only runs user-data on FIRST boot. Subsequent boots skip it. So fixes via re-running user-data require:

```bash
# Re-run cloud-init on next reboot (destructive — wipes some state)
sudo cloud-init clean --logs
sudo reboot
```

OR rebuild the AMI (preferred) — see Item AWS-9 in plan.

## Acceptance criteria

- App emits `BootReadyConfirmation` Telegram.
- `mcp__tickvault-logs__run_doctor` returns green.

## Post-incident

- If user-data has a regression → fix in `deploy/aws/terraform/user-data.sh.tftpl` + PR.
- If first-boot environmental flake → document; not actionable in code.
