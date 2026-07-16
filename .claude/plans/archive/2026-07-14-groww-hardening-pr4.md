# Implementation Plan: Groww Hardening PR-4 — seal no-fetch ratchet + live-bar freshness primitive

**Status:** APPROVED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) via coordinator session, 2026-07-14 — lossless-reconnect build PR-4 authorized

## Plan Items

- [x] Seal no-fetch ratchet — source-scan guard test pinning that the aggregator seal path (`crates/trading/src/candles/*`) never grows a REST/network fetch call site (seal is pure in-memory fold; any fetch on the seal path would violate live-feed purity + the cold-path-only REST grants).
  - Files: crates/trading/tests/seal_no_fetch_guard.rs
  - Tests: the guard test itself (build-failing ratchet)
- [x] Live-bar decision-freshness primitive — pure, total, saturating `live_bar_is_fresh(bar_ts_secs, now_secs, threshold_secs)` implementing the §38.8 decision-freshness gate contract (`groww-second-feed-scope-2026-06-19.md` §38.8): stale ⇒ false, future-skewed bar ⇒ fresh (clock-skew tolerant), unit + proptest covered.
  - Files: crates/common/src/live_bar_freshness.rs, crates/common/src/lib.rs
  - Tests: unit tests in the module + proptest never-panic/total coverage
- [x] Read-only wiring — additive `fresh_within_60s` bool on `FeedHealthRow` (GET /api/feeds/health), populated from `last_tick_age_secs` via the shared primitive; `None` age reads `false` (fail-closed, Rule 11). No behavioral change to any feed, toggle, or verdict path.
  - Files: crates/api/src/handlers/feeds.rs
  - Tests: existing feeds handler tests (serde-additive field; per-crate suite)

## Design

The seal path in `crates/trading/src/candles/` is a pure in-memory fold (tick → cell → sealed candle); the REST grants (`no-rest-except-live-feed-2026-06-27.md` §8/§9) are cold-path scheduled tasks. PR-4 adds a mechanical ratchet (`seal_no_fetch_guard.rs`) that source-scans the seal modules for HTTP/fetch needles so a future change can never smuggle a network call into the seal fold, and a shared freshness primitive in `crates/common` (`live_bar_freshness::live_bar_is_fresh`) that any future strategy consumer MUST use to fail closed on stale rows (§38.8). The single consumer wired now is the read-only `/api/feeds/health` row (`crates/api`), which exposes `fresh_within_60s` derived from the existing `last_tick_age_secs` — an age of A seconds maps to `live_bar_is_fresh(0, A, 60)`. Everything is cold-path/read-only; the tick hot path, aggregator, and OMS are untouched.

## Edge Cases

- `None` last-tick age (feed never streamed this session) → `fresh_within_60s = false` — fail-closed, never a false-OK.
- Age exactly at the threshold (60s) → fresh (inclusive `<=`, pinned by unit test).
- Future-skewed bar timestamp (bar_ts > now) → saturating_sub yields 0 → fresh (clock-skew tolerant, pinned by unit test + proptest totality).
- `u64::MAX` boundaries → no overflow/panic (saturating arithmetic, proptest-pinned).
- Guard-test needle false positives → verified zero matches on current `crates/trading/src/candles/*` tree; comment/string literals scanned identically (a needle in a comment fails loud, forcing an explicit review).

## Failure Modes

- A future PR adds a REST call inside the seal fold → the ratchet test fails the build (the whole point).
- The freshness primitive regresses (panic/overflow) → unit + proptest suites fail in `tickvault-common`.
- The API field breaks serialization → `tickvault-api` per-crate suite + CI full battery catch it; the field is additive so existing consumers of the JSON are unaffected.
- Threshold drift: the 60s display bound is a local named const at the single construction site; changing it is a reviewed one-line diff, never scattered.

## Test Plan

- `cargo test -p tickvault-trading --test seal_no_fetch_guard` — the ratchet passes on the current tree.
- `cargo test -p tickvault-common` — freshness unit tests + proptest.
- `cargo test -p tickvault-api` — feeds handler suite with the additive field.
- `cargo fmt --all -- --check` clean. Full 22-category battery runs in CI per testing-scope.md (3 touched crates locally per the block-scoped default).

## Rollback

Revert the single PR commit: the guard test, the freshness module + its `lib.rs` registration, and the additive API field are self-contained (no schema change, no config change, no migration). Removing the `fresh_within_60s` field is JSON-additive-in-reverse — no consumer contract breaks (the field shipped in the same PR).

## Observability

The new field surfaces on the existing read-only `GET /api/feeds/health` JSON (public read per the 2026-06-23 ruling) — no new counters, alarms, or ErrorCodes (no new failure mode exists: the primitive is pure and the guard is a test). The §38.8 gate contract this primitive implements is documented in `groww-second-feed-scope-2026-06-19.md` §38.8 and `rest-1m-pipeline-error-codes.md` §3; future strategy consumers wire their own paging when they arrive (own dated operator scope).

## Per-Item Guarantee Matrix

The 15-row + 7-row guarantee matrices apply by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md` (the canonical matrix
file). PR-4 specifics: no hot-path involvement (guard = test-only; primitive =
pure cold fn; API field = cold read-only handler), so the DHAT/Criterion rows
read N/A — not hot path; the ratchet row is satisfied by
`seal_no_fetch_guard.rs` itself (build-failing on regression); dedup/uniqueness
rows are N/A — no persisted table is touched.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Feed streaming, last tick 5s ago | `fresh_within_60s = true` |
| 2 | Feed silent 61s+ | `fresh_within_60s = false` |
| 3 | Feed never streamed (age None) | `fresh_within_60s = false` (fail-closed) |
| 4 | Future REST call added to seal path | build fails via seal_no_fetch_guard |
