# Implementation Plan: Dhan live-WS retirement Phase C-4 — ErrorCode variant sweep + dormant-code cleanup

**Status:** APPROVED
**Date:** 2026-07-15
**Approved by:** Parthiban (operator) — standing 2026-07-13 retirement directive
(`.claude/rules/project/websocket-connection-scope-lock.md` "2026-07-13 Amendment"
§A/§B) + the per-file "retained until the C4 variant sweep" rulings recorded in
`dhan-rest-only-noise-lock-2026-07-14.md` (AUTH-GAP-06 / REST-CANARY-01),
`wave-3-d-error-codes.md` (SLO PARK banner: "Phase C variant cleanup"), the PR-C3
(#1569) judgment calls #2/#3, and the token_manager.rs WIRING-EXEMPT note
("C4 sweep owns deletion"). Coordinator-relayed C4 kickoff 2026-07-15.
**Crates:** tickvault-common, tickvault-core, tickvault-storage

> **Guarantee matrices:** this plan carries the 15-row + 7-row matrices by
> cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`
> (operator-charter-forever.md §C) — every item inherits both matrices;
> this is a deletion-only sweep (no new event, no new hot-path code, no new
> failure mode); per-item deltas are named in Design / Observability.

## Plan Items

- [ ] Item 1 — Delete the 19 zero-emit-site ErrorCode variants retained from
  Phases A/B/C1/C2/C3: `InstrFetch01CsvHardFailed`, `InstrFetch02SchemaValidationFailed`,
  `InstrFetch03DanglingReferences`, `InstrFetch04UniverseSizeOutOfBounds`,
  `NtmConstituency01SourceDegraded`, `PrevDay01CoverageEmpty`,
  `CrossVerify1m01MismatchFound`, `CrossVerify1m02FetchDegraded`,
  `DhanLane01UniverseBuildFailed`, `DhanLane02WsPoolSpawnFailed`,
  `DhanLane03AuthFailed`, `DhanLane04TeardownTimeout`, `WsGap05PoolRespawn`,
  `WsGap06TickGapSummary`, `WsGap07LiveChannelClosed`, `WsGap08RateLimitCooldown`,
  `WsGap09WatchdogReconnectInPlace`, `AuthGap06FastBootCachedTokenValidation`,
  `RestCanary01ProbeFailed` — plus the parked SLO trio `Slo01Healthy` /
  `Slo02Degraded` / `Slo03PublisherRespawned` (22 total; wave-3-d banner
  authorizes "Phase C variant cleanup"). KEEP `WsGap04PostCloseSleep` +
  `WsGap10OrderUpdateOutage` (emitted by the retained-dormant
  `order_update_connection.rs`).
  - Files: crates/common/src/error_code.rs, crates/common/tests/error_code_rule_file_crossref.rs, crates/storage/src/instrument_fetch_audit_persistence.rs (dated doc-comment truth-sync)
  - Tests: test_all_variants_have_unique_code_str, test_code_str_roundtrip_via_from_str, test_all_list_length_matches_catalogue_size, every_error_code_variant_appears_in_a_rule_file, every_rule_file_code_has_an_enum_variant (REVERSE_CHECK_ALLOWLIST extended for WS-GAP-05..09 + AUTH-GAP-06 — tracked prefixes whose rule-file history stays per house banner convention)

- [ ] Item 2 — Delete the caller-less parked SLO evaluator with its variants:
  `crates/core/src/instrument/slo_score.rs` (only consumers were its own tests,
  its bench, and comments), its Criterion bench + `[[bench]]` entry, and the two
  `slo_score_*` budget keys (the C3 tick-gap-budget-removal precedent).
  - Files: crates/core/src/instrument/slo_score.rs (DELETE), crates/core/benches/slo_score.rs (DELETE), crates/core/Cargo.toml, crates/core/src/instrument/mod.rs, quality/benchmark-budgets.toml
  - Tests: cargo test -p tickvault-core green; bench_budget_elements_guard stays green (no slo pin exists)

- [ ] Item 3 — Delete `TokenManager::initialize_deferred` (WIRING-EXEMPT since
  PR-C2, zero callers — "C4 sweep owns deletion") and the C3 judgment-call #2
  caller-less storage fns: `bump_active_last_seen` + `build_bump_active_last_seen_sql`
  (instrument_lifecycle_persistence.rs) and `read_last_fetch_audit_sha`
  (instrument_fetch_audit_persistence.rs), with their unit tests; retire the
  `storage_exposes_o1_warm_primitives` pin in daily_universe_scope_guard.rs
  with a dated tombstone (its own comment defers the decision to C4). The SEBI
  tables + every other persistence fn stay.
  - Files: crates/core/src/auth/token_manager.rs, crates/storage/src/instrument_lifecycle_persistence.rs, crates/storage/src/instrument_fetch_audit_persistence.rs, crates/storage/tests/daily_universe_scope_guard.rs
  - Tests: cargo test -p tickvault-storage / -p tickvault-core green; daily_universe_scope_guard remaining pins green

- [ ] Item 4 — Rule-file dated truth-sync: stamp the "retained until the C4
  variant sweep" notes (dhan-rest-only-noise-lock-2026-07-14.md ×2,
  dhan-rest-canary-error-codes.md, wave-3-d-error-codes.md) and the Phase C
  retirement banners (daily-universe-instr-fetch-error-codes.md,
  ntm-constituency-error-codes.md, prev-day-ohlcv-error-codes.md,
  cross-verify-1m-error-codes.md, dhan-lane-error-codes.md,
  wave-2-error-codes.md WS-GAP banner) with "variants DELETED in the C4 sweep
  (2026-07-15)" one-liners. Historical content is NEVER deleted (house banner
  convention).
  - Files: the 9 rule files above (in-place dated one-line notes only)
  - Tests: error_code_rule_file_crossref (both directions), error_code_paging_filter_drift_guard (no tf entry names a deleted code — verified pre-flight)

## Design

Evidence-based deletion sweep, grep-before-delete: every deleted variant was
verified to have ZERO emit sites and ZERO enum references outside
`error_code.rs` (pre-flight grep of both the variant name and the `code_str`
wire string across `crates/`, comments/doc-strings/log-line test fixtures
excluded — e.g. `feed_scoreboard_boot.rs` parses historical `"WS-GAP-09"`
JSONL strings as DATA, never the enum). The two survivors (`WsGap04PostCloseSleep`,
`WsGap10OrderUpdateOutage`) have live emit sites in the retained-dormant
`order_update_connection.rs` (scope-lock §A.1: module retained for the
live-trading re-wire) and MUST NOT be deleted. Cross-ref integrity uses the
EXISTING two mechanisms only: (a) untracked prefixes (INSTR-FETCH-, SLO-,
DHAN-LANE-, PREVDAY-, CROSS-VERIFY-1M-, NTM-CONSTITUENCY-, REST-CANARY- are
not in `TRACKED_PREFIXES`, the same mechanism that let DEPTH-DYN-01 retire
2026-07-06 with its rule-file history intact); (b) `REVERSE_CHECK_ALLOWLIST`
entries with justification comments for the TRACKED prefixes (WS-GAP-05..09,
AUTH-GAP-06). No new scheme is invented. The tag guard derives its prefix set
from `ErrorCode::all()`, so it shrinks automatically; the paging drift guard
is unaffected (no `error_code_alerts` tf entry names any deleted code —
verified against `deploy/aws/terraform/error-code-alarms.tf`).

## Edge Cases

- A rule file mentioning a deleted TRACKED-prefix code (WS-GAP-05..09,
  AUTH-GAP-06) would fail the reverse cross-ref check → allowlist entries with
  dated justifications, mirroring the existing DH-911/STORAGE-GAP-05 rows.
- `Severity::Info` keeps ≥1 variant after Slo01 deletion (Selftest01Passed,
  AggregatorHb01Heartbeat) so `test_every_severity_is_assigned_to_at_least_one_code`
  stays green.
- The `instrument_fetch_audit` table's `error_code` COLUMN keeps storing the
  historical `"INSTR-FETCH-0x"` wire strings (SEBI data format, string
  constants in tests) — only the enum mapping doc-comment is truth-synced.
- `test_prev_day_01_coverage_empty_contract` (error_code.rs) dies with its
  variant; no other per-variant contract test references a deleted variant.
- C2/C3 tombstone guard files (tick_gap_reset_wiring_guard etc.) pin
  STAYS-DELETED negatives and main.rs wiring — none references a deleted
  variant identifier; they are KEPT (deleting them would remove the negative
  ratchets the C3 PR body promised).

## Failure Modes

- A missed surviving reference to a deleted variant → compile error (loud,
  build-failing — the sweep is fail-closed by the type system).
- A missed rule-file tracked-prefix mention → `every_rule_file_code_has_an_enum_variant`
  fails with the exact code named (loud).
- A stale tf paging entry → `error_code_paging_filter_drift_guard` fails
  (pre-verified: none exists).
- Test-count baseline drop from deleted tests → the documented guard override
  (`echo <count> > .claude/hooks/.test-count-baseline`), exactly the C3
  restore-to-upstream-truth procedure — never `--no-verify`.

## Test Plan

Touching `crates/common` ⇒ FULL `cargo test --workspace` (expected failures:
ONLY the 2 known sandbox SSM tests `test_fetch_api_bearer_token_errors_without_real_ssm`
+ `test_verify_public_ip_fails_without_real_ssm`, green in CI). Plus
`cargo fmt --check`, `cargo clippy --workspace --no-deps -- -D warnings -W clippy::perf`,
`bash .claude/hooks/banned-pattern-scanner.sh`, `bash .claude/hooks/plan-verify.sh`.
Adversarial parallel-agent review until 2 consecutive clean rounds (focus:
deleted-variant references in surviving code, reverse cross-ref direction,
dormant order-update codes must survive, rule-file banners not deleted).

## Rollback

Pure deletion PR — `git revert` of the squash-merge commit restores every
variant, module, bench, and fn byte-identically. No schema change, no config
change, no data migration, no behavioral change to any live path (all deleted
code had zero production callers/emitters — verified pre-flight).

## Observability

No new signal needed; removals only, in lockstep with producers that were
already deleted in C2/C3 (no orphaned monitor is created and none existed:
zero `error_code_alerts` tf entries, zero EMF rows, zero alarms name any
deleted code). The two slo_score budget keys leave `benchmark-budgets.toml`
with their bench (bench-gate matches by substring — a keyless bench or a
benchless key would be inert, but both sides are removed together). The
tag-guard prefix set and the crossref forward check shrink automatically with
`ErrorCode::all()`.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Surviving code references a deleted variant | Compile error — build fails loud |
| 2 | Rule file mentions WS-GAP-05..09 / AUTH-GAP-06 (history) | Reverse crossref passes via dated allowlist entries |
| 3 | Rule file mentions INSTR-FETCH-01 etc. (untracked prefix) | Reverse crossref ignores (existing DEPTH-DYN-01 mechanism) |
| 4 | Dormant order-update module compiles | WsGap04/WsGap10 retained — emit sites intact |
| 5 | Workspace test count drops | Documented baseline override, never bypass |
| 6 | Future live-trading re-wire needs a retired code | Fresh variant + rule-file edit per house protocol (history retained in banners) |
