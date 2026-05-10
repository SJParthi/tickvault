# Implementation Plan: Delete legacy v2-off depth path + retire `select_depth_instruments`

**Status:** DRAFT — awaiting approval
**Date:** 2026-05-10
**Approved by:** _pending_
**Branch:** `claude/<TBD>` (will branch off main after approval)

## Context

After PR #542 + #544 + #545, the entire boot-time depth path is owned by `depth_dynamic_pipeline_v2` under the `[features].depth_dynamic_pipeline_v2 = true` flag (live default, `config/base.toml:305`). The legacy v2-off branches are:

| Site | Lines | What it does (under `v2 = false`) |
|---|---|---|
| `main.rs:3878-3877` (`else if let Some(ref _plan) = subscription_plan { ... }`) | ~700 lines | Phase 1 boot ATM-wait + per-conn `select_depth_instruments` + spawn_twenty_depth_connection / spawn_two_hundred_depth_connection loops |
| `main.rs:4969-5052` (`} else if config.features.depth_dynamic_pipeline_v2 { ... }`) | ~80 lines | Wave-5 single-side v2-off arm (already gated; redundant under v2) |
| `main.rs:6177-6515` (`if !config.features.depth_dynamic_pipeline_v2 { /* anchor task */ } else { info! }`) | ~340 lines | 09:13 IST anchor task (last live caller of `select_depth_instruments`) |

Plus:
- `crates/core/src/instrument/depth_strike_selector.rs::select_depth_instruments` + sibling helpers + their unit tests
- `crates/core/benches/phase2_dispatch.rs` (uses `select_depth_instruments` and `DEPTH_ATM_STRIKES_EACH_SIDE`)
- `crates/common/src/config.rs::FeaturesConfig::depth_dynamic_pipeline_v2` field + default + test
- `crates/app/tests/feature_flag_rollback_guard.rs::EXPECTED_FLAGS` (decrement 16 → 15 + remove the entry)
- `config/base.toml:305` (the flag value)
- Doc comments in `crates/storage/src/depth_dynamic_diff_audit_persistence.rs`, `crates/app/src/depth_dynamic_pipeline_v2.rs`, etc.

**Scope total:** ~1,200 lines of deletion across 7+ files.

## Risk Analysis

| Risk | Mitigation |
|---|---|
| **Rollback path lost** — once flag is deleted, there's no `v2 = false` toggle if v2 has a latent issue in production. | Stage as 3 sub-PRs (below). Each is independently revertable; only the final sub-PR removes the flag. |
| **Legacy paths have bug fixes not replicated in v2** (e.g., audit-findings Rule 3 / 5 / 6 added to legacy main.rs ATM block; may not all be in v2 pipeline) | Diff-audit each `else` arm BEFORE deletion: confirm every `if is_market_hours`, `tracing::error!`, `is_within_persist_window` site has a v2 equivalent. Document in plan body. |
| **Meta-test `feature_flag_rollback_guard.rs` enforces flag presence** | Sub-PR 3 explicitly updates `EXPECTED_FLAGS` array (16 → 15 entries) + `array_size: [&str; 16]` → `[&str; 15]` annotation. |
| **Benchmark deletion** — `phase2_dispatch.rs` has Criterion budget entries in `quality/benchmark-budgets.toml`? | Audit `quality/benchmark-budgets.toml` before sub-PR 2 deletes the bench; remove matching entry. |
| **Tests for `select_depth_instruments`** — function has unit tests in `depth_strike_selector.rs::tests`; integration tests in `crates/core/tests/`? | Sub-PR 2 deletes function + ALL associated tests. Test count baseline (`.test-count-baseline`) will need bump-down. |

## Proposed staging — 3 sub-PRs

### Sub-PR 1: Delete the legacy `else` branches in main.rs (~1,000 lines)

- **Files:** `crates/app/src/main.rs` only
- **Changes:**
  - Replace the `if config.features.depth_dynamic_pipeline_v2 { info! } else if let Some(ref _plan) = subscription_plan { /* legacy 700 lines */ }` (line 3846 area) with just the v2 spawn body (already exists upstream at lines 3081-3264 — main.rs:3846-4968 collapses to a single `info!`).
  - Replace the `} else if config.features.depth_dynamic_pipeline_v2 { /* v2 no-op */ }` (line 4969 area) — the entire `else if` arm is dead code under v2 since v2 owns spawn upstream.
  - Replace the `if !config.features.depth_dynamic_pipeline_v2 { /* 09:13 anchor task spawn */ } else { info! }` (line 6177 area) with just the `info!` (PR #545's else branch).
- **Behaviour preserved under flag = true:** zero functional change (those branches were never taken).
- **Behaviour change under flag = false:** legacy paths gone — flag still exists but toggle off becomes a no-op spawn. Sub-PR 1 deliberately keeps the flag for now.
- **Tests:** `cargo test -p tickvault-app` (565+ tests), no test deletions in this sub-PR.
- **Diff-audit checklist** (one row per legacy ratchet — must verify v2 has equivalent):
  - [ ] Audit-findings Rule 3 (market-hours gate) — verify `depth_dynamic_pipeline_v2.rs` has equivalent
  - [ ] Audit-findings Rule 5 (flush failures = ERROR) — verify
  - [ ] Audit-findings Rule 6 (wall-clock + exchange-timestamp guards) — verify
  - [ ] DEPTH-STALE-ALERT (edge-trigger suppression) — verify
  - [ ] WS-GAP-04 (post-close sleep) — verify
  - [ ] BOOT-01/02 (QuestDB readiness) — verify
- **Honest 100% qualifier:** 100% inside the tested envelope under `v2 = true` (the live default). Under `v2 = false` after sub-PR 1, the flag is a no-op — operator should NOT toggle off until sub-PR 3 removes the flag entirely.

### Sub-PR 2: Delete `select_depth_instruments` + its tests + bench

- **Files:**
  - `crates/core/src/instrument/depth_strike_selector.rs` — delete `select_depth_instruments` + tests + sibling helpers that have no other callers
  - `crates/core/benches/phase2_dispatch.rs` — delete (or remove the `select_depth_instruments` calls and any test helpers that solely existed for it)
  - `quality/benchmark-budgets.toml` — remove matching budget entry (if present)
  - `.claude/hooks/.test-count-baseline` — decrement
- **Pre-flight check:** confirm no callers remain in main.rs after sub-PR 1 lands.
- **Tests:** verify `cargo test --workspace` stays green; no caller breakage.

### Sub-PR 3: Delete the `depth_dynamic_pipeline_v2` flag entirely

- **Files:**
  - `crates/common/src/config.rs` — delete `depth_dynamic_pipeline_v2: bool` field + default + test
  - `crates/app/tests/feature_flag_rollback_guard.rs` — `EXPECTED_FLAGS: [&str; 16]` → `[&str; 15]` + remove `"depth_dynamic_pipeline_v2"` entry + update doc comment
  - `config/base.toml:305` — delete the flag line
  - `crates/app/src/main.rs` — strip remaining `config.features.depth_dynamic_pipeline_v2` references in surrounding comments + `pipeline_v2_active` binding (line 3016) + `rebal_pipeline_v2_active` (line 6530)
  - `crates/storage/src/depth_dynamic_diff_audit_persistence.rs` — strip flag mention from doc comment
- **Tests:** `cargo test --workspace` stays green.
- **Operator gate:** explicit operator approval required BEFORE sub-PR 3 lands — this is the point of no rollback.

## Plan Items (sub-PR 1 first; sub-PR 2 + 3 spawned only after sub-PR 1 merges + a live in-session boot confirms v2 still works)

- [ ] **Sub-PR 1.0** — branch + DRAFT plan + present for approval (you are here)
- [ ] **Sub-PR 1.1** — diff-audit the 3 legacy branches against v2 pipeline (ratchet checklist above)
- [ ] **Sub-PR 1.2** — implement deletion in main.rs
- [ ] **Sub-PR 1.3** — `cargo test -p tickvault-app` green; commit + push + draft PR
- [ ] **Sub-PR 1.4** — operator merges, lives the build for one in-session boot
- [ ] **Sub-PR 2.0** — fresh branch off main; pre-flight check that no `select_depth_instruments` callers remain in main.rs after sub-PR 1
- [ ] **Sub-PR 2.x** — delete fn + tests + bench + budget entry; commit + push + draft PR
- [ ] **Sub-PR 3.0** — fresh branch; explicit operator approval
- [ ] **Sub-PR 3.x** — delete flag + update rollback guard; commit + push + draft PR; operator merges

## Verification

- `cargo test -p tickvault-app --lib` per sub-PR (564+ tests)
- `cargo test -p tickvault-app --tests` per sub-PR
- `cargo test --workspace` deferred to CI for each sub-PR
- `bash .claude/hooks/plan-verify.sh` per sub-PR

## Honest 100% claim qualifier (per `wave-4-shared-preamble.md` §8)

100% inside the tested envelope, with ratcheted regression coverage:
- Sub-PR 1: under `v2 = true` (live default), behaviour unchanged. Under `v2 = false` (post-sub-PR-1), flag becomes a documented no-op pending sub-PR 3 removal.
- Sub-PR 2: `select_depth_instruments` has zero callers after sub-PR 1; deletion is mechanical.
- Sub-PR 3: rollback path removed. Operator has explicitly approved this loss in exchange for cleaner code.

Beyond the envelope: if v2 develops a latent issue post-sub-PR 3, the only rollback is `git revert` of sub-PR 3 (which restores the flag) — the legacy code itself remains gone (sub-PR 1) and `select_depth_instruments` is gone (sub-PR 2). Restoring full legacy capability requires reverting all 3 sub-PRs.

## Open question for operator

**Q1:** Approve the 3-sub-PR staging? Or do you want it as one big PR?

**Q2:** Sub-PR 1 will run a thorough diff-audit (the ratchet checklist above) before deletion — this can take 20-30 min of review. OK?

**Q3:** Sub-PR 3 is the point of no return. After sub-PR 1 + 2 land, do you want a deliberate "wait for N in-session boots successful" gate before sub-PR 3, or proceed immediately?
