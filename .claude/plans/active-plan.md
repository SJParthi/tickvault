# Implementation Plan: 100% Test Coverage & Mechanical Enforcement

**Status:** DRAFT
**Date:** 2026-03-31
**Approved by:** pending

---

## Current State (Verified Facts — Not Hallucinated)

| Metric | Current Value | How Verified |
|--------|--------------|--------------|
| Test functions | 7,251 | `grep -rE '#\[test\]' crates/ \| wc -l` |
| 22 test types | 59/59 PASS | `bash scripts/test-coverage-guard.sh` |
| Dhan API endpoints | 54/58 implemented | Grep of constants.rs + api_client.rs |
| Coverage threshold config | 100% all crates | `quality/crate-coverage-thresholds.toml` |
| Pre-push gates | 7 gates | `.claude/hooks/pre-push-gate.sh` |
| Tick resilience | PARTIAL — fails on cold start | Code audit of tick_persistence.rs |
| Monitoring | 85% complete | Audit of observability.rs + dashboards |
| Disk space | 14GB free / 252GB | `df -h /` |

---

## BLOCK A: Zero Tick Loss Guarantee (Even If QuestDB Down From Start)

### Current Gap (Proven by Code Audit)
- `TickPersistenceWriter::new()` at `crates/storage/src/tick_persistence.rs` returns `Err` if QuestDB unreachable
- `crates/app/src/main.rs:873-892`: startup failure → `tick_writer = None` → ticks **discarded silently**
- Ring buffer (300,000 capacity) + disk spill to `data/spill/ticks-YYYYMMDD.bin` only activates AFTER initial QuestDB connection succeeds
- `recover_stale_spill_files()` exists but requires QuestDB sender to be `Some`
- **Result**: If QuestDB never connects during entire trading session, ALL ticks are lost

### Fix (3 items)

- [ ] A1: Make TickPersistenceWriter start in "disconnected buffering" mode when QuestDB unavailable
  - Files: `crates/storage/src/tick_persistence.rs`, `crates/app/src/main.rs`
  - Change: `new()` succeeds with `sender = None`, ring buffer + disk spill activate immediately. Background task polls QuestDB every 30s. On connect → drain buffer → set sender.
  - Tests: `test_tick_writer_starts_without_questdb`, `test_ticks_buffered_before_questdb_available`, `test_reconnect_drains_buffer_after_questdb_starts`

- [ ] A2: Add CRITICAL Telegram alert when tick persistence enters disconnected mode
  - Files: `crates/storage/src/tick_persistence.rs`, `crates/core/src/notification/events.rs`
  - Change: Fire CRITICAL alert on startup if QuestDB unreachable. Repeat every 5 min until connected.
  - Tests: `test_critical_alert_on_questdb_unavailable`, `test_alert_stops_after_questdb_connects`

- [ ] A3: Add tick drop counter metric (must always read 0 after this fix)
  - Files: `crates/storage/src/tick_persistence.rs`
  - Change: Add `dlt_ticks_dropped_total` Prometheus counter. Incremented only if both ring buffer AND disk spill fail (should be impossible).
  - Tests: `test_tick_drop_counter_zero_with_buffering`

### Proof Scenarios
| # | Scenario | Expected |
|---|----------|----------|
| 1 | QuestDB down from market open to close | All ticks in ring buffer + disk spill. Zero loss. CRITICAL alert fires every 5 min. |
| 2 | QuestDB starts 30 min after market open | Background reconnect detects QuestDB. Drains ring buffer first, then disk spill. All ticks preserved. Alert clears. |
| 3 | QuestDB crashes mid-day, recovers 10 min later | Existing reconnect logic handles this (already works). New fix covers only cold-start gap. |
| 4 | Ring buffer fills (300K ticks) before QuestDB starts | Overflow spills to disk. Disk spill has no size limit (bounded only by disk space). |

---

## BLOCK B: Wire 22 Test Type Enforcement Into Pre-Push Gate

### Current State
- `scripts/test-coverage-guard.sh` exists, passes 59/59, supports scoped checking
- NOT wired into `.claude/hooks/pre-push-gate.sh` — can be bypassed on push

### Fix (2 items)

- [ ] B1: Add Gate 8 to pre-push-gate.sh — scoped 22 test type check
  - Files: `.claude/hooks/pre-push-gate.sh`
  - Change: Detect changed crates via `git diff`, pass as scope to `scripts/test-coverage-guard.sh`. Fast (<5s). Only checks crates with modified `.rs` files.
  - Tests: Manual push verification

- [ ] B2: Update enforcement.md to document Gate 8
  - Files: `.claude/rules/project/enforcement.md`
  - Change: Add Gate 8 documentation, update gate count 7 → 8

---

## BLOCK C: Dhan API Coverage Verification Test

### Audit Results (54/58 — 4 Intentionally Skipped)

**Implemented (54 endpoints):**
- Orders: 7/7 — place, modify, cancel, slice, book, single, by-correlation
- Trades: 3/3 — book, by-order, trade-history
- Super Orders: 4/4 — place, modify, cancel, list
- Forever Orders: 4/4 — create, modify, delete, list
- Conditional Triggers: 5/5 — create, modify, delete, get-one, get-all
- Positions & Holdings: 4/4 — positions, convert, exit-all, holdings
- Funds & Margin: 3/3 — fund-limit, margin-single, margin-multi
- Option Chain: 2/2 — chain, expiry-list
- Historical Charts: 2/2 — daily, intraday
- EDIS: 3/3 — tpin, form, inquire
- Profile & Auth: 3/3 — profile, renew-token, generate-access-token
- IP Management: 3/3 — set, modify, get
- Kill Switch: 3/3 — activate, deactivate, status
- P&L Exit: 3/3 — configure, stop, status
- Statements: 2/2 — ledger, trade-history
- WebSocket: 4/4 — market-feed, order-update, 20-depth, 200-depth
- CSV Download: 2/2 — detailed, fallback

**Intentionally NOT Implemented (4 — with justification):**
| Endpoint | Why Skipped |
|----------|-------------|
| `POST /v2/marketfeed/ltp` | WebSocket provides real-time LTP continuously |
| `POST /v2/marketfeed/ohlc` | WebSocket + candle aggregator provides OHLCV |
| `POST /v2/marketfeed/quote` | WebSocket Full mode provides quote data |
| `GET /v2/instrument/{segment}` | CSV download is faster, no auth, no rate limit |

### Fix (1 item)

- [ ] C1: Add integration test verifying all 54 endpoint URL constants exist
  - Files: `crates/common/tests/dhan_api_coverage.rs`
  - Tests: `test_all_54_dhan_rest_endpoints_have_constants`, `test_all_4_websocket_urls_defined`, `test_skipped_endpoints_documented`

---

## BLOCK D: Monitoring & Alerting Gap Fixes

### Current State (85% Complete)
- 30+ Prometheus metrics defined and exported to `:9091`
- Telegram alerts: fully implemented, fire-and-forget async
- 5 Grafana dashboards: system-overview, market-data, trading-pipeline, logs, traefik
- OpenTelemetry: OTLP → Jaeger via `dlt-jaeger:4317`
- Health: `GET /health` returns subsystem status

### Gaps Found (3 items)

- [ ] D1: Add P&L visualization panel to trading-pipeline Grafana dashboard
  - Files: `deploy/docker/grafana/dashboards/trading-pipeline.json`
  - Change: New panel showing `dlt_realized_pnl` + `dlt_unrealized_pnl` time series
  - Tests: JSON validity (dashboard loads)

- [ ] D2: Add `#[tracing::instrument]` to API handlers for full request tracing
  - Files: `crates/api/src/handlers/health.rs`, `quote.rs`, `stats.rs`, `top_movers.rs`, `instruments.rs`, `index_constituency.rs`
  - Tests: Existing handler tests verify no regression

- [ ] D3: Add tick persistence health to `/health` endpoint subsystem check
  - Files: `crates/api/src/handlers/health.rs`
  - Change: Report tick_writer status (connected/buffering/disconnected) in health response
  - Tests: `test_health_reports_tick_persistence_status`

---

## BLOCK E: Test Quality Hardening

### Fix (2 items)

- [ ] E1: Add adversarial tests for tick persistence cold-start scenario
  - Files: `crates/storage/tests/tick_resilience.rs`
  - Tests: `test_zero_tick_loss_questdb_down_from_start`, `test_zero_tick_loss_questdb_intermittent`, `test_spill_file_recovery_after_restart`, `test_buffer_capacity_at_300k_limit`, `test_disk_spill_activates_when_buffer_full`

- [ ] E2: Add security tests for token handling across all code paths
  - Files: `crates/core/tests/security_audit.rs`
  - Tests: `test_token_never_in_log_output`, `test_token_never_in_error_display`, `test_token_never_in_debug_format`, `test_secret_zeroized_on_drop`

---

## BLOCK F: Documentation & Rule Updates

- [ ] F1: Update CLAUDE.md test count from ~2,439 to 7,251
  - Files: `CLAUDE.md`

- [ ] F2: Update enforcement.md with Gate 8 (22 test types)
  - Files: `.claude/rules/project/enforcement.md`

- [ ] F3: Update pre-push gate count in CLAUDE.md
  - Files: `CLAUDE.md`

---

## Implementation Order

```
Phase 1: B1 → B2 → F2 → F3           (Wire gate + docs — fastest win, <30 min)
Phase 2: A1 → A2 → A3 → E1           (Tick resilience — biggest gap fix + tests)
Phase 3: C1 → E2                      (API coverage test + security tests)
Phase 4: D1 → D2 → D3                (Monitoring gaps)
Phase 5: F1                           (Final doc update with new test count)
```

**Rationale:** Gate wiring first (immediately blocks bad pushes). Tick resilience second (biggest production risk). API coverage + security tests third. Monitoring last (already 85% done). Doc updates at end (accurate counts).

---

## Verification Scenarios

| # | Scenario | Expected | Block |
|---|----------|----------|-------|
| 1 | QuestDB down from start to end | All ticks buffered. Zero loss. CRITICAL alert. | A |
| 2 | QuestDB starts 30 min late | Buffer drains. All ticks preserved. | A |
| 3 | Push with missing test type in changed crate | Gate 8 blocks push with error. | B |
| 4 | Push with docs-only changes (no .rs files) | Gate 8 skips (NONE scope). Push proceeds. | B |
| 5 | CI merge with any crate below 100% coverage | Stage 6 blocks merge. Branch protection enforces. | (existing) |
| 6 | New pub fn without test | Existing Gate 6 blocks push. | (existing) |
| 7 | Token in log/error output | Security test catches it. | E |
| 8 | `/health` called while QuestDB down | Reports tick_persistence: "buffering" | D |

---

## What Is NOT In Scope (And Why)

| Excluded Item | Reason |
|---------------|--------|
| 4 skipped REST endpoints (ltp/ohlc/quote/instrument) | WebSocket provides same data real-time; REST is redundant |
| Alertmanager deployment | Telegram fires directly on CRITICAL; Alertmanager adds complexity for single-user |
| Mutation testing in pre-push | Too slow (20+ min); CI weekly Monday run by design |
| Fuzz testing in pre-push | Too slow; CI weekly Monday run by design |
| Valkey exporter sidecar | QuestDB indirect metrics sufficient; add later if needed |
| Previous plan items (Sandbox mode, etc.) | Separate plan, already IN_PROGRESS; this plan covers testing/enforcement only |

---

## Total Items: 16 across 6 blocks
- **Block A** (Tick resilience): 3 items — HIGHEST priority
- **Block B** (Gate wiring): 2 items — FASTEST win
- **Block C** (API coverage test): 1 item
- **Block D** (Monitoring): 3 items
- **Block E** (Test hardening): 2 items
- **Block F** (Docs): 3 items
- **No new dependencies** — all using existing crates
- **Every item has specific test functions listed**
