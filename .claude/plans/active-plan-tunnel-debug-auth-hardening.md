# Implementation Plan: Tunnel Port Reduction + Debug-Route Auth Hardening

**Status:** VERIFIED
**Date:** 2026-07-04
**Approved by:** Parthiban (operator) — "just fix everything, doesn't matter weekend or weekday" (full green light, this session)

## Context / verification-sweep finding

A verification sweep found the `scripts/tv-tunnel/` Tailscale **Funnel** (public
internet, no auth) exposes ports `9090 9093 9000 3000 3001`:

- **9000** = QuestDB raw HTTP `/exec?query=` — world-readable/writable SQL
  (`DROP`/`INSERT`, no auth). CRITICAL.
- **9090 / 9093 / 3000** = RETIRED services (Prometheus / Alertmanager / Grafana)
  — dead ports, needless surface.
- **3001** = tickvault API (operator's legitimate remote access). KEEP.

Additionally, the API's `/api/debug/*` read-only log routes are **public** (no
auth) and live on the KEPT port 3001 — so reducing the funnel alone does NOT
protect them. They must be bearer-gated.

## Plan Items

- [x] **Item 1 — Funnel port list → `3001` only.** In
  `scripts/tv-tunnel/com.tickvault.tunnel.plist` (`ProgramArguments`) and
  `scripts/tv-tunnel/tickvault-tunnel.service` (`ExecStart`), drop the raw-DB
  port `9000` and the 3 retired ports `9090 9093 3000`; funnel only `3001`.
  - Files: scripts/tv-tunnel/com.tickvault.tunnel.plist, scripts/tv-tunnel/tickvault-tunnel.service

- [x] **Item 2 — Consistency in the tunnel tooling** (same funnel-scope reduction,
  so the installer/doctor don't probe now-closed ports). `doctor.sh` probe list
  → `3001` only + accept `401` on the debug probe (a gated route IS the win);
  `install-mac.sh` / `install-aws.sh` header comments, echo summaries, and the
  30s readiness probe → `3001` only.
  - Files: scripts/tv-tunnel/doctor.sh, scripts/tv-tunnel/install-mac.sh, scripts/tv-tunnel/install-aws.sh

- [x] **Item 3 — Bearer-gate the 4 `/api/debug/*` routes.** Move
  `/api/debug/logs/summary`, `/api/debug/logs/jsonl/latest`,
  `/api/debug/spill/status`, `/api/debug/cross-verify/latest` from the public
  router into the `require_bearer_auth`-layered protected router. Add a code
  comment documenting the exposure rationale + that local MCP file-reading tools
  are unaffected.
  - Files: crates/api/src/lib.rs
  - Tests: test_debug_routes_require_auth_401_without_token, test_debug_routes_pass_with_valid_token, test_debug_routes_public_when_auth_disabled

- [x] **Item 4 — Keep the operator-portal Cross-verify card working (found
  during impl, 2026-07-04).** The portal Lambda's cross-verify snapshot runs
  (via SSM RunShellScript ON the box) `curl http://127.0.0.1:3001/api/debug/cross-verify/latest`
  with NO bearer token — Item 3 would 401 it and the card would degrade to a
  false "no run yet". Fix: the SSM command fetches the bearer token ON-BOX
  (`aws ssm get-parameter /tickvault/prod/api/bearer-token --with-decryption`,
  using the instance role the app itself uses at boot) and sends
  `Authorization: Bearer …`. The token never appears in the Lambda's command
  TEXT (fetched at runtime on the box) and is never echoed to command output.
  Failure degrades exactly as before (`|| echo CV_DATE=` → truthful blank card).
  Also: truth-sync the funnel-port mentions in `config/claude-mcp-endpoints.toml`
  comments + `docs/runbooks/claude-mcp-access.md` (funnel now fronts 3001 only).
  - Files: deploy/aws/lambda/operator-control/handler.py, config/claude-mcp-endpoints.toml, docs/runbooks/claude-mcp-access.md
  - Tests: existing test_handler.py cross-verify tests stay green (python3 -m unittest)

## Design

The Tailscale Funnel is a **public, unauthenticated** ingress. Two orthogonal
hardening moves:

1. **Shrink the ingress surface** to the single port the operator actually needs
   remotely (`3001`, the tickvault API). This closes the unauthenticated QuestDB
   SQL port (`9000`) and 3 dead retired-service ports in one edit. Consequence,
   stated honestly: remote (over-funnel) QuestDB SQL via `https://<host>:9000` is
   GONE. The MCP `questdb_sql` tool still works LOCALLY (127.0.0.1:9000, on the
   box) — only the public path is removed. That public path was the vulnerability.

2. **Authenticate the surviving debug surface.** The `/api/debug/*` routes live on
   3001 (kept) and are currently public. Gate them with the EXISTING
   `require_bearer_auth` middleware (same one guarding `POST /api/feeds/{feed}`).
   In the router, this means relocating the 4 routes from `public_routes` into the
   `protected_base` that is `.layer(require_bearer_auth)`-wrapped. No new
   middleware, no new token source — the bearer token already resolves from AWS
   SSM `/tickvault/<env>/api/bearer-token`.

**MCP-contract impact (verified, not assumed):** The canonical MCP observability
tools (`summary_snapshot`, `tail_errors`, `list_novel_signatures`,
`triage_log_tail`, `signature_history`) read `data/logs/*` files DIRECTLY on the
box (`scripts/mcp-servers/tickvault-logs/server.py`), NOT via the `/api/debug/*`
HTTP routes. Gating the HTTP routes therefore does NOT break the documented MCP
read-only observability contract. Only the redundant `tool_tickvault_api` generic
HTTP passthrough to a debug path would now need the token — and that is not the
canonical path (CLAUDE.md's automation-first list is the file-reading tools).

## Edge Cases

- **Auth disabled (dry-run + no SSM token):** `require_bearer_auth` passes through
  → debug routes public on a local dev box. Acceptable (no public funnel token
  configured = local dev). On prod/local-with-SSM-token, auth is enabled → gated.
- **Non-UTF8 / malformed / lowercase `bearer` header:** already handled by
  `require_bearer_auth` → 401 (existing middleware tests cover this).
- **`doctor.sh` probing a gated debug route unauthenticated:** returns 401 →
  treated as PASS (route exists + protected = the intended posture).
- **Funnel `--bg 3001` single-port form:** valid tailscale funnel invocation
  (a single target port); the previous multi-port form is simply narrowed.

## Failure Modes

- **SSM unreachable at boot:** unchanged — main.rs hard-fails boot (existing
  `fetch_api_bearer_token` semantics). Not introduced by this change.
- **Operator loses remote QuestDB SQL:** intended by the operator's baked
  decision ("drop the raw DB SQL port"). Documented in the PR honest-envelope.
- **Operator's remote MCP debug HTTP calls now 401:** mitigated — the canonical
  file-reading MCP tools are unaffected; only the redundant HTTP passthrough
  needs a token.

## Test Plan

- `cargo test -p tickvault-api` (full crate suite) — must stay green.
- New router-level tests in `crates/api/src/lib.rs`:
  - debug route returns 401 without a token when auth enabled;
  - debug route returns 200 with the valid bearer token;
  - debug route passes through (non-401) when auth disabled (dry-run).
- Existing `debug_spill_and_health_detail.rs` unaffected (calls handlers directly,
  not through the auth-layered router).
- `cargo test -p tickvault-common` — `claude_mcp_endpoints_config_guard.rs` still
  green (it does not pin the funnel port list).
- Real test output pasted as evidence in the PR (Verified label).

## Rollback

- Revert the single PR. The plist/service/scripts revert restores the prior
  funnel port list; the router revert restores the public debug routes.
- No schema, no persisted state, no migration — pure config + router-wiring change.
- Feature-flag-free: rollback is `git revert <sha>`.

## Observability

- `require_bearer_auth` already logs `warn!("GAP-SEC-01: API auth failed …")` on
  rejected requests and the `request_tracing` middleware records
  `tv_api_request_duration_ms{status}` (401s now visible on debug paths).
- No new counter needed — a 401 on `/api/debug/*` is captured by the existing
  request-tracing histogram + structured access log.
- `doctor.sh` verifies the funnel + a protected debug probe (accepts 401).

## Per-Item Guarantee Matrix

Carries the 15-row + 7-row guarantee matrix by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md`. Item-specific proof:
- Code coverage: api crate floor 98.6 held (new tests add debug-route coverage).
- Security: this IS a security-hardening item; security-reviewer + hostile agent
  run before + after impl.
- Testing: smoke + happy-path + error-scenario (401) router tests added.
- Extreme check: gating is enforced by the new 401-without-token router test
  (fails the build if a future edit re-publicizes the debug routes).
- Zero-loss / hot-path: N/A — cold boot-time router construction only, no tick path.
- O(1): N/A — cold path (router build once at boot).
