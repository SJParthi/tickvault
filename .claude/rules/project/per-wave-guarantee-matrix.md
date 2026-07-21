# Per-Wave / Per-Item / Per-Block Guarantee Matrix (PROJECT-WIDE, MANDATORY)

> **Authority:** CLAUDE.md > this file > defaults.
> **Scope:** Every wave plan, every item, every block, every phase doc in
> `.claude/plans/active-plan*.md` MUST carry the matrix below.
> **Enforcement:** Mechanical via `.claude/hooks/per-item-guarantee-check.sh`
> + `make wave-guarantee-check` (CI gate). Items without the matrix fail
> the build at PR-merge.
> **Trigger:** Always loaded for any session editing a plan file.

## Why this rule exists

Operator demand 2026-05-01 (verbatim, repeated 5 times across sessions):
> "for every waves and for every blocks or for every items we need all
> these guarantee and assurance right dude because every block is
> extremely critical right dude?"

This rule converts the aspirational charter into a mechanical gate. No
hand-wave, no "I tried hard" — every plan item carries proof; the hook
blocks the PR if proof is absent.

## The 15-row 100% Guarantee Matrix (mandatory)

Every plan item / wave / block MUST carry the rows below in its 9-box
checklist or in a dedicated "Per-Item Guarantee Matrix" subsection:

| Demand | Mechanical proof artefact | Real-time check | Per-item gate |
|---|---|---|---|
| 100% code coverage | `quality/crate-coverage-thresholds.toml` ratcheted per-crate floors (68.3–99.5, target 100%), floors only move up; `scripts/coverage-gate.sh` | post-merge llvm-cov report | item PR includes coverage delta |
| 100% audit coverage | `<event>_audit` table per typed event with DEDUP UPSERT KEYS | `mcp__tickvault-logs__questdb_sql` | item adds/extends audit table |
| 100% testing coverage | 22 test categories per `testing.md` (unit/integration/property/loom/dhat/fuzz/mutation/sanitizer/coverage/etc.) | `cargo test --workspace` green | item declares which 22 it covers |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan + 8 pre-commit gates | pre-push mandatory | all gates green |
| 100% code performance | DHAT zero-alloc + Criterion p99 budgets + bench-gate ≤5% regression | `cargo bench` + `scripts/bench-gate.sh` | DHAT test if hot path |
| 100% monitoring | 7-layer telemetry (Prom counter + gauge + tracing span + Loki log + Telegram event + Grafana panel + audit table) | `mcp__tickvault-logs__run_doctor` | 9-box completes 7 layers |
| 100% logging | tracing macros mandatory (no println!/dbg!); ERROR → Telegram | hourly errors.jsonl rotation | every error path uses `error!` with `code` |
| 100% alerting | `alerts.yml` Prom rule + `resilience_sla_alert_guard.rs` ratchet | `mcp__tickvault-logs__run_doctor` (CloudWatch alarms) | item adds alert for new failure |
| 100% security | banned-pattern + secret-scan + `Secret<T>` + security-reviewer agent | `cargo audit` post-deploy | item runs security-reviewer |
| 100% security hardening | static IP enforcement + pre-commit secret scan + `unused_must_use` lint | post-deploy IP verify | item declares attack-surface delta |
| 100% bugs fixing | adversarial 3-agent review (proven 4-bug catch rate) | pre-PR + post-impl agent pass | item runs all 3 agents |
| 100% scenarios covering | 9-box + chaos test for new failure mode | chaos suite | item declares scenarios in ratchet |
| 100% functionalities covering | every pub fn has call site + test (`pub-fn-wiring-guard.sh` + `pub-fn-test-guard.sh`) | pre-push gates 6+11 | item adds tests for every new pub fn |
| 100% code review | adversarial 3-agent on diff before AND after impl | per-PR | item PR includes both passes |
| 100% extreme check | all of above + ratchet tests fail build on regression | every commit | item adds ratchet test |

## The 7-row Resilience Demand Matrix (mandatory)

| Demand | Honest envelope | Per-item proof |
|---|---|---|
| Zero ticks lost | Bounded zero loss inside chaos envelope: ring 2M → spill NDJSON → DLQ | item must not introduce new tick-drop path |
| WS never disconnects | DETECT ≤5s, reconnect with `SubscribeRxGuard`, sleep-until-open post-close | item must not break SubscribeRxGuard or pool watchdog |
| Never slow/locked/hanged | DHAT ≤4 alloc blocks/8KB across 10K calls; Criterion p99 ≤100ns; tick-gap >30s Telegram; core_affinity Core 0 | item must not add hot-path allocation |
| QuestDB never fails | ABSORB via 3-tier rescue→spill→DLQ + schema self-heal | item must not break self-heal |
| O(1) latency | `from_le_bytes` + `papaya` + `Arc<HashMap>` + SPSC bounded; bench-gate ≤5% regression | item adds Criterion bench if hot path |
| Uniqueness + dedup | Composite `(security_id, exchange_segment)` per I-P1-11 + DEDUP UPSERT KEYS + meta-guard | item DEDUP key includes segment |
| Real-time proof | 7-layer telemetry + SLO-01/SLO-02 @ 10s + market-open self-test @ 09:16:30 IST | item ratchet pins all 7 layers |

## The honest 100% claim (mandatory wording)

When any plan / PR / commit body uses the phrase "100% guarantee", it MUST be
qualified exactly:

> "100% inside the tested envelope, with ratcheted regression coverage:
> ≤60s QuestDB outage absorbed by rescue→spill→DLQ;
> ≤200,000-seal ring buffer capacity (constant `SEAL_BUFFER_CAPACITY`,
> `crates/trading/src/candles/seal_ring.rs`, ratcheted by
> `seal_ring.rs::test_seal_buffer_capacity_constant_is_locked_value`)
> → NDJSON spill → DLQ; bench-gated
> O(1) hot path; composite-key uniqueness;
> chaos-tested 65h Fri 16:00 IST → Mon 09:00 IST weekend sleep/wake
> (`crates/core/tests/ws_sleep_resilience.rs`). Beyond the envelope,
> DLQ NDJSON catches every payload as recoverable text. Outstanding
> (Wave-6): >65h holiday-weekend dormant sleep test (W6-2)."

Promising literal "WebSocket never disconnects" or "QuestDB never fails"
without the envelope qualifier = REJECT IN REVIEW.

## Mechanical enforcement

| Gate | Where | What it does |
|---|---|---|
| `.claude/hooks/per-item-guarantee-check.sh` | runs on `pre-pr-gate.sh` + `make wave-guarantee-check` | scans every `.claude/plans/active-plan*.md` for the 15-row + 7-row matrices; fails if any item lacks them |
| `make wave-guarantee-check` | `Makefile` target | invokes the hook explicitly; CI gate |
| Pre-PR gate | `.claude/hooks/pre-pr-gate.sh` | calls the matrix check before any PR opens |
| `plan-verify.sh` | existing hook | extended to also check matrix presence |

## Per-session automation (operator demand: every Claude session does everything automatically)

Already wired in `.claude/hooks/session-context-brief.sh` (committed earlier).
Every NEW Claude Code session auto-runs at start:

| Step | Tool | What it surfaces |
|---|---|---|
| 1 | `mcp__tickvault-logs__run_doctor` | 7-section health snapshot |
| 2 | `mcp__tickvault-logs__summary_snapshot` | last hour ERROR signatures |
| 3 | `mcp__tickvault-logs__run_doctor` (CloudWatch alarms) | active firing alerts |
| 4 | `session-context-brief.sh` | active plans + open PR + errors-last-hour + 5-step protocol |
| 5 | `session-auto-health.sh` | doctor + validate-automation + mcp-doctor (background) |
| 6 | `session-sanity.sh` | branch + uncommitted check + auto-save remote |

These are SessionStart hooks in `.claude/settings.json` lines 168-181.
**Fire automatically on every session, no manual setup.**

## Per-session deep-research demand

Operator demand: "always activate and use all the agents and all subagents
as well to do a deep thorough research."

For any decision touching > 3 crates or > 1,000 LoC: spawn 3 specialist
agents in parallel per `wave-4-shared-preamble.md` §3:
1. `hot-path-reviewer` — review for hot-path violations
2. `security-reviewer` — review for secret exposure / ILP injection / I-P1-11
3. `general-purpose` (hostile bug-hunt) — race conditions, edge-trigger,
   market-hours gates, false-OK, integration with parallel plans

Wait for all 3, synthesize into CRITICAL/HIGH/MEDIUM/LOW table, fix every
CRITICAL+HIGH inline before opening the PR.

## Trigger

This rule activates when editing files matching:
- `.claude/plans/active-plan*.md`
- `.claude/plans/archive/*.md`
- `.claude/rules/project/wave-*-error-codes.md`
- `.claude/rules/project/wave-*-shared-preamble.md`
- Any file mentioning "Wave N", "Item N", "Phase N", "Block N" in plan context
