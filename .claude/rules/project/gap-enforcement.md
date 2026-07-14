---
paths:
  - "crates/**/*.rs"
---

# Gap Enforcement Rules

Mechanical rules enforcing all instrument and system gaps.
Every rule here is checked by tests, hooks, or clippy — never by human review alone.

## I-P0-03: Expiry Check at Gate 4 (OMS)
- `engine.rs` Gate 4 must verify `expiry_date >= today` before order submission
- Stale universe = expired contract order = CRITICAL
- Test: `test_expired_contract_rejection`

## I-P1-05: Compound DEDUP Key
- `DEDUP_KEY_DERIVATIVE_CONTRACTS` must include `underlying_symbol`
- Prevents mixed historical data when security_id reused across underlyings
- Test: `test_dedup_key_derivative_contracts_includes_underlying`

## I-P1-06: Segment in Tick DEDUP
- `tick_persistence.rs` DEDUP key must include `exchange_segment`
- Prevents cross-segment tick collision
- Test: `test_tick_dedup_key_includes_segment`

## I-P1-08: Single-Row-Per-Instrument with Constant Designated Timestamp (REWRITTEN 2026-04-17)
**Rationale:** Pre-Phase-2 the snapshot tables accumulated one row per day
per instrument. Running the app on 2 days = 2x rows. Dashboard showed 442
underlyings (2x 221) and 207,796 contracts (2x ~104k) looking like duplicates.
Root cause: designated timestamp was today's IST midnight, which is part of
the QuestDB DEDUP key, so same business key on different days = different rows.
Phase 1 pinned the designated timestamp to constant epoch 0 so DEDUP fired on
the business key alone, but the `1970-01-01` display was confusing and there
was no clean way to answer "what's active right now?".

**Phase 2 design (live since 2026-04-17):**
- `fno_underlyings`, `derivative_contracts`, `subscribed_indices` each gain
  three lifecycle columns:
  - `status SYMBOL` — `active` or `expired`
  - `last_seen_date TIMESTAMP` — last IST date today's CSV contained this row
  - `expired_date TIMESTAMP` — IST date the row flipped to expired (null for active)
- `build_snapshot_timestamp()` still returns CONSTANT epoch 0 (QuestDB DEDUP
  must include the designated timestamp in the key, so we pin it to 0 and let
  the business key columns do the deduplication).
- Every write emits `status='active'`, `last_seen_date=today_ist`. After the
  ILP flush, `mark_missing_as_expired(today)` runs three UPDATE statements
  that flip any row with `status='active' AND last_seen_date < today` to
  `status='expired'`, `expired_date=today`.
- Reappearing contracts are auto-reactivated on the next write — ILP UPSERT
  replaces every column, so `status` flips back to `active` and `expired_date`
  is reset to null.
- `instrument_persistence.rs` MUST NOT contain `DELETE FROM` for lifecycle
  tables — SEBI 5-year retention applies.
- Dashboard + operator queries MUST include `WHERE status = 'active'`.
  Enforced mechanically by
  `crates/storage/tests/grafana_dashboard_snapshot_filter_guard.rs`.
- `instrument_build_metadata` is NOT a lifecycle table — it intentionally
  accumulates one row per daily build (build history).
- Migration: run `scripts/migrate-instrument-tables-phase2.sql` once before
  first deployment of the new code. It `DROP TABLE IF EXISTS` the three
  lifecycle tables (safe — rebuilt from Dhan CSV on every app start).

**Tests (in `crates/storage/src/instrument_persistence.rs` unless noted):**
- `test_build_snapshot_timestamp_returns_epoch_zero_constant`
- `test_build_snapshot_timestamp_is_constant_across_calls`
- `test_build_snapshot_timestamp_is_zero_not_positive`
- `test_ddl_contains_status_column_all_three_tables`
- `test_ddl_contains_last_seen_and_expired_date_columns`
- `test_instrument_status_active_and_expired_constants`
- `crates/storage/tests/instrument_uniqueness_guard.rs` — property guard
- `crates/storage/tests/grafana_dashboard_snapshot_filter_guard.rs` — scans
  every dashboard JSON for missing `status =`-style predicates

## I-P1-11: segment-aware security_id uniqueness (NEW 2026-04-17)
**Rationale:** Dhan reuses the same numeric `security_id` across different
`ExchangeSegment` values — e.g. FINNIFTY's IDX_I index value and an
NSE_EQ instrument both had `security_id = 27` in the live CSV on
2026-04-17. Any collection keyed on `security_id` alone silently
drops one of the two, leading to missing WebSocket subscriptions,
incorrect tick enrichment, and silent data loss.

**Rule:** `security_id` alone is NOT unique. The only unique instrument
key is `(security_id, exchange_segment)`. See the full rule body in
`.claude/rules/project/security-id-uniqueness.md` (auto-loaded).

**Fixed sites (7 commits on 2026-04-17 branch `claude/fix-duplicate-timestamps-3M2o0`):**
- **Planner dedup** (`subscription_planner.rs`): `HashSet<(u32, ExchangeSegment)>`
- **CSV parse dedup** (`universe_builder.rs`): `HashSet<(SecurityId, char)>`
- **InstrumentRegistry composite index** (`instrument_registry.rs`):
  `by_composite: HashMap<(SecurityId, ExchangeSegment), _>` stores BOTH
  entries; `iter()`, `by_exchange_segment()`, `len()`, category counts
  and `underlying_symbol_to_security_id` all iterate the composite map
  (NOT the legacy single-segment map). Commit `d8bfce5`.
- **Production lookup migration** (commit `c7397b3`): 5 sites migrated
  `registry.get(id)` → `registry.get_with_segment(id, segment)`:
  - `crates/core/src/pipeline/tick_processor.rs` (3 movers lookups)
  - `crates/trading/src/greeks/aggregator.rs:364`
  - `crates/trading/src/greeks/inline_computer.rs:187`
- **Banned-pattern hook** (commit `e05e66c`): `.claude/hooks/banned-pattern-scanner.sh`
  category 5 blocks new `HashSet<u32>` / `HashSet<SecurityId>` /
  `HashMap<u32, _>` / `HashMap<SecurityId, _>` / `.registry.get(id)` /
  `.registry.contains(id)` in instrument paths without a `// APPROVED:`
  comment.
- **Storage DEDUP keys** (commit `6f5e5c3`):
  - `DEDUP_KEY_DERIVATIVE_CONTRACTS` = `"security_id, underlying_symbol, exchange_segment"`
  - `DEDUP_KEY_SUBSCRIBED_INDICES` = `"security_id, exchange"`
  - Meta-guard `crates/storage/tests/dedup_segment_meta_guard.rs` scans
    every `DEDUP_KEY_*` constant and fails if any mentions `security_id`
    without `segment`/`exchange_segment`/`exchange`.
- **FnoUniverse runtime collision detection** (commit `01bd833`):
  `universe_builder.rs` emits WARN + counter `tv_fno_universe_derivative_collisions_total`
  when NSE_FNO/BSE_FNO derivative ids collide. `delta_detector.rs` has
  documented single-segment assumption referencing this counter.
- **Prometheus metrics** (commit `d049bd6`): `InstrumentRegistry`
  exposes `cross_segment_collisions()` getter. `subscription_planner`
  emits gauges `tv_instrument_registry_cross_segment_collisions`
  and `tv_instrument_registry_total_entries`. The construction WARN
  was upgraded to `error!` so Loki routes it to Telegram.

**Tests (19 total):**
- `subscription_planner::tests::test_regression_seen_ids_key_type_is_pair`
  — compile-time guard; fails to compile if someone regresses the key
- `subscription_planner::tests::test_regression_finnifty_id27_both_segments_are_kept`
  — scenario repro using NIFTY id=13 IDX_I + synthetic stock id=13 NSE_EQ
- `instrument_registry::tests::test_iter_returns_both_colliding_segments`
- `instrument_registry::tests::test_by_exchange_segment_returns_both_colliding_segments`
- `instrument_registry::tests::test_len_counts_both_colliding_segments`
- `instrument_registry::tests::test_category_counts_include_both_colliding_segments`
- `instrument_registry::tests::test_underlying_reverse_lookup_distinct_symbols_across_colliding_ids`
- `instrument_registry::tests::test_cross_segment_collisions_zero_when_no_collisions`
- `instrument_registry::tests::test_cross_segment_collisions_counts_both_colliding_pairs`
- `instrument_registry::tests::test_cross_segment_collisions_ignores_same_segment_duplicates`
- `instrument_registry::tests::test_empty_registry_cross_segment_collisions_is_zero`
- `crates/core/tests/regression_cross_segment_subscribe.rs` (4 integration tests)
  — asserts the full pipeline `registry.iter() → InstrumentSubscription →
  build_subscription_messages()` emits BOTH colliding entries in the
  serialized WebSocket subscribe JSON
- `crates/storage/tests/dedup_segment_meta_guard.rs` (4 meta-guard tests)
  — workspace-scanning guard
- `crates/storage/tests/grafana_dashboard_snapshot_filter_guard.rs`
  `dashboard_distinct_security_id_must_include_segment` — blocks any
  `count_distinct(security_id)` on cross-segment tables without
  segment-qualified distinct.

## LIVE-FEED-PURITY: no historical→ticks backfill (NEW 2026-04-17)
**Directive:** Parthiban, 2026-04-17 verbatim: "in our live market feed
websockets nowhere the backfill should happen, make it as hard
enforcement ... live market feed should contain only live market feed
data alone ... historical candle data fetch is a separate functionality".

**Rule:** the `ticks` QuestDB table is populated EXCLUSIVELY from live
WebSocket frames. Historical REST-API data NEVER crosses into `ticks`.

**Three enforcement layers:**
1. **Source deleted** (commit `8d527ab`): `crates/core/src/historical/backfill.rs`
   (836 lines) + `crates/core/tests/dhat_backfill_synth.rs` (119 lines) removed.
   9 `tv_backfill_*` Prometheus metrics removed from the catalog.
2. **Banned-pattern hook category 6**: any file under `crates/core/src/historical/`
   or a `backfill`/`synth` named file that references
   `TickPersistenceWriter`, `append_tick(`, `BackfillWorker`,
   `run_backfill`, `synthesize_ticks`, `GapBackfillRequest`, or
   `pub mod backfill` — commit blocked.
3. **Runtime guard** `crates/storage/tests/live_feed_purity_guard.rs`
   (6 tests): fails build if `backfill.rs` is recreated, if
   `pub mod backfill` reappears in `historical/mod.rs`, if any banned
   symbol appears in the historical flow, or if `cross_verify.rs`
   ever writes to ticks.

Full rule: `.claude/rules/project/live-feed-purity.md` (auto-loaded).

## DEPTH-STALE-ALERT: market-hours + edge-trigger suppression (HOTFIX 2026-04-17)
**Trigger:** Parthiban reported 15+ Telegram alerts `Depth spot price STALE`
for FINNIFTY + MIDCPNIFTY starting at 15:45 IST, post-market close.

**Rule:** `run_depth_rebalancer` (commit `e7926d9`) now enforces:
- **Market-hours gate**: `is_within_market_hours_ist()` reads
  `TICK_PERSIST_START/END_SECS_OF_DAY_IST` constants. Outside
  09:00-15:30 IST the loop `continue`s, clearing the stale-set so
  next market-open detects rising edge correctly.
- **Edge-triggered alerts**: `currently_stale: HashSet<String>` tracks
  which underlyings have already fired a Telegram alert. Rising edge
  (was_stale=false, is_stale=true) fires ONCE. Falling edge emits an
  INFO log only (no Telegram — operator already alerted).

**Tests** (4 in `depth_rebalancer::tests`):
- `test_is_within_market_hours_at_0830_ist_is_false`
- `test_is_within_market_hours_helper_exists_and_returns_bool`
- `test_market_hours_gate_is_wired_into_rebalancer_loop` (source scan)
- `test_edge_triggered_stale_alert_suppression_is_wired` (source scan)

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

### 2026-07-10 Update — 401-burst counter + CloudWatch pager (wave-2 #2)

Every bearer-auth rejection (all 3 arms of `require_bearer_auth` — invalid
token / malformed Authorization header / missing header) now increments the
SINGLE UNLABELED counter `tv_api_auth_failed_total`
(`crates/api/src/middleware.rs`). Deliberately NO new per-401 log line — the
counter is the burst signal; the pre-existing BUG-3 `warn!`s (path +
sanitized peer, 2026-07-05) remain the per-event forensic surface
(log-amplification defence, the same reasoning as the #1458 `debug!`-only
429 handling). The paging chain is the metrics-log-group delta-extraction
house pattern (`feed-stall-restart-alarm.tf` full-extraction variant):
`/tickvault/<env>/metrics` log metric filter
`{ $.tv_api_auth_failed_total = * }` → derived metric (raw name reused,
`host` dimension) → alarm `tv-<env>-api-auth-failed` (Sum ≥ 25 per aligned
300s window, `treat_missing_data = notBreaching`, always armed — the funnel
is probeable 24/7) → SNS `tv-prod-alerts` → Telegram
(`deploy/aws/terraform/auth-failed-alarm.tf`). Threshold rationale: a
fat-fingered token paste stays in single-digit 401s per 5 min (never
pages); a brute-force sweep against the public funnel is sustained
hundreds+/min (always lands ≥ 25 in some aligned window). First-sample
baseline (the feed-stall round-5 lesson): the series is pre-registered at 0
in `crates/api/src/lib.rs::build_router_with_auth` (router construction —
the single choke point both boot paths call, provably before the first
servable 401 since no router means no request, and after the boot Step-3
recorder install: main.rs calls `observability::init_metrics` before either
API-server spawn), so the CW agent's dropped-first-sample delta baseline is
the harmless 0, never part of the session's first burst. Lockstep ratchet:
`crates/app/tests/auth_failed_alarm_wiring_guard.rs` (api-crate registration
site + read-only main.rs install-before-router source-order scan +
3 emit sites + tf shape built from the real metric literal + HCL-stripper
self-test). Honest envelope: an aligned-window straddle (e.g. 24+24) never
pages — only a sustained sweep does; if the metrics-log shipping leg is
degraded (the 2026-07-06 collect_list class) this alarm is blind with it
and the `warn!` lines in `/tickvault/<env>/app` are the fallback. Cost:
+1 alarm + 1 derived metric ≈ $0.40/mo.

### 2026-07-10 Update — token rotation without restart (wave-2 #7)

The bearer token is no longer frozen at boot (audit row 13). The expected
token lives behind a SHARED lock-free hot-swap holder inside
`ApiAuthConfig` (`crates/api/src/middleware.rs` — the token_manager.rs
arc-swap house pattern; every per-request clone shares the holder, so a
swap is instantly visible). A SUPERVISED app-side loop
(`crates/app/src/api_token_rotation.rs`, spawned from BOTH main.rs boot
arms, gated on `enabled`) re-reads `/tickvault/<env>/api/bearer-token`
every 300s (READ-ONLY SSM GetParameter — never a write) and swaps ONLY a
non-empty, shape-valid, genuinely-new value
(`ApiAuthConfig::rotate_bearer_token` — fail-open: an SSM outage or a
mis-seeded empty value keeps the CURRENT token working, never locks the
operator out, never becomes accept-all). A WELL-FORMED-but-mismatched
bearer additionally hints ONE out-of-band re-read, hard-floored at 60s
(`OOB_RELOAD_FLOOR_SECS`, CAS-gated) — attacker 401 spam is bounded to
≤1 SSM read per minute; the missing/malformed-header arms never hint.
Honest rotation window: operator updates SSM → live within ≤5 min (or
~60s under active mismatched use); the OLD token dies at the swap instant
(single-value semantics, no dual-accept window). Observability:
`tv_api_token_reloads_total{outcome="ok"|"unchanged"|"failed"|"rejected"}`
(pre-registered at 0, first-sample-baseline discipline) +
`tv_api_token_reload_respawn_total{reason}`; routine cycles are `debug!`,
an actual rotation is one `info!`, and ≥3 consecutive read failures (or a
rejected SSM value) fire ONE edge-latched `warn!` per episode.
Deliberately NO new ErrorCode and NO new alarm: a failing re-read leaves
auth WORKING on the old token — degraded-not-broken (the 401-burst pager
above covers the attack surface). Lockstep ratchet:
`crates/app/tests/api_token_rotation_wiring_guard.rs` (both boot-arm
spawns + enabled gate; loop reads SSM + rotates + counters + never writes
SSM; exactly one middleware OOB hint site).

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

### 2026-07-14 Update — OMS-GAP-03 now PAGES (dual route)

The DHAN order-side alerting PR (noise-lock §2.1 grant) wired the paging
chain for the pre-existing coded emit at
`crates/trading/src/oms/circuit_breaker.rs:148`:

1. **errcode log-filter alarm** `tv-<env>-errcode-oms-gap-03`
   (`deploy/aws/terraform/error-code-alarms.tf`): `ok_recovery = false` —
   the CB-open emit is a CAS-gated once-per-episode edge
   (circuit_breaker.rs:145-148); an auto-OK ~15 min later never means the
   breaker closed. The genuine recovery signals are
   `tv_circuit_breaker_state` returning to 0 + the `CircuitBreakerClosed`
   Telegram.
2. **metric alarm** `tv-<env>-circuit-breaker-open` on
   `tv_circuit_breaker_state` (Maximum ≥ 1 / 300s, ok_actions ON — the
   gauge genuinely returns to 0=Closed; the AGGREGATOR-DROP-01 dual-route
   precedent).
3. **Telegram**: `CircuitBreakerOpened` (High) / `CircuitBreakerClosed`
   (Medium) via the app-side alert bridge
   (`crates/app/src/oms_alert_bridge.rs`).

All dormant today — the OMS is never instantiated with
`dhan_enabled = false` + `dry_run = true`.

## OMS-GAP-04: SEBI Rate Limiting
- `OrderRateLimiter::new(0)` must panic (fail-fast at config time)
- SEBI limit: max 10 orders/sec enforced via GCRA algorithm
- Burst capacity exhausted → `OmsError::RateLimited` (no retry — regulatory risk)
- `DailyRequestTracker`: 5,000 orders/day, 100,000 data API calls/day
- Counter must NOT exceed limit (saturating decrement on rejection)
- `reset()` clears all counters to 0
- Test: all `rate_limiter::tests::*` + integration `oms_rate_limiter::*`

### 2026-07-14 Update — OMS-GAP-04 coded emit + pager

The coded ERROR emit lives in the app-side alert bridge
(`crates/app/src/oms_alert_bridge.rs`, the `RateLimitExhausted` arm), NOT
the trading crate (seam-minimal — the trading crate's rate limiter stays
untouched; the bridge fires the coded line + the `RateLimitExhausted`
Telegram when the OMS alert sink delivers the denial). Pages via the
errcode log-filter alarm `tv-<env>-errcode-oms-gap-04`
(`ok_recovery = true` — fires per denied order, so it repeat-emits during
a storm and the OK genuinely ≈ the storm stopped; the dh-901/ws-gap-07
class). Dormant until order flow is revived.

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

### 2026-07-14 Update — RISK-GAP-01 halt paging + semantic note

The halt event pages via the app-side alert bridge's coded emit
(`crates/app/src/oms_alert_bridge.rs::fire_risk_halt` →
`tv-<env>-errcode-risk-gap-01`, `ok_recovery = false` — the halt persists
until `reset_daily`/manual reset; the emit fires once per trigger, so an
auto-OK while halted would be a Rule-11 false recovery) + the Critical
`RiskHalt` Telegram + the independent `tv-<env>-daily-pnl-breach` backstop
alarm on `tv_daily_pnl` (≤ −20,000, Minimum/300s). HONEST NOTES:
(a) `ManualHalt` also routes through this code (an operator action paging
as a pre-trade-check code — intentional: a halt is a halt);
(b) per-order `PositionSizeLimitExceeded` rejections never call
`trigger_halt` and emit nothing — page-on-halt-only is the deliberate
noise posture;
(c) `tv_daily_pnl` updates on risk checks (any `total_unrealized_pnl`
caller), not on fills — staleness between signals is the envelope.
All dormant until order flow is revived.

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
