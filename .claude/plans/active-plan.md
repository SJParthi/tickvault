# Implementation Plan: Reject absurd-but-finite LTP before it poisons a candle/tick

**Status:** APPROVED
**Date:** 2026-06-05
**Approved by:** Parthiban ("do everything and fix everything and go ahead as per the plan") + security-reviewer agent HIGH finding on the candle fold path
**Crate(s) touched:** `core`, `common`

## Context (verified by the 3-agent deep research, no illusion)

The deep research on the candle high/low question produced three results:

1. **The "fold late ticks into the current minute" idea is REJECTED** (hostile
   agent CRITICAL): it would create a NEW exact-match cross-verify mismatch on
   the current minute while leaving the already-sealed earlier minute STILL
   wrong (immutable) — net WORSE, and fabricates data Dhan disagrees with. Not
   implemented.
2. **PR #1026 (merged) is the real candle fix for the dedup-drop cause** — dedup
   `continue` at `tick_processor.rs:1151` runs BEFORE the broadcast `send` at
   `:1251`, so a tick the old `(id,ts,ltp)` ring false-dropped never reached the
   candle aggregator. #1026's decided-identity key (which keeps every tick)
   restores it to BOTH the `ticks` table AND the candle. (Confirm with live data.)
3. **Security-reviewer HIGH (this PR):** `is_valid_ltp` accepts any finite,
   positive price — including `f32::MAX` (≈3.4e38) from a mangled frame.
   `check_tick_ohlc_integrity` only DETECTS, never filters
   (`tick_processor.rs:1062` "Detection-only (NOT a filter)"). So an absurd
   finite price flows into the O(1) candle fold and sets `high = 3.4e38`,
   permanently corrupting that minute's candle AND writing a garbage `ticks`
   row. This PR closes that hole.

## Plan Items

- [x] Item 1 — Add `MAX_PLAUSIBLE_LTP` ceiling constant (₹10 crore)
  - Files: `crates/common/src/constants.rs`
  - Tests: existing common build (constant referenced by core)
- [x] Item 2 — `is_valid_ltp` rejects LTP > `MAX_PLAUSIBLE_LTP`
  - Files: `crates/core/src/pipeline/tick_processor.rs`
  - Tests: `test_is_valid_ltp_positive_price` (real prices + ceiling pass),
    `test_is_valid_ltp_rejects_absurd_but_finite_price` (f32::MAX, 2×ceiling, 1e30 rejected)

## Design

1. `MAX_PLAUSIBLE_LTP: f32 = 100_000_000.0` (₹10 crore) in `constants.rs`.
   ~500× the priciest real NSE instrument (MRF ≈ ₹1.5 lakh; SENSEX ≈ 80k;
   BANKNIFTY ≈ 52k). An ABSOLUTE ceiling, NOT a per-instrument band — so a
   legitimate large move is NEVER rejected (prime directive: never miss a real
   tick). It only ever catches corruption.
2. `is_valid_ltp(ltp) = ltp.is_finite() && ltp > 0.0 && ltp <= MAX_PLAUSIBLE_LTP`.
   One extra comparison; still O(1), zero-alloc, `#[inline(always)]`.
3. The guard sits at the SINGLE validity gate that runs BEFORE the broadcast
   (`is_valid_tick` → `continue` at `tick_processor.rs:1044`, before the
   broadcast `send` at `:1251`), so it protects BOTH the `ticks` table AND the
   candle aggregator (a downstream broadcast subscriber) in one place.
4. A rejected garbage tick is counted as `junk` (`m_junk_filtered`) and
   `continue`d — already an accounted terminal in the PR #1024 conservation
   ledger, so the ledger stays balanced (no new leak path).

## Edge Cases

- **Exactly at the ceiling** (`== MAX_PLAUSIBLE_LTP`) → valid (inclusive `<=`).
- **Priciest real instruments** (MRF ~₹1.5L, SENSEX ~80k) → far below ceiling →
  always valid. Verified by test.
- **f32::MAX / 1e30 / 2×ceiling** → rejected as junk. Verified by test.
- **NaN / Inf / ≤ 0** → still rejected (unchanged).
- **Subnormal / min-positive** → still valid (unchanged).

## Failure Modes

- Ceiling too low would drop a legit price (data loss). Mitigated by ~500×
  headroom over the priciest real NSE instrument — impossible to hit
  legitimately. If NSE ever lists a > ₹10cr/unit instrument (it will not), the
  named constant is the single edit point.
- Hot path unchanged shape: one extra finite-scalar compare; DHAT/Criterion
  budgets hold.

## Test Plan

- `cargo test -p tickvault-core is_valid_ltp` — 10 tests green (incl. the 2
  new/updated ceiling cases).
- `cargo test -p tickvault-core --lib` (2030) + `cargo test -p tickvault-common --lib` (874) green.
- `cargo clippy -p tickvault-core -p tickvault-common -- -D warnings -W clippy::perf` clean.
- `bash .claude/hooks/banned-pattern-scanner.sh` exit 0.
- design-first wall (impl crates `core` + `common`, this APPROVED plan).

## Rollback

Single constant + one comparison. Revert restores the prior `is_valid_ltp`. No
schema/data change.

## Observability

The existing `tv_ticks_junk_filtered_total` counter now also catches
absurd-price corruption (previously it passed silently into a candle). A
non-zero rate on a clean feed becomes a corrupt-frame signal. No new metric.

## Per-Item Guarantee Matrix (cross-reference)

This plan and every item are bound by the 15-row 100% guarantee matrix and the
7-row resilience demand matrix — see
`.claude/rules/project/per-wave-guarantee-matrix.md`. All 15 + 7 rows apply.

**Honest envelope (per `wave-4-shared-preamble.md` §8):** any "100%" means "100%
inside the tested envelope, with ratcheted regression coverage" — the 10
`is_valid_ltp` unit tests + DHAT/Criterion gates. This closes a real
data-integrity hole (absurd-finite price poisoning candle high/low + ticks
rows) found by the security-reviewer agent. It is O(1)/zero-alloc and can NEVER
reject a genuine NSE price. It does NOT change candle bucketing (the rejected
"fold" approach is not implemented) and is independent of the still-pending
live-data confirmation that #1026 fixed the 23440 miss.
