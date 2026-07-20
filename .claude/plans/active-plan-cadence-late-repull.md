# Implementation Plan: Cadence degraded-leg recovery — hedged T+4s native retry, background history re-pull, Dhan pre-warm

**Status:** APPROVED
**Date:** 2026-07-20
**Approved by:** Parthiban (operator) — ZERO-PROMPT standing pre-authorization; dated operator quote (2026-07-20, verbatim intent): "retry native until the 4th second, then cross-fill; backfill never feeds live decisions"
**Branch:** `claude/cadence-t4s-retry`
**Crates:** tickvault-core (`crates/core/src/cadence/runner.rs`, `crates/core/src/cadence/assembly.rs`, new `crates/core/src/cadence/history_repull.rs`), tickvault-common (`crates/common/src/constants.rs`, `crates/common/src/error_code.rs`), tickvault-app (`crates/app/src/dhan_cadence_executor.rs`, `crates/app/src/cadence_boot.rs`)

## Design

Three mechanisms, BOTH brokers symmetric (Dhan + Groww), all cold-path (the cadence scheduler lane — never the tick hot path):

1. **Hedged T+4s native retry.** Today a 2xx-EMPTY leg answer (~T+1.3s after minute close; `DegradeFlags.chain_empty` set at runner.rs ~2367, `spot_empty` ~2530) goes straight to cross-fill at the assembly arm (runner.rs ~2774–2785). NEW: on an empty FIRST answer the lane schedules bounded micro-retries of ONLY the missing legs at pinned offsets after minute close — `CADENCE_LATE_RETRY_OFFSETS_MS = [2_000, 3_000, 3_800]` — while cross-fill preparation continues in parallel exactly as today. Hard pinned decision deadline `CADENCE_NATIVE_DECISION_DEADLINE_MS = 4_000`: native rows arrived before the deadline → the decision uses them; else the already-prepared cross-fill applies with ZERO extra wait. Stale-answer guard: a retry answer is accepted only when it carries the REQUESTED cycle minute; anything else is dropped + counted. Healthy first-try lanes are byte-untouched (no retry task is ever spawned).
2. **Per-lane retry outcome state (sibling seam).** At the cross-fill choke point the runner exposes `attempts: u32`, `resolved_at_ms_after_close: Option<i64>` and a resolution token ∈ {`native_first_try`, `native_late_retry`, `cross_fill`} — the LOCKED vocabulary of sibling session cse_014GANZBjTWv1iuZnPjdT6ZH's `cross_fill_audit` sink (threaded from `cadence_boot.rs`; the runner NEVER imports storage). THIS PR writes NO cross_fill_audit rows — the sibling owns that table; whoever lands second rebases.
3. **Background history re-pull.** When the deadline resolved via cross-fill, a DETACHED task re-asks the degraded broker's own leg at `CADENCE_HISTORY_REPULL_OFFSETS_MS = [30_000, 60_000]` (exactly 2 attempts), upserting the broker's OWN rows via the EXISTING DEDUP keys (feed-in-key). HARD-ISOLATED from decisions (operator rule): the task gets no handle to assembly/decision state — persistence sink only; a build-failing ratchet test pins the isolation. Background re-pull is NEVER a resolution value in any audit.
4. **Dhan client pre-warm.** `CADENCE_DHAN_PREWARM_LEAD_MS = 5_000` before each in-session minute close, one bounded lightweight GET on the ALREADY-POOLED shared `reqwest::Client` (dhan_cadence_executor.rs ~:186/:213) re-establishes idle-closed TLS before the critical window (the Groww §9.7 warm-up precedent). Best-effort; never delays the fire.

## Edge Cases

- BOTH brokers degraded the same minute: per-lane independent retry state; each lane retries only its own missing legs within its own broker budget; cross-fill may then be impossible (both empty) — the verdict stays the existing CADENCE-01 degrade; retries add at most 3 bounded extra requests per lane.
- Deadline race determinism: the T+4s check runs at ONE select point; a native answer is used iff fully assembled before that instant — ties resolve deterministically to cross-fill (test-pinned).
- Retry vs next volley: all retry offsets (≤3.8s) + the deadline (4s) end far before the next minute fire; any in-flight retry is aborted at cycle end — a retry can never cross into the next cycle.
- Restart mid-window: retry/re-pull state is in-memory only; a restart loses at most one minute's recovery (the next cycle is clean; history stays recoverable via the normal lanes).
- 429 mid-retry: the remaining micro-retries for that minute are ABORTED (`outcome=rate_limited`), honoring the executor ladder; never a storm.
- Day/expiry boundary: retries reuse the cycle's already-resolved expiry — never re-resolve mid-minute; the pre-warm is skipped when the next minute is outside the session.
- Late native answer AFTER the deadline: discarded + counted (never a decision input, never persisted from the retry path — the T+30/T+60 re-pull is the history mechanism).

## Failure Modes

- Retry request errors (HTTP error/timeout): counted `outcome=error`, remaining offsets still tried, deadline unaffected (cross-fill already prepared).
- All retries empty: `outcome=empty`, resolution=`cross_fill` exactly as today — no new degrade class.
- Re-pull failure after 2 attempts: one coded CADENCE-04 `error!` line + counter; never a page (log-sink-only per the Dhan noise-lock posture), never more attempts.
- Pre-warm failure: ignored (debug-level); the real fire carries its own timeout.
- Stale/wrong-minute answers: dropped + counted (`outcome=stale`).
- Budget safety: const-asserted worst case — 3 retries + 1 pre-warm + the normal volley stays inside the existing per-minute Dhan Data-API budget (the 3 rps shared limiter) and the Groww 300/min share.

## Test Plan

Scoped: `TMPDIR=/home/user/tmp cargo test -p tickvault-core -p tickvault-common` (+ `-p tickvault-app` for executor/boot tests).
- `test_late_retry_offsets_and_deadline_pinned` — pinned constants + orderings (offsets strictly ascending, all < deadline < next minute fire).
- `test_deadline_race_deterministic` — native-arrived-before vs -after the deadline; tie → cross-fill.
- `test_stale_minute_answer_dropped` — a wrong-minute retry answer never enters assembly.
- `test_healthy_lane_spawns_no_retry` — first-try success ⇒ zero retry tasks, resolution=`native_first_try`, attempts=0.
- `test_429_aborts_remaining_retries`.
- `test_history_repull_isolation_ratchet` — build-failing source-scan: the re-pull module references NO assembly/decision types (the operator isolation rule).
- `test_repull_offsets_pinned` — [30_000, 60_000], exactly 2 attempts.
- `test_prewarm_lead_pinned` + a pure lead-time computation test (app crate).
- The error-code crossref + runbook-path suites stay green (the CADENCE-04 rule-file section lands in the SAME PR).

## Rollback

Additive-only: new constants, one new module, one new ErrorCode variant, two counters. Revert = revert the ONE PR; the empty-arm → cross-fill path returns byte-identical to today. No schema/table changes (the sibling session owns cross_fill_audit). No config flips: the recovery rides the existing `[cadence] enabled` gate.

## Observability

- NEW `ErrorCode` variant with `code_str() == "CADENCE-04"`, log-sink-only (no Telegram, no CloudWatch alarm — the noise-lock posture); runbook = `.claude/rules/project/cadence-error-codes.md` §CADENCE-04 (added in the SAME PR, carrying the dated 2026-07-20 operator quote).
- Counters: `tv_cadence_late_retry_total{lane,outcome}` (outcome ∈ recovered|empty|error|stale|rate_limited) and `tv_cadence_history_repull_total{lane,outcome}` (outcome ∈ recovered|empty|error).
- The existing CADENCE-01 wrap-up line (runner.rs ~1614/1638) gains attempts + resolution fields — the same coalesced posture.

## Plan Items

- [ ] Pinned constants + budget const-asserts
  - Files: crates/common/src/constants.rs
  - Tests: test_late_retry_offsets_and_deadline_pinned, test_repull_offsets_pinned
- [ ] Hedged T+4s retry + deadline race + stale guard + outcome state
  - Files: crates/core/src/cadence/runner.rs, crates/core/src/cadence/assembly.rs
  - Tests: test_deadline_race_deterministic, test_stale_minute_answer_dropped, test_healthy_lane_spawns_no_retry, test_429_aborts_remaining_retries
- [ ] Background history re-pull + isolation ratchet
  - Files: crates/core/src/cadence/history_repull.rs, crates/app/src/cadence_boot.rs
  - Tests: test_history_repull_isolation_ratchet, test_repull_offsets_pinned
- [ ] Dhan pre-warm (T−5s)
  - Files: crates/app/src/dhan_cadence_executor.rs
  - Tests: test_prewarm_lead_pinned
- [ ] CADENCE-04 + counters + rule-file section + dated operator quote
  - Files: crates/common/src/error_code.rs, .claude/rules/project/cadence-error-codes.md
  - Tests: error_code_rule_file_crossref, error_code_tag_guard
- [ ] Dhan support-ticket draft (open-minute publication SLA)
  - Files: docs/dhan-support/2026-07-20-intraday-open-minute-sla.md
  - Tests: n/a (docs)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan empty at T+1.3s, native lands at T+2.1s | decision uses native (`native_late_retry`, attempts=1) |
| 2 | Dhan empty through T+3.8s | cross-fill at T+4s, zero extra wait; T+30/T+60 re-pull upserts Dhan's own rows |
| 3 | 429 on the first retry | remaining retries aborted; cross-fill; no storm |
| 4 | BOTH brokers empty | CADENCE-01 degrade as today; ≤3 bounded retries per lane |
| 5 | Healthy minute | byte-identical to today (`native_first_try`, attempts=0) |

## Per-Item Guarantee Matrix

Cross-reference (mandatory): every item above carries the 15-row "100% everything" matrix + the 7-row resilience matrix from `.claude/rules/project/per-wave-guarantee-matrix.md` — applied per that rule's cross-reference clause; the honest 100% claim wording binds the PR body. Honest envelope: the micro-retries recover SMALL vendor lag (a few seconds); the ~2-minute market-open lag (2026-07-20 09:15/09:16 IST evidence) is covered only by cross-fill + the background re-pull; nothing stronger is claimed.
