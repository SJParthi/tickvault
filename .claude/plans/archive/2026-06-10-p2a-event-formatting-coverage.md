# Implementation Plan: P2a — Telegram event formatting coverage battery

**Status:** APPROVED
**Date:** 2026-06-10
**Approved by:** Parthiban (operator, 2026-06-10: "once it gets merged go ahead with the plan always" — P2 was presented and approved as: "write the plain missing unit tests (Telegram message formats, error branches)"; P2a is its first slice)
**Authority:** `.claude/plans/research/coverage-gaps.md` §5 rank 5 + §6 P2 (merged #1076); P0 gate merged (#1079).

**Per-item guarantee matrix:** cross-references `.claude/rules/project/per-wave-guarantee-matrix.md`. Deltas: test-only PR — zero production code changes, no hot path, no new pub fn, no new dep, no schema.

## Plan Items

- [x] Add formatting coverage battery for un-exercised NotificationEvent arms
  - Files: crates/core/tests/event_formatting_coverage.rs
  - Tests: test_auth_and_profile_event_messages, test_ws_pool_event_messages,
    test_ws_sleep_wake_event_messages, test_depth_lifecycle_event_messages,
    test_depth_dynamic_event_messages, test_order_update_event_messages,
    test_self_test_and_slo_event_messages, test_option_chain_event_messages,
    test_boot_and_infra_event_messages, test_oms_and_questdb_event_messages,
    test_misc_event_messages, test_uncovered_arms_have_severity_and_policy,
    test_pool_online_fast_boot_path_and_ping_freshness_bands
  - Each test constructs the previously-unexercised variants (identified from
    the llvm-cov report: 60+ `to_message` arms + ~30 `severity` arms at main
    @ a6fe35c4), calls `to_message()`, asserts a distinctive substring per
    variant, and routes every constructed event through `severity()` +
    `dispatch_policy()` so those match arms execute too.

## Design

Pure black-box unit coverage through the public API (`NotificationEvent` and
its `to_message` / `severity` / `dispatch_policy` methods are pub). One new
integration-test file in `crates/core/tests/` — production source untouched.
Substring assertions pin operator-facing wording (10 Telegram commandments
stay enforceable per variant); severity assertions pin the documented tiers
(e.g. `WebSocketPoolHalt` ⇒ Critical-class behavior is asserted via tag text
where stable). The `BootPathLabel::Fast` branch and all 4 ping-freshness bands
(`None`, ≤10s ✓, 11–30s ⚠, >30s ❌) are exercised through
`WebSocketPoolOnline` / `WebSocketPoolPartialAfterDeadline` payloads.

## Edge Cases

- Vec-carrying variants tested both with samples and (where formatting
  branches on emptiness) with empty vecs (`SelfTestDegraded.failed`,
  `DepthDynamicV2DiffApplied.added/removed`, `OrphanPositionDetected.sample_symbols`).
- `DepthDynamicV2DiffApplied` unresolved-drop discrepancy branch covered via
  `stats_added > added.len()`.
- `format_ping_freshness` boundary values: None / 0 / 10 / 11 / 30 / 31.
- HTML-escape path covered via a `DepthDiffEntry.display_label` containing
  `<` and `&`.
- No clock, no network, no filesystem — fully deterministic assertions.

## Failure Modes

- Wording drift in any pinned arm → substring assertion fails the build
  (intended ratchet; wording changes must update the test in the same PR).
- A variant's fields change → test fails to compile in PR CI (Test (core)
  job) — loud, immediate.
- False-green risk: none — every assertion checks rendered output, not just
  "doesn't panic".

## Test Plan

- `cargo test -p tickvault-core --test event_formatting_coverage` — all new
  tests green.
- Scoped per testing-scope.md: core-crate tests (`cargo test -p tickvault-core`).
- `cargo fmt --check`, CI-shaped `cargo clippy --workspace --no-deps`, strict
  clippy on the new test target, banned-pattern scanner, plan-verify.

## Rollback

`git revert` of the single squash commit deletes the test file; nothing else
references it. Zero production-behavior surface.

## Observability

- N/A at runtime (test-only). The observable effect is in CI: the Coverage &
  Perf gate's `core` row rises (~+250 covered lines ≈ +0.7pp), enabling a
  future floor ratchet per the thresholds-file ratchet rule. No floor change
  in THIS PR — floors move up only on re-measured baselines.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Every targeted variant rendered | distinctive substring found |
| 2 | Every targeted variant classified | severity() + dispatch_policy() execute without panic |
| 3 | Fast-boot pool message | "Mid-market crash recovery" label rendered |
| 4 | Ping freshness bands | ✓ / ⚠ / ❌ / "—" all rendered |
| 5 | Hostile display_label | HTML-escaped in rendered line |
