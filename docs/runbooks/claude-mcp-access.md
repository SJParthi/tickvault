# Claude MCP Access Runbook

> **Purpose:** make Claude Code able to read tickvault logs, Prometheus,
> Alertmanager, and QuestDB WITHOUT the operator pasting screenshots or
> query output. The user should NEVER have to forward data manually.

## The three deployment modes

| Mode | Where Claude Code runs | Where tickvault runs | What to configure |
|---|---|---|---|
| A | Your Mac | Your Mac | Nothing — defaults work |
| B | Your Mac | AWS EC2 | Set 4 env vars pointing at AWS |
| C | Sandbox / other host | Your Mac (or AWS) | Expose endpoints via tunnel, set 4 env vars |

The MCP server `tickvault-logs` reads these seven inputs:

| Input | Default | Env var to override |
|---|---|---|
| errors.jsonl directory | `<repo>/data/logs` | `TICKVAULT_LOGS_DIR` |
| Prometheus | `http://127.0.0.1:9090` | `TICKVAULT_PROMETHEUS_URL` |
| Alertmanager | `http://127.0.0.1:9093` | `TICKVAULT_ALERTMANAGER_URL` |
| QuestDB | `http://127.0.0.1:9000` | `TICKVAULT_QUESTDB_URL` |
| tickvault API | `http://127.0.0.1:3001` | `TICKVAULT_API_URL` |
| Grafana | `http://127.0.0.1:3000` | `TICKVAULT_GRAFANA_URL` |
| Grafana API token | _none_ | `TICKVAULT_GRAFANA_API_TOKEN` |

`.mcp.json` passes all seven env vars through to the server, so
setting them in your shell (or in your `.envrc` / `.zshenv`) is
enough — no need to edit `.mcp.json` per environment.

## Full MCP tool surface (16 tools)

Every surface of the tickvault stack is reachable through one MCP
server — no screenshots, no copy-paste.

| Tool | What it reads |
|---|---|
| `summary_snapshot` | `errors.summary.md` |
| `tail_errors` | `errors.jsonl.*` |
| `list_novel_signatures` | first-seen signatures over a time window |
| `signature_history` | all events matching a signature hash |
| `triage_log_tail` | `data/logs/auto-fix.log` |
| `find_runbook_for_code` | `.claude/rules/**` runbook lookup |
| `prometheus_query` | any PromQL |
| `questdb_sql` | any SQL against all 20 QuestDB tables |
| `list_active_alerts` | firing Alertmanager alerts |
| `run_doctor` | `make doctor` parsed output |
| `grep_codebase` | ripgrep over workspace |
| `git_recent_log` | last N commits |
| `tickvault_api` | **any tickvault REST API endpoint** |
| `grafana_query` | **any Grafana HTTP API endpoint** |
| `docker_status` | **`docker compose ps --format json`** |
| `app_log_tail` | **full INFO/DEBUG app log** |

## Mode A — Mac-local (simplest, recommended)

```bash
# On the Mac, in the tickvault repo root:
make run                 # starts Docker + tickvault
claude                   # launches Claude Code on the Mac
```

Nothing to configure. The MCP server reads `./data/logs/` and
`localhost:9000/9090/9093` directly.

Verify:

```bash
bash scripts/mcp-doctor.sh
# Expect: all 4 checks PASS.
```

## Mode B — Claude on Mac, tickvault on AWS EC2

Add to `~/.zshenv` (or your shell rc):

```bash
export TICKVAULT_PROMETHEUS_URL="https://<your-ec2-public-dns>:9090"
export TICKVAULT_ALERTMANAGER_URL="https://<your-ec2-public-dns>:9093"
export TICKVAULT_QUESTDB_URL="https://<your-ec2-public-dns>:9000"
# Keep logs local via SSH rsync cron OR point at a shared EFS mount:
export TICKVAULT_LOGS_DIR="$HOME/tickvault-logs-sync"
```

Set up a rsync cron on the Mac (every 30s is enough because the summary
is refreshed every 60s):

```cron
* * * * * rsync -az --delete ec2-user@<ec2>:/home/ec2-user/tickvault/data/logs/ ~/tickvault-logs-sync/
```

Restart Claude Code so it picks up the new env. Verify with
`scripts/mcp-doctor.sh`.

## Mode C — Claude in a sandbox (gVisor / container / web)

This is the case where `127.0.0.1:9000` inside the sandbox is NOT your
Mac. There are two supported sub-modes.

### C.1 — Tailscale

1. Install Tailscale on the Mac AND on the sandbox host.
2. Join both to the same tailnet.
3. On the Mac, find your Tailscale IP (`tailscale ip -4`).
4. In the sandbox shell:
   ```bash
   export TICKVAULT_PROMETHEUS_URL="http://<tailscale-ip>:9090"
   export TICKVAULT_ALERTMANAGER_URL="http://<tailscale-ip>:9093"
   export TICKVAULT_QUESTDB_URL="http://<tailscale-ip>:9000"
   ```
5. Mount the Mac's `data/logs/` into the sandbox (e.g. `tailscale serve
   --bg 8080` + filesystem sync, or Syncthing, or a scp cron).
6. Point `TICKVAULT_LOGS_DIR` at the mount point.

### C.2 — SSH reverse tunnel

Single command from the Mac:

```bash
ssh -N -R 9090:localhost:9090 \
       -R 9093:localhost:9093 \
       -R 9000:localhost:9000 \
       <sandbox-user>@<sandbox-host>
```

Then inside the sandbox, the defaults `http://127.0.0.1:9090` etc. work
unchanged. Still need to sync `data/logs/` separately (see C.1 step 5).

## Mode C limitations (honest)

- The log files must physically exist on the host Claude runs on, or
  appear there via a file-sync mechanism. The MCP server reads files
  with Python `open()` — there is no "fetch over HTTP" mode.
- If you don't want to run a sync job, Mode A is the right answer.

## Verification — after ANY mode change

```bash
bash scripts/mcp-doctor.sh
```

All 4 checks must pass. If any fails, the specific check name tells
you exactly which endpoint is unreachable.

## When Claude cannot reach a specific service

Claude will tell you honestly which service is unreachable (via
`mcp-doctor.sh` output or via the MCP tool's error response). Claude
will NOT fabricate status for unreachable services.

## Tests that guard this

- `crates/common/tests/tickvault_logs_mcp_guard.rs` — enforces the MCP
  server tool set + self-test.
- `scripts/validate-automation.sh` check `tickvault-logs MCP self-test
  passes` — runs every time you `make validate-automation`.
