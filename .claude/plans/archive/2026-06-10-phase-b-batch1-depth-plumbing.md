# Implementation Plan: Phase B Module Deletions (deletion-audit execution)

**Status:** VERIFIED
**Date:** 2026-06-10
**Approved by:** Parthiban, 2026-06-10 (verbatim): "yes go ahead with your recommendation" — authorizing Phase B (module deletions) of `.claude/plans/active-plan-deletion-audit.md`, with the additional operator constraint in the same task: "Do NOT touch indicators/ or strategy/ (operator boundary §28)."

**Guarantee matrices:** See `per-wave-guarantee-matrix.md` (15-row + 7-row matrices cross-referenced per the canonical rule; this is a deletion-only change — no new pub fns, no new tables, no new hot-path code).

---

## Design

Phase B of the deletion audit lists 15 steps. The audit is dated **2026-05-25**; live-repo verification on 2026-06-10 shows most steps are **stale, already done, or superseded by post-audit operator locks**. This plan executes the verified-still-dead subset and records the evidence for every skip.

### Steps EXECUTED this session

**Batch 1 — dead depth-20/200 telemetry plumbing (PR 1, ~400 LoC, crates: `api`, `app`, `core`).**
The depth-20/200 WebSocket pools were deleted in AWS-lifecycle PR #4 (2026-05-19). The telemetry plumbing survived:
- `crates/api/src/state.rs` — `depth_20_connections`/`depth_200_connections` AtomicU64s + 4 set/get methods. The setters have **zero production call sites** (grep proof: only test files call `set_depth_*`), so the counters are permanently 0.
- `crates/api/src/handlers/health.rs` — `/health` permanently reports `depth_20: disconnected` / `depth_200: disconnected` (false-alarm noise).
- `crates/app/src/main.rs` — 09:14:00 readiness, 09:15:30 heartbeat, and 10s SLO scheduler all read the permanently-zero atomics; the SLO `active_total` adds two constant zeros.
- `crates/core/src/notification/events.rs` — `depth_20_active`/`depth_200_active` fields on `MarketOpenReadinessConfirmation`, `MarketOpenStreamingConfirmation`, `MarketOpenStreamingFailed`; the daily market-open Telegram prints dead "Depth-20: 0/4 / Depth-200: 0/4" lines.
- `crates/api/tests/panic_safety.rs` — struct-literal fields for the removed `SubsystemStatus` fields.

Action: remove the fields, methods, reads, event fields, format lines, and the 2 dedicated state.rs test fns; edit (not delete) tests that also cover surviving behaviour. Update the test-count baseline with justification in the same commit.

**Follow-up candidate (NOT part of this plan — needs its own approved plan): unconstructed depth-era NotificationEvent variants (crates: `core`).**
`DepthRebalanced`, `DepthRebalanceFailed`, `Depth20DynamicTopSetEmpty`, `Depth20DynamicSwapChannelBroken`, `MidMarketBootComplete`, `Phase2Complete`, `Phase2Failed` have zero construction sites outside `crates/core/src/notification/` (grep evidence captured 2026-06-10). Deleting them requires coordinated updates to `crates/core/tests/depth_rebalance_telegram_suppression_guard.rs` (3 source-scan ratchets pin the suppression code) and `.claude/rules/project/observability-architecture.md` clauses 7/8/10 — a rules+code coordinated PR with its own plan.

### Steps SKIPPED with evidence (stale / superseded / false premise)

| Audit step | Skip reason (verified 2026-06-10) |
|---|---|
| B-2 TF 21→2 collapse | SUPERSEDED: operator Quote 3 (2026-05-27, `daily-universe-scope-expansion-2026-05-27.md` §0) locks "calculating entire 21 timeframes"; §31 (2026-06-06) re-budgets RAM at "~1,000 SIDs × 21 TFs". The audit predates both locks. |
| B-3 `core_pinning.rs` | ALREADY DONE — file absent (`ls` fails). |
| B-4 `bin/smoke_test.rs` | FALSE PREMISE — audit said "DROP **if** it's the depth-200 smoke test". It is NOT: it's the S7-Step4 AWS deploy gate (config/sandbox/spill/locked-facts) with lockdown guard `crates/common/tests/smoke_test_binary_wiring.rs` ("AWS deploy workflow depends on it"). The depth-200 smoke (`boot_smoke_test.rs`) is already deleted. |
| B-6 observability.rs Prom/OTLP slim | Belongs to `active-plan-observability-cloudwatch-only.md` (separate in-progress plan); otel quartet was version-bumped 2 commits ago (#1077) — actively maintained, not dead. |
| B-7 trading::oms/risk deletion + B-8 trading_pipeline deletion | SUPERSEDED by post-audit S7 sandbox work: `config/base.toml` now carries `mode = "sandbox"`, `sandbox_only_until = "2026-06-30"`; `config/strategies.toml` exists so `init_trading_pipeline` returns `Some` and the pipeline spawns at boot; the deploy smoke gate checks the sandbox window. Live trading gate opens in 20 days — deleting the paper-trading runtime now contradicts the trajectory and the §28 boundary's spirit. Also blocked mechanically: `gap-enforcement.md` mandates OMS-GAP-01..06 + RISK-GAP-01..03 tests; `oms::rate_limiter` is consumed by live boot paths (`prev_day_ohlcv_boot.rs`, `cross_verify_1m_boot.rs`); `risk::tick_gap_tracker` is wired at `main.rs:5821`; `oms::types` consumed by `orphan_position_watchdog.rs` + api contract tests. Requires a fresh operator decision + its own plan. |
| B-9..B-13 4-SID slims (candle_fetcher, subscription_planner, tick_processor, connection_pool, events bulk-slim) | SUPERSEDED — premised on the 4-SID `LOCKED_UNIVERSE`, which the 2026-05-27 daily-universe (~250 SIDs) + 2026-06-06 NTM (§31, cap 1200) locks replaced. The planner/processor/pool are sized for the live universe again. |
| B-14 `order_types.rs` | OMS stays (B-7 skipped) → stays. |
| B-15 dead-test sweep | Follows its parent deletions; only the tests of code deleted in Batches 1-2 are removed. |

## Edge Cases

- `HealthResponse` JSON shape changes: `/health` consumers lose `subsystems.depth_20`/`subsystems.depth_200`. Verified consumers: `make doctor` / MCP `run_doctor` (reads overall + named fields defensively), `scripts/` health checks. `overall_status()` never used depth (verified by read) — no status-flap risk.
- Telegram market-open messages lose the two depth lines — operator-facing wording change, correct per the retired-depth reality (the lines have read "0/4" since 2026-05-19, which is misleading noise).
- `MarketOpenStreamingFailed` shares the depth fields — all three variants change together, keeping the events compilable.
- Tests that call `set_depth_*` solely to exercise the setter are deleted (2 fns); tests that incidentally assert the default-0 values are edited to drop only those asserts.
- Test-count ratchet only counts up: baseline update is included WITH justification in the same commit (net −2 test fns in `api`, possibly −1..−3 in `core` test literals which are edits, not deletions).
- The SLO `ws_health` denominator (`slo_ws_expected_total`) already excludes depth (main-feed + order-update only, verified at `main.rs:3840-3846`) — removing two zero-summands from the numerator cannot change any score.

## Failure Modes

- **Compile break in an unseen consumer:** mitigated by repo-wide `rg` proof per symbol before deletion (pasted in PR body) + `cargo build --workspace` before commit.
- **Ratchet/guard failure (test count, pub-fn wiring, banned patterns):** full pre-commit + pre-push gate run; baseline updated with justification; no `--no-verify` ever.
- **Hidden serde consumers of the `/health` fields:** the fields are `&'static str`-status structs serialized outward only; no deserializer of `SubsystemStatus` exists in-repo (grep proof in PR).
- **CI divergence from local:** PR opened as draft, CI monitored to green before ready-for-review; auto-merge (squash) only after gates pass.
- **Batch 2 ratchet web (suppression guards + rule clauses):** executed only as its own PR with its own grep proofs; if any guard interaction is ambiguous, stop and report rather than force.

## Test Plan

- Scoped tests per touched crate after each batch: `cargo test -p tickvault-api -p tickvault-core -p tickvault-app` (the three touched crates; `common` untouched → no workspace escalation per `testing-scope.md`).
- `cargo build --workspace` + `cargo fmt --check` + `cargo clippy --workspace -- -D warnings -W clippy::perf`.
- `bash .claude/hooks/banned-pattern-scanner.sh` + plan-gate + pre-push gates.
- Evidence discipline: paste the actual `test result: ok. N passed` lines in the PR body.

## Rollback

- Each batch is a single squash-merged PR; `git revert <sha>` restores the deleted plumbing verbatim (pure deletion, no migrations, no data-shape changes in QuestDB).
- No config/table/schema changes anywhere in this plan — rollback is source-only.

## Observability

- No new code paths → no new telemetry needed. The change REMOVES permanently-zero gauges from `/health` and dead lines from 3 Telegram events; the surviving fields (websocket, order_update, questdb, token, pipeline, tick_persistence) are untouched.
- The SLO score pipeline (SLO-01/SLO-02) is verified unchanged: numerator loses two constant zeros; denominator already excluded depth.
- PR body carries before/after of the `/health` JSON and the market-open Telegram text so the operator sees the exact operator-facing delta.

## Plan Items

- [x] Batch 1 — dead depth-20/200 telemetry plumbing
  - Files: crates/api/src/state.rs, crates/api/src/handlers/health.rs, crates/api/tests/panic_safety.rs, crates/app/src/main.rs, crates/core/src/notification/events.rs
  - Tests: existing suites in tickvault-api / tickvault-core / tickvault-app (deletion-only; 4 dead-setter test fns removed with baseline justification in the commit body)
  - Evidence: cargo test green — api `178 passed` (lib) + integration suites all ok; app `568 passed` (lib) + integration ok; core `2359 passed` (lib, `--features daily_universe_fetcher` per CI) + integration ok. CI-exact clippy (`--workspace --no-deps`): zero warnings in touched files (1 pre-existing warning in `constituent_resolver.rs`, untouched).

(The depth-era NotificationEvent variant deletion is deliberately NOT a plan item — see "Follow-up candidate" in Design; it requires its own coordinated rules+code plan.)
