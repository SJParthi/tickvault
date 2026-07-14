# Implementation Plan: Aggregation automation ratchets — catch-up/midnight spawn pins + 3 paging-map entries (PR-3 of the automation-gaps series)

**Status:** VERIFIED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) — automation-coverage audit follow-up (scratchpad `automation-map.md`, 2026-07-10 read-only audit), PR-3 dispatched via the coordinator session 2026-07-14

> **Per-item guarantee matrix:** this plan cross-references
> `.claude/rules/project/per-wave-guarantee-matrix.md` (the 15-row + 7-row
> matrices apply per that rule's cross-reference clause). Scope is
> tests + terraform + rule-file docs ONLY — no hot-path code, no §28 area,
> no WS endpoints, no behavior change to the aggregator (crates touched:
> `tickvault-app` tests + `tickvault-common` drift-guard surfaces; zero
> `crates/*/src` edits).

## Design

Close the 3 Verified automation gaps from the 2026-07-10 coverage audit:

1. **GAP-1 (HIGH) — catch-up seal drivers can silently stop.** The
   BOUNDARY-01 watermark catch-up drivers (main.rs `spawn_engine_b_aggregator`
   Task 4 for Dhan; `groww_bridge.rs::spawn_groww_catchup_seal` for Groww)
   have NO spawn wiring ratchet — a refactor dropping either `tokio::spawn`
   compiles green. Fix (a): new source-scan ratchet
   `crates/app/tests/aggregation_task_wiring_guard.rs` (house pattern of
   `seal_drop_paging_wiring_guard.rs` + `boot_helpers.rs::
   ratchet_main_rs_spawns_close_time_force_seal`) pinning: the Task 4 block
   (marker + `compute_catchup_cutoff(` + `catch_up_seal_all(` +
   `CATCHUP_SEAL_POLL_INTERVAL_SECS`), and the Groww driver (defined AND
   called in the production region, with the same cutoff gate + seal call).
   Fix (b) — liveness-floor DECISION (evidence-based, per the audit's
   decision framework): **NO floor alarm.** Evidence:
   `tv_boundary_catchup_total{feed}` increments ONLY per ROUTED catch-up
   seal (main.rs:5925, groww_bridge.rs:1826 — inside the seal closure), and
   `tv_boundary_catchup_skipped_total` only during a poisoning episode; on a
   healthy day catch-up seals are legitimately ~0, so the metric is SPARSE —
   silence is ambiguous and a "0 in-market" floor alarm would false-page
   every healthy low-lag day. Additionally, even a registered counter keeps
   emitting per-scrape samples from the metrics RECORDER regardless of the
   driver task's liveness, so metric PRESENCE cannot prove the task is
   alive; only a per-wave heartbeat increment could — and adding one is a
   `crates/*/src` behavior change outside this PR's scope. The honest fix is
   the spawn ratchet + a dated residual note in
   `wave-6-error-codes.md` (BOUNDARY-01 section).

2. **GAP-2 (HIGH) — IST-midnight force-seal (Task 3) spawn unpinned.** The
   same new ratchet pins the main.rs Task 3 block (marker +
   `force_seal_all(` + trading-day gate) AND the BOUNDARY-01-mandated
   ordering `force_seal_all(…)` → `reset_watermark()` inside BOTH midnight
   tasks (Dhan Task 3 + `spawn_groww_ist_midnight_force_seal` — the
   existing `test_run_groww_bridge_spawns_ist_midnight_force_seal` pins the
   Groww spawn but not the watermark-reset ordering).

3. **GAP-3 (MED) — CROSS-VERIFY-1M-01/02 + TICK-CONSERVE-01 page nobody.**
   Add 3 entries to `error_code_alerts` in
   `deploy/aws/terraform/error-code-alarms.tf` (canonical coded shape
   `{ $.code = "X" && $.level = "ERROR" }`, period 300 / threshold 1 /
   eval 3 / dta 1), each `ok_recovery = false`: all three are daily
   one-shot audit findings (15:31 cross-verify mismatch/degrade; 15:40
   conservation residual) — the auto-OK ~15 min after the single datapoint
   ages out would be a Rule-11 false recovery (the AGGREGATOR-DROP-01 /
   ws-reinject-01 precedent). All 3 codes have Verified ERROR-level
   tag-guard emit sites in `crates/app/src` production regions
   (cross_verify_1m_boot.rs:477/573/735/747/862,
   tick_conservation_boot.rs:438), so the drift guard's dead-filter check
   passes. Update in lockstep (the bidirectional drift guard
   `error_code_paging_filter_drift_guard.rs` enforces tf↔doc agreement):
   `observability-architecture.md` "Which codes page" paragraph + dated
   2026-07-14 delivery-boundary notes in
   `cross-verify-1m-error-codes.md` and
   `tick-conservation-audit-error-codes.md`. Cost: +3 log-filter alarms
   ≈ $0.30/mo, documented inline in the tf header (the 2026-07-09/07-10
   error-code-alarm sibling convention — the aws-budget.md COST NOTE
   sections are the custom-metric/EMF-series convention, not used by the
   07-09/07-10 log-filter additions).

## Plan Items

- [x] GAP-1a+2: new ratchet `crates/app/tests/aggregation_task_wiring_guard.rs`
  - Files: crates/app/tests/aggregation_task_wiring_guard.rs
  - Tests: test_main_rs_spawns_ist_midnight_force_seal_with_watermark_reset,
    test_main_rs_spawns_watermark_catchup_driver,
    test_groww_bridge_spawns_catchup_driver,
    test_groww_midnight_force_seal_resets_watermark_after_seal,
    test_synthetic_ordering_helper_detects_planted_drift
- [x] GAP-1b: documented residual (no floor alarm — evidence above)
  - Files: .claude/rules/project/wave-6-error-codes.md
  - Tests: N/A — docs (decision + evidence recorded in the rule file)
- [x] GAP-3: 3 error_code_alerts entries + lockstep docs
  - Files: deploy/aws/terraform/error-code-alarms.tf,
    .claude/rules/project/observability-architecture.md,
    .claude/rules/project/cross-verify-1m-error-codes.md,
    .claude/rules/project/tick-conservation-audit-error-codes.md
  - Tests: existing crates/common/tests/error_code_paging_filter_drift_guard.rs
    (all 4 live-file assertions re-run green over the new entries)
- [x] GAP-3 rider (found during verification): drift-guard production-region
  scanner upgraded from TRUNCATE-at-first-test-mod to EXCISE-test-mods —
  `cross_verify_1m_boot.rs` carries a MID-FILE `#[cfg(test)] mod
  start_decision_tests` at ~line 134 with the real CROSS-VERIFY-1M emits
  AFTER it; the old truncation reported both new entries as DEAD filters
  (a false-dead in the guard itself, the inverse-false-OK class it
  exists to catch). Char-level string-aware brace matching (a per-line
  delta cannot see a single-line balanced `mod t { … }`).
  - Files: crates/common/tests/error_code_paging_filter_drift_guard.rs
  - Tests: synthetic_emit_detector_ignores_comments_and_test_regions
    (new mid-file-module + decl-only + multi-line regression pins)
- [x] V7 plan-cap: archived 2026-07-14-spot-1m-diagnostics.md (PR #1524
  merged at ee96a3d0, 0 unchecked items)
  - Files: .claude/plans/archive/2026-07-14-spot-1m-diagnostics.md
  - Tests: N/A — archival per plan-enforcement.md convention

## Edge Cases

- **Concurrent PR #1528 overlap (coordinator flag, 2026-07-14):** another
  in-flight PR is adding error_code_alerts entries (AUTH-GAP-05, SPOT1M,
  CHAIN, telegram-drop) to the SAME tf map + doc list + possibly the drift
  guard. Resolution rule when whichever lands second rebases: **KEEP BOTH
  sides' entries** in the tf map AND the doc paragraph (the drift guard is
  additive-safe — it uses `>= 9` self-checks + bidirectional set agreement,
  never an exact pinned set). This PR appends its 3 entries at the END of
  the map (after wal-suspend-01), one entry per tight hunk.
- Comment text must never satisfy ratchet needles: the new guard strips
  `//` line comments (`://` URL-aware) from every scanned window; block
  anchors use the raw source markers, needles use the stripped window
  (main.rs Task 3b carries a literal comment "no `reset_watermark()` here"
  — stripping makes it inert).
- groww_bridge.rs has an EARLY `#[cfg(test)]` at line 908 gating a
  test-only fn (not a mod) — a naive first-token production-region split
  would hide the production spawns at 2370/2656 (the WS-GAP-07 false-dead
  class); the guard uses the module-aware `production_region` (drift-guard
  house pattern) with synthetic self-tests.
- `force_seal_all_session_scoped(` (Task 3b) contains `force_seal_all` as
  a substring — needles use the exact `force_seal_all(` call form (next
  char `(`), which the session-scoped call does not match.
- The doc paging-list parser picks ANY valid code token between
  "Filtered+alarmed codes" and "Everything else" — the added sentence
  names ONLY the 3 new codes (no BOUNDARY-01 or other valid-code mentions
  in that paragraph).

## Failure Modes

- A future refactor drops the Task 3 / Task 4 / Groww catch-up spawn →
  the new ratchet fails the build with a named message (previously: silent
  compile-green regression, catch-up/midnight sealing dead).
- A midnight task loses its `reset_watermark()` (or reorders it before the
  seal) → ratchet fails (a poisoned watermark would never self-heal;
  BOUNDARY-01 contract).
- A tf entry names a typo'd code / wrong shape / loses its emit site → the
  existing drift guard fails (dead-filter / malformed-shape / bidirectional
  checks).
- The 3 new alarms mis-set `ok_recovery = true` → Rule-11 false-recovery
  OK pages on daily one-shot findings; prevented by review + the tf locals
  rationale comment (drift guard does not check ok_recovery — the
  seal-drop guard precedent pins it only for aggregator-drop-01; accepted
  residual, documented here).
- Terraform syntax error → `terraform fmt -check`/validate (or byte-shape
  parity with sibling entries if terraform is unavailable in the sandbox)
  + the drift guard's HCL parser (which fails loudly on a malformed map).

## Test Plan

- `cargo test -p tickvault-app --test aggregation_task_wiring_guard` — all
  new ratchets green against the real tree; synthetic anti-vacuity tests
  prove planted drift is detected.
- `cargo test -p tickvault-common --test error_code_paging_filter_drift_guard`
  — the 4 live-file assertions accept the 3 new entries (valid codes,
  pinned shape, real ERROR-level emits, tf↔doc agreement).
- `cargo fmt --check` + clippy on touched crates (tests-only Rust changes).
- `terraform fmt -check deploy/aws/terraform/error-code-alarms.tf` (if the
  terraform binary is available; else state honestly: syntax checked by the
  drift guard's parser + byte-shape match with sibling entries).

## Rollback

- Pure-additive change: revert the single commit. No schema, no runtime
  behavior, no hot path touched. Removing the tf entries reverts the 3
  alarms at the next `terraform apply` (alarms are for_each-generated from
  the map). The archived plan file moves back with `git mv` if ever needed.

## Observability

- GAP-3 IS the observability delta: 3 previously log-sink-only High codes
  now page via the canonical errcode chain (error! → errors.jsonl → CW
  Logs filter → tv_errcode_* → alarm ≤5 min → SNS → Telegram), each with
  `ok_recovery = false` (no false-recovery OK page). One-time apply-evening
  INSUFFICIENT_DATA→OK settling produces NO page for these 3 (ok_actions
  empty).
- GAP-1/2 add build-time observability (ratchets), not runtime signals;
  the honest runtime residual (no liveness floor for a silently-stopped
  catch-up driver) is documented with evidence in wave-6-error-codes.md.
- Cost note: +3 log-filter alarms ≈ $0.30/mo, recorded in the tf header
  (sibling convention).

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the
Task 3 / Task 4 / Groww catch-up spawn sites + midnight watermark-reset
ordering are build-failing source-scan ratchets with synthetic
anti-vacuity self-tests; the 3 new paging entries are drift-guarded on all
three surfaces (tf map ↔ doc list ↔ ERROR-level emit sites). NOT claimed:
runtime liveness detection of a silently-stopped catch-up driver (no dense
per-wave metric exists; a sparse-counter floor alarm would false-page —
the residual is documented, never camouflaged), and ok_recovery flags are
review-pinned, not machine-checked (stated residual).
