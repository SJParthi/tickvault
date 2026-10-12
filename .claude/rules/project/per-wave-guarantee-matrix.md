# Per-Wave / Per-Item / Per-Block Guarantee Matrix (PROJECT-WIDE, MANDATORY)

> **Authority:** CLAUDE.md > this file > defaults.
> **Scope:** Every wave plan, every item, every block, every phase doc in
> `.claude/plans/active-plan*.md` MUST carry the matrix below.
> **Enforcement:** Mechanical via `.claude/hooks/per-item-guarantee-check.sh`
> + `make wave-guarantee-check` (CI gate). Items without the matrix fail
> the build at PR-merge.
> **Trigger:** Always loaded for any session editing a plan file.
> **⚠ CORRECTED 2026-10-12 (deep audit):** nothing in `.github/` or in
> `pre-pr-gate.sh` runs the matrix check, so it is NOT a CI gate and does not
> fail a PR. It runs only when someone runs `make wave-guarantee-check` or
> the hook by hand. The table under "Mechanical enforcement" is wrong in the
> same way.

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
| 100% code coverage | `quality/crate-coverage-thresholds.toml` ratcheted per-crate floors (72.6–99.4, target 100%), floors only move up; `scripts/coverage-gate.sh` | ~~post-merge llvm-cov report~~ **⚠ CORRECTED 2026-10-11:** llvm-cov on every PR, inside All Green (the `coverage-and-perf` job is in its needs list), and again after merge | item PR includes coverage delta |
| 100% audit coverage | `<event>_audit` table per typed event with DEDUP UPSERT KEYS | `mcp__tickvault-logs__questdb_sql` | item adds/extends audit table |
| 100% testing coverage | 22 test categories per `testing.md` (unit/integration/property/loom/dhat/fuzz/mutation/sanitizer/coverage/etc.) | `cargo test --workspace` green | item declares which 22 it covers |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan + 8 pre-commit gates | pre-push mandatory | all gates green |
| 100% code performance | DHAT zero-alloc + Criterion p99 (CORRECTED 2026-10-12, TRU-2: bench-gate reads Criterion's MEDIAN against the budget, after merge and nightly only; no percentile is measured) budgets + bench-gate ≤5% regression | `cargo bench` + `scripts/bench-gate.sh` | DHAT test if hot path |
| 100% monitoring | 7-layer telemetry (Prom counter + gauge + tracing span + Loki log + Telegram event + Grafana panel + audit table) | `mcp__tickvault-logs__run_doctor` | 9-box completes 7 layers |
| 100% logging | tracing macros mandatory (no println!/dbg!); ERROR → Telegram | hourly errors.jsonl rotation | every error path uses `error!` with `code` |
| 100% alerting | `alerts.yml` Prom rule + `resilience_sla_alert_guard.rs` ratchet | `mcp__tickvault-logs__run_doctor` (CloudWatch alarms) | item adds alert for new failure **⚠ CORRECTED 2026-10-12 (deep audit TRU-8): `resilience_sla_alert_guard.rs` does not exist in the tree; NOT ENFORCED.** |
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
| Zero ticks lost | Bounded zero loss inside chaos envelope: ~~ring 2M~~ ~~→ spill NDJSON → DLQ~~. **⚠ CORRECTED 2026-10-11:** there is no 2M ring. The live frame ring is one bounded channel with many producers, capped by `FRAME_RING_CAPACITY` (frames) and `FRAME_RING_MAX_BYTES` (bytes) in `dhan_feed_stack.rs` (read the constants); the frame is ~~written to the WAL at receipt~~ queued to the WAL writer at receipt, before the ring, so a full ring is a lag signal. ~~Loss paths that remain are counted and paged (spill channel full, spill writer dead, spill byte cap, WAL loss after a shed, rows dropped at the database writer).~~ **⚠ CORRECTED 2026-10-11 (later the same day):** ticks: WAL → ring → QuestDB, with the rescue spill and the inline fallback; the tick tier has NO DLQ (only candles have one: seal ring → spill → DLQ). Loss paths read from source 2026-10-11 include (not an exhaustive list): WAL and ring both refused (no recovery; paged), spill channel full, spill writer dead, spill byte cap, WAL loss after a shed, rescue rows abandoned at shutdown, offload writer shutdown incomplete, re-injected frames dropped, oversize frames refused; each is counted and paged. Rows dropped at the database writer are usually rescued to the spill or replayed from the WAL, so that page is not a loss on its own. Two losses are not counted: up to ~1 s plus the syncer's time on a power cut, and records still queued for the WAL when the process is killed. | item must not introduce new tick-drop path |
| WS never disconnects | DETECT ≤5s, reconnect with `SubscribeRxGuard`, sleep-until-open post-close | item must not break SubscribeRxGuard or pool watchdog |
| Never slow/locked/hanged | DHAT ≤4 alloc blocks/8KB across 10K calls; Criterion p99 (CORRECTED 2026-10-12, TRU-2: bench-gate reads Criterion's MEDIAN against the budget, after merge and nightly only; no percentile is measured) ≤100ns; tick-gap >30s Telegram; ~~core_affinity Core 0~~ **⚠ CORRECTED 2026-10-11:** the `core_affinity` crate is gone (removed 2026-08-10). The socket read workers are pinned to one core (core 1 by default, never core 0) by `reader_runtime`, through `libc`; nothing has measured that the pin helps. | item must not add hot-path allocation |
| QuestDB never fails | ABSORB via 3-tier rescue→spill→DLQ + schema self-heal | item must not break self-heal |
| O(1) latency | `from_le_bytes` + ~~`papaya` + `Arc<HashMap>` + SPSC bounded~~; bench-gate ≤5% regression. **⚠ CORRECTED 2026-10-11:** hot-path lookups are a plain `HashMap` on the composite key (the aggregator rejects papaya on purpose); papaya is used only where other tasks read concurrently. The frame ring is a bounded channel with many producers (one per socket read task), not single-producer. A depth packet is O(levels), a tick packet O(1). | item adds Criterion bench if hot path |
| Uniqueness + dedup | Composite `(security_id, exchange_segment)` per I-P1-11 + DEDUP UPSERT KEYS + meta-guard | item DEDUP key includes segment |
| Real-time proof | 7-layer telemetry + SLO-01/SLO-02 @ 10s + market-open self-test @ 09:16:30 IST | item ratchet pins all 7 layers |

## The honest 100% claim (mandatory wording)

When any plan / PR / commit body uses the phrase "100% guarantee", it MUST be
qualified exactly:

> "100% inside the tested envelope, with ratcheted regression coverage:
> ≤60s QuestDB outage absorbed by rescue→spill→DLQ;
> the derived seal-ring capacity `AGGREGATOR_MAX_SLOTS × TF_COUNT` (never restate this as a number: the docs quoted 200,000, then 525,000, 600,000 and 225,000 as `TF_COUNT` moved, and each went stale. Read the constant) (constant `SEAL_BUFFER_CAPACITY`,
> `crates/trading/src/candles/seal_ring.rs`, ratcheted by
> `seal_ring.rs::test_seal_buffer_capacity_constant_is_locked_value`)
> → NDJSON spill → DLQ; bench-gated
> O(1) hot path; composite-key uniqueness;
> calendar-tested 65h Fri 16:00 IST → Mon 09:00 IST weekend sleep-until-open
> schedule (`crates/core/tests/ws_sleep_resilience.rs` — date arithmetic only:
> no socket is held open across it; the box is stopped overnight and at
> weekends). Beyond the envelope,
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

> **⚠ CORRECTED 2026-10-12:** of the rows above, only the Makefile target
> calls the hook. `pre-pr-gate.sh` does not call it and no CI job runs it, so
> "CI gate" and "fails the build at PR-merge" are not true today.

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
