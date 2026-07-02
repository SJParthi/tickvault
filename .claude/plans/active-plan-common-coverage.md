# Implementation Plan: crates/common coverage floor top-up (inline test-only)

**Status:** APPROVED
**Date:** 2026-07-02
**Approved by:** Parthiban (operator) — standing fix-CI instruction: post-merge main CI
"Coverage & Perf" fails on `common` ~0.01pp below its 99.5% floor; PR #1310's author
scoped this follow-up as "a few more inline-credited tests (sanitize/candle_fold
micro-branches)". Inline `#[cfg(test)]` tests only — zero production-logic change.

**Guarantee matrices:** cross-reference `.claude/rules/project/per-wave-guarantee-matrix.md`
(15-row + 7-row). This change is test-only inside `crates/common` `#[cfg(test)]` modules —
no hot path, no schema, no new pub fn, no new failure mode; the coverage gate itself
(`quality/crate-coverage-thresholds.toml` + `scripts/coverage-gate.sh`) is the ratchet
this plan services.

## Design

llvm-cov only credits lines executed by the same binary, so the top-up is inline
`#[cfg(test)]` tests in `crates/common/src/*` (crate `tickvault-common`), not
integration tests. Two mechanisms:

1. **Real new tests** covering genuinely reachable, previously-missed branches:
   - `sanitize.rs`: UTF-8 boundary-walk in `sanitize_ilp_symbol` truncation (3-byte
     char, 256 % 3 == 1); bidi-isolate strip (U+2066..U+2069) in
     `sanitize_audit_string`; `redact_json_string_field` key-without-colon and
     non-string-value `continue` branches.
   - `feed_health.rs`: `impl Default for FeedHealthRegistry`.
   - `instrument_registry.rs`: `instrument_type_tag()` "UNKNOWN" arm (derivative
     category with `instrument_kind: None`).
   - `trading_calendar.rs`: `count_trading_days` succ_opt→break saturation at
     `NaiveDate::MAX`; `secs_until_next_market_open` None beyond NaiveDate range.
   - `candle_fold.rs`: `as_sealed`/`as_amended` extractor helpers with an explicit
     test covering both arms.
2. **Restructuring uncoverable failure-only lines in EXISTING tests** (behavior
   identical): multi-line `panic!` arms in `let-else`/`match` (candle_fold, sanitize)
   replaced by Option extractors / `matches!` asserts; multi-line `assert!` messages
   whose method-call format args sat on failure-only lines (error_code.rs, feed.rs)
   hoisted into always-executed `let` bindings + single-region assert lines.

## Edge Cases

- 3-byte vs 4-byte UTF-8 cap: 256 % 4 == 0 lands ON a char boundary (existing test);
  256 % 3 == 1 lands MID-char and must walk back — new test pins output = 255 bytes.
- `redact_json_string_field`: key token without a colon is NOT a field assignment;
  numeric value after colon must survive unredacted (documented contract).
- `NaiveDate::MAX` arithmetic: `count_trading_days(MAX-1, MAX)` must break cleanly,
  never panic; huge wall-clock (~year 275760 CE) must return `None`.
- Un-instrumented `FeedHealthRegistry::default()` reports zero counters / no
  last-tick — never a false Down (audit Rule 11 parity with `new()`).
- Derivative instrument with unresolved kind → "UNKNOWN", never INDEX/EQUITY.

## Failure Modes

- Remaining known-uncoverable misses are accepted and documented: `market_hours.rs`
  wall-clock-dependent branches (they flip by CI run time — the historical ~0.01pp
  variance source), `config.rs` sandbox-date-locked bail (dead after
  LIVE_TRADING_EARLIEST_DATE), `instrument_registry.rs` tracing-macro field lines
  (need an active subscriber), `trading_calendar.rs:201` (structurally unreachable
  succ_opt guard). Post-change margin (0.23pp) absorbs the market_hours swing.
- If rustfmt re-splits an assert, only literal/inline-capture lines land on
  failure-only lines (measured: those are not counted as missed; method-call arg
  lines were).

## Test Plan

- [x] `cargo test -p tickvault-common` — 963 lib + all integration/doc suites green
- [x] `cargo fmt --check` — clean
- [x] `cargo llvm-cov --package tickvault-common --summary-only` before: 99.49%
      lines (57/11158 missed) — BELOW the 99.5% floor
- [x] after: 99.73% lines (30/11212 missed) — 0.23pp margin above the floor
- [x] New tests: `test_sanitize_ilp_symbol_cap_walks_back_to_char_boundary`,
      `test_sanitize_audit_string_strips_bidi_isolates`,
      `test_redact_json_field_key_without_colon_left_untouched`,
      `test_redact_json_field_non_string_value_left_untouched`,
      `test_outcome_extractors_reject_non_matching_variants`,
      `test_registry_default_matches_new_for_unwired_feed`,
      `test_instrument_type_tag_returns_unknown_for_derivative_without_kind`,
      `test_count_trading_days_saturates_at_date_range_ceiling`,
      `test_secs_until_next_market_open_none_beyond_date_range`
- Note: `crates/common` changes normally escalate to workspace tests
  (testing-scope.md); these additions are `#[cfg(test)]`-only (compile away in prod)
  and CI runs the full battery on the PR.

## Rollback

`git revert` of the single commit restores the prior test bodies verbatim. No
schema, no config, no production code path is touched — rollback risk is zero
beyond re-tripping the coverage floor.

## Observability

No new runtime code → no new metrics/logs/alerts. The observable artifact is the
post-merge CI "Coverage & Perf" job on `common` flipping red→green via
`scripts/coverage-gate.sh` against `quality/crate-coverage-thresholds.toml`; margin
is verifiable any time with `cargo llvm-cov --package tickvault-common --summary-only`.

## Plan Items

- [x] Cover reachable missed branches with real inline tests — Files: sanitize.rs,
      feed_health.rs, instrument_registry.rs, trading_calendar.rs, candle_fold.rs —
      Tests: listed in Test Plan
- [x] Restructure uncoverable failure-only lines in existing tests — Files:
      candle_fold.rs, sanitize.rs, error_code.rs, feed.rs — Tests: existing ratchet
      tests unchanged in name and assertion semantics
- [x] Re-measure before/after and record verbatim numbers — see Test Plan
