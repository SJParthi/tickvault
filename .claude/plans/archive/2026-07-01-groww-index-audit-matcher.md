# Implementation Plan: Fix Groww index-coverage audit matcher (false absent=28 → true 10)

**Status:** APPROVED
**Date:** 2026-06-28
**Approved by:** operator (task directive 2026-06-28)
**Crate:** core (escalates to common — `INDEX_SYMBOL_ALIASES` additive aliases)

## Design

The Groww index-coverage audit (`groww_indices_absent_vs_dhan` in
`crates/core/src/feed/groww/instruments.rs`) OVER-REPORTS absent indices.

A live boot (2026-06-28) logged `absent_on_groww=28` when the true count is
**10**. Root cause: the audit canonicalizes the index `WatchEntry`'s short
`exchange_token` (e.g. `NIFTYJR`, `NIFTYAUTO`, `NIFTYPHARMA`, `NIFTYTOTALMCAP`)
and compares it against the Dhan `NSE_INDEX_ALLOWLIST` *full names*
(`NIFTY NEXT 50`, `NIFTY AUTO`, `NIFTY PHARMA`, `NIFTY TOTAL MKT`). The short
Groww codes do NOT canonicalize to the Dhan names, so ~22 truly-present indices
are falsely flagged absent.

Neither the Groww `exchange_token` ALONE nor the Groww `name` ALONE matches the
allowlist for all 24 indices:
- The allowlist mixes *trading symbols* (`NIFTY`, `BANKNIFTY`, `MIDCPNIFTY`,
  `NIFTYMCAP50`, `NIFTYIT`) and *descriptive names* (`NIFTY AUTO`, `NIFTY NEXT 50`).
- Groww `exchange_token` matches the trading-symbol entries (`NIFTY`, `BANKNIFTY`,
  `FINNIFTY`, `NIFTYIT`); Groww `name` (display, e.g. "NIFTY Auto", "Nifty Next 50")
  matches the descriptive entries.

**Fix = canonicalize BOTH the Groww `exchange_token` AND the Groww display `name`,
and treat an allowlist entry as PRESENT if EITHER resolves to it.** Three
spelling bridges remain (verified) and are added to the shared
`INDEX_SYMBOL_ALIASES` (additive, alias→canonical, reused by both feeds — no
parallel matcher):
- `NIFTY MIDCAP SELECT` → `MIDCPNIFTY` (Groww name "Nifty Midcap Select")
- `NIFTY MIDCAP 50` → `NIFTYMCAP50` (Groww name "NIFTY MIDCAP 50")
- `NIFTY TOTAL MARKET` → `NIFTY TOTAL MKT` (Groww name "Nifty Total Market")

With token-OR-name + these 3 aliases, the absent set is EXACTLY the operator's
10: NIFTY 200, GIFTNIFTY, NIFTY ENERGY, NIFTYINFRA, NIFTY MNC, NIFTY CONSUMPTION,
NIFTY SERV SECTOR, NIFTY MID100 FREE, NIFTY SMALLCAP 50, NIFTY MICROCAP250.

To make the display name available to the audit, the Groww `name` CSV column
(already present in the header, currently unparsed) is threaded:
`GrowwInstrumentRow.name` → index `WatchEntry.symbol_name` (was `None` for indices).

This is audit-accuracy ONLY. Which indices are subscribed/persisted is UNCHANGED
(25 stays 25). Cold-path (once per master load); no hot-path change; no new
`pub fn`.

## Plan Items

- [x] Add `name` field to `GrowwInstrumentRow`; parse the existing `name` column
  - Files: crates/core/src/feed/groww/instruments.rs
  - Tests: test_parse_groww_master_parses_name_column
- [x] Store Groww `name` into index `WatchEntry.symbol_name` in `extract_index_entries`
  - Files: crates/core/src/feed/groww/instruments.rs
  - Tests: test_extract_index_entries_retains_display_name
- [x] Rewrite `groww_indices_absent_vs_dhan` to canonicalize token-OR-name
  - Files: crates/core/src/feed/groww/instruments.rs
  - Tests: test_groww_indices_absent_vs_dhan_is_exactly_the_ten_real_spellings
- [x] Add 3 additive bridge aliases to `INDEX_SYMBOL_ALIASES`
  - Files: crates/common/src/constants.rs
  - Tests: existing INDEX_SYMBOL_ALIASES test extended
- [x] Replace synthetic FIX-C test with REAL Groww short-code spellings; assert exactly 10
  - Files: crates/core/src/feed/groww/instruments.rs
  - Tests: test_groww_indices_absent_vs_dhan_is_exactly_the_ten_real_spellings

## Edge Cases

- BSE SENSEX (`exchange == "BSE"`) is excluded from the NSE-only allowlist compare — unchanged.
- An index whose `name` is empty falls back to token-only matching (e.g. legacy/synthetic rows) — preserves old full-coverage test.
- Dual-naming: token AND name both resolving to the same allowlist entry is idempotent (HashSet of present-canonicals).
- Aliases are additive (alias→canonical); they only ADD fallback matches, never remove an existing match — Dhan-side `extract_indices` unaffected for already-matching rows.

## Failure Modes

- If Groww drops/renames an index, the audit still reports it absent (the genuine-limitation signal is preserved) — fail-VISIBLE, never silent.
- If the `name` column disappears from the Groww CSV, `name` parses empty → token-only fallback (degrades to the old behaviour for affected rows, but logged via the WARN). No panic, no boot block (cold-path audit only).
- No data-correctness path touched: subscription/persist set is unchanged.

## Test Plan

- Unit (`#[test]`): name-column parse; index `symbol_name` retention; the real-spelling exactly-10 assertion (fails on old code, passes on new); full-coverage empty; alias additions.
- Scoped: `cargo test -p tickvault-core --features daily_universe_fetcher feed::groww`, `cargo test -p tickvault-common` (alias), dedup meta-guard, error-code crossref.
- The exactly-10 test uses the 24 REAL Groww NSE index `(exchange_token, name)` pairs from the live master, asserting `len()==10` and the canonicalized set equals the 10 — this is the regression that the synthetic test missed.

## Rollback

- Pure cold-path audit + additive aliases. Revert the single commit to restore prior behaviour; no migration, no schema change, no data touched. The audit only changes a log line + a counter; subscription/persist behaviour is byte-identical either way.

## Observability

- Unchanged sinks: the `info!`/`warn!` "Groww index coverage vs Dhan allowlist" boot line + `tv_groww_index_absent_total` counter. After the fix the WARN fires for the true 10 (or the INFO full-coverage line) instead of a false 28 — accurate operator signal. No new `error!`/event/table.

## Per-Item Guarantee Matrix

This plan item carries the mandatory 15-row + 7-row guarantee matrix by
cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`. The
change is a cold-path audit-accuracy fix (no hot-path allocation, no new tick
path, no DEDUP key change, no new pub fn): uniqueness/dedup rows are N/A (no
storage key touched); O(1)/performance rows are N/A (cold-path boot audit, the
fn is already `// O(1) EXEMPT: cold-path daily build only`); the
audit/logging/monitoring rows are satisfied by the existing
`tv_groww_index_absent_total` counter + the boot WARN/INFO line, now ACCURATE;
testing/coverage/code-review/extreme-check rows are satisfied by the new
real-spelling regression test that fails-on-old / passes-on-new plus the retained
full-coverage test.
