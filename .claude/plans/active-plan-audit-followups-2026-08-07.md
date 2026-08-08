# Implementation Plan: 2026-08-07 audit follow-ups (clock drift, CI guard wiring, strategy slot repair)

**Status:** VERIFIED
**Date:** 2026-08-07
**Approved by:** Parthiban (operator) — verbatim, this session, in direct response to a
message naming exactly these three fixes as "fix the clock" / "wire the guards" /
"fix the strategy": *"fi everyhtugn dude oaky?"*

Source: the 6-agent deep audit of 2026-08-07 (post PR #1726 / #1727). Every finding below
was personally re-verified in source before this plan was written.

## Plan Items

- [x] **Item 1 — Clock drift: 7 sites still hardcode the pre-NSE-CAS 15:30 close**
  - Files: `crates/app/src/feed_scoreboard_boot.rs`, `crates/app/src/spot_crossverify_boot.rs`,
    `crates/app/src/brutex_crossverify_compare.rs`, `crates/app/src/groww_option_chain_1m_boot.rs`,
    `crates/trading/src/oms/groww/user.rs`, `crates/aws-lambdas/src/operator_control.rs`
  - Tests: `session_end_pinned_to_canonical_close`, `market_hours_end_pinned_to_canonical_close`,
    plus repair of `feed_scoreboard_boot.rs` tests pinning the stale 55_800

- [x] **Item 2 — Wire the three ratchet guards into CI**
  - Files: `.github/workflows/ci.yml`
  - Tests: `crates/common/tests/audit_fix_guard.rs::ratchet_guards_are_wired_into_ci`

- [x] **Item 3 — Strategy evaluator slot repair (§28 frozen area)**
  - Files: `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md` (dated quote FIRST),
    `crates/trading/src/strategy/evaluator.rs`, `crates/trading/src/indicator/types.rs`,
    `crates/trading/src/indicator/engine.rs`,
    `crates/storage/tests/operator_boundary_indicator_strategy_guard.rs` (re-bless)
  - Tests: `test_evaluator_processes_banded_security_id`, `test_evaluator_slot_isolation`

## Design

**Item 1.** The 2026-08-03 NSE Closing Auction change moved session close 15:30 → 15:40
(`MARKET_CLOSE_IST_NANOS` 55_800 → 56_400). Modules carrying a
`const _: () = assert!(… == MARKET_CLOSE_IST_NANOS)` drift-pin failed the build and were
updated. Modules that hardcoded the literal silently kept the old boundary. Fix: replace each
local literal with a constant derived from / pinned to `MARKET_CLOSE_IST_NANOS`, using the
exact pattern already proven in `tf_consistency_boot.rs:138-145`.

**Item 2.** `test-count-guard.sh`, `pub-fn-test-guard.sh` and `financial-test-guard.sh` were
made fail-closed-under-CI earlier today, but appear in ZERO workflow files — verified by
grep. The fail-closed logic never executes server-side. Fix: invoke all three from the
existing `repo-guards` job (an existing All-Green-gated job, so no new job is added to the
fan-in — required by `merge-gate-lock-2026-07-04.md` §5).

**Item 3.** `evaluator.rs:52` computes `snapshot.security_id as usize` and bails when
`>= states.len()` (25_000). Since ids are namespace-banded (Groww `[2^62,2^63)`, GDF
`[2^60,2^62)`, TrueData `[2^59,2^60)`) every live id exceeds the bound, so `evaluate()`
returns `Signal::Hold` for every instrument, permanently and silently. This is the identical
defect §28.2 repaired in `IndicatorEngine`, one stage downstream: `engine.rs:406` populates
the snapshot with the RAW banded id rather than the dense slot it just resolved. Fix: carry
the already-resolved dense slot on the snapshot as a separate field and have the evaluator
index by that, leaving `security_id` untouched for all downstream consumers (audit rows,
logs, persistence) which legitimately need the real id.

## Edge Cases

- Item 1: a site may legitimately want the OLD boundary (e.g. a historical-replay window).
  Each is inspected individually; only session-gating uses are moved.
- Item 1: `feed_scoreboard_boot.rs:3983` asserts the stale value as the contract — the test
  must be repaired in the same commit or the correct fix reads as a regression.
- Item 3: capacity exhaustion — engine returns a default snapshot with no slot; the evaluator
  must treat that as Hold WITHOUT indexing (already covered by the `is_warm` check, which is
  false on a default snapshot).
- Item 3: `slot` must be within `states.len()`; the engine guarantees it by construction, but
  the evaluator keeps a bounds check as defence in depth.

## Failure Modes

- Item 1 fix wrong direction → in-session window widens past the real close, causing
  post-close noise. Mitigated by pinning to the canonical constant rather than a new literal.
- Item 2 guards fire on a fresh CI runner with no baseline → vacuous pass or spurious fail.
  Guards already fail closed when `CI` is set and the baseline is now tracked in git.
- Item 3 wrong slot → cross-instrument state corruption (worse than the current no-op).
  Mitigated by reusing the engine's own allocator output rather than recomputing.

## Test Plan

- `cargo test -p tickvault-app -p tickvault-trading -p tickvault-storage -p tickvault-common --tests`
- New tests per item above; the boundary guard re-blessed only AFTER the dated quote lands.
- `bash scripts/bench-gate.selftest.sh`, `cargo fmt --check`, banned-pattern scan, plan-gate.

## Rollback

Each item is an independent commit. Item 1 and 2 revert cleanly. Item 3 reverts by restoring
`evaluator.rs` and re-blessing the boundary manifest; the rule-file quote stays as the audit
record (house convention: dated records are never rewritten).

## Observability

- Item 1: no new signals; RESTORES existing ones (scoreboard coverage, crash classification,
  both cross-verify comparators) over the 15:30–15:40 window.
- Item 2: the three guards' output appears in the `repo-guards` CI job log.
- Item 3: reuses the §28.2 `tv_indicator_slot_exhausted_total` counter and coded error; no
  new metric. The evaluator's Hold-on-cold-snapshot path stays silent by design (a
  not-yet-warm instrument is normal, not an error).

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Process dies 15:31 IST, restarts 15:33 | Classified as a real in-session death, not `post_close_restart` |
| 2 | Daily scoreboard for a full trading day | Minute coverage denominator includes 15:30–15:40 |
| 3 | Spot / BruteX cross-verify post-close run | Compares rows through 15:40 |
| 4 | A new pub fn added without a test | `pub-fn-test-guard` fails the CI `repo-guards` job |
| 5 | Tick for a Groww index id (~2^62) | Evaluator processes it; FSM transitions |
| 6 | Engine at capacity, default snapshot | Evaluator returns Hold without indexing |
