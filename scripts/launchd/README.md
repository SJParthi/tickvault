# Mac auto-deploy via launchd

Polls GitHub for green CI on the configured branch and runs `git pull
&& cargo build --release && make restart` automatically.

Designed as a stop-gap for the period **before** the AWS instance is
provisioned. On AWS this is replaced by a proper GitHub Actions deploy
job — the script is then no longer needed.

## Files

| File | Purpose |
|---|---|
| `auto-deploy.sh` | Polling loop; safe-guards; performs the deploy |
| `com.tickvault.autodeploy.plist` | launchd manifest — registers the script as a user agent |

## Install (one-time, on the Mac)

1. Edit `com.tickvault.autodeploy.plist` and replace `YOUR_USERNAME`
   with your macOS username (3 occurrences).

2. Copy the plist into your user LaunchAgents directory and load it:

   ```bash
   cp scripts/launchd/com.tickvault.autodeploy.plist \
      ~/Library/LaunchAgents/com.tickvault.autodeploy.plist
   launchctl load ~/Library/LaunchAgents/com.tickvault.autodeploy.plist
   ```

3. Verify it's running:

   ```bash
   launchctl list | grep tickvault
   tail -f data/logs/auto-deploy.log
   ```

## Uninstall

```bash
launchctl unload ~/Library/LaunchAgents/com.tickvault.autodeploy.plist
rm ~/Library/LaunchAgents/com.tickvault.autodeploy.plist
```

## Configuration

Set via environment variables in the plist's `EnvironmentVariables`
dict (or just edit defaults at the top of `auto-deploy.sh`):

| Var | Default | Meaning |
|---|---|---|
| `TV_DEPLOY_BRANCH` | `main` | Branch to track |
| `TV_DEPLOY_POLL_SECS` | `60` | Poll interval |
| `TV_REPO_DIR` | `$HOME/IdeaProjects/tickvault` | Repo location |

## Safety guards (built in)

| Guard | What it prevents |
|---|---|
| Skip during 09:15–15:30 IST market hours | Build never interrupts live trading |
| Skip on weekends (IST `dow > 5`) | No churn outside trading days |
| Refuse if working tree dirty | Never overwrites local edits |
| Refuse if HEAD is on a different branch | Never silently switches branches |
| `git pull --ff-only` | Never auto-merges |
| Build BEFORE stop | A bad commit leaves the running app alive |
| `last-deployed-sha` cache | Same commit never deployed twice |
| Only deploys on `state == "success"` from GitHub combined-status API | Never deploys a red build |

## Logs

- `data/logs/auto-deploy.log` — script's own structured log
- `data/logs/auto-deploy.launchd.out` — stdout from launchd
- `data/logs/auto-deploy.launchd.err` — stderr from launchd

## When this becomes obsolete

Once the AWS instance is provisioned (`scripts/setup-aws.sh`), set up a
GitHub Actions `deploy` job that SSHes into the instance on green CI for
`main`. At that point: uninstall this launchd agent, delete this folder.
