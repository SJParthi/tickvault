# Implementation Plan: Log-Driven Fixes (index allowlist alias + PREVDAY-01)

**Status:** APPROVED
**Date:** 2026-06-26
**Approved by:** Parthiban (operator) — 2026-06-26 live-logs directive

Two independent, log-driven fixes shipped as ONE PR. Both derive from the
operator's 2026-06-26 live boot logs. Changed crates: **core** (Fix A),
**common** (Fix A alias + Fix B ErrorCode), **app** (Fix B observability).

---

## Design

### Fix A — alias-aware index allowlist (NIFTY NEXT 50 self-heal)

**Root cause (verified in code, 2026-06-26):**
`crates/core/src/instrument/index_extractor.rs::extract_indices` matches each
Dhan `IDX_I` row's normalized `SYMBOL_NAME` against `NSE_INDEX_ALLOWLIST` by
EXACT equality (`*allowed == norm`). It does NOT consult the existing
`tickvault_common::constants::INDEX_SYMBOL_ALIASES` map (`constants.rs:1076`,
already `("NIFTYNXT50","NIFTY NEXT 50")`). Dhan publishes that index as
`NIFTY NXT 50` / `NIFTYNXT50`, neither of which equals the allowlisted
`"NIFTY NEXT 50"` → the row is dropped and its live value is never subscribed
(live WARN `index allowlist MISS ... NIFTY NEXT 50`).

**Design:** make the allowlist match **alias-aware** without changing the
allowlist set (so `allowlist_has_exactly_32_nse_indices` stays valid):

1. Add a pure helper `canonicalize_index_symbol(&str) -> String` in
   `index_extractor.rs` that (a) normalizes via the existing
   `normalize_index_symbol`, then (b) maps the normalized form through
   `INDEX_SYMBOL_ALIASES` (alias → canonical) with a single O(1)-ish scan of
   the fixed small map; returns the canonical allowlist form when an alias
   matches, else the normalized symbol unchanged.
2. In `extract_indices`, compare `canonicalize_index_symbol(&row.symbol_name)`
   (not the raw normalized symbol) against `NSE_INDEX_ALLOWLIST`. Both
   `NIFTY NXT 50` and `NIFTYNXT50` now resolve to the allowlisted
   `NIFTY NEXT 50`.
3. `INDEX_SYMBOL_ALIASES` lacks the spaced form, so ADD
   `("NIFTY NXT 50","NIFTY NEXT 50")` to it (keeping the existing
   `NIFTYNXT50` and `SENSEX50` entries). Alias keys are matched after
   normalization (uppercase, single-spaced), matching the entries' existing
   uppercase single-spaced form.

Cold path only (boot CSV parse) — O(1) per row (fixed 3-entry alias map);
no tick-hot-path impact. `NSE_INDEX_ALLOWLIST` length unchanged (32).

### Fix B — PREVDAY-01 typed error + per-empty observability

**Root cause (verified, 2026-06-26):**
`crates/app/src/prev_day_ohlcv_boot.rs::run_prev_day_ohlcv_fetch` counts the
`Ok(None)` arm (Dhan 200-with-empty-body) as `skipped` SILENTLY (no log, no
metric on that arm) — 774 silent empties were invisible. And the
EMPTY-coverage `error!` at `main.rs:~3211` carries NO
`code = ErrorCode::X.code_str()` field, so the ERROR is untyped (violates the
observability `code =` contract).

**Design:**
1. Add `ErrorCode::PrevDay01CoverageEmpty` (`code_str() == "PREVDAY-01"`,
   `severity() == High` — a real boot-data gap, not a halt;
   `is_auto_triage_safe() == true` via the Critical-only gate — visibility,
   operator informs; `runbook_path()` → new rule file). Keep `all()` +
   code_str/severity/runbook matches in sync (catalogue-size + roundtrip
   tests).
2. Create `.claude/rules/project/prev-day-ohlcv-error-codes.md` documenting
   PREVDAY-01 trigger/triage (so `error_code_rule_file_crossref` +
   `every_runbook_path_exists_on_disk` pass).
3. Tag the EMPTY-coverage `error!` in `main.rs` with
   `code = ErrorCode::PrevDay01CoverageEmpty.code_str()`. (The
   `final flush failed` `error!` already routes via Loki; no existing code
   variant cleanly fits it and inventing a 2nd code is out of scope — leave
   it untyped, its message contains NO tracked code prefix so the tag-guard
   does not fire.)
4. Add per-empty observability on the `Ok(None)` arm: a static-label counter
   `tv_prev_day_ohlcv_empty_total` incremented once per empty + a single
   coalesced summary `debug!` carrying the running empty count (NOT 774
   separate info/warn lines).

---

## Edge Cases

- **Fix A:** a Dhan row already named exactly `NIFTY NEXT 50` still matches
  (canonicalize is a no-op when no alias hits). A non-allowlisted, non-alias
  index (`Nifty GS 10Yr`) is still dropped. Lowercase / extra-space variants
  (`nifty nxt 50`) normalize first, then alias-resolve. `SENSEX50` alias maps
  to `SNSX50` (FNO direction) — harmless: `SNSX50` is not in the NSE allowlist
  so it stays dropped exactly as before (no behaviour change for it).
- **Fix B:** `expected == 0` (Indices4Only / empty universe) already routes to
  `PrevDayCoverage::Empty`; the new `code =` tag attaches there too. Counter is
  static-label (no per-symbol cardinality). Empty-count summary fires once
  after the loop, gated on `empty > 0`.

## Failure Modes

- **Fix A:** if `INDEX_SYMBOL_ALIASES` ever contained a cycle or a bad
  mapping, canonicalize returns the mapped value as-is (single lookup, no
  recursion) — can never loop. If the alias maps to a non-allowlisted string,
  the row is dropped (fail-closed, same as today).
- **Fix B:** the new ERROR is visibility-only — boot is fail-soft and never
  blocks regardless. Counter/`debug!` emission cannot fail the fetch. The
  `code =` tag is a pure string; no new failure surface.

## Test Plan

- **Fix A (core):** `index_extractor.rs::tests::test_nifty_next_50_resolves_via_alias`
  — a Dhan row named `NIFTY NXT 50` AND one named `NIFTYNXT50` both land in
  `nse_indices` as the allowlisted `NIFTY NEXT 50` (not in `allowlist_misses`).
  Plus `canonicalize_index_symbol` unit coverage (alias hit + passthrough).
  Existing `allowlist_has_exactly_32_nse_indices` must stay green.
- **Fix A (common):** assert `INDEX_SYMBOL_ALIASES` contains the new spaced
  entry; existing alias entries preserved.
- **Fix B (common):** ErrorCode roundtrip (`from_str("PREVDAY-01")`), severity
  == High, `runbook_path()` exists on disk; `all()` length / catalogue tests;
  `error_code_rule_file_crossref` + `error_code_tag_guard` green.
- `cargo test -p tickvault-core --lib --tests`,
  `cargo test -p tickvault-common --lib --tests`,
  `cargo test -p tickvault-app --lib --tests`.

## Rollback

Single PR, no schema/migration, no config flag. Revert the squash-merge commit
to fully undo. ErrorCode addition is additive (new variant only); reverting
removes it cleanly from `all()` + the matches. Fix A reverts to exact-match
allowlist (the prior behaviour). No data written, no live-feed path touched.

## Observability

- **Fix A:** the existing `allowlist_misses` boot WARN now correctly OMITS
  `NIFTY NEXT 50` once it self-heals — the WARN itself is the observable
  signal that the fix worked.
- **Fix B:** new `error!(code = "PREVDAY-01", ...)` on EMPTY coverage routes to
  Telegram (Severity::High via Loki). New static-label counter
  `tv_prev_day_ohlcv_empty_total` + a single coalesced `debug!` empty-count
  summary make a future 774-empty event diagnosable. PREVDAY-01 runbook gives
  triage steps.

---

## Plan Items

- [x] Fix A — alias-aware index allowlist
  - Files: crates/core/src/instrument/index_extractor.rs, crates/common/src/constants.rs
  - Tests: test_nifty_next_50_resolves_via_alias, canonicalize_index_symbol unit tests, allowlist_has_exactly_32_nse_indices (preserved)

- [x] Fix B — typed PREVDAY-01 + per-empty observability
  - Files: crates/common/src/error_code.rs, crates/app/src/main.rs, crates/app/src/prev_day_ohlcv_boot.rs, .claude/rules/project/prev-day-ohlcv-error-codes.md, .claude/triage/error-rules.yaml
  - Tests: error_code roundtrip/severity/runbook-exists for PrevDay01CoverageEmpty, error_code_rule_file_crossref, error_code_tag_guard, catalogue-size, triage_rules_full_coverage_guard

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan IDX_I row `NIFTY NXT 50` | resolves to allowlisted `NIFTY NEXT 50`, subscribed |
| 2 | Dhan IDX_I row `NIFTYNXT50` | resolves to allowlisted `NIFTY NEXT 50`, subscribed |
| 3 | Dhan IDX_I row `Nifty GS 10Yr` | dropped (not allowlisted, no alias) |
| 4 | prev-day fetch returns `Ok(None)` for a symbol | counter++ + coalesced debug; counted as skipped |
| 5 | prev-day coverage EMPTY at boot | `error!(code="PREVDAY-01")` → Telegram |

---

## Per-Item Guarantee Matrix

See `.claude/rules/project/per-wave-guarantee-matrix.md`. All 15 rows of the
Guarantee Matrix and all 7 rows of the Resilience Demand Matrix apply to every
item in this plan. Both fixes are cold-path / observability-only — Fix A
(alias-aware index allowlist + canonical-name dedup) is O(1) in the once-daily
index extraction and preserves composite `(security_id, exchange_segment)`
uniqueness; Fix B (PREVDAY-01 typed error + per-empty counter/coalesced log)
adds no tick-drop path and no hot-path allocation.
