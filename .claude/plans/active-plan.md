# Implementation Plan: Daily-Universe GO-LIVE (250-SID DailyUniverse, prod + local)

**Status:** VERIFIED
**Date:** 2026-05-28
**Approved by:** Parthiban — "entire 250 SIDs + new scope as production AND local, everywhere"; boot behavior = "Block until CSV (§4 lock)" (AskUserQuestion 2026-05-28)
**Branch (PR-1):** `claude/daily-universe-boot-wiring`

## End state

The ~250-SID `DailyUniverse` scope is ON in **production AND local** — the
WebSocket actually streams the ~250 SIDs, and every boot downloads the Dhan
Detailed CSV, builds the universe, and reconciles the
instrument_lifecycle / *_audit / instrument_fetch_audit tables. **Fail-closed
per §4:** prod+local boot BLOCKS (infinite retry, escalating Telegram) until a
fresh validated CSV is in hand — never trades on stale/partial data.

## Serial PR sequence (one at a time to merge, per pr-completion-protocol)

| PR | Scope | Prod effect on merge |
|---|---|---|
| **PR-1 (this)** | Boot Step-6c wiring, fail-closed §4, feature-gated | none yet (flag still default-OFF) |
| **PR-2** | Scope-aware subscription planner consumes DailyUniverse → 250-SID WS plan; thread universe from Step-6c; update scope-lock guard tests | none yet |
| **PR-3** | Flip prod `default = ["daily_universe_fetcher"]` + `config/base.toml [subscription] scope = "daily_universe"` | **GO-LIVE — 250 SIDs everywhere** |

## PR-1 Plan Items

- [x] Add feature-gated, fail-closed Step 6c block to the boot sequence
  - Files: crates/app/src/main.rs
  - Tokens: ensure_instrument_lifecycle_table, ensure_instrument_lifecycle_audit_table,
    ensure_instrument_fetch_audit_table, CsvDownloader, run_daily_universe_boot
  - Behaviour: ensure 3 tables → Arc<CsvDownloader> fetch_fn → run_daily_universe_boot
    (dry_run=false, max_attempts=None infinite §4) → `?`-bail on Err (fail-closed) → log outcome.
  - IST nanos per the lifecycle persistence convention (IST wall-clock nanos).

- [x] Wiring ratchet test (source-scan, matches the `*_is_wired` guard pattern)
  - Files: crates/app/tests/daily_universe_boot_wiring_guard.rs
  - Tests: test_daily_universe_boot_is_wired_into_main,
    test_step_6c_ensures_three_lifecycle_tables,
    test_step_6c_is_feature_gated_and_fail_closed

## Scenarios (PR-1)

| # | Scenario | Expected |
|---|----------|----------|
| 1 | `cargo run` (feature OFF — current default) | identical to today — 4-IDX_I LOCKED_UNIVERSE, no sha2, no Step 6c |
| 2 | `cargo run --features daily_universe_fetcher`, Dhan CSV reachable | CSV downloaded, ~250-SID universe built, 3 tables created + populated, INFO summary |
| 3 | feature ON, Dhan CSV unreachable | infinite retry, escalating Telegram (4/11/21); boot BLOCKS (§4) — never proceeds |
| 4 | feature ON, QuestDB unreachable mid-reconcile | run_daily_universe_boot returns Err → boot HALTS (fail-closed) |

