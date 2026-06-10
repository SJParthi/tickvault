# Implementation Plan: P2c — lifecycle + shadow persistence error-arm coverage

**Status:** VERIFIED
**Date:** 2026-06-10
**Approved by:** Parthiban (operator, 2026-06-10: "once it gets merged go ahead with the plan always" — P2 approved as "write the plain missing unit tests (error branches)"; P2c follows merged P2b #1082)
**Authority:** `.claude/plans/research/coverage-gaps.md` §5 ranks 15+18 + §6 P2 (merged #1076).

**Per-item guarantee matrix:** cross-references `.claude/rules/project/per-wave-guarantee-matrix.md`. Deltas: test-only additions inside the two files' existing `#[cfg(test)] mod tests` — zero production lines changed, no hot path, no new pub fn, no new dep (mock HTTP/TCP helpers replicate the file-local idiom already used in `tick_persistence.rs::tests`).

## Plan Items

- [x] Cover instrument_lifecycle_persistence HTTP/ILP I/O fns (152 missed lines)
  - Files: crates/storage/src/instrument_lifecycle_persistence.rs
  - Tests: test_ensure_lifecycle_tables_with_mock_200,
    test_append_lifecycle_row_mock_200_and_500,
    test_append_lifecycle_audit_row_mock_200_and_500,
    test_update_state_and_bump_last_seen_mock_200_and_500,
    test_append_lifecycle_rows_ilp_drain_and_error,
    test_append_lifecycle_audit_rows_ilp_drain_and_error
  - Mock-HTTP 200/500 covers the `/exec` happy + bail arms of
    `append_instrument_lifecycle_row`, `append_instrument_lifecycle_audit_row`,
    `update_lifecycle_state`, `bump_active_last_seen`, and both `ensure_*`
    DDL walks; a TCP drain server covers the ILP plural fns (+ empty-slice
    early return + connect-failure Err arm via port 1).

- [x] Cover shadow_persistence DDL walk + legacy-drop marker lifecycle (116 missed)
  - Files: crates/storage/src/shadow_persistence.rs
  - Tests: test_ensure_shadow_candle_tables_with_mock_200,
    test_ensure_shadow_candle_tables_with_mock_400,
    test_drop_legacy_candle_objects_marker_lifecycle
  - Mock-HTTP 200 + 400 cover `ensure_shadow_candle_tables` (21-table DDL +
    DEDUP + ALTER walk, both arms). The marker test runs the legacy-drop
    sweep against mock 200 (marker absent → full sweep → marker written),
    then re-runs to hit the early-skip arm; the cwd-relative marker file is
    removed before AND after (self-cleaning, no repo pollution).

## Design

Black-box via the files' pub async fns, pointed at file-local mock servers
(same `spawn_mock_http_server` / `spawn_tcp_drain_server` idioms as
`tick_persistence.rs::tests` — kept module-local per repo convention).
Existing `sample_index_row()` / `sample_audit_row()` fixtures are reused for
row payloads. No live QuestDB, no Docker, deterministic.

## Edge Cases

- Non-2xx `/exec` response: body reflected into the error capped at 200 chars
  (asserted via a 500 with a long body NOT exceeding the cap in the message).
- Empty `rows` slice on both ILP plural fns → Ok without connecting.
- ILP connect failure (port 1) → Err surfaces, no panic.
- Legacy-drop marker present → entire sweep skipped (early return).
- Marker write best-effort semantics untouched (not asserted on failure —
  needs an unwritable cwd; out of scope, flagged honest).

## Failure Modes

- If a refactor breaks the non-2xx bail (silent Ok on QuestDB error), the
  mock-500 assertions fail.
- If the ILP plural path stops building rows or starts panicking on connect
  failure, the drain/error tests fail.
- If the marker gate inverts (sweep re-runs every boot), the second-run
  assertion fails.
- Flake surface: none — loopback sockets bound to port 0 (OS-assigned), no
  timing assumptions; the marker test owns its path exclusively and cleans up.

## Test Plan

- `cargo test -p tickvault-storage --lib` new tests green.
- Scoped suite `cargo test -p tickvault-storage --features daily_universe_fetcher`.
- `cargo fmt --check`; pre-commit invariant gates on warm cache.

## Rollback

`git revert` of the single squash commit removes only `#[cfg(test)]` code.

## Observability

- N/A at runtime (cfg(test)-only). CI effect: storage coverage rises further
  (~+200 lines ≈ +1.6pp vs the 91.35 baseline); floors ratchet on the next
  re-measured baseline, not in this PR.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | All lifecycle HTTP fns vs mock 200 | Ok |
| 2 | Same fns vs mock 500 | Err containing status, no panic |
| 3 | ILP plural appends vs drain server | Ok; empty slice short-circuits |
| 4 | ILP plural appends vs port 1 | Err, no panic |
| 5 | Shadow DDL walk vs mock 200 / 400 | completes both arms, no panic |
| 6 | Legacy drop: no marker → sweep + marker; marker → skip | both arms execute |
