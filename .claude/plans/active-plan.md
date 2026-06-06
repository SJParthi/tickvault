# Implementation Plan: NTM Sub-PR #4 — constituent → Dhan security_id ISIN join

**Status:** APPROVED
**Date:** 2026-06-06
**Approved by:** Parthiban — "if merged, go ahead" standing directive after #1037; builds to §31.1 (merged #1036).
**Crate(s) touched:** `core` (`csv_parser.rs` +`isin`; new `constituent_resolver.rs`;
`subscription_planner.rs` test literal), `app` (`today_instrument.rs` test literal).
Feature-gated behind `daily_universe_fetcher`.

## Context

Implements the §31.1 mapping contract: resolve niftyindices constituents (Sub-PR #3
`IndexConstituencyMap`) → Dhan NSE-EQ `security_id` via the **ISIN-primary** join with
**symbol fallback**, **fail-closed** on >0.5% dangling, **deduped** by
`(security_id, exchange_segment)`. Role tagging + the union with F&O underlyings are
deferred to Sub-PR #5 (subscription wiring), where the union actually happens.

## Plan Items

- [x] Item 1 — `CsvRow.isin` (the §31.1 PRIMARY join key)
  - Files: `crates/core/src/instrument/csv_parser.rs` (+`isin` field, `ColumnIndices`,
    `find("ISIN")`, `extract_row`)
  - Tests: existing csv_parser suite recompiles + passes (20)
- [x] Item 2 — `constituent_resolver.rs` — the ISIN-primary / symbol-fallback join
  - Files: `crates/core/src/instrument/constituent_resolver.rs`, `mod.rs`
  - O(1) `HashMap<ISIN,(sid,seg)>` + `HashMap<Symbol,(sid,seg)>` over NSE-EQ-EQUITY
    rows; ISIN-primary, symbol fallback; fail-closed >0.5% dangling; dedup by
    `(security_id, exchange_segment)`.
  - Tests: ISIN-primary, rename-proof (ISIN match ignores symbol), symbol fallback,
    non-NSE-EQ rejected, dedup, fail-closed >0.5%, tolerates <0.5%, empty-ok (9)
- [x] Item 3 — fix CsvRow full-literal test sites for the new field
  - Files: `subscription_planner.rs`, `today_instrument.rs`

## Design

`CsvRow` gains `isin` (optional — `usize::MAX` sentinel when the Compact CSV lacks the
column; `opt_str` sanitizes it). `resolve_constituents(&IndexConstituencyMap, &[CsvRow])`
filters Dhan rows to canonical `NSE_EQ` + `EQUITY`, builds the two O(1) indexes, then
for each unique constituent prefers the ISIN key and falls back to symbol. ISIN is the
honest precise key (rename/series-proof); symbol-alone is never primary. The >0.5%
fail-closed gate guards against a bad CSV silently mapping half the universe to nothing.

## Edge Cases

- Constituent symbol renamed but ISIN stable → still resolves (rename-proof test).
- Constituent missing ISIN → symbol fallback (test).
- Same ISIN on a derivative row → NOT matched (NSE-EQ-EQUITY filter; test).
- Two constituents → same Dhan SID → deduped (test).
- Empty constituency → `total=0`, no div-by-zero, OK (test).

## Failure Modes

- `> 0.5%` constituents unresolvable → `ConstituentResolveError::TooManyDangling`
  (fail-closed; the orchestrator in Sub-PR #10 decides halt vs operator override).
- Inputs are pre-sanitized by Sub-PR #3 (constituents) + `csv_parser` (`CsvRow` via
  `sanitize_audit_string`), so the resolver handles only clean strings.

## Test Plan

- `cargo test -p tickvault-core --features daily_universe_fetcher -- constituent_resolver csv_parser index_constituency` → 56 passed, 0 failed (verified).
- `cargo test -p tickvault-app --lib --no-run` → compiles (test literal fixed).
- Full-workspace test-compile to catch CsvRow literal breakage everywhere (lesson from
  #1037: `cargo check` skips test code — must compile tests).

## Rollback

- Feature-gated + unwired (no production caller until Sub-PR #5/#10) → `git revert`
  removes it with zero runtime impact. `CsvRow.isin` is an additive optional field.

## Observability

- Pure function returning typed `Result` + diagnostics (`unresolved_symbols`, `via_isin`).
  Runtime telemetry (counters, Telegram on fail-closed reject) attaches at boot wiring
  (Sub-PR #6/#10). N/A for this unwired join.

## Adversarial review

Resolver is a PURE function over PRE-SANITIZED inputs (Sub-PR #3 + csv_parser both run
`sanitize_audit_string`). Covered by 9 adversarial-style unit tests (fail-closed, dedup,
rename-proof, fallback, non-NSE-EQ rejection, boundary fractions). A fresh 3-agent spawn
was constrained by container disk pressure this session; flagged for the next pass.

## Per-Item Guarantee Matrix

| Demand | Sub-PR #4 |
|---|---|
| 100% testing | 9 resolver tests (incl. boundary 0.5% gate) + 20 parser + recompile |
| O(1)/perf | O(1) ISIN + symbol HashMaps; per-constituent O(1) lookup; cold path |
| security | inputs pre-sanitized; ISIN-primary (no wrong-security mapping); non-NSE-EQ filtered |
| uniqueness/dedup | dedup by `(security_id, exchange_segment)` (I-P1-11) |
| honest envelope | fail-closed >0.5% dangling; role tagging + union explicitly deferred to #5 |
| no half-finished (Rule 14) | `resolve_constituents` is a real callable + fixture-tested; feature-gated like its siblings; Sub-PR #5/#10 call it |
