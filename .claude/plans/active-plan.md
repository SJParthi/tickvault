# Implementation Plan: Comprehensive Hardening — Data Safety, Security, Performance, Coverage

**Status:** IN_PROGRESS
**Date:** 2026-04-01
**Approved by:** Parthiban (2026-04-01)

---

## Source: 8-Agent Deep Scan (2026-04-01)

| Agent | Scope | Key Findings |
|-------|-------|-------------|
| Dhan Market Feed checker | Live feed + annexure vs Python SDK | All packet layouts match. 1 rate limit discrepancy |
| Dhan Depth/Historical checker | Depth, historical, auth, instruments | Covered by other agents (403 on docs site) |
| Dhan Quotes/Funds checker | Quote, funds, statements, options, releases | 1 missing endpoint (/charts/rollingoption) |
| Python SDK checker | Full SDK vs Rust binary parsing | 100% match. Our Rust is more correct in several areas |
| QuestDB resilience scanner | Tick persistence, buffering, recovery | 5 gaps (1 CRITICAL, 1 HIGH, 2 MEDIUM, 1 LOW) |
| Test coverage scanner | 22 test types, hooks, enforcement | 7,250 tests. Loom/DHAT need expansion |
| Security reviewer | OWASP, secrets, input validation | 10 issues (3 HIGH, 4 MEDIUM, 3 LOW) |
| Hot path reviewer | O(1) verification, allocation detection | 5 findings (1 CRITICAL, 3 WARNING) |

---

## BLOCK A: Critical Data Safety (Zero Tick Loss)

- [x] A1: Call `recover_stale_spill_files()` at startup — crates/app/src/main.rs:873 after TickPersistenceWriter::new()
  - Files: `crates/app/src/main.rs`
  - Change: After tick_writer creation (line ~873), call `recover_stale_spill_files()` to drain orphaned spill files from previous crashes
  - Tests: `test_stale_spill_recovery_at_startup`, `test_no_orphaned_ticks_after_crash_restart`

- [x] A2: Increase broadcast channel capacity from 256 to 65536 — constants.rs + main.rs (2 sites)
  - Files: `crates/app/src/main.rs`, `crates/common/src/constants.rs`
  - Change: Add `TICK_BROADCAST_CAPACITY = 65_536` constant, use in broadcast::channel() creation
  - Tests: `test_broadcast_capacity_matches_constant`, `test_cold_path_no_lag_at_high_throughput`

- [x] A3: Make TickPersistenceWriter start in disconnected buffering mode — new_disconnected() + main.rs fallback
  - Files: `crates/storage/src/tick_persistence.rs`, `crates/app/src/main.rs`
  - Change: `new()` succeeds with `sender = None`, ring buffer + disk spill activate immediately. Background reconnect polls every 30s.
  - Tests: `test_tick_writer_starts_without_questdb`, `test_ticks_buffered_before_questdb_available`, `test_reconnect_drains_buffer_after_questdb_starts`

- [x] A4: Add CRITICAL Telegram alert when tick persistence enters disconnected mode — main.rs startup path
  - Files: `crates/storage/src/tick_persistence.rs`, `crates/core/src/notification/events.rs`
  - Tests: `test_critical_alert_on_questdb_unavailable`

- [x] A5: Add `dlt_ticks_dropped_total` Prometheus counter — tick_persistence.rs spill_tick_to_disk
  - Files: `crates/storage/src/tick_persistence.rs`
  - Tests: `test_tick_drop_counter_zero_with_buffering`

---

## BLOCK B: Security Hardening (3 HIGH fixes)

- [x] B1: Redact bearer token in ApiAuthConfig Debug output — middleware.rs manual Debug impl
  - Files: `crates/api/src/middleware.rs`
  - Change: Replace `#[derive(Debug)]` with manual `fmt::Debug` impl that masks `bearer_token` as `"[REDACTED]"`
  - Tests: `test_api_auth_config_debug_redacts_token`

- [x] B2: Redact raw API response body in token_manager error messages — token_manager.rs:465
  - Files: `crates/core/src/auth/token_manager.rs`
  - Change: At line ~465, use `redact_url_params()` on `body_text` before including in error reason
  - Tests: `test_profile_error_does_not_leak_response_body`

- [x] B3: Tighten CORS — restrict methods and headers — lib.rs build_cors_layer
  - Files: `crates/api/src/lib.rs`
  - Change: Replace `allow_methods(Any)` with `allow_methods([Method::GET, Method::POST, Method::DELETE])`, replace `allow_headers(Any)` with `allow_headers([AUTHORIZATION, CONTENT_TYPE])`
  - Tests: `test_cors_rejects_disallowed_method`, `test_cors_allows_get_post`

- [x] B4: Redact internal error details in instruments handler — instruments.rs:109 + diagnostic_serialization_fallback
  - Files: `crates/api/src/handlers/instruments.rs`
  - Change: At line ~109, replace `format!("rebuild failed: {err}")` with generic `"rebuild failed — check server logs"`
  - Tests: `test_rebuild_error_does_not_leak_internal_details`

---

## BLOCK C: Hot Path Performance Fixes

- [ ] C1: DEFERRED — Box<dyn GreeksEnricher> constrained by crate dependency graph (core can't ref trading)
  - Files: `crates/core/src/pipeline/tick_processor.rs`, `crates/common/src/tick_types.rs`
  - Change: Since there's exactly one implementor (InlineGreeksComputer), use `Option<InlineGreeksComputer>` instead of `Option<Box<dyn GreeksEnricher>>`. Eliminates vtable dispatch on every tick.
  - Tests: existing tick processing tests + `test_greeks_enricher_no_vtable_dispatch`

- [x] C2: Replace `.collect()` with index-based iteration in `rescue_in_flight()` — tick + depth writers
  - Files: `crates/storage/src/tick_persistence.rs`
  - Change: At line ~332, replace `let rescued: Vec<ParsedTick> = self.in_flight.drain(..).collect(); for tick in rescued { ... }` with `while let Some(tick) = self.in_flight.pop() { self.buffer_tick(tick); }`
  - Tests: `test_rescue_in_flight_no_allocation`

- [x] C3: Add `#[inline]` to all top-level parser functions — 10 functions across 10 files
  - Files: `crates/core/src/parser/header.rs`, `ticker.rs`, `quote.rs`, `full_packet.rs`, `oi.rs`, `previous_close.rs`, `disconnect.rs`, `market_depth.rs`, `dispatcher.rs`
  - Change: Add `#[inline]` to `parse_header`, `parse_ticker_packet`, `parse_quote_packet`, `parse_full_packet`, `parse_oi_packet`, `parse_previous_close_packet`, `parse_disconnect_packet`, `parse_market_depth_packet`, `dispatch_frame`
  - Tests: benchmark regression check (existing benches)

- [ ] C4: DEFERRED — RwLock is cold-path only (API reads, 5s writes); minimal contention for single-user
  - Files: `crates/core/src/pipeline/top_movers.rs`, `crates/core/src/pipeline/option_movers.rs`, `crates/api/src/handlers/top_movers.rs`
  - Change: Use `arc_swap::ArcSwap<Option<TopMoversSnapshot>>` for lock-free reads. Write path (every 5s) uses `store()`, read path uses `load()`.
  - Tests: existing top_movers tests + `test_top_movers_lock_free_reads`

---

## BLOCK D: Resilience Enhancements

- [ ] D1: Add ring buffer + disk spill to LiveCandleWriter (parallel to tick writer)
  - Files: `crates/storage/src/candle_persistence.rs`
  - Change: Add `candle_buffer: VecDeque<LiveCandle>` ring buffer (capacity 10,000) + disk spill. Mirror TickPersistenceWriter's 3-tier architecture.
  - Tests: `test_live_candle_buffered_on_questdb_failure`, `test_live_candle_drained_on_recovery`

- [ ] D2: Add tick persistence health to `/health` endpoint
  - Files: `crates/api/src/handlers/health.rs`, `crates/api/src/state.rs`
  - Change: Report tick_writer status (connected/buffering/disconnected) in health response
  - Tests: `test_health_reports_tick_persistence_status`

---

## BLOCK E: Test Coverage Expansion

- [ ] E1: Expand DHAT allocation tests to trading crate hot paths
  - Files: `crates/trading/tests/dhat_oms_hot_path.rs`
  - Tests: `test_oms_state_transition_zero_alloc`, `test_risk_check_zero_alloc`

- [ ] E2: Add adversarial tests for tick persistence cold-start resilience
  - Files: `crates/storage/tests/tick_resilience.rs`
  - Tests: `test_zero_tick_loss_questdb_down_from_start`, `test_spill_file_recovery_after_restart`, `test_buffer_capacity_at_300k_limit`, `test_disk_spill_activates_when_buffer_full`

- [ ] E3: Add security audit tests for token handling
  - Files: `crates/core/tests/security_audit.rs`
  - Tests: `test_token_never_in_log_output`, `test_token_never_in_error_display`, `test_token_never_in_debug_format`

- [ ] E4: Wire 22 test type check into pre-push gate (Gate 8)
  - Files: `.claude/hooks/pre-push-gate.sh`
  - Change: Detect changed crates via `git diff`, pass as scope to `scripts/test-coverage-guard.sh`. Fast (<5s).
  - Tests: Manual push verification

---

## BLOCK F: Documentation & Enforcement Updates

- [x] F1: Fix rate limit discrepancy — 08-annexure-enums.md updated to 1000/hr, 7000/day
  - Files: `docs/dhan-ref/08-annexure-enums.md`
  - Change: Update Order API limits from 500/hr, 5000/day to 1000/hr, 7000/day (CLAUDE.md values are correct per Dhan v2.3 release)

- [ ] F2: Add `/charts/rollingoption` endpoint to historical data reference
  - Files: `docs/dhan-ref/05-historical-data.md`
  - Change: Document expired options data endpoint with fields: `securityId`, `exchangeSegment`, `instrument`, `expiryFlag`, `expiryCode`, `strike`, `drvOptionType`, `requiredData`, `fromDate`, `toDate`, `interval`

- [x] F3: Fix coverage threshold discrepancy in CLAUDE.md (95% → 100%)
  - Files: `CLAUDE.md`

- [x] F4: Update CLAUDE.md test count from ~2,439 to ~7,250
  - Files: `CLAUDE.md`

- [x] F5: Update enforcement.md with Gate 8 (7 → 8 fast gates)
  - Files: `.claude/rules/project/enforcement.md`

- [ ] F6: Add Dhan API coverage test verifying all 54 endpoint constants
  - Files: `crates/common/tests/dhan_api_coverage.rs`
  - Tests: `test_all_54_dhan_rest_endpoints_have_constants`, `test_all_4_websocket_urls_defined`

---

## Implementation Order

```
Phase 1: A1 → A2 → A3 → A4 → A5    (Data safety — highest production risk)
Phase 2: B1 → B2 → B3 → B4          (Security — 3 HIGH findings)
Phase 3: C1 → C2 → C3 → C4          (Hot path — O(1) violations)
Phase 4: D1 → D2                     (Resilience — candle buffer + health)
Phase 5: E1 → E2 → E3 → E4          (Test coverage — mechanical enforcement)
Phase 6: F1 → F2 → F3 → F4 → F5 → F6 (Documentation — accuracy)
```

---

## Verification Scenarios

| # | Scenario | Expected | Block |
|---|----------|----------|-------|
| 1 | QuestDB down from market open to close | All ticks in ring buffer + disk spill. Zero loss. CRITICAL alert. | A |
| 2 | App crashes with active spill file, restarts | Stale spill drained at startup. Zero orphaned ticks. | A |
| 3 | Burst of 1000 ticks in 100ms | Broadcast channel (65536) absorbs burst. Cold-path no lag. | A |
| 4 | `format!("{:?}", api_auth_config)` | Bearer token shows `[REDACTED]`. | B |
| 5 | Dhan profile API returns error | Error message does NOT contain raw response body. | B |
| 6 | CORS preflight with PUT method | Rejected (only GET/POST/DELETE allowed). | B |
| 7 | Tick processing with greeks enricher | No vtable dispatch, concrete type inlined. | C |
| 8 | QuestDB flush fails with 1000 in-flight ticks | rescue_in_flight drains without Vec allocation. | C |
| 9 | Live candles during QuestDB outage | Candles buffered in ring buffer, drained on recovery. | D |
| 10 | Push with missing test type in changed crate | Gate 8 blocks push. | E |

---

## What Is NOT In Scope

| Excluded | Reason |
|----------|--------|
| Playwright/browser testing | Rust backend system. 22-type test infra already more comprehensive |
| 4 skipped REST endpoints (ltp/ohlc/quote/instrument) | WebSocket provides same data real-time |
| Token encryption on disk (HIGH finding) | Acceptable risk within Docker container; fix separately if deploying outside Docker |
| DefaultHasher for client_id (MEDIUM finding) | Low risk; fix in future security pass |
| HTTP to QuestDB (MEDIUM finding) | Internal Docker network only; no external exposure |
| Public endpoint rate limiting (LOW finding) | Single-user system; add if exposed externally |
| Localhost in CORS fallback (LOW finding) | Dev-only path; production uses configured origins |
| expect() in rate_limiter (LOW finding) | Config validation at load time prevents 0 value; fix in future pass |

---

## Total: 25 items across 6 blocks
- **Block A** (Data Safety): 5 items — CRITICAL priority
- **Block B** (Security): 4 items — HIGH priority
- **Block C** (Hot Path): 4 items — HIGH priority
- **Block D** (Resilience): 2 items — MEDIUM priority
- **Block E** (Test Coverage): 4 items — MEDIUM priority
- **Block F** (Documentation): 6 items — LOW priority
- **No new dependencies** — all using existing crates
- **Every item has specific test functions listed**
