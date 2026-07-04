# Claude MCP Access Runbook — Universal, Branch-Independent

> **CloudWatch-only migration note (#O1–#O5, 2026-05-19/05-30):** the
> Grafana, Alertmanager, and Prometheus containers (and Valkey) were
> removed. Observability in prod is AWS CloudWatch (metrics + logs +
> alarms + dashboards); ad-hoc queries use the QuestDB web console. The
> app still exposes Prometheus-wire-format metrics on its `/metrics`
> endpoint (scraped by the CloudWatch agent), but the `prometheus_query`
> and `list_active_alerts` MCP tools were RETIRED in #O5 (2026-05-30) —
> they pointed at the now-removed Prometheus (:9090) and Alertmanager
> (:9093) containers. Use `questdb_sql` / CloudWatch metrics for live
> counter values and CloudWatch alarms / `run_doctor` for firing
> alerts. The retired Grafana/Alertmanager funnel rows, the
> `grafana_url`/`alertmanager_url`/`prometheus_url` inputs, and the
> `grafana_query` tool below have been removed accordingly.

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
- Port 3001 (tickvault API, bearer-auth) is exposed via a stable `.ts.net` URL
  — security trim 2026-07-04: ports 9090/9093/3000 are retired
  (CloudWatch-only migration) and QuestDB 9000 is intentionally NO LONGER
  funnelled (auth-less raw SQL — local access only)
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
│ (QuestDB :9000 no longer funnelled — auth-less SQL, local)   │
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
- **Log endpoints exist over HTTP** (`/api/debug/logs/*`, bearer-gated since
  2026-07-04); the MCP server's log tools themselves read the LOCAL
  filesystem (`logs_source = "local"` — no HTTP log fetch is implemented).
- **Auto-heal**: launchd/systemd restart the funnel on crash or reboot.

## Five inputs the MCP server reads

Read from `config/claude-mcp-endpoints.toml` under the active profile.
Env vars (when set) override the file. The file overrides hardcoded
localhost defaults.

| Input | Env override | Profile key |
|---|---|---|
| Prometheus URL (app exporter / CloudWatch source) | `TICKVAULT_PROMETHEUS_URL` | `prometheus_url` |
| QuestDB HTTP URL | `TICKVAULT_QUESTDB_URL` | `questdb_url` |
| tickvault API URL | `TICKVAULT_API_URL` | `tickvault_api_url` |
| Logs source | `TICKVAULT_LOGS_SOURCE` (use `local` — no HTTP log fetch is implemented) | `logs_source` |
| Logs directory (when `local`) | `TICKVAULT_LOGS_DIR` | `logs_dir_local` |

Profile selection order:
1. `TICKVAULT_MCP_PROFILE` env var (single-session override)
2. `active = "<name>"` at the top of `claude-mcp-endpoints.toml`
3. Fallback to `"local"` (everything on `127.0.0.1`)

## Full MCP tool surface (15 tools)

Every surface of the tickvault stack is reachable through one MCP server
— no screenshots, no copy-paste, no manual tailing.

| Tool | What it reads |
|---|---|
| `summary_snapshot` | `errors.summary.md` (local filesystem read) |
| `tail_errors` | `errors.jsonl.*` (local filesystem read) |
| `list_novel_signatures` | first-seen signatures over a time window |
| `signature_history` | all events matching a signature hash |
| `triage_log_tail` | `data/logs/auto-fix.log` |
| `find_runbook_for_code` | `.claude/rules/**` runbook lookup |
| `questdb_sql` | any SQL against all 20 QuestDB tables |
| `run_doctor` | `make doctor` parsed output |
| `grep_codebase` | ripgrep over workspace |
| `git_recent_log` | last N commits |
| `tickvault_api` | any tickvault REST API endpoint |
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
- Loads the launchd service, which calls `tailscale funnel --bg 3001`
  (security trim 2026-07-04: only the bearer-auth tickvault API is funnelled;
  QuestDB 9000 — auth-less raw SQL — is intentionally NOT funnelled)
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
# QuestDB is no longer funnelled (auth-less raw SQL) — local-only URL.
questdb_url       = "http://127.0.0.1:9000"
tickvault_api_url = "https://your-mac.tailnet-name.ts.net:3001"
# log tools read the local filesystem — no HTTP log fetch is implemented
logs_source       = "local"
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
- The debug endpoints (`/api/debug/*`) are read-only and — since the
  2026-07-04 security trim — gated by the SAME bearer-auth wall as the
  mutating endpoints (`route_layer(require_bearer_auth)`, applied
  unconditionally). They pass through tokenless ONLY when auth itself is
  disabled (an empty-token construction, i.e. tests) — never a separate
  "outside the wall" exemption. Every real boot fetches the bearer token
  from SSM (hard-fail), so on a running host these endpoints ALWAYS
  demand `Authorization: Bearer <token>` and 401 otherwise. Fetch the
  token from SSM: `aws ssm get-parameter --name
  /tickvault/<env>/api/bearer-token --with-decryption --query
  Parameter.Value --output text`, then
  `curl -H "Authorization: Bearer $TOK" https://<host>:3001/api/debug/logs/summary`.
  Tokenless observability fallback: CloudWatch logs + local MCP file reads.
- The Dhan access token, AWS credentials, and private keys are never
  logged to `errors.jsonl` (enforced by
  `.claude/hooks/banned-pattern-scanner.sh`).
