# Implementation Plan: Phase B batch 3 — final depth-era NotificationEvent family sweep

**Status:** APPROVED
**Date:** 2026-06-10
**Approved by:** Parthiban, 2026-06-10: "yes go ahead with your recommendation" (Phase B step 5: delete unused events.rs NotificationEvent variants) + "once it gets merged go ahead with the plan always". Batches 1 (#1084) and 2 (#1088) merged.

**Guarantee matrices:** See `per-wave-guarantee-matrix.md` (cross-referenced; deletion-only change — no new pub fns, tables, or hot-path code).

---

## Design

Completes the depth-event family deletion started in #1088. All 12 remaining depth-era `NotificationEvent` variants have **zero production constructors** (verified per-variant by repo-wide grep, 2026-06-10 post-#1088): `DepthIndexLtpTimeout`, `DepthUnderlyingMissing`, `DepthTwentyConnected`, `DepthTwentyDisconnected`, `DepthTwentyDisconnectedOffHours`, `DepthTwentyReconnected`, `DepthTwoHundredConnected`, `DepthTwoHundredDisconnected`, `DepthTwoHundredDisconnectedOffHours`, `DepthTwoHundredReconnected`, `DepthDynamicV2DiffApplied`, plus the `DepthDiffEntry` helper struct (used only by `DepthDynamicV2DiffApplied`; exported via `notification/mod.rs`). Their emitters (depth-20/200 connections, depth rebalancer, depth-dynamic pipeline v2 — `depth_dynamic_pipeline_v2.rs` no longer exists) were all deleted in AWS-lifecycle PR #4 (2026-05-19).

Deletion surface (all `crates/core`):
1. `notification/events.rs` — 12 variant defs + `DepthDiffEntry` struct/impl (`format_line`, html-escape USAGE only — the shared `html_escape()` helper itself stays if any surviving event uses it; verified before cut) + to_message/topic/severity arms + internal tests (`test_depth_diff_entry_*` ×3, `test_depth_20/200_disconnected_*` ×5).
2. `notification/mod.rs` — `DepthDiffEntry` re-export.
3. `tests/event_formatting_coverage.rs` — the depth render blocks (`test_depth_connection_event_messages`-class fns and remaining depth arms incl. `DepthDynamicV2DiffApplied`).
4. `tests/plan_p4_p5_alert_proof.rs` — the 4 depth-specific proof tests (the 4 QuestDB/WS tests stay; file header doc updated).
5. `tests/ws_telegram_visibility_guard.rs` — the source-scan asserts REQUIRING the 6 depth connection variants to exist in events.rs (test-time trap; removed in the same commit, WS asserts stay).

## Edge Cases

- `ws_telegram_visibility_guard.rs` is a source-scan guard: it greps events.rs for variant names — deleting variants without updating it fails at TEST time, not compile time. Updated in the same commit.
- `html_escape()` (or equivalent escaping used by `DepthDiffEntry::format_line`) may be shared — checked for surviving callers before deletion; only `DepthDiffEntry`-private machinery is removed.
- Severity/topic matches are exhaustive — a missed arm fails compile (safety net).
- Test-count baseline drops (~12-14 test fns across 4 files); documented override with justification in the commit body.
- Coalescer topic strings are arbitrary `&str` (proven in batch 2) — untouched.

## Failure Modes

- **Hidden consumer:** per-variant grep proof pasted in PR body; `cargo build --workspace` + full core suite (CI feature flag) before commit.
- **Source-scan guard traps:** all `crates/*/tests/*guard*.rs` grepped for every deleted symbol name before commit.
- **Concurrent main movement:** fetch + merge origin/main before push; re-verify after merge.
- **CI divergence:** draft PR → CI green → ready → auto-merge (squash) → monitored to merge.

## Test Plan

- `cargo test -p tickvault-core --features daily_universe_fetcher` (CI's invocation) — paste real result lines.
- `cargo build --workspace`, `cargo fmt --check`, CI-exact clippy (`--workspace --no-deps`) — zero new warnings in touched files.
- Hook gates: banned-pattern, plan-gate, pre-commit chain, test-count override.

## Rollback

Single squash-merged PR; `git revert` restores all 12 variants + struct + guards verbatim. No config/schema/data changes.

## Observability

- Deletes only never-emitted operator surface (zero emitters since 2026-05-19); no live Telegram/log/metric output changes.
- No metric names are removed in this batch (verified: none of the deleted arms emit counters).
- PR body documents the guard-test reductions so reviewers see exactly which pins were removed and why.

## Plan Items

- [x] Batch 3 — delete the 12 remaining depth-era variants + DepthDiffEntry + guard-test updates
  - Files: crates/core/src/notification/events.rs, crates/core/src/notification/mod.rs, crates/core/src/notification/coalescer.rs (string-fixture rename, review finding), crates/core/tests/event_formatting_coverage.rs, crates/core/tests/plan_p4_p5_alert_proof.rs, crates/core/tests/ws_telegram_visibility_guard.rs
  - Tests: tickvault-core suite (deletion-only; 13 dead-variant test fns removed with baseline justification 6848 -> 6835)
  - Evidence: `cargo test -p tickvault-core --features daily_universe_fetcher` → lib `test result: ok. 2344 passed; 0 failed` + 61 green suite lines; fmt clean; CI-exact clippy → 2 pre-existing warnings only (constituent_resolver type-complexity + subscription-planner test dead_code, both untouched). 3-agent review: hot-path CLEAN, security 1 MEDIUM (PRE-EXISTING unescaped `reason` interpolation in surviving Telegram HTML arms — named follow-up, not introduced here), hostile no CRITICAL/HIGH (MEDIUM stale comment + LOW coalescer fixture both fixed in this diff).

**Named follow-up (next session / own plan):** reintroduce a module-level `html_escape()` applied to externally-sourced `reason` interpolations in surviving Telegram HTML arms (WebSocketDisconnected, OrderUpdateDisconnected, OrderRejected, RiskHalt, profile-check events) — pre-existing MEDIUM from the batch-3 security review.
