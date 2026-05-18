# Runbook — Tickvault Exit Loop (Case F)

> **Severity:** Critical.
> **Detection latency:** ≤30s after 3rd systemd restart via CW Logs filter.
> **Companion:** `docs/architecture/aws-daily-lifecycle.md` §3 Case F.

## Symptom

- `systemctl status tickvault` shows `failed (Result: exit-code)`.
- `journalctl -u tickvault` shows multiple boot attempts within ~10s.
- After 3 retries within `StartLimitIntervalSec`, systemd halts the unit.

## Likely causes

| # | Cause | How to check |
|---|---|---|
| 1 | Bad config TOML | `journalctl -u tickvault | grep -i 'parse\|config'` |
| 2 | Corrupt binary (interrupted deploy) | `file /opt/tickvault/bin/tickvault`; `sha256sum` vs CI artifact |
| 3 | Missing SSM secret | `journalctl -u tickvault | grep -i 'ssm\|secret'` |
| 4 | glibc / shared-lib mismatch | `ldd /opt/tickvault/bin/tickvault` |
| 5 | Port already in use (3001 collision) | `ss -tlnp | grep 3001` |

## Immediate actions

```bash
INSTANCE_ID=...
aws ssm start-session --region ap-south-1 --target $INSTANCE_ID

# 1. Get the actual error
sudo journalctl -u tickvault -n 100 --no-pager

# 2. Try a manual run in foreground (bypasses systemd)
sudo -u tickvault /opt/tickvault/bin/tickvault --config /etc/tickvault/base.toml

# 3. If config issue — diff against git
diff -u /etc/tickvault/base.toml \
  /opt/tickvault/repo/config/base.toml

# 4. If binary issue — rollback to last known-good
sudo /opt/tickvault/scripts/rollback.sh  # if such a script exists
# OR redeploy via GitHub Actions
```

## Emergency stop (if it keeps eating CPU on retry)

```bash
sudo systemctl stop tickvault
sudo systemctl mask tickvault   # prevents auto-restart
# fix the issue, then:
sudo systemctl unmask tickvault
sudo systemctl start tickvault
```

## Acceptance criteria

- App stays up for ≥10 min without exit.
- `BootReadyConfirmation` Telegram arrives.

## Post-incident

- If config regression → file fix + ratchet test that validates the config schema at CI time.
- If binary issue → file CI artifact integrity check.
- See also `aws-disaster-recovery.md` §11 (Emergency rollback).
