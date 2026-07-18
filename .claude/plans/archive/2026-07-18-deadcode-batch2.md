# Implementation Plan: Dead-code batch 2 — crates/common + crates/core zero-caller removal

**Status:** APPROVED
**Date:** 2026-07-18
**Approved by:** Parthiban (operator, standing dead-code removal order 2026-07-17, relayed via coordinator; LANE COMPLETE received)

## Design

Remove verified-dead (zero production callers on origin/main @ 191fdd72) code from
`crates/common` and `crates/core`, per the read-only dead-code inventory
(scratchpad deadcode/INVENTORY.md + area1-common-core.md), re-verified row-by-row
against current origin/main before each deletion:

1. **crates/common whole modules** (lib.rs decls replaced with dated tombstone
   comments, the stage-2 house pattern): `open_price_rest_fallback.rs` +
   `open_price_source.rs` (pair — the retired `/marketfeed/quote` open-price
   fallback, already a §3 REMOVE class), `phase.rs`, `feed_parity.rs` (the
   CANCELLED §33.2 Groww live-vs-backtest comparer — NOT the index_futures
   FUTIDX-02 seam), `formulas.rs`, `candle_fold.rs` (zero consumers; the app's
   `rest_candle_fold` is a NAME-TWIN with its own types, verified untouched).
2. **crates/core whole module**: `scheduler/` dir (describes the deleted WS
   timeline; zero refs) + its lib.rs decl tombstone.
3. **crates/core/src/instrument**: `csv_row.rs` (whole module) together with its
   only two consumers — the dead Dhan-leg halves `index_extractor.rs::
   extract_indices` (+ `IndexExtraction`, `IndexExtractError`, private
   `is_idx_i_index`/`is_bse_sensex` helpers, CsvRow test fixtures + tests) and
   `index_futures.rs::select_index_future_contracts` (+ `IndexFutureSelection`,
   its tests/helpers/proptest). KEEP (guard-pinned/alive):
   `NSE_INDEX_ALLOWLIST`, `canonicalize_index_symbol`, `normalize_index_symbol`,
   `GIFT_NIFTY_SYMBOL`, `select_index_future_expiries`/`select_index_future_expiry`,
   `MAX_INDEX_FUTURE_TARGETS`, `MAX_MONTHLY_EXPIRIES_PER_UNDERLYING`,
   `MAX_UNDERLYING_SYMBOLS_EVIDENCE` (live via feed/groww/instruments.rs),
   `IndexFutureMiss{,Reason}`, `FUTIDX_SAME_EXPIRY_CANDIDATE_CAP`, the whole
   FUTIDX-02 comparator (`FeedFutureSelection`, `compare_index_future_selections`,
   `record_index_future_selection`). `daily_universe_scope_guard.rs` pins only
   the KEEP set — no guard edit needed.
4. **crates/core/src/instance_lock.rs**: delete the retired Groww-fleet named-lock
   arms `try_acquire_named_lock`, `renew_named_lock`, `release_named_lock`,
   `spawn_named_lock_heartbeat` (+ the transitively-orphaned
   `NAMED_LOCK_RENEWAL_ERRORS_ESCALATE_THRESHOLD` const and its spans-ttl test).
   The SSM-stub behavioral tests are REWIRED to the ALIVE legacy Dhan wrappers
   (`try_acquire_instance_lock`/`renew_instance_lock`/`release_instance_lock`/
   `spawn_instance_lock_heartbeat`) so coverage of the shared `*_at_path` bodies
   (the live Dhan lock logic) is preserved. KEEP: `compute_named_lock_path`
   (live via `compute_instance_lock_path` delegation + pinned by
   `test_named_lock_path_dhan_name_matches_legacy_path`) and
   `GROWW_SCALE_FLEET_LOCK_NAME` (SKIPPED — test-pinned by the kept
   compute_named_lock_path suite incl. the dhan-path pin test).
5. **crates/core/src/auth/token_cache.rs**: delete the fast-boot arms
   `load_token_cache_fast` + `FastCacheResult` (+ the auth/mod.rs re-export +
   the `*_fast_*` tests). SKIP `delete_cache_file` — re-verification found LIVE
   in-file production callers inside the alive `load_token_cache` corrupt/expired
   arms (inventory row corrected).
6. **crates/common/src/tick_types.rs**: delete `OptionGreeksSnapshot`,
   `GreeksEnricher` + `NoopGreeksEnricher`, `DhanDailyResponse` (+ their tests).
   KEEP `DhanIntradayResponse` (pinned by common/tests/schema_validation.rs).

## Edge Cases

- Name-twin: common's `candle_fold` vs app's `rest_candle_fold` — every grep hit
  for `candle_fold` outside common/src is the app twin; deletion targets ONLY
  `crates/common/src/candle_fold.rs` + its lib.rs decl.
- `delete_cache_file` looked dead in the inventory but is called by the ALIVE
  `load_token_cache` path — skipped (fail-safe direction).
- `GROWW_SCALE_FLEET_LOCK_NAME` is asserted by the KEEP test
  `test_named_lock_path_dhan_name_matches_legacy_path` — skipped per the
  guard-pin-prefers-skip rule.
- `MAX_UNDERLYING_SYMBOLS_EVIDENCE` doc references the deleted
  `IndexFutureSelection` — doc reworded, const kept (groww consumer).
- Comment-only residuals (main.rs historical notes, loom test pattern comments,
  instrument/mod.rs KEEP table) are reworded/tombstoned so no deleted symbol
  dangles as a live claim.
- Test-count decreases: the test-count ratchet baseline is gitignored/local-only
  (merge-gate lock §3 row 6) — a fresh worktree establishes a fresh baseline.

## Failure Modes

- A missed caller → compile error in the per-crate test sweep (fail-loud, fixed
  or the row is reverted before push).
- Coverage drift in core from removing covered dead code + tests → surfaced by
  the pre-merge Coverage & Perf gate on the DRAFT PR; reported, not hidden.
- A guard/ratchet needle matching a deleted symbol → the relevant guard test
  fails in the sweep; only mechanical row-drops are applied, judgment calls are
  skipped and reported.
- Concurrent main movement → branch is off origin/main @ 191fdd72; conflicts
  surface at PR time and are resolved by rebase, never force-push.

## Test Plan

- Per-crate sequential sweep (workspace-equivalent per testing-scope.md, common
  touched → escalation): `for c in common core trading storage api app; do
  cargo test -p tickvault-$c; done` with CARGO_PROFILE_DEV_DEBUG=0.
- `cargo fmt --check`; `cargo clippy -p tickvault-common -p tickvault-core
  --no-deps -- -D warnings`.
- `bash .claude/hooks/banned-pattern-scanner.sh`.
- `cargo test -p tickvault-common --test error_code_rule_file_crossref` (no
  ErrorCode variants touched — must stay green).
- Hostile self-review: per deleted symbol, a grep across crates/** +
  .claude/hooks + scripts proving zero remaining non-tombstone references.

## Rollback

Single-purpose branch `claude/deadcode-batch2-common-core`; nothing touches
main. Rollback = close the PR / `git revert` the squash commit. No config, no
schema, no runtime-behaviour change (all deleted code had zero production
callers); the deleted modules are recoverable verbatim from git history.

## Observability

No metrics, alarms, error codes, or Telegram surfaces are touched (no ErrorCode
variant is deleted; the crossref test is the mechanical proof). lib.rs/mod.rs
tombstone comments date every removal (2026-07-18, batch 2) so future sessions
can trace deletions without archaeology. The PR body carries the per-row
evidence table (grep proofs) + the skipped-rows list.

## Per-Item Guarantee Matrix

Pure-deletion batch (zero behavior change): the 15-row + 7-row guarantee
matrices apply by cross-reference — see `per-wave-guarantee-matrix.md`
(the canonical rule) for both matrices; every row's proof here is the
existing ratchet/test suite staying green after deletion (no new envelope
is created, none is weakened).

## Plan Items

- [x] Delete 6 common modules + lib.rs tombstones
  - Files: crates/common/src/{open_price_rest_fallback,open_price_source,phase,feed_parity,formulas,candle_fold}.rs, crates/common/src/lib.rs
  - Tests: per-crate sweep (tickvault-common)
- [x] Delete core scheduler module + lib.rs tombstone
  - Files: crates/core/src/scheduler/mod.rs, crates/core/src/lib.rs
  - Tests: per-crate sweep (tickvault-core)
- [x] Delete csv_row + dead extractor/selector halves
  - Files: crates/core/src/instrument/{csv_row.rs,index_extractor.rs,index_futures.rs,mod.rs}
  - Tests: daily_universe_scope_guard (unchanged, must stay green), per-crate sweep
- [x] Delete instance_lock named-lock arms + rewire behavioral tests to legacy wrappers
  - Files: crates/core/src/instance_lock.rs
  - Tests: rewired SSM-stub suite via try_acquire_instance_lock/renew/release + spawn_instance_lock_heartbeat
- [x] Delete token_cache fast-boot arms
  - Files: crates/core/src/auth/token_cache.rs, crates/core/src/auth/mod.rs
  - Tests: remaining save/load suite
- [x] Delete tick_types dead items
  - Files: crates/common/src/tick_types.rs
  - Tests: remaining ParsedTick/DhanIntradayResponse suite + schema_validation.rs (untouched)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Any deleted symbol still referenced | compile error in sweep → fixed before push |
| 2 | Guard needle on deleted symbol | guard test red → mechanical row-drop or row skipped |
| 3 | Twin-name confusion (candle_fold) | only common's module deleted; app twin grep-verified intact |
