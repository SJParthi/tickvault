# Wave 3 — Visibility & SLO (Items 10–13)

**Wave 3 PR target branch:** `claude/wave-3-visibility-slo` (from main after Wave 2 merge)
**Estimated LoC:** ~2,400
**Files touched:** ~25
**Merge precondition:** Waves 1+2 merged + all 4 items VERIFIED + `make 100pct-audit` returns exit 0

## Item 10 — Pre-open movers (09:00..=09:12 IST)

| Aspect | Value |
|---|---|
| Files | `crates/core/src/pipeline/preopen_movers.rs` (NEW), `crates/app/src/main.rs` |
| Change | From 09:00 to 09:12 IST, compute % change for every stock + index using pre-open buffer ticks vs `previous_close` (Item 4 dependency). Persist to `stock_movers` (with `phase = 'PREOPEN'` column added by Item 2). Snapshot every 30s. SENSEX (BSE) NOT in pre-open feed → mark `phase = 'PREOPEN_UNAVAILABLE'`, do NOT error. |
| Data source | `preopen_price_buffer` already captures NIFTY=13, BANKNIFTY=25 IDX_I + F&O stocks (per `live-market-feed-subscription.md` 2026-04-22 §10) |
| Tests | `test_preopen_movers_uses_buffer_ticks`, `test_preopen_movers_window_is_0900_to_0912_inclusive`, `test_preopen_movers_sensex_marked_unavailable_not_error`, `test_preopen_movers_persists_phase_column` |
| ErrorCode | `MOVERS-03` |
| Prom | `tv_preopen_movers_total`, `tv_preopen_movers_unavailable_total{symbol}` |
| Grafana | New `preopen-movers.json` dashboard |

---

## Item 11 — Telegram dispatcher hardening (NEW from agent 3 audit, G15)

**Source:** Background agent 3 (2026-04-27) verified ALL 4 gaps:
- No global rate-limit bucket
- No per-chat bucket
- No coalescing layer
- No `tv_telegram_dropped_total` counter

| Sub-item | Action |
|---|---|
| 11.1 | Add `governor` GCRA bucket: 30/sec global, 1/sec per chat, 20/min per chat (Telegram bot API limits). Wrap `send_telegram_chunk_with_retry`. |
| 11.2 | Add `TelegramCoalescer` — `papaya::HashMap<(EventKind, CoalesceKey), CoalesceState>`. Window = configurable per event kind (default 60s). Bursts within window → 1 summary message with count + top-10 samples. |
| 11.3 | Add Prom counters: `tv_telegram_sent_total{severity}`, `tv_telegram_dropped_total{reason="bucket_full"|"queue_full"|"coalesced"}`, `tv_telegram_coalesced_total{event_kind}`, `tv_telegram_429_retry_total` |
| 11.4 | Add bounded queue (capacity 1024) replacing unbounded `tokio::spawn` per event. Overflow → drop oldest with counter increment. |
| 11.5 | Coalesce-key spec per event kind: `TickGap` → `("tick_gap", per-60s-window)`, `WebSocketDisconnected` → `("ws_disc", feed)`, `DepthRebalanced` → `("depth_rebal", underlying)`, `Phase2*` → `("phase2", trading_date_ist)`. Generic spec: any event with `Severity::Critical` BYPASSES coalescer (always sent immediately). |

### Coalesce window matrix

| Event kind | Window | Key | Severity bypass |
|---|---|---|---|
| TickGap | 60s | global | no |
| WebSocketDisconnected (off-hours) | 300s | feed | no |
| WebSocketDisconnected (in-market) | none — bypass | — | yes |
| DepthRebalanced (Low) | 30s | underlying | no |
| DepthRebalanceFailed (High) | none — bypass | — | yes |
| QuestDB write failure | 60s | table | no |
| TokenForceRenewedOnWake | none — bypass | — | yes (rare event) |
| Critical (any) | NEVER | — | yes |

### Tests

| Test | Asserts |
|---|---|
| `test_telegram_global_bucket_30_per_sec` | Burst of 100 events in 1s → only 30 sent, 70 dropped |
| `test_telegram_per_chat_bucket_1_per_sec` | Same chat ID → only 1/sec |
| `test_telegram_coalescer_collapses_repeat_events` | 406 TickGap events in 60s → 1 summary message with count=406 |
| `test_telegram_critical_bypasses_coalescer` | Critical event sent within 100ms regardless of bucket |
| `test_telegram_dropped_counter_increments_on_overflow` | Bounded queue full → counter increments |
| `test_telegram_429_retry_with_backoff` | Existing 100→400→1600ms backoff preserved |
| `chaos_telegram_429_storm` | Simulated 429 storm → no cascading retries |

### 9-box

| Box | Value |
|---|---|
| Typed event | (none — this IS the dispatcher) |
| ErrorCode | `TELEGRAM-01` (drop), `TELEGRAM-02` (429 retry exhausted), `TELEGRAM-03` (bucket exceeded) |
| Prom | (above) |
| Grafana | New `telegram-health.json` panel set |
| Alert | `tv_telegram_dropped_total` rate > 0.1/sec for 60s → HIGH |

---

## Item 12 — `MarketOpenSelfTestPassed` at 09:16 IST

| Aspect | Value |
|---|---|
| Files | `crates/app/src/main.rs`, `crates/core/src/notification/events.rs` |
| Change | At 09:16:00 IST (after 09:13 anchor, 09:15:30 heartbeat, all expected ticks settled), run `MarketOpenSelfTest`: assert (a) heartbeat fired, (b) anchor fired per index ≥ 3, (c) code-6 prev-close received per IDX_I, (d) all 5 main-feed WS connected, (e) depth-20 + depth-200 streaming, (f) Phase2 emitted, (g) movers tables non-empty for trading-day partition. If ANY check fails → `MarketOpenSelfTestFailed{failed_checks: Vec<String>}` (Severity::Critical) → kill switch ACTIVATE (per G10). |
| Tests | `test_self_test_passes_when_all_checks_green`, `test_self_test_failed_activates_kill_switch`, 7 × `test_self_test_detects_<sub_check>_failure` |
| ErrorCode | `SELFTEST-01` (passed), `SELFTEST-02` (failed — Critical) |
| Prom | `tv_self_test_total{result="passed"|"failed"}`, `tv_self_test_subcheck_total{check, result}` |
| Audit | row in `selftest_audit` table (Item 9) |

---

## Item 13 — `tv_realtime_guarantee_score` SLO + `make 100pct-audit` + Grafana 100pct dashboard

| Sub-item | Action |
|---|---|
| 13.1 | Add Prometheus gauge `tv_realtime_guarantee_score` (0-100) computed every 30s from 12 sub-gauges (formula below) |
| 13.2 | Add 12 sub-gauges (one per dimension) |
| 13.3 | Create `scripts/100pct-audit.sh` (runtime version reads live Prom; CI version reads `data/snapshots/last-green.json`) |
| 13.4 | Add `Makefile` targets: `make 100pct-audit` (runtime), `make 100pct-audit-ci` (snapshot) |
| 13.5 | Create `deploy/docker/grafana/dashboards/100pct.json` with 1 stat panel (the score), 12 sub-stats, 1 timeseries (24h) |
| 13.6 | Add Prom alert `tv_realtime_guarantee_score < 100 for > 60s` → HIGH |

### Score formula

```
score = (
  100 *
  bool(tv_websocket_connections_active{kind="main"} == 5) *
  bool(tv_websocket_connections_active{kind="depth_20"} == 4) *
  bool(tv_websocket_connections_active{kind="depth_200"} == 2) *
  bool(tv_websocket_connections_active{kind="order_update"} == 1) *
  bool(tv_ticks_dropped_total == 0) *
  bool(tv_prev_close_persisted_total{source="CODE6"} >= 3) *
  bool(tv_prev_close_persisted_total{source="QUOTE_CLOSE"} >= 1) *
  bool(tv_prev_close_persisted_total{source="FULL_CLOSE"} >= 1) *
  bool(tv_phase2_outcome_total{result="complete"} >= 1) *
  bool(tv_market_open_depth_anchor_total >= 3) *
  bool(tv_questdb_write_errors_total == 0) *
  bool(tv_telegram_dropped_total == 0)
)
```

### `make 100pct-audit` exit-code semantics

| Source | When | Exit |
|---|---|---|
| Runtime (live Prom) | Operator daily, post-market | 0 if score == 100, non-zero with table report otherwise |
| Snapshot (`data/snapshots/last-green.json`) | CI pre-merge | 0 if last-green snapshot score == 100, fail otherwise. Snapshot updated only on `main` post-merge by green build |

### Tests

| Test | Asserts |
|---|---|
| `test_score_is_100_when_all_subgauges_pass` | Composite formula |
| `test_score_drops_to_0_when_any_subgauge_fails` | Multiplication semantics |
| `test_100pct_audit_exits_0_on_runtime_pass` | Shell script |
| `test_100pct_audit_ci_reads_snapshot` | CI mode |
| `test_grafana_100pct_dashboard_pinned` | Snapshot test of `100pct.json` |

### 9-box

| Box | Value |
|---|---|
| Typed event | `RealtimeGuaranteeScoreDropped{score, failing_dimensions: Vec<String>}` |
| ErrorCode | `SLO-01` (score dropped), `SLO-02` (snapshot stale) |
| Prom | `tv_realtime_guarantee_score`, 12 sub-gauges |
| Grafana | NEW `100pct.json` (operator's single-glance dashboard) |
| Alert | score < 100 for > 60s → HIGH |
| Triage | rules for `SLO-01/02` |
