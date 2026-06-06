# Implementation Plan: NTM index value (§31 item 1) + allowlist self-verify

**Status:** VERIFIED
**Date:** 2026-06-06
**Approved by:** Parthiban — AskUserQuestion 2026-06-06: "Standard name + boot self-verify" +
"NTM index value now, full map later" (after he asked whether the NTM index value was actually added).
**Crate(s) touched:** `core` (`index_extractor.rs` — add the 32nd allowlist entry;
`daily_universe_orchestrator.rs` — wire the `allowlist_misses` boot log). Feature:
`daily_universe_fetcher`.

## Context

Operator verification caught a real gap: #3/#4/#5/#10b subscribe the ~750 NTM **constituent
stocks**, but the **NIFTY Total Market INDEX value itself** (§31 item 1, the 33rd tracked index)
was NEVER added to `NSE_INDEX_ALLOWLIST` (31 NSE entries, no NTM). Also found: `allowlist_misses`
was a **dead field** — computed in `extract_indices` but never logged — so the "self-verify" was an
illusion. Operator's "no hallucination / real-time guarantee" demands both fixed.

## Design

- **Add `"NIFTY TOTAL MARKET"`** as the 32nd `NSE_INDEX_ALLOWLIST` entry → it flows through the
  existing index path: `extract_indices` matches the live Dhan IDX_I row → `SubscriptionTarget`
  with `role = Index` → subscribed in Quote mode like the other 31 indices (+ SENSEX = 33 total).
- **Wire `allowlist_misses` → loud boot log** in `daily_universe_orchestrator::build_universe_from_bytes`:
  empty ⇒ `info!` positive signal; non-empty ⇒ `warn!` naming the missed index(es). This is the
  **real, non-illusory self-verify**: if Dhan's exact IDX_I `SYMBOL_NAME` for NTM differs from the
  standard name, the boot log LOUD-warns "index allowlist MISS … NIFTY TOTAL MARKET" instead of
  silently dropping it — operator then corrects with the verbatim symbol.

## Edge Cases

- Dhan's symbol == "NIFTY TOTAL MARKET" → matched, subscribed, `info!` OK.
- Dhan's symbol differs (e.g. "NIFTY TOTAL MKT") → `warn!` miss at boot; index value not subscribed
  until corrected (constituents unaffected — they come from niftyindices, not this allowlist).
- Index temporarily absent from the master → same `warn!` path (correct: it IS a miss).
- +1 index vs the `[100,1200]` universe envelope → ample headroom; no overflow.

## Failure Modes

- Wrong/renamed Dhan symbol → loud `warn!` (visible), NOT silent drop — the operator-chosen
  self-verify. No boot block (an index-value miss is a degrade, not fail-closed).
- The constituent-stock subscription is independent of this allowlist (niftyindices-sourced), so a
  miss here never affects the ~750 stocks.

## Test Plan

`cargo test -p tickvault-core --features daily_universe_fetcher` (index_extractor + orchestrator).
Count assertions updated (31→32, 32→33, 30→31 misses); `allowlist_has_exactly_32_nse_indices`
asserts membership; new `build_consumes_allowlist_misses_for_boot_self_verify` source-scan guard
blocks the dead-field regression (audit Rule 13). Orchestrator + daily_universe + boot suites
recompile green (no count drift — fixtures use dynamic `len()`).

## Rollback

`git revert` removes the one allowlist entry + the log wiring; the universe returns to the prior 31
NSE indices. No schema/migration. The constituent-stock path is untouched.

## Observability

`info!`/`warn!` boot self-verify line (the new real telemetry). `tv_universe_size{kind="index"}`
(existing) reflects the +1 index on a healthy boot. No new error code (a miss is a `warn!`, not a
typed failure class).

## Plan Items

- [x] Item 1 — add `"NIFTY TOTAL MARKET"` to `NSE_INDEX_ALLOWLIST`; update count tests + docs
  - Files: `crates/core/src/instrument/index_extractor.rs`
  - Tests: `allowlist_has_exactly_32_nse_indices`, `full_32_allowlist_plus_sensex_yields_33_indices`,
    `reports_allowlist_misses_when_an_index_absent`
- [x] Item 2 — wire `allowlist_misses` → loud boot `info!`/`warn!` self-verify
  - Files: `crates/core/src/instrument/daily_universe_orchestrator.rs`
  - Tests: `build_consumes_allowlist_misses_for_boot_self_verify`

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan symbol = "NIFTY TOTAL MARKET" | NTM index value subscribed; `info!` self-verify OK |
| 2 | Dhan symbol differs | `warn!` miss at boot naming NIFTY TOTAL MARKET; operator corrects |
| 3 | revert | back to 31 NSE indices; constituents unaffected |
