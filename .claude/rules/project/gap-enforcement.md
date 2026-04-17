---
paths:
  - "crates/**/*.rs"
---

# Gap Enforcement Rules

Mechanical rules enforcing all instrument and system gaps.
Every rule here is checked by tests, hooks, or clippy — never by human review alone.

## I-P0-01: Duplicate Security ID = Hard Error
- `universe_builder.rs` must reject duplicate security_ids (not warn+skip)
- HashMap insertion must check for existing entry BEFORE overwrite
- Test: `test_duplicate_security_id_rejected`

## I-P0-02: Count Consistency After Build
- Post-build validation must check derivative count >= minimum threshold
- Truncated CSV (< 100 derivatives) = hard error, not silent continue
- Test: `test_validation_derivative_count_below_minimum_fails`

## I-P0-03: Expiry Check at Gate 4 (OMS)
- `engine.rs` Gate 4 must verify `expiry_date >= today` before order submission
- Stale universe = expired contract order = CRITICAL
- Test: `test_expired_contract_rejection`

## I-P0-04: Cache Persistence Validation
- `validate_cache_persistence()` must verify cache dir is not on tmpfs
- Linux: parse /proc/mounts for mount type
- Test: `test_cache_persistence_*`

## I-P0-05: S3 Remote Backup
- `S3BackupConfig` must validate bucket + region are non-empty
- Whitespace-only values = not configured
- Key layout: `{prefix}/{date}/filename` and `{prefix}/latest/filename`
- Test: all `s3_backup::tests::*`

## I-P0-06: Emergency Download Override
- When all caches missing during market hours, force-download as last resort
- INFO log is insufficient — must be CRITICAL + Telegram alert
- Never silently run with zero instruments

## I-P1-01: Daily CSV Refresh Scheduler
- `compute_next_trigger_time()` must always return positive duration (1..=86400s)
- `parse_daily_download_time()` requires strict HH:MM:SS format
- Default config is DISABLED (opt-in, not opt-out)
- Trading day check before refresh (skip weekends/holidays)
- Test: all `daily_scheduler::tests::*`

## I-P1-02: Delta Detector Full Field Coverage
- `delta_detector.rs` must track ALL mutable fields, not just lot_size + expiry
- Fields: lot_size, expiry_date, strike_price, option_type, tick_size, segment, display_name
- Test: delta detection tests for each field

## I-P1-03: Security ID Reuse Detection
- Delta detector must flag when security_id appears in both added and expired sets
- Compound identity match: symbol + expiry + strike + type
- Test: `test_delta_detects_security_id_reuse`

## I-P1-05: Compound DEDUP Key
- `DEDUP_KEY_DERIVATIVE_CONTRACTS` must include `underlying_symbol`
- Prevents mixed historical data when security_id reused across underlyings
- Test: `test_dedup_key_derivative_contracts_includes_underlying`

## I-P1-06: Segment in Tick DEDUP
- `tick_persistence.rs` DEDUP key must include `exchange_segment`
- Prevents cross-segment tick collision
- Test: `test_tick_dedup_key_includes_segment`

## I-P1-08: Single-Row-Per-Instrument with Constant Designated Timestamp (REWRITTEN 2026-04-17)
**Rationale:** Old design accumulated one snapshot row per day per instrument.
Running the app on 2 days = 2x rows. Dashboard showed 442 underlyings (2x 221)
and 207,796 contracts (2x ~104k) looking like duplicates. Root cause: designated
timestamp was today's IST midnight, which is part of QuestDB DEDUP key, so
same business key on different days = different rows.

**New design:**
- `build_snapshot_timestamp()` returns CONSTANT epoch 0 for all snapshot writes
- QuestDB DEDUP UPSERT KEYS effectively operate on business key alone
  (fno_underlyings: underlying_symbol; derivative_contracts: security_id +
  underlying_symbol; subscribed_indices: security_id)
- Running the app 1000 times same day → 221 rows (idempotent)
- Running on a new day → 221 rows (UPSERT, no accumulation)
- `instrument_persistence.rs` MUST NOT contain `DELETE FROM` for snapshot tables
- `naive_date_to_timestamp_nanos()` retained for historical-query helpers only
- Grafana dashboard queries should NOT include `WHERE timestamp = max(timestamp)`
  (harmless but redundant; prefer simple `SELECT count() FROM <table>`)

**Phase 2 (planned):** Add `status`, `first_seen_date`, `last_seen_date`,
`expired_date` columns for lifecycle tracking. See
`.claude/plans/instrument-uniqueness-redesign.md`.

**Tests (in `crates/storage/src/instrument_persistence.rs`):**
- `test_build_snapshot_timestamp_returns_epoch_zero_constant`
- `test_build_snapshot_timestamp_is_constant_across_calls`
- `test_build_snapshot_timestamp_is_zero_not_positive`
- `test_build_snapshot_epoch_nanos_is_zero_regardless_of_today`
- `test_no_delete_from_snapshot_tables_in_persist_path`
- Dashboard guard: `crates/storage/tests/grafana_dashboard_snapshot_filter_guard.rs`

## I-P2-02: Trading Day Guard on Download
- `instrument_loader.rs` must log when downloading on weekends
- Weekend detection uses IST timezone, not UTC
- Test: weekend detection logic

## GAP-NET-01: IP Monitor
- `compare_ips()` must be a pure function (no I/O)
- `is_valid_ipv4()` rejects IPv6, ports, whitespace
- Disabled config must exit immediately (not block)
- CancellationToken must stop the background task
- Test: all `ip_monitor::tests::*`

## GAP-SEC-01: API Auth Middleware
- Bearer token from `TV_API_TOKEN` env var
- Disabled = passthrough (dev mode)
- Case-sensitive "Bearer " prefix (not "bearer ")
- Empty/missing/wrong token = 401 Unauthorized
- Test: all `api_auth::tests::*`

---

# OMS (Order Management System) Gap Enforcement

## OMS-GAP-01: Order Lifecycle State Machine (State Transitions)
- `is_valid_transition(from, to)` must enforce the full DAG (26 valid transitions)
- Terminal states (Traded, Rejected, Cancelled, Expired) must reject ALL outgoing transitions
- Self-transitions (`Pending → Pending`) must be rejected
- `parse_order_status()` must handle all Dhan variants including `PART_TRADED` and `PARTIALLY_FILLED`
- Unknown status strings must return `None` (not panic)
- Test: all `state_machine::tests::*` + integration `oms_state_machine::*`

## OMS-GAP-02: Order Reconciliation (REST Sync)
- `reconcile_orders()` must be a pure function (no mutation of inputs)
- Status mismatch → ERROR log (triggers Telegram alert)
- Fill data comparison uses `f64::EPSILON` tolerance
- Ghost order detection: non-terminal OMS order missing from Dhan = WARNING
- Unknown Dhan status → skip (not panic), not counted in `total_checked`
- Test: all `reconciliation::tests::*` + integration `oms_reconciliation::*`

## OMS-GAP-03: Circuit Breaker (3-State FSM)
- Initial state must be `Closed`
- Opens after `OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD` consecutive failures
- `record_success()` resets failure counter to 0
- Half-Open allows exactly ONE probe request (CAS gate)
- Manual `reset()` always returns to Closed
- Test: all `circuit_breaker::tests::*` + integration `oms_circuit_breaker::*`

## OMS-GAP-04: SEBI Rate Limiting
- `OrderRateLimiter::new(0)` must panic (fail-fast at config time)
- SEBI limit: max 10 orders/sec enforced via GCRA algorithm
- Burst capacity exhausted → `OmsError::RateLimited` (no retry — regulatory risk)
- `DailyRequestTracker`: 5,000 orders/day, 100,000 data API calls/day
- Counter must NOT exceed limit (saturating decrement on rejection)
- `reset()` clears all counters to 0
- Test: all `rate_limiter::tests::*` + integration `oms_rate_limiter::*`

## OMS-GAP-05: Idempotency (Correlation Tracking)
- `generate_id()` must return valid UUID v4
- Two consecutive calls must return different UUIDs
- `track()` + `get_order_id()` must roundtrip correctly
- `clear()` must remove all tracked correlations
- Test: all `idempotency::tests::*` + integration `oms_idempotency::*`

## OMS-GAP-06: Dry-Run Safety Gate
- `dry_run` field defaults to `true` (safe by default)
- When `dry_run = true`, all HTTP calls are blocked
- Orders simulated with `PAPER-{counter}` IDs
- Test: OMS engine tests in dry-run mode

---

# WebSocket Gap Enforcement

## WS-GAP-01: Disconnect Code Classification
- `DisconnectCode::from_u16()` must handle all 12 known codes from annexure Section 11
- Codes: 800, 804, 805, 806, 807, 808, 809, 810, 811, 812, 813, 814
  (NOTE: codes 801, 802, 803 do NOT exist in annexure — they map to Unknown)
- `from_u16()` ↔ `as_u16()` must roundtrip for all variants
- Reconnectable: 800 (InternalServerError), 807 (AccessTokenExpired), Unknown
- Non-reconnectable: 804, 805, 806, 808, 809, 810, 811, 812, 813, 814
- Only 807 (AccessTokenExpired) requires token refresh
- Naming per annexure: 808=AuthenticationFailed, 809=AccessTokenInvalid, 810=ClientIdInvalid
- Test: integration `ws_disconnect_codes::*`

## WS-GAP-02: Subscription Batching
- `build_subscription_messages()` must clamp batch_size to [1, 100]
- Empty instrument list → empty Vec (no message sent)
- Single instrument → exactly 1 message
- 101+ instruments → split into multiple messages
- SecurityId must serialize as STRING in JSON (not number)
- Test: integration `ws_subscription_builder::*`

## WS-GAP-03: Connection State Machine
- 4 states: Disconnected, Connecting, Connected, Reconnecting
- Display impl must produce human-readable strings
- All states must be distinct (PartialEq)
- Test: integration `ws_connection_state::*`

---

# Risk Engine Gap Enforcement

## RISK-GAP-01: Pre-Trade Risk Checks
- Auto-halt: once halted, ALL subsequent orders rejected
- Daily loss check: `abs(realized + unrealized) >= max_loss_threshold` → reject
- Position limit: `(current + new).abs() > max_lots` → reject
- Order of checks: halt → daily loss → position limit
- Test: integration `risk_engine::*`

## RISK-GAP-02: Position & P&L Tracking
- `record_fill()`: reducing fills → realized P&L computed correctly
- `update_market_price()`: rejects non-positive and non-finite prices
- `total_unrealized_pnl()`: conservative — skips securities with no market price
- `reset_daily()`: clears ALL state (positions, prices, lots, P&L, halt)
- Test: integration `risk_pnl_tracking::*`

## RISK-GAP-03: Tick Gap Detection
- Warmup phase: first N ticks suppress alerts (avoids false positives)
- Warning threshold: `gap_secs >= TICK_GAP_ALERT_THRESHOLD_SECS`
- Error threshold: `gap_secs >= TICK_GAP_ERROR_THRESHOLD_SECS` → triggers Telegram
- Out-of-order timestamps: `saturating_sub` prevents underflow
- Per-security isolation: gap in security A must not affect security B
- `reset()` clears all tracking state
- Test: integration `risk_tick_gap::*`

---

# Auth Gap Enforcement

## AUTH-GAP-01: Token Expiry Validation
- `TokenState::is_valid()` returns false when token expired
- `TokenState::needs_refresh()` returns true inside refresh window
- `TokenState::time_until_refresh()` returns `Duration::ZERO` past window
- Test: auth module unit tests + integration `auth_token_lifecycle::*`

## AUTH-GAP-02: Disconnect Code → Token Refresh Mapping
- Only `DisconnectCode::AccessTokenExpired` (807) triggers token refresh
- All other codes do NOT trigger token refresh
- Test: integration `ws_disconnect_codes::test_only_807_requires_refresh`

---

# Storage Gap Enforcement

## STORAGE-GAP-01: Tick DEDUP Key Includes Segment
- `DEDUP_KEY_TICKS` constant must contain `segment`
- Prevents cross-segment collision (NSE_EQ vs BSE_EQ with same security_id)
- Test: `test_tick_dedup_key_includes_segment`

## STORAGE-GAP-02: f32→f64 Precision
- `f32_to_f64_clean()` prevents IEEE 754 widening artifacts
- `21004.95_f32` → `21004.95_f64` (not `21004.94921875`)
- Handles zero, infinity, NaN correctly
- Test: tick_persistence unit tests

---

## Cross-Cutting Enforcement
- All gap implementations must have `// I-P*-*:` or `// GAP-*:` or `// OMS-GAP-*:` or `// WS-GAP-*:` or `// RISK-GAP-*:` or `// AUTH-GAP-*:` comment at key code locations
- Every new pub fn implementing a gap must have a corresponding test
- No `#[allow(unused)]` on gap-related code
- No silent error swallowing — every error path must log or propagate
