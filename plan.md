# Plan: Historical Data Fixes + Mechanical Enforcement Upgrades

## PART A: Historical Data Issues (Bug Fixes + New Requirements)

### Root Cause Analysis

| # | Issue | Root Cause | File:Line |
|---|-------|-----------|-----------|
| A1 | Only 3-4 days fetched (not 90) | `config/base.toml` has `lookback_days = 5` — overrides the default of 90 | `config/base.toml:188` |
| A2 | Legacy `historical_candles_1m` table still exists | DDL still creates it; Grafana queries still reference it | `candle_persistence.rs:294`, `grafana_query_tests.rs` (12+ queries) |
| A3 | Fetch runs at boot, not after 3:30 PM | `fetch_historical_candles()` spawned at startup, no WebSocket lifecycle dependency | `main.rs:1341` |
| A4 | IDX_I sec IDs 13,25 not pulled | Both may need fetching in both IDX_I and NSE_EQ segments; dedup set blocks second fetch by same security_id | `candle_fetcher.rs:190-215` |
| A5 | No formal rate limit enforcement for data API | Uses simple `sleep(500ms)` delay; no GCRA or backoff on 805/DH-904 | `candle_fetcher.rs:237-239` |
| A6 | Cross-validation only checks candle COUNT | Doesn't compare live tick prices vs historical candle prices | `cross_verify.rs:97-104` |
| A7 | Mismatch details not sent to Telegram | Only logs pass/fail; no specific date/time/data in Telegram notification | `cross_verify.rs:166-173` |
| A8 | No test that validates config lookback_days = 90 | Config default is 90 but file overrides to 5; tests didn't catch it | Missing |

### A1: Fix lookback_days to 90
**File:** `config/base.toml`
- Change `lookback_days = 5` → `lookback_days = 90`
- Add config validation test: `lookback_days` in `base.toml` must be 90 (compile-time guarantee)

### A2: Remove legacy `historical_candles_1m` table
**Files:**
- `crates/storage/src/candle_persistence.rs` — Remove `CANDLES_1M_CREATE_DDL`, remove creation call from `ensure_candle_table_dedup_keys()`
- `crates/common/src/constants.rs` — Remove `QUESTDB_TABLE_CANDLES_1M` constant
- `crates/storage/tests/grafana_query_tests.rs` — Update all 12+ queries from `historical_candles_1m` → `historical_candles` (add `WHERE timeframe = '1m'`)
- `crates/core/tests/websocket_protocol_e2e.rs` — Remove the 1m table name assertion

### A3: Move historical fetch to AFTER 3:30 PM + after ALL WebSocket disconnect
**Files:**
- `crates/app/src/main.rs` — Move `fetch_historical_candles()` from boot sequence into post-market phase
- `crates/core/src/scheduler/mod.rs` — Add `PostMarketDataFetch` phase or trigger after `PostMarket` state
- New sequence:
  1. 15:30 IST: Market close
  2. 16:00 IST: All 3 WebSockets disconnected (live market feed + order update + full depth) via CancellationToken
  3. 16:01 IST: StorageGate closed
  4. 16:02 IST: Historical fetch begins (all 214 instruments × 5 timeframes × 90 days)
  5. After fetch: Cross-verification runs
  6. After verify: Telegram notification with results
- The `CancellationToken` cancellation at 16:00 already kills all 3 WebSockets — historical fetch must wait for this signal before starting
- Add `tokio::select!` that waits for cancellation token + verifies all WebSocket handles completed

### A4: Fix IDX_I instrument fetching
**File:** `crates/core/src/historical/candle_fetcher.rs`
- Problem: If security IDs 13 (NIFTY 50) and 25 (BANK NIFTY) exist as `MajorIndexValue` with `IDX_I` segment, they SHOULD be fetched. Code doesn't skip them.
- Investigation needed: Are they actually being skipped by the Dhan API? Or is the dedup set preventing a second fetch?
- Fix approach:
  - Change dedup key from `security_id` alone to `(security_id, exchange_segment)` — allows same security_id in different segments
  - Add explicit logging: log every instrument that gets fetched AND every instrument that gets skipped, with reason
  - Add test: verify NIFTY (13, IDX_I) and BANKNIFTY (25, IDX_I) are in the fetch set

### A5: Proper rate limiting for historical data API
**File:** `crates/core/src/historical/candle_fetcher.rs`
- Replace simple `sleep(request_delay_ms)` with proper enforcement:
  - Dhan data API limit: 5/sec, 100,000/day
  - Implement sliding window: track last 5 request timestamps, ensure gap >= 1 second
  - On HTTP 429 or Dhan error 805: STOP ALL for 60 seconds, then resume one at a time
  - On DH-904: Exponential backoff (10s → 20s → 40s → 80s → give up + CRITICAL alert)
  - Track daily request counter: if approaching 100K, HALT + alert

### A6: Enhanced cross-validation (live vs historical)
**File:** `crates/core/src/historical/cross_verify.rs`
- Current: Only checks candle count per instrument
- Enhanced:
  1. **Price range validation**: For each instrument, compare live tick data (from `candles_1s` table) vs historical candles. The 1m candle high should be >= max tick LTP in that minute; 1m candle low should be <= min tick LTP.
  2. **Volume consistency**: Historical volume for a candle should approximate the sum of tick volumes in that minute window.
  3. **Gap detection**: Identify specific minutes where live data exists but historical candle is missing (or vice versa).
  4. **Per-instrument mismatch report**: Collect the specific date, time, security_id, expected vs actual for each mismatch.

### A7: Detailed Telegram notification for mismatches
**Files:**
- `crates/core/src/notification/events.rs` — Add new event type `CandleMismatchDetected` with fields: `mismatches: Vec<MismatchDetail>` where `MismatchDetail` includes security_id, date, time, field (open/high/low/close/volume), expected, actual.
- `crates/core/src/notification/mod.rs` — Format mismatch details as HTML list for Telegram
- `crates/core/src/historical/cross_verify.rs` — Return mismatch details from verification
- `crates/app/src/main.rs` — Send mismatch notification after cross-verification

### A8: Config validation test
**Files:**
- `crates/common/src/config.rs` — Add test: `test_base_toml_lookback_days_is_90` that loads `config/base.toml` and asserts `lookback_days == 90`
- Add test: `test_base_toml_historical_enabled` that asserts historical fetch is enabled
- These tests catch config drift at CI time

---

## PART B: Mechanical Enforcement Upgrades (From Previous Plan)

### B1. Session Verification Protocol Rule
**File:** `.claude/rules/project/session-verification.md` (NEW)

Mechanical rules Claude MUST follow in every session:
- After ANY code change: `cargo check -p <crate>` before proceeding
- After ANY test addition: `cargo test -p <crate>` + paste actual output (not summary)
- After claiming coverage complete: Run `bash scripts/test-coverage-guard.sh <crate>` and paste output
- Before ending session: Run scoped `cargo clippy -p <crate>` + `cargo test -p <crate>`
- **Anti-hallucination protocol:** Never say "all tests pass" without pasting the `test result: ok. N passed` line
- **Proof format:** Every completion claim must include the actual terminal output showing pass/fail counts

### B2. Test Quality Validator Script
**File:** `scripts/test-quality-validator.sh` (NEW)

Goes beyond naming conventions — validates test CONTENT:
- **Error tests (Type 3):** Must contain `is_err()`, `unwrap_err()`, or `assert!(...err...)` assertion
- **Edge case tests (Type 4):** Must test with at least one boundary value (0, empty, MAX, MIN, exact boundary)
- **Stress tests (Type 5):** Must use values > 1000 or loop/iterate
- **Panic safety (Type 13):** Must use `catch_unwind` or `#[should_panic]`
- **Financial tests (Type 14):** Must use values like `i64::MAX`, `f64::MAX`, or explicit overflow check
- **Dedup tests (Type 20):** Must assert count/length reduction or duplicate detection

Exit 2 = block. Called from pre-push gate as Gate 11.5 (after type coverage, before clippy).

### B3. Verification Receipt Script
**File:** `scripts/verification-receipt.sh` (NEW)

Produces a single-output receipt after running all checks for a crate:
```
═══ VERIFICATION RECEIPT ═══
Crate:     core
Timestamp: 2026-03-14 10:30:00 IST
Commit:    abc1234
Tests:     1293 passed, 0 failed
22-Types:  19/19 PASS
Quality:   All content validators PASS
Clippy:    0 warnings
Fmt:       Clean
═══════════════════════════
```

### B4. Session Resilience Rule
**File:** `.claude/rules/project/session-resilience.md` (NEW)

Handles Claude Code session errors:
- **Cargo timeout (>120s):** Cancel, try `cargo test -p <single-crate>` instead of workspace
- **Session near context limit:** Immediately commit + push what's done, summarize progress
- **Build error loop (>3 attempts same error):** Stop, explain the blocker, ask user
- **Test failure loop (>2 fixes for same test):** Stop, show the actual error, ask user
- **Network error on push:** Retry with exponential backoff (2s, 4s, 8s, 16s), max 4 retries
- **Never silently skip failures** — every failure must be reported with actual output
- **Checkpoint protocol:** After every 3 successful changes, commit (don't batch 10+ changes)

### B5. Coverage Threshold Alignment
**Files:**
- `quality/crate-coverage-thresholds.toml` — Ratchet up
- `.github/workflows/ci.yml` — Align

| Crate   | CI Now | Doc Target | New CI |
|---------|--------|------------|--------|
| core    | 70%    | 95%        | 80%*   |
| trading | 60%    | 95%        | 75%*   |
| common  | 85%    | 90%        | 85%    |
| storage | 65%    | 90%        | 70%*   |
| api     | 65%    | 90%        | 70%*   |
| app     | 50%    | 80%        | 55%*   |

*Ratcheted up gradually toward documented 95% target.

### B6. Establish Financial Test Baseline
**File:** `.claude/hooks/.financial-test-baseline`

Run guard in "all" mode, write current count. Activates ratchet.

### B7. Deprecate Old Guard + CI Cleanup
**Files:**
- `.github/workflows/ci.yml` — Remove `testing-standards-guard.sh` (19-type), keep only `test-coverage-guard.sh` (22-type)
- `.claude/hooks/testing-standards-guard.sh` — Add deprecation header

### B8. Update CLAUDE.md Testing Section
**File:** `CLAUDE.md`

Add:
- Reference to session verification protocol
- Anti-hallucination requirements
- Checkpoint protocol
- Historical fetch timing requirement (post-market only, after WS disconnect)
- Config validation requirements

---

## Further Enhancements & Refinements Identified

1. **Historical fetch progress Telegram**: Send periodic progress updates during the 214-instrument fetch (e.g., every 50 instruments: "Fetched 50/214, 18,750 candles so far")
2. **QuestDB health check before fetch**: Verify QuestDB is reachable and has disk space before starting 90-day bulk fetch
3. **Resumable fetch**: If historical fetch fails mid-way (e.g., token expired at instrument 150), save checkpoint and resume from instrument 151 after token refresh
4. **Per-timeframe verification**: Cross-verify not just 1m candles but all 5 timeframes (5m, 15m, 60m, daily) against live data
5. **Weekend/holiday awareness**: Historical fetch should skip weekends/holidays when computing 90-day lookback (90 trading days, not 90 calendar days — though Dhan handles this server-side by returning empty data for non-trading days)
6. **Stale data detection**: If historical fetch returns data more than 1 day old, alert — might indicate Dhan API issues
7. **Grafana dashboard migration**: After removing `historical_candles_1m`, update Grafana JSON dashboard files to use `historical_candles WHERE timeframe = '1m'`

---

## Files Created/Modified Summary

| File | Action | Why |
|------|--------|-----|
| `config/base.toml` | MODIFY | Fix lookback_days 5→90 |
| `crates/storage/src/candle_persistence.rs` | MODIFY | Remove legacy 1m table DDL |
| `crates/common/src/constants.rs` | MODIFY | Remove QUESTDB_TABLE_CANDLES_1M |
| `crates/storage/tests/grafana_query_tests.rs` | MODIFY | Update queries to unified table |
| `crates/core/tests/websocket_protocol_e2e.rs` | MODIFY | Remove 1m table assertion |
| `crates/app/src/main.rs` | MODIFY | Move historical fetch to post-market |
| `crates/core/src/scheduler/mod.rs` | MODIFY | Add post-market data fetch trigger |
| `crates/core/src/historical/candle_fetcher.rs` | MODIFY | Fix dedup key, add IDX_I logging, rate limit |
| `crates/core/src/historical/cross_verify.rs` | MODIFY | Enhanced validation (price, volume, gaps) |
| `crates/core/src/notification/events.rs` | MODIFY | Add CandleMismatchDetected event |
| `crates/core/src/notification/mod.rs` | MODIFY | Format mismatch details for Telegram |
| `crates/common/src/config.rs` | MODIFY | Add config validation tests |
| `.claude/rules/project/session-verification.md` | CREATE | Anti-hallucination protocol |
| `.claude/rules/project/session-resilience.md` | CREATE | Session error handling |
| `scripts/test-quality-validator.sh` | CREATE | Test content validation |
| `scripts/verification-receipt.sh` | CREATE | Proof receipt generation |
| `quality/crate-coverage-thresholds.toml` | MODIFY | Align thresholds |
| `.github/workflows/ci.yml` | MODIFY | Remove deprecated guard, update thresholds |
| `.claude/hooks/.financial-test-baseline` | ESTABLISH | Activate ratchet |
| `.claude/hooks/pre-push-gate.sh` | MODIFY | Add Gate 11.5 |
| `CLAUDE.md` | MODIFY | Add session + historical requirements |

---

## Execution Order

### Phase 1: Bug Fixes (A1, A2, A4, A8) — immediate, no new features
1. Fix `lookback_days = 90` in config
2. Remove legacy `historical_candles_1m` table + update all references
3. Fix dedup key to include segment for historical fetch
4. Add config validation tests
5. Verify: `cargo test --workspace` + `cargo clippy --workspace`

### Phase 2: New Requirements (A3, A5, A6, A7) — structural changes
6. Move historical fetch to post-market (after WebSocket disconnect)
7. Implement proper data API rate limiting
8. Enhance cross-verification (price, volume, gap detection)
9. Add detailed Telegram notifications for mismatches
10. Verify: `cargo test --workspace`

### Phase 3: Enforcement Upgrades (B1-B8)
11. Create session-verification rule
12. Create session-resilience rule
13. Create test-quality-validator script
14. Create verification-receipt script
15. Integrate into pre-push gate
16. Establish financial baseline
17. Align coverage thresholds + deprecate old guard
18. Update CLAUDE.md
19. Final verification + commit + push
