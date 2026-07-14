# Phase 0 Item 20 Error Codes

> **Authority:** This file is the runbook target for the
> `OrphanPosition01Detected` ErrorCode variant added in Phase 0 Item 20
> (orphan position 15:25 IST watchdog). Cross-ref test
> `crates/common/tests/error_code_rule_file_crossref.rs` requires every
> variant in `ErrorCode` to be mentioned in at least one rule file
> under `.claude/rules/`. This file serves that contract.

## ORPHAN-POSITION-01 — open position observed at 15:25 IST

**Trigger:** the orphan-position watchdog (`crates/core/src/instrument/
orphan_position_watchdog.rs::run_orphan_position_watchdog`) fires at
15:25:00 IST every trading day. It calls Dhan REST `GET /v2/positions`,
inspects every row, and emits this code if ANY row has `net_qty != 0`.

**Why 15:25 IST and not 15:30:** the watchdog must complete the
DETECT → AUDIT → Telegram → (Phase 1+: cancel-and-exit) chain BEFORE
NSE closes at 15:30 IST. A 5-minute headroom covers rate-limit
backoff, REST retries, and operator reaction time. After 15:30 IST
exit orders are rejected by the exchange and the position carries
overnight — which is exactly what this watchdog exists to prevent.

**Severity:** Critical. The strategy is intraday option-buying;
overnight derivative positions expose the account to gap risk +
margin calls. An operator MUST act before the close.

**Behaviour by phase:**

| Phase | `dry_run` | DETECT → AUDIT → Telegram | Auto-exit |
|---|---|---|---|
| Phase 0 (current) | `true` | YES | NO — outcome row carries `dry_run=true` |
| Phase 1+ | `false` | YES | YES — cancel Super Order legs + market exit; row outcome upgrades to `AutoClosed` (success) or `ExitFailed` |

**Triage:**

1. Telegram payload names every orphan symbol + net_qty + segment.
   Operator confirms each in the Dhan web UI.
2. `mcp__tickvault-logs__questdb_sql "select * from orphan_position_audit
   where trading_date_ist = today() and outcome != 'no_orphans'"` returns
   the full forensic snapshot.
3. **Phase 0 (dry-run):** operator manually exits each position via
   Dhan web UI before 15:30 IST.
4. **Phase 1+ (live):** the watchdog already attempted exit; check
   `outcome = 'exit_failed'` rows for failures, then manually finish.

**Auto-triage safe:** NO (Severity::Critical; operator MUST verify
each exit landed before NSE close).

**Source:**
- `crates/common/src/error_code.rs::ErrorCode::OrphanPosition01Detected`
- `crates/core/src/instrument/orphan_position_watchdog.rs::run_orphan_position_watchdog`
- `crates/storage/src/orphan_position_audit_persistence.rs`
- `crates/core/src/notification/events.rs::NotificationEvent::OrphanPositionDetected`
- Boot wiring: `crates/app/src/main.rs` (15:25 IST scheduler spawn).

**Ratchet tests:**
- `orphan_position_audit_persistence::tests::*` — DDL + DEDUP + outcome enum
- `orphan_position_watchdog::tests::*` — pure-function evaluator + clock helpers
- Source-scan guard `secret_manager.rs::test_orphan_position_watchdog_is_wired_into_main`
  pins boot spawn site so future refactors can't silently remove the watchdog.

## 2026-07-14 Update — spawn re-homed process-global + stale-source corrections

The sections above are retained as history; the CURRENT state:

- **Source moved:** the pure logic lives in
  `crates/trading/src/orphan_position_watchdog.rs`
  (`evaluate_orphan_positions` + clock helpers, no I/O); the supervised async
  runner lives in `crates/app/src/orphan_position_watchdog_boot.rs` (NOT
  `crates/core/src/instrument/orphan_position_watchdog.rs` as the header
  above says).
- **`orphan_position_audit` table + persistence were DELETED 2026-05-20**
  (the QuestDB table cleanup — see the removed-guard marker in
  `crates/core/src/auth/secret_manager.rs`). The `questdb_sql ... from
  orphan_position_audit` triage step above is HISTORICAL — the table does
  not exist. Current signals: the Telegram events
  (`OrphanPositionsClean` / `OrphanPositionDetected` / the degraded
  Custom-Critical arms), `error!(code = ORPHAN-POSITION-01)` on detection,
  and the counters `tv_orphan_position_watchdog_runs_total` /
  `tv_orphan_position_watchdog_fetch_failures_total` /
  `tv_orphan_position_watchdog_respawns_total`.
- **Spawn re-homed (2026-07-14):** the spawn previously lived ONLY inside the
  Dhan-gated `spawn_post_market_tasks` family in `crates/app/src/main.rs` —
  dead on `dhan_enabled = false` boots (the production default since the
  2026-07-13 Dhan live-WS retirement), so the daily 15:25 IST broker-position
  cross-check never ran. The PRIMARY spawn now lives in the main.rs
  PROCESS-GLOBAL prefix (next to the tick-conservation hoist) using
  `WatchdogAuth::GlobalAtFireTime` — at each 15:25 fire it resolves the
  global `TokenManager` (registered by the Dhan lane or the Dhan REST
  stack's Phase 2); no manager = LOUD degraded Critical page, never a clean
  signal. The fast crash-recovery arm keeps its `WatchdogAuth::Static` spawn
  via `spawn_post_market_tasks`; a module-level once-guard
  (`ORPHAN_WATCHDOG_CLAIMED`) makes a duplicate spawn a no-op.
- **Live wiring ratchet:** `crates/app/tests/orphan_position_watchdog_wiring_guard.rs`
  (the `secret_manager.rs::test_orphan_position_watchdog_is_wired_into_main`
  guard named above was removed 2026-05-20 with the audit table) — pins the
  process-global-prefix-first topology, the exactly-2 spawn count, the
  fire-time `global_token_manager()` resolution, and the degraded-arm
  wording (stub-guard).
