# Implementation Plan: Retire 09:13 IST anchor task under v2

**Status:** VERIFIED
**Date:** 2026-05-10
**Approved by:** Parthiban — `AskUserQuestion` 2026-05-10 01:19 IST: "Retire 09:13 anchor task under v2"
**Branch:** `claude/retire-09-13-anchor-under-v2-Q9k4M`
**Triggering context:** PR #544 ("F+") body listed this as out-of-scope cleanup — the 09:13 anchor task in `crates/app/src/main.rs` (lines 6161-6496 pre-edit) still calls `select_depth_instruments` and emits `MarketOpenDepthAnchor` Telegram every trading day, but under `depth_dynamic_pipeline_v2 = true` (the live default since 2026-05-09 per `config/base.toml:305`) the per-underlying dispatch is already short-circuited (`anchor_single_side_enabled = false`). The task computes ATM, dispatches to an empty `HashMap<String, _>` of senders (v2 uses `HashMap<u8, _>`), and drops the result. Skipping the spawn entirely retires the last call site of `select_depth_instruments` from the live boot path.

## Plan Items

- [x] **Wrap the 09:13 anchor task in `if !config.features.depth_dynamic_pipeline_v2 { ... } else { info! }`**
  - Files: `crates/app/src/main.rs` (anchor scope around line 6161)
  - Behaviour: under v2 (live default), the anchor task is no longer spawned. A single `info!` notes the skip + reason at boot. Under legacy (`v2 = false`), behaviour unchanged.
  - Verification: existing `tickvault-app` test suite stays green (564 lib + integration binaries). No new ratchet — same pattern as PR #544 F+.

## Scenarios

| # | When | Before | After |
|---|---|---|---|
| 1 | v2 default (live), 09:13 IST | Anchor task spawns → calls `select_depth_instruments` → dispatches to empty senders map → no-op + Telegram noise | Task not spawned; single boot `info!` line |
| 2 | v2 off (legacy mode) | Anchor task spawns + dispatches to per-underlying senders (live behaviour) | Unchanged |
| 3 | v2 default + boot off-hours / weekend | Task spawns, sleeps until 09:13 IST, then runs | Task not spawned at all |

## Honest 100% claim qualifier (per `wave-4-shared-preamble.md` §8)

100% inside the tested envelope, with ratcheted regression coverage: under v2 the anchor scope is structurally bypassed; under legacy mode behaviour is unchanged. The `MarketOpenDepthAnchor` Telegram is replaced under v2 by the existing `PR-C2 cutover: depth_dynamic_pipeline_v2 spawn complete` boot log + the unified diff dispatcher's per-cycle `info!`. Beyond the envelope: if v2 is ever toggled OFF mid-deploy, the legacy anchor will resume firing — that's the desired behaviour (v2 toggle is a rollback path).

## Verification

- [x] `cargo check -p tickvault-app` — green
- [x] `cargo test -p tickvault-app --lib` — 564 passed
- [x] `cargo test -p tickvault-app --tests` — all binaries green
- [x] `bash .claude/hooks/plan-verify.sh`
