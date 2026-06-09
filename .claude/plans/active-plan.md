# Implementation Plan: Boot-symmetry — post-market cross-verify + EOD digest must run on BOTH boot paths

**Status:** VERIFIED
**Date:** 2026-06-09
**Approved by:** Parthiban ("go ahead with the plan" — 2026-06-09, after #1068 merged)
**Crate(s) touched:** `tickvault-app` (`crates/app/src/main.rs`) + new ratchet test in `tickvault-storage`/`tickvault-app` tests

## Context / the gap (verified in code)

`main.rs` has two mutually-exclusive boot paths:
- **Slow boot** (normal morning / off-hours): builds the universe via `cold_build_daily_universe()` which calls `prev_day_ohlcv_boot::stash_universe(...)` (line ~5366), then in its tail spawns the **end-of-day digest** (15:31:30 IST, lines ~3568-3633) and the **post-market 1-minute cross-verify** (15:31:00 IST, lines ~3635-3750).
- **Fast boot** (mid-session crash restart, 09:00-15:30 IST): loads instruments via `load_instruments()` → `fresh_universe: Option<Arc<DailyUniverse>>` (line ~1105), wires the pipeline, then awaits shutdown and `return Ok(())` BEFORE ever reaching the slow-boot tail.

**Result:** on a mid-session restart (today's exact case), the 15:31 cross-verify and 15:31:30 EOD digest are **silently skipped** — which is why the operator couldn't see the post-market cross-verification today. Both are future-of-day events the fast-booted process is alive for, so both SHOULD run.

Additionally, the cross-verify reads `prev_day_ohlcv_boot::stashed_universe()`, populated globally inside `cold_build_daily_universe`. The COLD fast-boot path also goes through `cold_build_daily_universe`, so it IS stashed there; only the INSTANT warm-snapshot fast-path builds no `DailyUniverse` object (it returns `None`), so cross-verify there self-skips. (`fresh_universe` from `load_instruments` is an `FnoUniverse`, a different type — it cannot be stashed as the `DailyUniverse` cross-verify needs.)

## Design (decisions resolved — Rule 15/17)

1. **Extract** the two inline spawn blocks into ONE module-scope helper
   `fn spawn_post_market_tasks(notifier: Arc<NotificationService>, health_status: SharedHealthStatus, trading_calendar: Arc<TradingCalendar>, token_handle: TokenHandle, config: &ApplicationConfig)`.
   The helper body is the EXISTING code moved verbatim (EOD digest spawn + cross-verify spawn that reads `stashed_universe()` internally). Single source of truth → no copy-paste drift.
2. **Slow boot:** replace the inline blocks (3568-3750) with one call to the helper.
3. **Fast boot:** call the helper before the fast-boot shutdown await. NO `fresh_universe` stash — `fresh_universe` is an `FnoUniverse`, not the `DailyUniverse` cross-verify needs. The `DailyUniverse` is stashed globally inside `cold_build_daily_universe` (which the cold fast-boot path runs), so cross-verify works on the cold fast-boot path; the warm-snapshot fast-path has no `DailyUniverse` and cross-verify self-skips there (honest limitation — warm-path cross-verify is a follow-up). The EOD digest has no universe dependency and runs on both paths unconditionally.
4. **Ratchet:** source-scan test asserting (i) `spawn_post_market_tasks(` is **called from ≥2 sites** in main.rs (both boot paths), and (ii) the `EndOfDayDigest` notify + `run_cross_verify_1m` call live ONLY inside the helper (not duplicated inline) — so a future edit can't wire a post-market task into only one path.

**Why a helper, not duplicate blocks:** duplication is the exact drift this fix removes; the repo's boot-symmetry rule (S6-G4) requires both boot paths wired from one source.

## Edge Cases

- **Fast boot warm-snapshot path (no DailyUniverse stashed):** cross-verify reads `stashed_universe() == None` → existing `if let Some(...)` skips cleanly (no panic). EOD digest still runs. Honest limitation, documented inline.
- **Mid-evening fast boot past 15:31:** both tasks already self-skip via their internal `now >= target` / `decide_cross_verify_start(SkipPastTrigger)` guards (audit Rule 3) — unchanged.
- **Non-trading day:** both tasks self-skip via `is_trading_day` checks — unchanged.
- **Double-spawn risk:** the two paths are mutually exclusive (`if fast { ...; return } else { ... }`), so the helper runs exactly once per process. The ratchet counts call SITES (source), not runtime invocations.

## Failure Modes

- Helper param-type mismatch → caught by `cargo build -p tickvault-app`.
- Fast boot lacks one of the params → verified present: `fast_notifier`, `health_status` (1327), `token_handle` (1028), `trading_calendar` (235, shared), `config` all in scope.
- Cross-verify finding no targets on the warm fast-boot path → expected + documented (the `DailyUniverse` only exists on the cold path, which stashes it in `cold_build_daily_universe`).

## Test Plan

- `crates/app/tests/boot_symmetry_post_market_guard.rs` (new):
  - `post_market_tasks_called_from_both_boot_paths` (≥2 call sites)
  - `eod_digest_and_cross_verify_live_only_in_the_helper` (no inline duplication)
- `cargo build -p tickvault-app` clean.
- `cargo test -p tickvault-app` scoped green.

## Rollback

Single commit; revert restores the slow-boot-only inline blocks. No schema, no data, no wire-protocol change — pure boot wiring of already-shipped tasks.

## Observability

No new metric/event — reuses the existing `EndOfDayDigest` notification + `CROSS-VERIFY-1M-01/02` codes + the `"PROOF: ... fired"` info logs. The only change is that fast boot now ALSO emits them. The boot-symmetry ratchet is the regression lock. 15+7 guarantee matrices cross-referenced from `.claude/rules/project/per-wave-guarantee-matrix.md`; applicable rows: Functionality coverage (both paths) + Scenario coverage (mid-session restart day) + Recovery validation.

## Plan Items

- [x] Extract `spawn_post_market_tasks` module-scope helper from the two inline slow-boot spawn blocks
  - Files: crates/app/src/main.rs
- [x] Slow boot: replace inline blocks with a call to the helper
  - Files: crates/app/src/main.rs
- [x] Fast boot: call the helper before shutdown await (universe stashed globally in cold_build; no fresh_universe stash — wrong type)
  - Files: crates/app/src/main.rs
- [x] Boot-symmetry ratchet test (both call sites + no inline duplication)
  - Files: crates/app/tests/boot_symmetry_post_market_guard.rs
  - Tests: post_market_tasks_called_from_both_boot_paths, eod_digest_and_cross_verify_live_only_in_the_helper

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Normal morning (slow) boot, trading day | EOD digest + cross-verify run (unchanged) |
| 2 | Mid-session crash restart (fast boot) — today's case | EOD digest + cross-verify NOW run (the fix) |
| 3 | Fast boot, no universe (Indices4Only) | cross-verify skips cleanly; EOD digest still runs |
| 4 | Future edit wires a post-market task into one path only | Ratchet test fails the build |
