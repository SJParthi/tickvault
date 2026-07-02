# Runbook — Tickvault Exit Loop (Case F)

> **Severity:** Critical.
> **Detection latency:** ≤30s after the unit flips to `failed` via CW Logs filter.
> **Companion:** `docs/architecture/aws-daily-lifecycle.md` §3 Case F.

## Symptom

- `systemctl status tickvault` shows `failed (Result: exit-code)`.
- `journalctl -u tickvault` shows multiple boot attempts within ~10s.
- After **8 restarts in 10 min** (`StartLimitBurst=8`,
  `StartLimitIntervalSec=600` in `deploy/systemd/tickvault.service`), systemd
  transitions the unit to `failed` and STOPS restarting it.
- A plain `systemctl restart`/`start` on the `failed` unit returns
  **"Start request repeated too quickly"** and is a no-op — the start-limit
  counter must be cleared with `systemctl reset-failed` FIRST.

## Auto-recovery (already wired)

`scripts/aws-autopilot.sh` (every 15 min) now un-sticks a `failed` crash-loop on
its own: for an enabled-but-inactive unit it runs
`systemctl reset-failed tickvault` (clears the start-limit counter) then
`systemctl start tickvault`. So a StartLimit-`failed` box recovers without a
human. Autopilot still pages if the app is inactive again after the restart (a
genuine repeating crash it cannot mask). This runbook covers the manual path if
autopilot is unavailable or the crash keeps repeating.

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
sudo systemctl reset-failed tickvault   # clear the StartLimit `failed` state — a
                                        # bare start on a `failed` unit is a no-op
sudo systemctl start tickvault
```

## Acceptance criteria

- App stays up for ≥10 min without exit.
- `BootReadyConfirmation` Telegram arrives.

## Post-incident

- If config regression → file fix + ratchet test that validates the config schema at CI time.
- If binary issue → file CI artifact integrity check.
- See also `aws-disaster-recovery.md` §11 (Emergency rollback).
