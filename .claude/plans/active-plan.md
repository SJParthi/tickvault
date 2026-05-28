# Implementation Plan: Daily-Universe Go-Live — PR-2 (scope-aware 250-SID subscription)

**Status:** APPROVED
**Date:** 2026-05-28
**Approved by:** Parthiban (full go-live everywhere, fail-closed §4; "go ahead with the plan" after PR-1 #852 merged)
**Branch:** `claude/daily-universe-scope-aware-subscription`

## Go-live sequence

| PR | What | Prod effect |
|----|------|-------------|
| PR-1 #852 (merged) | Boot Step-6c wiring, fail-closed §4 | none (flag OFF) |
| **PR-2 (this)** | Scope-aware planner -> WS subscribes the ~250 SIDs when scope=DailyUniverse | none yet (flag OFF) |
| PR-3 | Flip prod `default` flag ON + `scope = "daily_universe"` — GO-LIVE | 250 SIDs everywhere |

## Design (from deep investigation)

**Structural decision:** consolidate the daily-universe fetch into `load_instruments`'
scope branch so the built `DailyUniverse` flows straight into the subscription
plan (instead of threading an `Arc` across ~2,500 boot lines). The standalone
Step-6c block from PR-1 moves into the `DailyUniverse` arm of `load_instruments`
(same tokens preserved so the wiring ratchet still passes).

| Seam | File | Change |
|---|---|---|
| `DailyUniverseBootOutcome` | daily_universe_boot.rs | `run_daily_universe_boot` returns `(outcome, Arc<DailyUniverse>)` (tuple — no new struct field) |
| New planner fn | subscription_planner.rs | `build_subscription_plan_from_daily_universe(&DailyUniverse, &SubscriptionConfig, today) -> SubscriptionPlan` — `subscription_targets` -> `InstrumentRegistry` (Quote mode §8), I-P1-11 `(sid, segment)` dedup |
| `load_instruments` | main.rs | `match config.subscription.scope`: Indices4Only -> existing; DailyUniverse (feature-gated) -> fetch+build plan; not(feature) -> bail |
| Remove standalone Step-6c | main.rs | moved into the load_instruments DailyUniverse arm |
| pool size | config.rs `effective_main_feed_pool_size` | stays 1 (250 SIDs fit on 1 conn, Dhan cap 5000) |

## Plan Items

- [ ] `run_daily_universe_boot` returns `(DailyUniverseBootOutcome, Arc<DailyUniverse>)`
  - Files: crates/app/src/daily_universe_boot.rs
  - Tests: existing Err-path tests unaffected
- [ ] `build_subscription_plan_from_daily_universe`
  - Files: crates/core/src/instrument/subscription_planner.rs
  - Tests: test_daily_universe_plan_emits_all_sids, test_daily_universe_plan_dedup_composite_key, test_daily_universe_plan_quote_mode
- [ ] scope-aware `load_instruments` + relocate Step-6c into the DailyUniverse arm (feature-gated, fail-closed §4)
  - Files: crates/app/src/main.rs
- [ ] Guard updates
  - Files: crates/core/tests/indices4only_scope_lock_guard.rs, crates/app/tests/daily_universe_boot_wiring_guard.rs
  - Tests: test_subscription_scope_has_two_variants

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | feature OFF (default) | unchanged — 4-IDX_I, no DailyUniverse path compiled |
| 2 | feature ON, scope=Indices4Only | unchanged 4-IDX_I plan |
| 3 | feature ON, scope=DailyUniverse, CSV ok | ~250-SID plan, 3 WS batches (100/100/50), Quote mode, lifecycle tables populated |
| 4 | feature ON, scope=DailyUniverse, CSV/QuestDB down | fail-closed §4 — boot blocks/halts |
