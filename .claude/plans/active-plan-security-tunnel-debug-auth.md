# Implementation Plan: Tunnel Trim to 3001-Only + /api/debug/* Bearer-Auth Gating

**Status:** APPROVED
**Date:** 2026-07-04
**Approved by:** Parthiban (operator directive, Session A task, 2026-07-04 — verbatim: "trim to only 3001 ... gate the 4 /api/debug/* routes ... behind the existing auth middleware WITHOUT breaking the tickvault-logs MCP read-only contract; router tests for authd-200/unauthd-401")

## Plan Items

- [x] Trim the public Tailscale funnel port list from 5 ports (9090 9093 9000 3000 3001) to the single tickvault API port 3001. Ports 9090/9093/3000 (Prometheus/Alertmanager/Grafana) are retired per the CloudWatch-only migration; port 9000 is QuestDB raw SQL with NO auth — the security hole this closes.
  - Files: scripts/tv-tunnel/tickvault-tunnel.service, scripts/tv-tunnel/com.tickvault.tunnel.plist, scripts/tv-tunnel/install-mac.sh, scripts/tv-tunnel/install-aws.sh, docs/runbooks/claude-mcp-access.md
  - Tests: python3 plistlib validation of the edited plist (XML well-formed); grep proves no funnel invocation carries 9090/9093/9000/3000

- [x] Gate the 4 `/api/debug/*` routes in `crates/api` behind the existing `require_bearer_auth` middleware, applied UNCONDITIONALLY via a dedicated `debug_routes` sub-router `route_layer` (NOT gated on `feed_toggle_public`). The middleware self-passthroughs when auth is disabled, so local dry-run / tickvault-logs MCP flows are unchanged.
  - Files: crates/api/src/lib.rs
  - Tests: debug_auth_gate.rs (new — see item below), existing crates/api/tests/route_coverage.rs + debug_spill_and_health_detail.rs stay green

- [x] New router-level integration test file proving the auth gate on all 4 debug paths.
  - Files: crates/api/tests/debug_auth_gate.rs
  - Tests: test_debug_routes_401_without_token, test_debug_routes_401_with_wrong_token, test_debug_routes_pass_auth_with_correct_token, test_debug_routes_pass_when_auth_disabled, test_health_stays_public_without_auth

- [x] Docs/scripts sync: every remaining mention of the 5-port funnel list updated to the 3001-only reality, with an explicit note that QuestDB port 9000 is intentionally no longer funnelled (auth-less raw SQL surface).
  - Files: docs/runbooks/claude-mcp-access.md, scripts/tv-tunnel/install-mac.sh, scripts/tv-tunnel/install-aws.sh
  - Tests: grep sweep for "9090 9093 9000 3000" returns zero funnel-invocation hits

Per-item guarantee matrix: cross-references .claude/rules/project/per-wave-guarantee-matrix.md (15-row + 7-row) — single-item PR, matrix applies as referenced.

## Design

The change has two independent halves, both in the exposure-reduction direction:

1. **Tunnel trim.** The Tailscale Funnel launchers (`scripts/tv-tunnel/tickvault-tunnel.service` on Linux/systemd, `scripts/tv-tunnel/com.tickvault.tunnel.plist` on macOS/launchd) currently publish five ports to the public internet: 9090 (Prometheus), 9093 (Alertmanager), 9000 (QuestDB HTTP — auth-less raw SQL), 3000 (Grafana), 3001 (tickvault API). Prometheus/Alertmanager/Grafana were retired by the CloudWatch-only migration (#O1/#O2/#O3), so those funnel targets are dead weight; QuestDB 9000 is a live UNAUTHENTICATED SQL console and is the actual security hole. The funnel invocation is trimmed to `tailscale funnel 3001` (service) / the single `3001` ProgramArguments entry (plist). Installer scripts and the runbook are updated to the 3001-only reality.

2. **Debug-route auth gating in `crates/api/src/lib.rs::build_router_with_auth`.** The 4 read-only debug routes (`GET /api/debug/logs/summary`, `GET /api/debug/logs/jsonl/latest`, `GET /api/debug/spill/status`, `GET /api/debug/cross-verify/latest`) move OFF `public_routes` into a dedicated `debug_routes: Router<SharedAppState>` which gets `.route_layer(axum::middleware::from_fn_with_state(auth_config.clone(), require_bearer_auth))` applied ALWAYS — deliberately NOT conditioned on `feed_toggle_public`. `require_bearer_auth` (crates/api/src/middleware.rs) passes through when `config.enabled == false` (dry-run / no token configured), so every local development and tickvault-logs MCP flow keeps working byte-identically; in production (SSM-resolved token, auth enabled) the debug routes now demand `Authorization: Bearer <token>` and return 401 otherwise. Route paths and handler bindings stay byte-identical; only their router placement changes. Final router assembly becomes `public_routes.merge(debug_routes).merge(protected_routes)` with the existing tracing + CORS layers unchanged.

MCP contract preserved by construction: `scripts/mcp-servers/tickvault-logs/server.py` reads log files locally (never `/api/debug/*` over HTTP), and its `tool_tickvault_api` targets local/dry-run instances where auth is disabled → passthrough.

## Edge Cases

- **Auth disabled (dry-run / no token):** middleware passthrough — all 4 debug routes behave exactly as today (200/404 depending on filesystem state). This is what keeps `build_router(state, &[], true)` tests and MCP local flows green.
- **Malformed Bearer header** (`Basic ...`, `bearer x` lowercase, `Bearertok`, `Bearer  tok` double-space, empty header): all 401 when auth enabled — inherited from the existing `require_bearer_auth` constant-time compare, already exhaustively tested in auth_middleware.rs.
- **Case-sensitive prefix:** only `Bearer ` (capital B, single space) is accepted — unchanged middleware behavior; debug routes inherit it.
- **Empty token config:** `ApiAuthConfig::new(String::new())` constructs DISABLED auth → passthrough (documented GAP-SEC-01 behavior); debug routes follow the same rule, never a half-enabled state.
- **MCP `tool_tickvault_api` local calls:** no auth header sent, but local/dry-run runs with auth disabled → passthrough. Verified against server.py — no HTTP call to `/api/debug/*` exists in the MCP server at all.
- **Funnel reset on stop:** `ExecStopPost=/usr/bin/tailscale funnel reset` in the systemd unit is retained, so stopping the trimmed service still tears down ALL funnel state, including any stale 5-port config left from a prior install.
- **Stale installed unit/plist on hosts:** hosts that installed the old 5-port unit keep serving 5 ports until `install-aws.sh`/`install-mac.sh` is re-run; the installers are idempotent and the runbook documents the re-run.

## Failure Modes

- **Prod token missing from SSM** (`/tickvault/<env>/api/bearer-token` unfetchable): main.rs's existing behavior governs; if auth ends up enabled with a token the operator does not hold, the 4 debug routes return 401 — **fail-closed is the intended semantic** for a security gate. The read-only observability fallback is CloudWatch logs + local MCP file reads, so no operational blindness.
- **Tunnel service restart with stale port list:** a host running the OLD unit file continues funnelling 5 ports until the installer is re-run (`systemctl daemon-reload` + restart / `launchctl unload+load`). Mitigation: runbook + installer text updated; `tailscale funnel reset` in ExecStopPost clears state on stop.
- **CI plan-gate:** the crates/api/src edit requires this plan file (Status APPROVED, six sections, references crates/api) — this file satisfies `.claude/hooks/plan-gate.sh` and the server-side Design-First Wall.
- **Accidental route regression:** if a future refactor moves the debug routes back onto `public_routes`, the new `debug_auth_gate.rs` 401 assertions fail the build — the test file is the ratchet.

## Test Plan

- New `crates/api/tests/debug_auth_gate.rs` (router-level, `tower::ServiceExt::oneshot`, patterned on route_coverage.rs `test_state()` + auth_middleware.rs assertions). For EACH of the 4 debug paths:
  - enabled auth + no `Authorization` header → **401**
  - enabled auth + wrong token → **401**
  - enabled auth + correct `Bearer` token → status **!= 401** (200 or 404 accepted — handlers touch the filesystem)
  - disabled auth → status **!= 401**
  - plus one assertion that the non-debug public route `/health` still returns **200** with no auth while auth is ENABLED.
- Existing suites must stay green: `cargo test -p tickvault-api` (includes route_coverage.rs, auth_middleware.rs, debug_spill_and_health_detail.rs — the latter calls handlers directly and is unaffected by router placement).
- Lint: `cargo fmt --check`, `cargo clippy -p tickvault-api -- -D warnings -W clippy::perf`.
- Plist validation: `python3 -c "import plistlib,sys; plistlib.load(open(sys.argv[1],'rb'))" scripts/tv-tunnel/com.tickvault.tunnel.plist`.

## Rollback

`git revert` of the two implementation commits (tunnel-trim commit + api-gating commit) restores the prior router topology and the 5-port funnel list exactly; the plan-file commit is docs-only. No schema change, no data migration, no config-format change, no QuestDB impact — rollback is a pure code/scripts revert. Hosts that re-ran the installer would need one more installer re-run after a revert to restore the old port list (not recommended — the trim is the security posture).

## Observability

- 401 rejections on the debug routes are visible through the existing `request_tracing` middleware layer (every request/response already traced) — no new metrics needed; `require_bearer_auth` already logs rejects with the GAP-SEC-01 discipline.
- The tunnel exposure reduction itself is the security win: the auth-less QuestDB raw-SQL surface (port 9000) and three retired-service ports leave the public internet entirely; `scripts/tv-tunnel/doctor.sh` already probes only 9000+3001 and treats 401 as a PASS code (`^(200|204|301|302|401|403)$`), so the doctor remains truthful post-gating.
- No new ErrorCode: no new Rust `error!` emit site is introduced (middleware reuse only), so the error-code cross-ref contract is untouched.
