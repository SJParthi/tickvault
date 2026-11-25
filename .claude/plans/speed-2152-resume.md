# Implementation Plan: Resume preserved speed-health dashboard #2152

**Status:** VERIFIED
**Date:** 2026-10-31
**Approved by:** User instruction to resume unfinished work; coordinator's isolated #2152 continuation assignment.

## Scope and provenance

Base: green #2158 `0fd844582cbdac209f54e4394459b799f2ba797a`.
Preserved original: `2ec6b8f0fe5000ab539576bbb55863253f35bb3f`, retained on
`preserved/speed-health-2152`. This candidate imports that PR's code, the saved
five-test API patch, and the reviewed strict-proof correction. It does not
rewrite the original branch or import its stale plan status.

The coordinator has authorized scoped validation followed by a native-gated
commit and draft PR against main, explicitly depending on #2158. No merge,
deployment, environment publication, live service call, or withheld probe/fuzz
retry is part of this continuation. Cargo started only after the coordinator's
slot grant and the previous owner's explicit release of the shared target.

## Design

Retain the original bounded, bearer-gated cold-path API and its thin renderer.
Size telemetry arrays from the producer's complete `ALL_STAGES` catalog. Present
broad operational stall colours separately from strict lifetime evidence and
dated session proof. Frame populations retain their actual units and overlaps;
runtime/upkeep diagnostics never become strict frame rows. A current response
envelope does not establish measurement freshness or coverage.

## Edge Cases

Empty/inconsistent sample counts, first-bucket ambiguity, histogram overflow,
ring-refused frames, overlapping receipt timing, hidden tabs, stalled refreshes,
legacy envelopes, future clocks and exactly-expired leases have explicit
UNKNOWN or scoped adverse behavior. The new seven-stage test traverses every
producer index, including both indexes absent from the original five-slot API.

## Failure Modes

Unavailable queries/metrics degrade to grey tiles; oversized metric bodies and
HTTP errors are refused whole. Independent expiry withdraws current colours
while shared refresh work is stuck. A later histogram bucket may expose a
lifetime failure, but no available producer can certify a strict dated-session
PASS. Future WAL publication times cannot manufacture a fresh backlog reading.

## Test Plan

Run actual-JavaScript DOM/clock fixtures with stubbed fetch and an original-UI
negative control; execute fresh speed-health, dashboard, auth and shared-state
Rust tests after cleaning all eight local packages. Run applicable guards,
workspace validation and native hooks before the authorized draft publication.
Keep old-base focused positives/negative controls separate from new integration
results. Independent security review checks immutable source hashes. No fresh
coverage measurement or live performance proof is inferred from these checks.

## Rollback

The original #2152 history and both preserved input patches remain available.
This continuation stays on its own branch. Reverting this candidate's commit
removes the new route/panel and its boot/cache wiring; it does not rewrite or
migrate database rows. No deployment is performed as part of the rollback plan.

## Observability

The dashboard reads existing in-memory counters, bounded stored rows and the
configured metrics endpoint. Cache-hit counters retain the existing API metric.
Labels identify sample counts, operation population, since-startup window and
unknown freshness/session coverage; the 75-second published maximum is separate.
No new hot-path instrument, alert or strict measurement version is introduced.

## Plan Items

- [x] Preserve the original dashboard, bearer-gated route, bounded cached API,
  metrics configuration and regression tests on the current base.
  - Files: crates/api/src/handlers/dashboard_page.rs, crates/api/src/handlers/speed_health.rs, crates/api/src/handlers/mod.rs, crates/api/src/lib.rs, crates/api/src/state.rs, crates/api/tests/debug_auth_gate.rs, crates/app/src/main.rs, crates/common/src/ingest_shed.rs, crates/common/tests/browser_surface_and_toolchain_guard.rs, crates/common/tests/dead_const_ratchet.rs
  - Tests: test_fetch_metrics_text_accepts_exactly_16_mib_without_truncation, test_fetch_metrics_text_refuses_chunked_16_mib_plus_one_whole, test_fetch_metrics_text_refuses_503_with_a_valid_metrics_body, test_read_minute_table_without_designated_column_only_queries_columns, test_read_minute_table_queries_discovered_timestamp_and_returns_rows
- [x] Reconcile all seven current telemetry stages without treating broad stall
  health as strict p99 proof. Keep unknown/expired/legacy data grey and valid
  recorded lifetime failures visible with session UNKNOWN.
  - Files: crates/api/src/handlers/speed_health.rs, crates/api/src/handlers/dashboard_page.rs
  - Tests: test_current_stage_catalog_is_complete_and_diagnostics_are_separate, test_strict_target_never_promotes_lifetime_buckets_to_session_proof, test_zero_and_inconsistent_samples_are_unknown_even_with_a_small_bucket, test_fresh_wal_reading_withdraws_a_reading_older_than_two_and_a_half_polls
- [x] Validate the combined candidate at its exact source hash, including local
  Rust tests/guards and independent source review. Native commit/draft publication
  follows the verified implementation; CI and merge remain separate states.
  - Files: crates/api/src/handlers/speed_health.rs, crates/api/src/handlers/dashboard_page.rs
  - Tests: test_current_stage_catalog_is_complete_and_diagnostics_are_separate, test_response_capture_contract_is_not_a_sample_timestamp

## Completed local implementation validation

The unchanged reviewed API source passed 44 focused tests and a fresh full
workspace run: 15,480 passed, 0 failed, 33 ignored (unit, integration and doctest
summaries). The full run includes six debug bearer-auth regressions, sixteen
browser/toolchain guards and three dead-constant checks. Workspace all-target
Clippy passed with `-D warnings -W clippy::perf`. The actual-JavaScript DOM/clock
harness passed nineteen cases; the original UI fails its legacy-green negative
control. The DOM auth/refresh case is source-pattern checking, not an executed
browser Save/401 flow. No ignored harness is counted as a pass.

Independent source review found no blocker in the reconciliation, checking the
seven-stage catalog, strict UNKNOWN/lifetime failure scope, WAL timestamp ages,
bearer attachment and text rendering. Reviewed API SHA256:
`6d8d5b96a4e619a44deafee0f503b550387b19ad9d927cd376020c33ffbf9b05`.
Reviewed dashboard SHA256:
`57f7ed96f12f14faecdd07fb8c4064984caec8011d95db0c0e0291f494417ec4`.

VERIFIED here means these local implementation checks completed. Native
commit/push gates and draft publication follow; CI, merge, deployment, live
sample freshness, complete session evidence and measured coverage remain
separate states. No performance guarantee is established by this plan status.

## Scenarios

| Scenario | Expected |
|---|---|
| Legacy, expired, future or malformed response | UNKNOWN, no current colour |
| Zero/inconsistent histogram denominator | UNKNOWN |
| Lifetime p99 in first bucket <=1,024 ns | Strict UNKNOWN |
| Lifetime p99 in a later bucket whose lower edge exceeds 1,000 ns | Recorded lifetime FAIL, session UNKNOWN |
| Runtime timers / metrics upkeep | Operational diagnostics, excluded from strict frame rows |
| Receipt-to-fold | Live main-feed frames, overlapping earlier timing, excludes ring-refused frames |
| Missing requested-date interval/freshness evidence | No strict PASS or Monday/session guarantee |
| Future WAL publication stamp | Unknown age, no fresh green backlog reading |

## Per-Item Guarantee Matrix

All items cross-reference the 15-row and 7-row matrices in
`.claude/rules/project/per-wave-guarantee-matrix.md`. Those are verification
obligations, not established guarantees. Validation results are recorded with
their exact scope before publication; CI remains pending on a new draft PR.
The saved five-test evidence belongs to the old base, not this integration.
Hot-path producers, dependencies, schema and alerting remain outside this change.
