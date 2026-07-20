# Implementation Plan: Cadence degraded-leg recovery — hedged T+4s native retry, background history re-pull

**Status:** APPROVED
**Date:** 2026-07-20
**Approved by:** Parthiban (operator) — ZERO-PROMPT standing pre-authorization; dated operator quote (2026-07-20, verbatim intent): "retry native until the 4th second, then cross-fill; backfill never feeds live decisions"
**Branch:** `claude/cadence-t4s-retry` (stacked on `claude/cadence-verify-fixes` / PR #1689 — builds ON its Malformed-vs-Empty classification)
**Crates:** tickvault-core (`crates/core/src/cadence/runner.rs`, `crates/core/src/cadence/ladder.rs`, new `crates/core/src/cadence/history_repull.rs`), tickvault-common (`crates/common/src/constants.rs`, `crates/common/src/error_code.rs`)

## Design

Three mechanisms, BOTH brokers symmetric (Dhan + Groww), all cold-path (the cadence scheduler lane — never the tick hot path):

1. **Hedged T+4s decision deadline + class-aware retry budgets.** A 2xx-EMPTY leg answer (~T+1.3s) already schedules bounded in-cycle retries through the EXISTING gated dispatch machinery; this PR makes the budgets class-aware (`ladder::late_retry_budget` — the designed policy seam) and DERIVES the feasible grid from the DhanGates constants per §0d H4: **spots** get 2 Empty-class retries on the appended rolling-window grid (`CADENCE_LATE_RETRY_SPOT_OFFSETS_MS = [2_000, 3_000]` at the default T+1s burst — answers ~+300ms, inside the deadline); **chains** get the single legal slot (`CADENCE_LATE_RETRY_CHAIN_OFFSET_MS = 4_000` — the ≥3s per-key CAS gate makes anything earlier infeasible; the existing `dhan_chain_retry_slots_ms` grid). A new `CycleAction::NativeDeadline` event at `CADENCE_NATIVE_DECISION_DEADLINE_MS = 4_000` forces the fallback rungs (cross-fill + chain-embedded) for a still-incomplete lane — regardless of in-flight retries, ZERO added wait; retries queued at/after T+4.0 on a deadline-resolved lane are skipped (the re-pull owns their record recovery). When the donor has nothing fresh, the lane continues on the UNCHANGED retry/cutoff machinery. Healthy lanes are byte-untouched (no deadline effect — already resolved). **Retry-eligibility set (explicit):** Empty YES (spot 2 / chain 1); Timeout/Transport/Auth/QueueDelay = the pre-existing single retry (single-flight per leg — retries schedule only AT completion, the §3c first-write-wins precedent); RateLimited = its one retry, and a 429 ON a retry finds the budget spent (remaining retries abort); **Malformed NEVER** (schema break — same-second retry re-fetches the same broken shape; the Groww per-leg malformed latch excludes it from verdict/L3 fallback symmetrically). Stale-answer guard = the executor's own wrong-minute filter (`empty_stale` → Empty).
2. **Per-lane retry outcome state (sibling seam).** `LaneRun` carries `late_retry_attempts` + `resolution` ∈ {`native_first_try`, `native_late_retry`, `cross_fill`} — the LOCKED vocabulary of #1688's `cross_fill_audit` (merged on main, OUTSIDE this stacked base): the runner computes the token at decide time and the CADENCE-01 wrap-up line carries `attempts` + `resolution`; the audit-row wiring is a marked `SEAM(#1688)` — whoever lands second rebases. THIS PR writes NO cross_fill_audit rows.
3. **Background history re-pull.** When the decision resolved via cross-fill, a DETACHED task (`history_repull.rs`) re-asks the degraded broker's OWN CrossSource legs at `CADENCE_HISTORY_REPULL_OFFSETS_MS = [30_000, 50_000]` (2 waves — T+50s NOT T+60s, which lands ON the next boundary; §0d H5), non-blocking DhanGates acquire per Dhan leg (contended → `gate_skipped`, nominal traffic always wins), the executor's EXISTING DEDUP-keyed upsert. HARD-ISOLATED from decisions (operator rule) via a build-failing source-scan ratchet — the module cannot name assembly/decision/ladder/cycle-state types, so a re-pull 429 can NEVER arm the shape ladder or feed `rate_limited` streaks. Skipped in dry-run (F10 noise posture).
4. **Dhan client pre-warm — DEFERRED (dated note).** reqwest exposes no TLS-handshake-only warm-up through its pool; a real pre-warm is a recurring HTTP GET requiring a `no-rest-except-live-feed-2026-06-27.md` §3 KEEP row (operator surface — §0d M13). `CADENCE_DHAN_PREWARM_LEAD_MS` stays pinned RESERVED; no code consumes it.

## Edge Cases

- BOTH brokers degraded the same minute: per-lane independent budgets; the deadline finds no donor → no premature skip, the existing cutoff machinery owns both lanes (pinned by the all-Empty dry-run suite's exact fire counts: Dhan 6 chains + 12 spots, Groww 6 + 8 — loud, bounded).
- Deadline race determinism: the T+4s check runs at ONE event; native is used iff its completion was processed before that instant (biased select drains queued completions first); a tie at the instant resolves to the prepared fallback (test-pinned: the same-ms chain-retry slot sorts AFTER the deadline event and is skipped on a resolved lane).
- Retry vs next volley: every retry offset + the deadline end far before the next minute fire; a deadline-resolved lane skips its post-deadline retries; re-pull waves complete ~T+51s < the next burst.
- Restart mid-window: retry/re-pull state is in-memory only; a restart loses at most one minute's recovery (the next cycle is clean; `repull_sleep_ms` clamps a late spawn to fire-immediately, still 2 bounded waves).
- 429 mid-retry: the spent RateLimited class budget aborts the remaining micro-retries (`retry_rate_limited` coalesced CADENCE-05); never a storm.
- Day/expiry boundary: retry dispatches re-read the DAY-LOCKED expiry store (stable within the day — never a mid-minute re-resolution to a different value); re-pull expiries are resolved AT SPAWN by the runner and passed as plain data.
- Late native answer AFTER resolution: audit-only late response (existing counter); the executor already persisted its rows — the re-pull's DEDUP upsert is idempotent against them.

## Failure Modes

- Retry request errors: counted `tv_cadence_late_retry_total{outcome="error"}`; remaining budget still applies; the deadline is unaffected (fallback already prepared).
- All retries empty: coalesced CADENCE-05 `retry_still_empty` + `outcome="exhausted"`; resolution=`cross_fill` — no new degrade class.
- Re-pull failure after both waves: one coded CADENCE-05 `repull_exhausted` `error!` + counter; log-sink-only (the Dhan noise-lock posture), never more attempts.
- Wrong-minute vendor rows: the executor's `empty_stale` class (→ Empty) — never enters assembly.
- Budget safety: every Dhan retry AND re-pull fire passes the DhanGates (cap-5 rolling ring + per-key CAS) at dispatch — structurally inside the broker budget; Groww rides its executor-internal pacing (300/min share).

## Test Plan

Scoped: `TMPDIR=/home/user/tmp cargo test -p tickvault-core -p tickvault-common` (+ `-p tickvault-app`).
- `test_late_retry_offsets_and_deadline_pinned` — DERIVED constants (spot grid = burst + k·window; chain = burst + spacing floor; budget == grid len; all spot offsets < deadline).
- `test_repull_offsets_pinned` — [30_000, 50_000], both > deadline and < 60_000 (never ON the boundary).
- `test_late_retry_eligibility_set_pinned` — the explicit eligibility set incl. Malformed=0 and the 429-mid-retry abort.
- `test_deadline_race_deterministic` — runner-level: Dhan all-Empty + Groww healthy ⇒ deadline cross-fills at T+4.0, post-deadline retries skipped (exact fire counts); the no-donor case keeps full retry fire counts (all-Empty suite).
- `test_history_repull_isolation_ratchet` — build-failing source-scan: the re-pull module names NO assembly/decision/ladder/cycle-state type.
- `test_repull_sleep_ms_clamps_and_targets` + `test_classify_repull_result_covers_all_classes` + `test_plan_pending_bookkeeping` — the re-pull pure core.
- `ratchet_runner_spawns_history_repull_for_both_lanes` — source-scan: the wrap-up spawns the re-pull for BOTH lanes, gated on resolution==cross_fill and !dry_run.
- Existing suites updated to the deadline semantics: `cadence_fallback_proof` (6-fire signature), `cadence_runner_dry_run` (all-Empty exact counts; mid-ladder cycle-3 4-fire), `cadence_zero_429_replay` (new signature).
- The error-code crossref + runbook-path suites stay green (the CADENCE-05 rule-file section lands in the SAME PR; CADENCE-04 was consumed by #1688 on main — collision avoided deliberately).

## Rollback

Additive-only: new constants, one new module, one new ErrorCode variant, two counters, one new cycle event. Revert = revert the ONE PR; the empty-arm → cross-fill path returns byte-identical to the base. No schema/table changes (the sibling session owns cross_fill_audit). No config flips: the recovery rides the existing `[cadence] enabled` gate.

## Observability

- `ErrorCode::Cadence05RecoveryDegraded` (`code_str() == "CADENCE-05"` — CADENCE-04 taken by #1688's audit-write class), log-sink-only; runbook = `.claude/rules/project/cadence-error-codes.md` §CADENCE-05 (same PR, dated 2026-07-20 operator quote).
- Counters: `tv_cadence_late_retry_total{lane,outcome}` (native_landed|empty_again|rate_limited|error|exhausted) and `tv_cadence_history_repull_total{lane,outcome}` (recovered|empty|gate_skipped|error|exhausted).
- The CADENCE-01 wrap-up line gains `attempts` + `resolution` fields — the same coalesced posture.

## Plan Items

- [x] Pinned constants + derived-grid tests (audit-reconciled: spot [2_000, 3_000] / chain 4_000 / re-pull [30_000, 50_000])
  - Files: crates/common/src/constants.rs
  - Tests: test_late_retry_offsets_and_deadline_pinned, test_repull_offsets_pinned
- [x] Class-aware retry eligibility (Malformed never; spot Empty ×2; Groww malformed latch)
  - Files: crates/core/src/cadence/ladder.rs, crates/core/src/cadence/runner.rs
  - Tests: test_late_retry_eligibility_set_pinned
- [x] T+4s NativeDeadline event + resolution/attempts state + CADENCE-05 wrap-up + counters
  - Files: crates/core/src/cadence/runner.rs
  - Tests: test_deadline_race_deterministic
- [x] Background history re-pull + isolation ratchet + spawn wiring
  - Files: crates/core/src/cadence/history_repull.rs, crates/core/src/cadence/runner.rs, crates/core/src/cadence/mod.rs
  - Tests: test_history_repull_isolation_ratchet, test_repull_sleep_ms_clamps_and_targets, test_classify_repull_result_covers_all_classes, test_plan_pending_bookkeeping, ratchet_runner_spawns_history_repull_for_both_lanes
- [x] Dhan pre-warm — DEFERRED with dated note (reserved constant only, no code; §0d M13 KEEP-row is an operator surface)
  - Files: crates/common/src/constants.rs, .claude/rules/project/cadence-error-codes.md
  - Tests: test_prewarm_lead_pinned
- [x] CADENCE-05 ErrorCode + rule-file section + triage rule + dated operator quote
  - Files: crates/common/src/error_code.rs, .claude/rules/project/cadence-error-codes.md, .claude/triage/error-rules.yaml
  - Tests: test_cadence_error_codes_pinned
- [x] Dhan support-ticket draft (open-minute publication SLA)
  - Files: docs/dhan-support/2026-07-20-intraday-open-minute-sla.md
  - Tests: n/a (docs)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan spot empty at T+1.3s, native lands at T+2.3s | decision uses native (`native_late_retry`) |
| 2 | Dhan empty through the grid | cross-fill at T+4.0s, zero extra wait; T+30/T+50 re-pull upserts Dhan's own rows |
| 3 | 429 on a retry | remaining retries aborted (spent budget); CADENCE-05 `retry_rate_limited`; no storm |
| 4 | BOTH brokers empty | no donor at the deadline → existing cutoff machinery unchanged; bounded loud retries per lane |
| 5 | Healthy minute | byte-identical to today (`native_first_try`, attempts=0, zero retry tasks) |
| 6 | Malformed body | never retried; cross-fill covers; shape-drift page owns diagnosis |

## Per-Item Guarantee Matrix

Cross-reference (mandatory): every item above carries the 15-row "100% everything" matrix + the 7-row resilience matrix from `.claude/rules/project/per-wave-guarantee-matrix.md` — applied per that rule's cross-reference clause; the honest 100% claim wording binds the PR body. Honest envelope: the micro-retries recover SMALL vendor lag (answers by ~T+3.3s); the ~2-minute market-open lag (2026-07-20 09:15/09:16 IST evidence) is covered only by cross-fill + the background re-pull; nothing stronger is claimed.
