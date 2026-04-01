# Implementation Plan: Comprehensive Hardening V2 — Zero Loss, Full Automation, Sandbox Enforcement

**Status:** IN_PROGRESS
**Date:** 2026-04-01
**Approved by:** Parthiban (2026-04-01)

---

## Source: 15-Agent Deep Scan V2 (2026-04-01)

| Agent | Scope | Key Findings |
|-------|-------|-------------|
| Tick Loss Auditor | QuestDB resilience, ring buffer, spill | 9 gaps (1 CRITICAL, 2 HIGH, 2 MEDIUM, 2 LOW, 2 ACCEPTED) |
| Automation Auditor | Monitoring, alerting, self-healing | 8 silent failure scenarios |
| Python SDK Verifier | Byte offsets, struct formats, enums | 112/115 perfect match (3 cosmetic, 1 informational) |
| Dhan Docs Verifier | Live docs vs reference docs (WebSocket) | Pending (403 on some pages, agents running) |
| Dhan REST Verifier | REST API docs vs reference docs | Pending |
| Dhan Orders Verifier | Orders/portfolio/super-order docs | Pending |
| Test Coverage Auditor | 22 test types × 6 crates | Pending (cargo test running) |
| Prior Plan (V1) | 25 items, 15 completed | 10 remaining (2 deferred, 8 actionable) |

---

## BLOCK A: CRITICAL — Zero Tick Loss (Fast Boot Gap)

- [x] A1: Fix `run_tick_persistence_consumer` to use `new_disconnected` when QuestDB unavailable
  - Files: `crates/app/src/main.rs`
  - Change: At line ~1753, replace `return;` on writer failure with `TickPersistenceWriter::new_disconnected(&questdb_config)`. Consumer keeps running, buffering all ticks until QuestDB comes back.
  - Tests: `test_fast_boot_tick_consumer_buffers_when_questdb_down`, `test_fast_boot_zero_tick_loss`

---

## BLOCK B: HIGH — Depth Persistence Resilience

- [x] B1: Add `DepthPersistenceWriter::new_disconnected()` method
  - Files: `crates/storage/src/tick_persistence.rs`
  - Change: Mirror `TickPersistenceWriter::new_disconnected()` pattern for depth writer
  - Tests: `test_depth_writer_new_disconnected_buffers`

- [x] B2: Use `new_disconnected` fallback when depth writer fails at startup (slow boot)
  - Files: `crates/app/src/main.rs`
  - Change: At line ~925-937, on `DepthPersistenceWriter::new()` failure, create `new_disconnected()` instead of `None`
  - Tests: `test_depth_writer_fallback_on_startup_failure`

- [x] B3: Add Telegram alert when depth writer fails at startup
  - Files: `crates/app/src/main.rs`, `crates/core/src/notification/events.rs`
  - Change: Add `NotificationEvent::DepthWriterUnavailable` and fire it when depth writer fails
  - Tests: `test_depth_failure_telegram_alert`

---

## BLOCK C: MEDIUM — Monitoring & Alerting Gaps

- [x] C1: Add programmatic Telegram alert for mid-session QuestDB disconnect
  - Files: `crates/core/src/notification/events.rs`, `crates/core/src/pipeline/tick_processor.rs`
  - Change: Add `NotificationEvent::QuestDbDisconnected` + `QuestDbReconnected`. Track `is_connected()` state transitions in tick processor.
  - Tests: `test_questdb_disconnect_fires_telegram`, `test_questdb_reconnect_fires_telegram`

- [x] C2: Add broadcast lag alert (fast boot: ticks permanently lost on Lagged error)
  - Files: `crates/app/src/main.rs`
  - Change: On `RecvError::Lagged(N)` at line ~1794, fire CRITICAL Telegram alert with count. Add `dlt_ticks_permanently_lost` counter.
  - Tests: `test_broadcast_lag_fires_critical_alert`

- [x] C3: Add periodic health check timer for disk space + memory RSS
  - Files: `crates/app/src/main.rs`
  - Change: Spawn background task that calls `check_disk_space()` + `check_memory_rss()` every 5 minutes, fires Telegram on thresholds
  - Tests: `test_periodic_health_check_runs`

---

## BLOCK D: Sandbox Enforcement (Until June 30)

- [x] D1: Add compile-time sandbox guard that prevents `mode = "live"` before July 2026
  - Files: `crates/trading/src/oms/engine.rs`, `crates/common/src/config.rs`
  - Change: Add `LIVE_TRADING_EARLIEST_DATE = "2026-07-01"` constant. In OMS constructor, if `mode == "live"` and `Utc::now() < date`, panic with clear message. This is fail-fast at boot, not hidden.
  - Tests: `test_sandbox_guard_blocks_live_before_july`, `test_sandbox_guard_allows_live_after_july`

- [x] D2: Add config validation test that `dry_run = true` in base.toml
  - Files: `crates/common/tests/config_safety.rs`
  - Tests: `test_base_config_dry_run_is_true`, `test_base_config_mode_is_sandbox`

---

## BLOCK E: Remaining V1 Plan Items (Carried Forward)

- [ ] E1: Add ring buffer + disk spill to LiveCandleWriter (V1 item D1)
  - Files: `crates/storage/src/candle_persistence.rs`
  - Change: Already has ring buffer (100K) + disk spill. VERIFY it works end-to-end.
  - Tests: `test_live_candle_buffered_on_questdb_failure`, `test_live_candle_drained_on_recovery`

- [ ] E2: Add tick persistence health to `/health` endpoint (V1 item D2)
  - Files: `crates/api/src/handlers/health.rs`, `crates/api/src/state.rs`
  - Change: Report tick_writer status (connected/buffering/disconnected), buffer size, spill count
  - Tests: `test_health_reports_tick_persistence_status`

- [ ] E3: Expand DHAT allocation tests to trading crate (V1 item E1)
  - Files: `crates/trading/tests/dhat_oms_hot_path.rs`
  - Tests: `test_oms_state_transition_zero_alloc`, `test_risk_check_zero_alloc`

- [x] E4: Add adversarial tick resilience tests (V1 item E2)
  - Files: `crates/storage/tests/tick_resilience.rs`
  - Tests: `test_zero_tick_loss_questdb_down_from_start`, `test_spill_file_recovery_after_restart`, `test_buffer_capacity_at_300k_limit`, `test_disk_spill_activates_when_buffer_full`

- [ ] E5: Add security audit tests for token handling (V1 item E3)
  - Files: `crates/core/tests/security_audit.rs`
  - Tests: `test_token_never_in_log_output`, `test_token_never_in_error_display`, `test_token_never_in_debug_format`

- [ ] E6: Wire 22 test type check into pre-push gate (V1 item E4)
  - Files: `.claude/hooks/pre-push-gate.sh`, `scripts/test-coverage-guard.sh`
  - Tests: Manual push verification

---

## BLOCK F: Documentation Updates

- [x] F1: Add `/charts/rollingoption` endpoint to historical data reference (V1 item F2) — already documented in ref doc line 232
  - Files: `docs/dhan-ref/05-historical-data.md`

- [ ] F2: Add Dhan API coverage test verifying all 54 endpoint constants (V1 item F6)
  - NOTE from orders agent: Order/trade book response structs need additional Optional fields
    (tradingSymbol, legName, drvExpiryDate, drvOptionType, etc.) before BO/CO trading.
    Super order needs validation rules (target > price for BUY). Low priority until Phase 2.
  - Files: `crates/common/tests/dhan_api_coverage.rs`
  - Tests: `test_all_54_dhan_rest_endpoints_have_constants`, `test_all_4_websocket_urls_defined`

- [ ] F3: Document 200-depth URL discrepancy (Python SDK vs docs)
  - Files: `docs/dhan-ref/04-full-market-depth-websocket.md`
  - Change: Add note about Python SDK using bare `wss://full-depth-api.dhan.co/` vs our `/twohundreddepth` path
  - Tests: N/A (documentation only)

---

## NOT IN SCOPE (Assessed and Excluded)

| Excluded | Reason |
|----------|--------|
| Playwright/browser testing | Rust backend system. No browser UI to test. 22-type test infra already more comprehensive. Not needed. |
| AI testing plugins | Marketing hype. Our mechanical enforcement (34 hook scripts) + 7,250 tests + property testing + fuzz testing is more rigorous. |
| C1/C4 from V1 (vtable, RwLock) | DEFERRED — architectural constraints. Low impact for single-user system. |
| Greeks persistence buffering | By design: recomputed every second, ephemeral. Code documents this intentionally. |
| Movers persistence buffering | By design: ephemeral analytics. |
| `std::thread::sleep` in reconnect | Cold path only, bounded to 7s max. Low impact. |
| Historical `interval` type (string vs int) | API likely accepts both. Our string format matches Dhan docs. |

---

## Implementation Order

```
Priority 1: A1                  (CRITICAL — zero tick loss in fast boot)
Priority 2: B1 → B2 → B3       (HIGH — depth persistence resilience)
Priority 3: C1 → C2 → C3       (MEDIUM — monitoring/alerting)
Priority 4: D1 → D2             (MEDIUM — sandbox enforcement)
Priority 5: E1 → E2 → E3 → E4 → E5 → E6  (Test coverage)
Priority 6: F1 → F2 → F3       (Documentation)
```

---

## Verification Scenarios

| # | Scenario | Expected | Block |
|---|----------|----------|-------|
| 1 | Fast boot with QuestDB down from start | `run_tick_persistence_consumer` uses `new_disconnected`, buffers all ticks. ZERO loss. | A |
| 2 | Slow boot with QuestDB down — depth | Depth writer starts in disconnected mode, buffers to ring buffer + disk spill. | B |
| 3 | QuestDB drops mid-session | Telegram CRITICAL alert fired. Ticks buffered. Auto-reconnect + drain on recovery. | C |
| 4 | Broadcast channel lags in fast boot | CRITICAL Telegram alert with count. `dlt_ticks_permanently_lost` counter incremented. | C |
| 5 | Disk space or memory approaching limits | Periodic check fires Telegram warning. | C |
| 6 | Config changed to `mode = "live"` before July | OMS constructor panics at boot with clear error message. | D |
| 7 | `dry_run = false` in base.toml committed | Test `test_base_config_dry_run_is_true` fails, CI blocks merge. | D |
| 8 | Push with missing DHAT test in trading crate | Gate 8 blocks push (after E6 wired). | E |
| 9 | All 54 Dhan endpoint constants defined | `test_all_54_dhan_rest_endpoints_have_constants` passes. | F |

---

## Dhan API Verification Summary

### Python SDK vs Rust Code: 112/115 PERFECT MATCH
- All 10 packet types: byte offsets, field types, sizes = IDENTICAL
- All 9 ExchangeSegment enum values = IDENTICAL (including gap at 6)
- All feed request/response codes = IDENTICAL
- All REST API headers, URLs, JSON formats = IDENTICAL
- 3 cosmetic differences (signedness, URL path, disconnect labels) — NO code changes needed
- 1 informational note (interval type) — our code follows Dhan docs, SDK may accept both

### Live Dhan Docs vs Reference Docs
- Agents still running (some pages returned 403)
- Will incorporate findings when available

---

## Total: 18 items across 6 blocks
- **Block A** (Critical): 1 item — CRITICAL priority
- **Block B** (Depth): 3 items — HIGH priority
- **Block C** (Monitoring): 3 items — MEDIUM priority
- **Block D** (Sandbox): 2 items — MEDIUM priority
- **Block E** (Tests): 6 items — MEDIUM priority
- **Block F** (Docs): 3 items — LOW priority
- **No new dependencies** — all using existing crates
- **Every item has specific test functions listed**
