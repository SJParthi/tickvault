# Claude MCP Access Runbook — Universal, Branch-Independent

> **Purpose:** every Claude Code session, every claude.ai web sandbox,
> every claude cowork session — on any branch, any machine — can read
> tickvault's full runtime state (metrics, logs, alerts, DB) WITHOUT
> the operator pasting screenshots or query output.
>
> **Design principle:** the config lives IN THE REPO, not in
> per-session env vars. One-time tunnel setup per environment, forever
> after every clone has working endpoints.

This is **Milestone 1** of the Autonomous Operations Plan
(`.claude/plans/autonomous-operations-100pct.md`). It is the prerequisite
for Layers 2-5 (Diagnose / Decide / Act / Verify+Rollback) — without
observability access from any Claude session, nothing else works.

## TL;DR — what you run, once per environment

| You are | Tickvault is | One-time command | Forever after |
|---|---|---|---|
| On your Mac (Claude Code CLI) | Running locally on the same Mac | Nothing | MCP works against `127.0.0.1` |
| On your Mac, but need remote Claude access | Running locally on the Mac | `bash scripts/tv-tunnel/install-mac.sh` | Remote Claude reads live via Tailscale Funnel |
| On your Mac | Running on AWS EC2 | `bash scripts/tv-tunnel/install-aws.sh` (on EC2) | Mac Claude + remote Claude both read live via Funnel |
| In a claude.ai sandbox / claude cowork | Running on Mac or AWS | Same as above (installed on host) | Sandbox Claude reads live via committed repo URLs |

After install-mac.sh / install-aws.sh runs:
- Tailscale Funnel auto-starts on reboot (launchd on Mac, systemd on AWS)
- Ports 9090/9093/9000/3000/3001 are exposed via stable `.ts.net` URLs
- The URLs never change
- Write them into `config/claude-mcp-endpoints.toml`, commit, push
- Every future clone of the repo has working MCP — zero per-session setup

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│ Claude session (anywhere: Mac, web, sandbox, cowork)        │
│ ┌───────────────────────────────────────────────────────┐   │
│ │ tickvault-logs MCP server (Python, local to session)  │   │
│ │  ↓ reads config/claude-mcp-endpoints.toml             │   │
│ │  ↓ selects active profile (mac-dev | aws-prod | local)│   │
│ └────────┬──────────────────────────────────────────────┘   │
└──────────│──────────────────────────────────────────────────┘
           │ HTTPS
           ↓
┌──────────────────────────────────────────────────────────────┐
│ Tailscale Funnel (stable public URL on .ts.net)              │
│ mac-hostname.your-tailnet.ts.net:9090  → Prometheus         │
│ mac-hostname.your-tailnet.ts.net:9093  → Alertmanager       │
│ mac-hostname.your-tailnet.ts.net:9000  → QuestDB HTTP       │
│ mac-hostname.your-tailnet.ts.net:3000  → Grafana            │
│ mac-hostname.your-tailnet.ts.net:3001  → tickvault API      │
│                                         ├── /api/debug/logs/summary         │
│                                         └── /api/debug/logs/jsonl/latest    │
└──────────────────────────────────────────────────────────────┘
           ↓ (Mac launchd | AWS systemd restarts on crash/reboot)
┌──────────────────────────────────────────────────────────────┐
│ Your tickvault Docker stack (localhost)                      │
└──────────────────────────────────────────────────────────────┘
```

Key properties:
- **Stable URLs**: `.ts.net` hostnames never change across Tailscale restarts.
- **Branch-independent**: endpoints in repo, so every branch checkout works.
- **Universal**: works identically on Mac dev, AWS prod, claude.ai sandbox.
- **Logs over HTTP, not filesystem**: `/api/debug/logs/*` eliminates rsync.
- **Auto-heal**: launchd/systemd restart the funnel on crash or reboot.

## Seven inputs the MCP server reads

Read from `config/claude-mcp-endpoints.toml` under the active profile.
Env vars (when set) override the file. The file overrides hardcoded
localhost defaults.

| Input | Env override | Profile key |
|---|---|---|
| Prometheus URL | `TICKVAULT_PROMETHEUS_URL` | `prometheus_url` |
| Alertmanager URL | `TICKVAULT_ALERTMANAGER_URL` | `alertmanager_url` |
| QuestDB HTTP URL | `TICKVAULT_QUESTDB_URL` | `questdb_url` |
| Grafana URL | `TICKVAULT_GRAFANA_URL` | `grafana_url` |
| tickvault API URL | `TICKVAULT_API_URL` | `tickvault_api_url` |
| Logs source | `TICKVAULT_LOGS_SOURCE` (`http` or `local`) | `logs_source` |
| Logs directory (when `local`) | `TICKVAULT_LOGS_DIR` | `logs_dir_local` |

Profile selection order:
1. `TICKVAULT_MCP_PROFILE` env var (single-session override)
2. `active = "<name>"` at the top of `claude-mcp-endpoints.toml`
3. Fallback to `"local"` (everything on `127.0.0.1`)

## Full MCP tool surface (16 tools)

Every surface of the tickvault stack is reachable through one MCP server
— no screenshots, no copy-paste, no manual tailing.

| Tool | What it reads |
|---|---|
| `summary_snapshot` | `errors.summary.md` (local OR via `/api/debug/logs/summary`) |
| `tail_errors` | `errors.jsonl.*` (local OR via `/api/debug/logs/jsonl/latest`) |
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
| `tickvault_api` | any tickvault REST API endpoint |
| `grafana_query` | any Grafana HTTP API endpoint |
| `docker_status` | `docker compose ps --format json` |
| `app_log_tail` | full INFO/DEBUG app log |

## Mode A — Claude on Mac, tickvault on Mac (simplest)

```bash
# On the Mac, in the tickvault repo root
make run                 # starts Docker + tickvault
claude                   # launches Claude Code
```

Active profile: `local` (or whichever is set in the committed config).
No tunnel, no env vars. Logs read from `./data/logs/` directly.

Verify:
```bash
bash scripts/mcp-doctor.sh
```

## Mode B — Claude on Mac, tickvault on AWS EC2

1. On AWS: `bash scripts/tv-tunnel/install-aws.sh` (see "One-time AWS setup" below).
2. On Mac: edit `config/claude-mcp-endpoints.toml`, set `active = "aws-prod"`, commit, push.
3. On Mac: `claude` — it now queries the AWS stack live.

## Mode C — Claude in a sandbox / claude.ai / cowork (universal)

This replaces the old (broken) Mode C that assumed SSH reverse tunnels —
sandboxes can't accept inbound connections.

### C.1 — One-time Mac setup

```bash
bash scripts/tv-tunnel/install-mac.sh
```

The script:
- Verifies macOS, installs Tailscale via brew if missing
- Launches `tailscale up` (login flow in browser) if not already logged in
- Verifies Funnel feature is enabled on your tailnet (if not, points you to the admin ACL URL)
- Installs `~/Library/LaunchAgents/com.tickvault.tunnel.plist`
- Loads the launchd service, which calls `tailscale funnel --bg 9090 9093 9000 3000 3001`
- Probes for the funnel to come up within 30s
- Prints your stable URLs — paste them into `config/claude-mcp-endpoints.toml`

### C.2 — One-time AWS setup (when EC2 is provisioned)

```bash
# On the EC2 instance
bash scripts/tv-tunnel/install-aws.sh
```

Same flow, but installs `/etc/systemd/system/tickvault-tunnel.service`
and uses `systemctl enable --now`.

### C.3 — One-time repo update

After one or both installs finish, run the doctor in emit-config mode:

```bash
bash scripts/tv-tunnel/doctor.sh --emit-config
```

This prints a TOML block like:
```toml
[profiles.mac-dev]
prometheus_url    = "https://your-mac.tailnet-name.ts.net:9090"
alertmanager_url  = "https://your-mac.tailnet-name.ts.net:9093"
questdb_url       = "https://your-mac.tailnet-name.ts.net:9000"
grafana_url       = "https://your-mac.tailnet-name.ts.net:3000"
tickvault_api_url = "https://your-mac.tailnet-name.ts.net:3001"
logs_source       = "http"
logs_dir_local    = "./data/logs"
```

Paste it into `config/claude-mcp-endpoints.toml` (replacing the
placeholder Mac or AWS profile), set `active = "mac-dev"` or
`"aws-prod"`, commit, push.

Any Claude session on any branch from that point on reads live data.

## Verification — after ANY mode change

```bash
bash scripts/mcp-doctor.sh            # MCP-level probes (4 checks)
bash scripts/tv-tunnel/doctor.sh       # Tunnel-level probes (7 checks)
```

Both should exit 0.

## When Claude cannot reach a specific service

Claude reports honestly which endpoint is unreachable (via the
`mcp-doctor.sh` or `tv-tunnel/doctor.sh` output, or directly via the
MCP tool's error response). Claude never fabricates status for
unreachable services.

## Tests that guard this

- `crates/common/tests/tickvault_logs_mcp_guard.rs` — enforces MCP
  server tool set + self-test.
- `scripts/validate-automation.sh` check `tickvault-logs MCP self-test
  passes` — runs every `make validate-automation`.
- `crates/api/src/handlers/debug.rs` unit tests — 11 tests covering
  route handlers, env-var resolution, missing-file handling.
- `crates/common/tests/claude_mcp_endpoints_config_guard.rs` — enforces
  the committed config file contains all required profile keys and is
  parseable by the MCP server.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `tailscale funnel status` says "inactive" | `sudo launchctl load -w ~/Library/LaunchAgents/com.tickvault.tunnel.plist` (Mac) / `sudo systemctl start tickvault-tunnel` (AWS) |
| Funnel status OK but curl returns 000 | Check tickvault Docker is up: `make docker-status` |
| MCP always hits localhost | Check `active = "mac-dev"` or `aws-prod"` in `config/claude-mcp-endpoints.toml` — default is `local` |
| Need to point session at different env | `TICKVAULT_MCP_PROFILE=aws-prod claude` — overrides committed config for that session only |
| `/api/debug/logs/summary` returns 404 | App hasn't written `errors.summary.md` yet (60s refresh cadence). Check `ls data/logs/errors.summary.md` on the host. |

## Security notes

- Tailscale Funnel exposes to the public internet via HTTPS. Access is
  gated by the Funnel feature's per-node ACL.
- The debug log endpoints return ERROR signatures and metric values
  only — never auth tokens, order payloads, or user PII.
- tickvault's protected endpoints (order place/cancel) remain behind
  `TV_API_TOKEN` bearer auth. The debug endpoints are read-only and
  intentionally outside that wall so observability is always available.
- The Dhan access token, AWS credentials, and private keys are never
  logged to `errors.jsonl` (enforced by
  `.claude/hooks/banned-pattern-scanner.sh`).
