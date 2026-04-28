# Wave 4 — Shared Preamble (auto-loaded canonical charter)

> **Authority:** CLAUDE.md > this file > defaults.
> **Scope:** every Wave 4 sub-PR session (Wave-4-A through Wave-4-E3)
> MUST read + obey this file before any code change.
> **Trigger:** always loaded by Claude Code sessions touching paths
> under `.claude/rules/project/` or `.claude/plans/active-plan-wave-4.md`.

This file is the canonical charter that every Wave 4 sub-PR session
inherits. It encodes the operator's literal demands — restated honestly
where physics or third-party systems make the literal impossible — and
maps each demand to a mechanical ratchet that fails the build on
regression.

---

## SECTION 1 — Extreme Automation Charter (mandatory before any code)

The operator's literal demand: "every new Claude Code session should
automatically do everything as an automated process always — logs,
queries, dbs, project access — entirely accessible, local or AWS,
dynamic + scalable, no manual setup."

The infrastructure exists in this repo (zero-touch Phases 0–12.6 shipped
per `.claude/rules/project/observability-architecture.md`). This session
MUST USE IT — not duplicate it, not reinvent it.

| Need | Auto-available tool | When to use |
|---|---|---|
| Health check | `mcp__tickvault-logs__run_doctor` | At session start, before code |
| Tail recent ERRORs | `mcp__tickvault-logs__tail_errors` | Investigating any failure |
| Find runbook for ErrorCode | `mcp__tickvault-logs__find_runbook_for_code` | Emitting/referencing an ERROR |
| Live Prometheus query | `mcp__tickvault-logs__prometheus_query` | Verifying counter increments |
| Live QuestDB SQL | `mcp__tickvault-logs__questdb_sql` | Verifying table writes / DEDUP |
| Live Grafana panel | `mcp__tickvault-logs__grafana_query` | Verifying dashboard renders |
| Detect novel errors | `mcp__tickvault-logs__list_novel_signatures` | After any ERROR-emitting change |
| Source-grep | `mcp__tickvault-logs__grep_codebase` | Faster than spawning an agent |
| Latest summary | `mcp__tickvault-logs__summary_snapshot` | At session start |
| Active Prom alerts | `mcp__tickvault-logs__list_active_alerts` | Before opening PR |
| Local app log tail | `mcp__tickvault-logs__app_log_tail` | Debugging runtime issues |
| Triage log tail | `mcp__tickvault-logs__triage_log_tail` | Reviewing auto-fix outcomes |
| Docker container status | `mcp__tickvault-logs__docker_status` | When a service is OFFLINE |

**Mandatory automation steps at session start (BEFORE TodoWrite, BEFORE code):**

1. Run `mcp__tickvault-logs__run_doctor` — capture current health.
2. Run `mcp__tickvault-logs__summary_snapshot` — read recent ERROR signatures.
3. Run `mcp__tickvault-logs__list_active_alerts` — fail loudly if anything red.
4. Run `bash .claude/hooks/session-auto-health.sh` if present — auto-doctor + validate-automation.
5. Read `data/logs/session-auto-health.latest.txt` if present — previous session's verdict carries forward.

If ANY step fails, fix the underlying issue BEFORE proceeding — do not
paper over with skip flags.

For AWS access: `aws` CLI auto-available; SSM path =
`/tickvault/<env>/<service>/<key>`. CloudWatch logs/alarms via the same
MCP tools (Phase 4 wiring).

---

## SECTION 2 — Resilience Charter (operator's literal demands, restated honestly)

The operator's verbatim charter (shouted across multiple sessions):

> "Zero ticks loss, WebSocket never disconnects or reconnects, never
> slow/locked/hanged/stuck, QuestDB never fails, O(1) latency +
> uniqueness + dedup, comprehensive automation/monitoring/alerting/
> auditing, real-time proof, no hallucination."

| Operator demand | Honest engineering guarantee | Why we can't promise the literal |
|---|---|---|
| Never miss a single tick | BOUNDED ZERO LOSS inside chaos-test envelope: rescue ring (600 K) → spill file → DLQ NDJSON. Beyond envelope DLQ catches every payload as recoverable text. | SEBI audit demands honesty; "never" without envelope is a lie. |
| WebSocket never disconnects | DETECT in <= 5 s via watchdog; RECONNECT with SubscribeRxGuard preserving subs; SLEEP-UNTIL-OPEN post-close (Wave-2-A/B). | TCP is not in our control; networks fail. |
| Never slow / locked / hanged | Hot path DHAT-tested <= 4 blocks/8 KB across 10 K calls; Criterion p99 <= 100 ns enqueue; tick-gap > 30 s fires Telegram (coalesced 60 s). | OS scheduler can preempt; we detect, we don't prevent. |
| QuestDB never fails | ABSORB via 3-tier resilience; `tv_questdb_disconnected_seconds > 30` fires CRITICAL. | Remote process; we cannot prevent its failures. |
| O(1) latency | DHAT zero-alloc + Criterion <= 50 ns budget on hot-path lookups; bench-gate fails build on > 5 % regression. | Without ratchet, drift returns within 1 month. |
| Uniqueness + dedup | Composite key `(security_id, exchange_segment)` per I-P1-11; DEDUP UPSERT KEYS on every storage table; `dedup_segment_meta_guard` scans every constant. | Without composite key the 2026-04-17 production bug recurs. |
| 100% guarantee no hallucination | The ONLY honest 100% claim is **"100% inside the tested envelope, with ratcheted regression coverage."** | Promising literal "never" = audit trap. |

This Wave 4 item ships ONE specific piece of the bounded guarantee
(see Section 4 SCOPE for what). It MUST NOT promise more than the charter.

---

## SECTION 3 — Deep Research Protocol (mandatory before writing code)

Spawn THREE specialist subagents IN PARALLEL (per `stream-resilience.md` B3)
BEFORE writing any production code:

- **Agent 1 (hot-path-reviewer):** Hot-path violations: no `.clone()`,
  `Vec::new()`, `.collect()`, `format!()`, `Box`, `dyn` on hot path.
  Bounded channels + zero-alloc enqueue. Report under 400 words.
- **Agent 2 (security-reviewer):** Audit: secret exposure in logs/labels,
  ILP injection, path traversal, unsafe blocks, missing input sanitization.
  Report under 400 words.
- **Agent 3 (general-purpose):** Hostile bug-hunt: race conditions,
  daily-reset collisions, missing market-hours gate, missing edge-trigger,
  flush/persist using `warn!` instead of `error!`, counters shown raw
  without `increase()`, pub fn defined-but-never-called, false-OK class
  bugs. Report under 600 words.

Wait for ALL THREE reports. Synthesize into a verdict table:
CRITICAL / HIGH / MEDIUM / LOW / FALSE-POSITIVE. Fix every CRITICAL and
HIGH inline BEFORE opening the PR. Document every false-positive triage
with grep evidence.

After implementation lands green, run the SAME 3 agents again as
adversarial review on the diff (proven pattern from PR #393 — caught
4 real bugs across 4 grill rounds).

---

## SECTION 4 — 7-Layer Observability Coverage (every new code path)

For EACH new code path, ALL SEVEN observability layers must fire.

| # | Layer | Mechanism | Verification |
|---|---|---|---|
| 1 | Prometheus counter | `metrics::counter!("tv_<name>_total", ...)` static labels | `mcp__tickvault-logs__prometheus_query` |
| 2 | Prometheus gauge | `metrics::gauge!("tv_<name>", ...)` | dashboard panel cites it |
| 3 | Tracing span | `#[instrument]` on hot-path entry fns | `error_code_tag_guard` |
| 4 | Loki structured log | `error!`/`warn!`/`info!` with `code = ErrorCode::X.code_str()` | `mcp__tickvault-logs__tail_errors` |
| 5 | Telegram event | `NotificationEvent::*` typed variant | notification_service routes Severity::High/Critical |
| 6 | Grafana panel | wraps counter in `increase()`/`rate()` per Rule 12 | `mcp__tickvault-logs__grafana_query` |
| 7 | Audit table | INSERT into `<event>_audit` table with DEDUP UPSERT KEYS | `mcp__tickvault-logs__questdb_sql` |

Mechanical guards (already shipped):
- `error_code_tag_guard.rs` — `error!` must carry `code` field
- `operator_health_dashboard_guard.rs` — counters must have a panel
- `resilience_sla_alert_guard.rs` — counters must have an alert rule
- `dedup_segment_meta_guard.rs` — every DEDUP key includes segment

---

## SECTION 5 — Verification Gates (real-time, between every commit)

Between EACH commit:

```
cargo check -p <changed-crate>
cargo test  -p <changed-crate> --lib
```

Before opening the PR:

```
FULL_QA=1 make scoped-check
cargo test --workspace
bash .claude/hooks/banned-pattern-scanner.sh
bash .claude/hooks/pub-fn-test-guard.sh "$PWD" all
bash .claude/hooks/pub-fn-wiring-guard.sh "$PWD"
bash .claude/hooks/plan-verify.sh
python3 -c "import yaml; yaml.safe_load(open('deploy/docker/grafana/provisioning/alerting/alerts.yml'))"
python3 -c "import json; json.load(open('deploy/docker/grafana/dashboards/operator-health.json'))"
mcp__tickvault-logs__list_active_alerts        # must be []
mcp__tickvault-logs__list_novel_signatures     # must be []
cargo bench                                    # if hot path touched — respect benchmark-budgets.toml
cargo test --features dhat                     # if hot path touched — 0 hot-path allocations
```

After all gates green: run the 3 adversarial review agents (Section 3)
on the diff. Fix every CRITICAL/HIGH inline. Document false-positives
with grep evidence.

---

## SECTION 6 — Protocol (mandatory rules, every commit)

- `stream-resilience.md` B1: file-first; chat carries pointers, not bulk
- `stream-resilience.md` B8: 9-box checklist before next item
- `stream-resilience.md` B9: hot paths need DHAT + Criterion + budget entry
- `stream-resilience.md` B10: every new QuestDB table needs DEDUP UPSERT
  KEYS + idempotent ALTER ADD COLUMN IF NOT EXISTS
- `gap-enforcement.md` I-P1-11: composite key (security_id, exchange_segment)
- `audit-findings-2026-04-17.md` Rule 3: market-hours-aware tokio tasks
- `audit-findings-2026-04-17.md` Rule 4: edge-triggered alerts only —
  Telegram fires on rising edge, INFO on falling edge
- `audit-findings-2026-04-17.md` Rule 5: flush/persist failures use
  `error!`, never `warn!`
- `audit-findings-2026-04-17.md` Rule 11: NO false-OK signals — gate
  success notifications on a positive progress signal
- `audit-findings-2026-04-17.md` Rule 12: every counter panel wrapped
  in `increase()` or `rate()`
- `audit-findings-2026-04-17.md` Rule 13: every new pub fn must have
  a call site
- `hot-path.md`: zero allocation on hot path; bench-gate fails build
  on > 5 % regression
- `rust-code.md`: NO `unwrap()`/`expect()` in prod; tracing macros only;
  `Secret<T>` for sensitive data; named constants/config; failures
  escalate (alert -> retry -> halt), never silent

If any banned-pattern hook fires, FIX THE UNDERLYING ISSUE.
NEVER bypass with `--no-verify` or `--no-gpg-sign`.

---

## SECTION 7 — 100% Coverage Charter (operator's "I need 100%" demand)

Operator's literal demand: "100% code coverage, 100% audit coverage,
100% testing coverage, 100% code checks, 100% performance, 100%
monitoring, 100% logging, 100% alerting, 100% security, 100% security
hardening, 100% bugs fixing, 100% scenarios covering, 100% functionality
covering, 100% code review, 100% extreme check."

Honest implementation (mechanical, not aspirational):

| 100% claim | Mechanical proof |
|---|---|
| Code coverage | `quality/crate-coverage-thresholds.toml` 100% min per crate, enforced by `scripts/coverage-gate.sh` in CI |
| Audit coverage | Every typed event writes to a `<event>_audit` table (Wave-2-D + Wave-4-E) |
| Testing coverage | 22 test categories per `testing.md`; scoped per changed crate |
| Code checks | banned-pattern-scanner + pub-fn-test-guard + pub-fn-wiring-guard + plan-verify + secret-scan + 8 pre-commit gates |
| Performance | DHAT zero-alloc + Criterion p99 budgets + bench-gate <= 5% regression |
| Monitoring | 7-layer telemetry (Section 4); `mcp__tickvault-logs__*` MCP tools |
| Logging | tracing macros mandatory; ERROR routes to Telegram; logs hourly-rotated |
| Alerting | Prometheus alerts in `alerts.yml`; `resilience_sla_alert_guard` ratchets |
| Security | banned-pattern-scanner; secret-scan; `Secret<T>` enforced; security-reviewer agent |
| Security hardening | Static IP enforcement; pre-commit secret scan; `unused_must_use` lint |
| Bug fixing | Adversarial 3-agent review (Section 3) — proven 4-bug catch rate per 30-commit PR |
| Scenarios covering | Wave-4-E1/E2/E3 = 50+ worst-case scenarios mechanically guarded |
| Functionality covering | 14-item plan + Wave 4 (28 final items) — `active-plan-*.md` is the contract |
| Code review | Adversarial 3-agent pass (Section 3) before AND after implementation |
| Extreme check | All of the above + ratchet tests that fail the build on regression |

Anything weaker than mechanical-ratchet enforcement = not 100%.
Aspirational claims like "I tried hard" = REJECT IN REVIEW.

---

## SECTION 8 — The Honest 100% Claim (forced PR-body wording)

When the PR description quotes "100% guarantee", it MUST be phrased exactly:

> "100% inside the tested envelope, with ratcheted regression coverage:
> <= 60s QuestDB outage absorbed by rescue->spill->DLQ; <= 600K rescue
> ring capacity; bench-gated O(1) hot path; composite-key uniqueness;
> chaos-tested 70h sleep/wake. Beyond the envelope, DLQ NDJSON catches
> every payload as recoverable text."

Promising "WebSocket never disconnects" or "QuestDB never fails"
without the envelope qualifier = REJECT IN REVIEW.

---

## Trigger

This rule activates for every Wave 4 sub-PR session and any session
editing `.claude/plans/active-plan-wave-4.md` or any file under
`.claude/rules/project/wave-4-*.md`.
