# Implementation Plan: Wire the 15:25 IST orphan-position watchdog (Phase 0 Item 20)

> **ARCHIVED 2026-06-14** — work completed and merged to `main` as **#1123**
> (`feat(trading): wire the 15:25 IST orphan-position watchdog (Phase 0 Item 20)`).
> Status flipped APPROVED → VERIFIED; the three plan items below are ticked to
> reflect the merged slice. The audit-table follow-up remains deferred per the
> operator scope decision recorded at approval time.

**Status:** VERIFIED (merged as #1123, 2026-06-13)
**Date:** 2026-06-13
**Approved by:** Parthiban, 2026-06-13 (AskUserQuestion): direction = "Wire it + add the real guard"; scope = "Runner + spawn + guard only" (audit table deferred); "Finish the watchdog + merge it".

## Guarantee matrices

See `.claude/rules/project/per-wave-guarantee-matrix.md`. All 15 rows of the
100% guarantee matrix and all 7 rows of the resilience matrix apply to every
item here. Applicable proof: testing (pure-helper unit tests + the trading
evaluator/clock tests + the wiring guard), code checks (banned-pattern +
pub-fn guards + plan-verify), monitoring (3 counters), logging (`error!` with
`code = ORPHAN-POSITION-01`), alerting (`OrphanPositionDetected` Critical +
degraded Custom pages), security (3-agent review; JWT exposed once never
logged; REST errors redacted via `capture_rest_error_body`), extreme check
(source-scan wiring ratchet fails the build if the spawn is removed). Any
"100%" wording means **100% inside the tested envelope, with ratcheted
regression coverage** — the runner is alert-only (dry-run/sandbox), the
fetch/token failure paths fail-LOUD (never false-OK), and the supervised
respawn bounds the blast radius of a transient fault.

## Design

The pure evaluator (`evaluate_orphan_positions`) + clock helpers
(`sleep_duration_until_orphan_watchdog`, etc.) already exist in
`tickvault_trading::orphan_position_watchdog`. This slice adds the missing
async **runner** in `crates/app/src/orphan_position_watchdog_boot.rs` (composition
root, mirrors `cross_verify_1m_boot.rs`) and spawns it from
`spawn_post_market_tasks` in `main.rs` (called from BOTH boot paths). Each day at
15:25 IST it reads the JWT, calls `OrderApiClient::get_positions`, evaluates, and
on open positions pages CRITICAL `OrphanPositionDetected` (dry-run alert-only),
else INFO `OrphanPositionsClean`. Counters: `tv_orphan_position_watchdog_runs_total`,
`_fetch_failures_total`, `_respawns_total`. A `test_orphan_position_watchdog_is_wired_into_main`
source-scan guard (the one the rule file always claimed) is added.

## Edge Cases

- Busy-loop at exactly 15:25:00 (clock helper returns 0) → 60s floor-sleep after
  each run/skip before recomputing the boundary.
- Non-trading day / weekend / holiday → `TradingCalendar::is_trading_day` gate
  skips the fetch+page, still floor-sleeps to next day.
- Token absent (`TokenHandle::load()` None) → degraded CRITICAL Custom page, NOT
  a "clean" ping (false-OK avoidance).
- Mid-session fast-boot restart → spawned from `spawn_post_market_tasks`, re-armed
  on both boot paths.

## Failure Modes

- `get_positions` transient error → ONE retry, then CRITICAL degraded Custom page
  + `_fetch_failures_total` — NEVER reported as flat (audit Rule 11).
- Task panic / unexpected exit → supervisor respawns after 30s backoff +
  `_respawns_total` (mirrors WS-GAP-05 / DISK-WATCHER-01); loop body has no
  `unwrap`/`expect`/`?`-out-of-task.
- REST error body could carry account details → routed through
  `capture_rest_error_body` before any log/Telegram (security review HIGH).

## Test Plan

- `cargo test -p tickvault-app -p tickvault-trading` — pure-helper tests
  (`ist_date_from_utc` × 3 + floor-sleep invariant) + the 3 wiring-guard tests +
  the existing 18 trading evaluator/clock tests.
- `cargo check --workspace --all-targets` exit 0.
- Pre-push gates (banned-pattern incl. literal `Duration::from_secs` → named
  consts; pub-fn-test/wiring; plan-verify; guarantee-matrix).

## Rollback

Single revert: the slice is additive (new file + one fn param + spawn + guard
test). `git revert <sha>` restores verbatim. Alert-only, dry-run — no order
ever placed; reverting only removes the daily check.

## Observability

3 counters above + `error!(code = ORPHAN-POSITION-01)` (already 7-layer wired in
`ErrorCode` + `OrphanPositionDetected`/`OrphanPositionsClean` events). Degraded
paths page via `NotificationEvent::Custom`. No new QuestDB table (audit table is
the deferred follow-up per operator scope decision).

## Plan Items

- [x] Add async runner + supervisor in `orphan_position_watchdog_boot.rs`
  - Files: crates/app/src/orphan_position_watchdog_boot.rs, crates/app/src/lib.rs
  - Tests: test_ist_date_from_utc_*, test_floor_sleep_exceeds_boundary_zero_window
- [x] Wire spawn into `spawn_post_market_tasks` (both boot paths)
  - Files: crates/app/src/main.rs
- [x] Add the source-scan wiring guard
  - Files: crates/app/tests/orphan_position_watchdog_wiring_guard.rs
  - Tests: test_orphan_position_watchdog_is_wired_into_main, test_orphan_position_watchdog_spawned_from_post_market_tasks, test_post_market_tasks_called_from_both_boot_paths
