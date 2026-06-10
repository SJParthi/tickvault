# Autonomous Operations: 100% Zero-Human-Input Plan

**Status:** DRAFT — Milestone 1 shipping now, 2-5 queued as follow-up PRs
**Date:** 2026-04-18
**Owner:** Parthiban (architect), Claude Code (builder)
**Scope:** Every bug, error, warning, exception across the entire tickvault product + codebase resolves without human action.

## The 5 layers of autonomous operations

| Layer | Capability | Status | Shipping PR |
|---|---|---|---|
| 1. Observe | Read metrics, logs, alerts, traces, data from any Claude session on any branch, Mac + AWS | **M1 — this PR** | `claude/autonomous-ops-m1` |
| 2. Diagnose | Classify errors via signature hashing + errors.summary.md | ✅ Shipped (Phases 1-5 of zero-touch plan) | — |
| 3. Decide | Rule-based triage — classifier picks safe auto-fix action per error code | ✅ Shipped — 100% variant coverage + TRIAGE-EXEMPT ratchet (PRs #1083 + prior incremental) | M2 (merged 2026-06-10) |
| 4. Act | Auto-fix script per rule; every classifier rule has a runnable remediation | Scripts + rollbacks + guards shipped, but ALL endpoints retired — see M3 status 2026-06-10 below | M3 (blocked on operator decision) |
| 5. Verify + Rollback | Post-fix metric re-probe; auto-revert if fix made things worse | Not built | M4 |
| 6. Runtime scaling + chaos | AWS autoscale, EBS resize, Dhan plan upgrade; nightly chaos tests prove self-heal | Not built (deferred post-AWS-provision) | M5 |

## Milestone 1 (THIS PR) — Universal observability

**Goal:** any Claude session, on any branch, on any host (Mac / AWS / claude.ai sandbox / claude cowork), can read tickvault's full runtime state with zero per-session setup.

**Design decisions:**
- **Transport:** Tailscale Funnel — free tier, stable HTTPS URLs that never change, both Mac and AWS Tailscale devices expose their local services via `<hostname>.<tailnet>.ts.net:<port>`.
- **Config:** committed file `config/claude-mcp-endpoints.toml` with explicit Mac + AWS profiles. Replaces per-environment env-var setup. Env vars still override for local testing.
- **Log access:** new HTTP endpoints on tickvault API (`GET /api/debug/logs/summary`, `GET /api/debug/logs/jsonl/latest`) — no file-sync/rsync needed. MCP reads logs over HTTPS through the tunnel, same as metrics.
- **Branch-independence:** the config file and `.mcp.json` changes go to `main`. Every future branch clone inherits them automatically. No per-branch env vars.

**Deliverables (this PR):**
- `config/claude-mcp-endpoints.toml` — profile config
- `scripts/tv-tunnel/install-mac.sh` — one-command Mac setup (Tailscale install check, funnel start, launchd plist)
- `scripts/tv-tunnel/install-aws.sh` — one-command AWS setup (apt install, funnel start, systemd unit)
- `scripts/tv-tunnel/com.tickvault.tunnel.plist` — launchd template
- `scripts/tv-tunnel/tickvault-tunnel.service` — systemd template
- `scripts/tv-tunnel/doctor.sh` — verify tunnel is up from the host it runs on
- `scripts/mcp-servers/tickvault-logs/server.py` — read `claude-mcp-endpoints.toml`, env vars as override
- `.mcp.json` — reference the new config path
- `crates/api/src/handlers/debug.rs` — two read-only log-access handlers
- `crates/api/src/lib.rs` — wire debug routes
- `docs/runbooks/claude-mcp-access.md` — rewrite with working Mode C
- `crates/common/tests/claude_mcp_endpoints_config_guard.rs` — config schema invariant
- `crates/api/tests/debug_endpoints_guard.rs` — debug routes are read-only + localhost-source-only when config says so

**What you run once per environment (after this PR merges):**
- Mac: `bash scripts/tv-tunnel/install-mac.sh`
- AWS (when provisioned): `bash scripts/tv-tunnel/install-aws.sh`

After that, any Claude session anywhere can query your live stack live.

## Milestone 2 — Full triage rule coverage — ✅ COMPLETE (2026-06-10, PR #1083)

**Goal:** every ErrorCode variant has a classifier rule in `.claude/triage/error-rules.yaml`.

**Shipped state (measured 2026-06-10):** all 105 live ErrorCode variants have
rules (113 blocks; the "53 variants / 46 missing" figures below were stale —
coverage was filled incrementally by prior PRs). PR #1083 added the milestone's
ratchet clause: a future variant must carry a rule OR a documented
`# TRIAGE-EXEMPT: <CODE> — <reason>` exclusion (column-0, pinned in the test's
PINNED_EXEMPTIONS allowlist, never allowed for non-`is_auto_triage_safe()`
codes), with build failure on neither. Guards live in
`crates/common/tests/triage_rules_full_coverage_guard.rs` (12 tests) +
`triage_rules_guard.rs` (7 tests).

**Original deliverables (historical):**
- 46 new classifier rules (one per ErrorCode variant not yet covered)
- Each rule: severity, signature pattern, decision (`auto-fix` | `escalate-only` | `observe`)
- Safety default: anything with severity Critical or confidence < 0.95 → `escalate-only`
- Meta-guard: `crates/common/tests/triage_rules_full_coverage_guard.rs` fails the build if any ErrorCode variant has no rule
- Expected runtime: Claude's `/loop` runbook polls `errors.summary.md`, routes to rule, decides.

## Milestone 3 — Full auto-fix script catalogue — ⚠ STATUS 2026-06-10: scripts done, endpoints retired, BLOCKED ON OPERATOR DECISION

**Goal:** every `auto-fix` rule in M2 has a runnable remediation script with bounded side effects.

**Measured state (2026-06-10 audit):**
- ✅ 6 fix + 6 rollback script pairs exist under `scripts/auto-fix-*.sh`
  (clear-spill, drain-spill, refresh-instruments, reset-ws-pool, rotate-token,
  activate-kill-switch). All support `--dry-run`, all audit-log to
  `data/logs/auto-fix.log`, M3+ scripts carry `corr_id=`.
- ✅ M4 dispatchers `scripts/triage/verify.sh` + `rollback.sh` exist.
- ✅ Meta-guard `crates/common/tests/autonomous_ops_m3_m4_guard.rs` enforces
  pair-completeness, rollback contract, rule→script existence.
- ✅ The stale `restart-depth` pair was RETIRED 2026-06-10 (depth-20/200 is
  deleted forever per `websocket-connection-scope-lock.md`; no rule referenced it).
- ❌ **ZERO scripts have a working endpoint.** The API was narrowed to 12
  read-only observability routes post-AWS-lifecycle; `/api/instruments/rebuild`
  was retired in PR #6b (2026-05-19). Every script fails safely (exit 1/2 +
  audit log), and the 3 rules referencing scripts (DATA-813, STORAGE-GAP-01,
  DATA-807) are at 0.85 confidence → escalate. The runtime triage chain is
  fail-safe but performs ZERO automated remediation today.
- The original "~20-30 scripts" target assumed the pre-narrowing runtime
  (depth, movers, greeks, Valkey — all deleted). It no longer applies.

**OPEN OPERATOR DECISION (required before any further M3 code):**
completing M3 means re-introducing authenticated MUTATING endpoints to the
deliberately read-only API — `/api/auth/rotate` (Dhan token rotation over
HTTP), `/api/kill-switch/activate` (order-control surface), `/api/spill/drain`,
`/api/ws/reset-pool`, possibly re-adding `/api/instruments/rebuild` (which
PR #6b retired, and which the daily-universe lock §3 constrains). This is new
attack surface contradicting the post-AWS-lifecycle narrowing and touches the
security charter. Options:
- (a) Approve a designed subset (e.g. spill-drain + ws-reset only; never
  kill-switch/token over HTTP), each behind GAP-SEC-01 auth + design-first plan.
- (b) Re-scope M3 as DONE-as-tooling: scripts remain operator-runbook tools,
  auto-execution stays escalate-only forever, M3 closes.
Until one is chosen, no endpoint code lands.

**Original deliverables (historical, for context):**
- ~20-30 new auto-fix scripts under `.claude/triage/auto-fix/`
- Each script: idempotent, side-effect bounded, emits audit log to `data/logs/auto-fix.log`
- Examples: `restart-websocket-pool.sh`, `rotate-token.sh`, `clear-questdb-wal.sh`, `restart-depth-rebalancer.sh`, `refresh-instrument-master.sh`, `kill-switch-activate.sh`, etc.
- Meta-guard: every `auto-fix` rule in `error-rules.yaml` has a corresponding executable script.
- Dry-run flag (`--dry-run`) on every script so Claude can preview before executing.
- `make triage-execute` runs the full auto-fix path; already exists, just extends to cover all rules.

## Milestone 4 (next PR) — Verify + rollback loop

**Goal:** after Claude triggers a fix, the verify step re-probes metrics/alerts; if the fix didn't clear the symptom OR made things worse, Claude auto-reverts.

**Deliverables:**
- `scripts/triage/verify.sh` — takes fix-id + expected-outcome predicate, polls MCP for 60-180s, pass/fail
- `scripts/triage/rollback.sh <fix-id>` — reverses every auto-fix in M3 (every script gets a corresponding `*-rollback.sh`)
- Claude's loop runbook: fix → verify → (pass: log success, escalate to info) | (fail: rollback → escalate to operator)
- Audit log: every fix + verify + rollback recorded in `data/logs/auto-fix.log` with correlation IDs.

## Milestone 5 (post-AWS-provision) — Runtime scaling + chaos

**Goal:** autonomous response to capacity events (tick flood, daily subscription plan exhaustion, EBS fill) + nightly chaos tests that prove self-heal.

**Deliverables:**
- AWS Lambda bridge for actions requiring AWS creds (EBS resize, ASG scale, SNS alert)
- Dhan rate-limit auto-backoff + subscription-plan upgrade nudge to operator (plan upgrades are paid actions — always escalate, never auto-act)
- Chaos tests (`scripts/chaos/`): kill QuestDB, kill Valkey, kill WebSocket connection, corrupt instrument cache — confirm recovery without operator
- Nightly CI job: full chaos battery runs against a dedicated AWS instance, fails the build if any scenario requires operator
- Zero-tick-loss proof: chaos + verify loop proves continuous tick capture through all scenarios

## The honesty clause

**Physical constraints that cannot be automated away:**
1. **First-time tunnel setup** — someone has to run `install-mac.sh` once on the Mac and `install-aws.sh` once on AWS. After that, launchd/systemd auto-restart forever.
2. **Paid actions** — Dhan subscription plan upgrade, AWS instance resize above budget, etc. These always escalate to operator (never auto-act), per the AWS-budget rule.
3. **Novel signatures** — a never-seen error signature always goes to operator via Telegram. Classifier confidence ≥ 0.95 is required for auto-fix (per the ALWAYS-ping rule in the Zero-Touch plan).
4. **Root-level OS changes** — installing brew packages, systemd reload, etc. require sudo. Handled during the one-time install script run.

**Everything else IS automatable** and is covered by M1-M5.

## Relationship to existing plans

This plan extends and depends on `.claude/plans/active-plan.md` (Zero-Touch Error Observability & Auto-Triage). That plan covers Layers 2 + parts of 3 and 6. This plan completes Layers 1, 3, 4, 5, 6.

On completion of M5, both plans fold into a single living doc: `docs/architecture/autonomous-operations.md`.

## Plan verification (before "VERIFIED" status)

Before this PR merges, `bash .claude/hooks/plan-verify.sh` must pass:
- [ ] Every deliverable in the M1 list is a real file in the diff
- [ ] Every listed test function exists and passes
- [ ] Runbook rewrite is non-trivial (>300 new lines)
- [ ] `scripts/validate-automation.sh` still passes (30/30)
- [ ] CI on the PR is green
