# Real-time Check / Guarantee / Proof Matrix

> Every operational concern, with the live check, the guarantee policy, and the proof artifact (test or alert) that pins it.

## Master matrix

| Concern | Real-time check | Guarantee | Proof artifact |
|---|---|---|---|
| 5/5 main-feed WS connections active | Pool watchdog 5s tick → `tv_websocket_connections_active{kind="main"}` | Alert if `< 5 for > 30s` → CRITICAL Telegram | `health_counter_fix7_guard.rs` |
| 4/4 depth-20 + 2/2 depth-200 + 1/1 order-update | Same per-pool gauge | Same alert per pool | `resilience_sla_alert_guard.rs` (extended for all 4 pools) |
| Zero ticks dropped | `tv_ticks_dropped_total` (counter) — must stay 0 | Alert any-increase → CRITICAL | `chaos_zero_tick_loss.rs` |
| Phase2 fires once per trading day | `tv_phase2_outcome_total{result}` (sum >= 1 by 09:14 IST) | Alert if `0 by 09:14 IST` → CRITICAL | `phase2_notify_guard.rs` (Item 1, G7) |
| Heartbeat fires at 09:15:30 | `tv_market_open_streaming_confirmation_total` | Alert if missing by 09:16 → CRITICAL | new ratchet |
| Depth anchor per index at 09:13:00 | `tv_market_open_depth_anchor_total{symbol}` (3 distinct values) | Alert if `< 3 by 09:14` → CRITICAL | new ratchet |
| Prev-close populated for all 3 segments | `tv_prev_close_persisted_total{source}` (≥ 3 distinct sources by 09:30) | Alert if any source missing by 10:00 → HIGH | new ratchet (Item 4) |
| All-checks-passed self-test | `tv_realtime_guarantee_score` gauge (composite 0–100) | Alert `< 100 for > 60s` → HIGH | new SLO + `make 100pct-audit` (Item 13) |
| Clock skew ≤ 2s | `tv_clock_skew_seconds` boot probe | CRITICAL on > 2s + HALT | new boot check (Item 7, G8) |
| Token > 4h to expiry pre-market | `tv_token_validity_remaining_seconds` | CRITICAL `< 14400 at 08:45 IST` | existing AUTH-GAP-01 |
| Token force-renewed on WS wake | `tv_token_force_renewal_total{trigger="ws_wake"}` | INFO event in Telegram + audit row | new ratchet (Item 5, G1) |
| QuestDB write-to-readback p99 ≤ 2s | `tv_questdb_writeback_lag_p99_seconds` (synthetic probe every 30s) | Alert > 2s → HIGH | new ratchet (Item 3, G9) |
| Telegram bucket not dropping | `tv_telegram_dropped_total` (must stay 0) | Alert any-increase → HIGH | new (Item 11, G15) |
| Telegram coalescer working | `tv_telegram_coalesced_total{event_kind}` > 0 during burst tests | Chaos test pins behavior | `chaos_telegram_429_storm.rs` |
| Tick-gap p99 ≤ 30s | `tv_tick_gap_seconds_p99` | Alert > 30s → MEDIUM (coalesced) | Item 8 |
| Tick-gap detector map size bounded | `tv_tick_gap_map_entries` gauge ≤ 25K | Reset at 15:35 IST asserted | new ratchet (G19) |
| Hot-path papaya lookup ≤ 50ns | Criterion bench in CI | bench-gate 5% regression fail | `quality/benchmark-budgets.toml` |
| Zero allocation on tick path | DHAT in CI: `total_blocks == 0` | Hard fail on regression | `dhat_tick_*.rs` (4 new) |
| WS sleep entered/resumed (off-hours) | `tv_ws_post_close_sleep_total{feed}` + typed events `WebSocketSleepEntered/Resumed` | INFO event; no spam | new ratchet (Item 5/6) |
| Pool supervisor respawn rate | `tv_ws_pool_respawn_total{feed}` | Alert > 1/min for 5min → HIGH | new ratchet |
| FAST BOOT QuestDB ready latency | `tv_boot_questdb_ready_seconds` histogram | CRITICAL at +30s; HALT at +60s | new ratchet (Item 7) |
| Self-test outcome at 09:16 IST | `tv_self_test_total{result}` | Failed → kill switch ACTIVATE (Item 12, G10) | `selftest_audit` row |
| 6 audit tables receiving rows | `tv_audit_persisted_total{table}` per table | Alert any table 0 rows for > 1h during market hours → HIGH | new ratchet (Item 9) |
| ILP flush interval ≤ 1000ms | Boot-time config assertion | Boot fails fast on misconfiguration | new (G9) |

## Daily proof script — `make doctor`

`make doctor` runs 7 sections, each PASS/FAIL with citation:

| Section | Check | Citation file |
|---|---|---|
| 1. WebSocket | All 4 pools at expected counts (5/4/2/1) | `crates/api/src/handlers/health.rs` |
| 2. Subscription | `select count(*) from instrument_master` ≈ 24K | `tickvault-logs MCP::questdb_sql` |
| 3. Tick flow | `tv_ticks_processed_total` rate > 0 in last 5min during market | Prom |
| 4. Persistence | All 9 new tables have rows in last 24h | `tickvault-logs MCP::questdb_sql` |
| 5. Telegram | `tv_telegram_dropped_total == 0` | Prom |
| 6. SLO | `tv_realtime_guarantee_score == 100` | Prom |
| 7. Auto-triage | No novel signatures in last hour | `tickvault-logs MCP::list_novel_signatures` |

## Continuous proof — Prometheus alerting tree

```
ANY alert → Alertmanager → Telegram (Critical/High → SMS)
                        ↘ AWS SNS (Critical → SMS to operator + escalation)
```

Alert categories (post-Wave-3):

| Priority | Trigger | Action |
|---|---|---|
| CRITICAL | `tv_realtime_guarantee_score < 100 for > 60s` | Telegram + SMS + page |
| CRITICAL | `tv_ticks_dropped_total` rate > 0 | Telegram + SMS + page |
| CRITICAL | `tv_websocket_connections_active{kind="main"} < 5 for > 30s` | Telegram + SMS |
| CRITICAL | `tv_self_test_total{result="failed"} > 0` | Telegram + SMS + kill switch ACTIVATE |
| CRITICAL | `tv_clock_skew_seconds > 2` (boot) | HALT |
| HIGH | `tv_telegram_dropped_total` rate > 0.1/sec for 60s | Telegram |
| HIGH | `tv_ws_pool_respawn_total` rate > 1/min for 5min | Telegram |
| HIGH | `tv_questdb_writeback_lag_p99_seconds > 2` | Telegram |
| HIGH | `tv_audit_persisted_total{table=*}` 0 for 1h during market | Telegram |
| MEDIUM | `tv_tick_gap_seconds_p99 > 30` | Telegram (coalesced, 60s window) |
| LOW | `tv_ws_post_close_sleep_total{feed}` increment (off-hours, info-only) | Telegram |

## Continuous proof — Grafana dashboards (post-Wave-3)

| Dashboard | Purpose |
|---|---|
| `100pct.json` | The score + 12 sub-stats + 24h timeseries (NEW Item 13) |
| `operator-health.json` | Existing — extended with hot-path I/O panel |
| `market-data.json` | Existing — extended with prev-close coverage panel |
| `audit-trails.json` | NEW (Item 9) — 6 panels, one per audit table |
| `telegram-health.json` | NEW (Item 11) — bucket utilization, coalesce rate, drop rate |
| `preopen-movers.json` | NEW (Item 10) |
| `depth-flow.json` | Existing — unchanged |

## Real-time guarantee statement (operator-facing)

> "tickvault provides real-time market data acquisition, persistence, and visibility for the configured F&O universe (~24,324 instruments) with the following hard guarantees, each pinned by an automated test:
>
> 1. **Zero tick loss** during market hours (chaos-tested under WS drop, QuestDB pause, ring overflow)
> 2. **All WebSocket pools auto-reconnect** until next market open (no manual restart for off-hours blips)
> 3. **Token auto-refresh on wake** for sleeps > 24h
> 4. **Hot-path latency O(1)** with Criterion-pinned budgets
> 5. **Composite-key uniqueness** `(security_id, exchange_segment)` everywhere
> 6. **Idempotent persistence** via QuestDB DEDUP UPSERT KEYS
> 7. **Telegram delivery** — bucketed, coalesced, never drops Critical
> 8. **Self-test at 09:16 IST** — kills switch on failure
> 9. **Single-glance SLO score** via `tv_realtime_guarantee_score == 100`
> 10. **Zero-touch automation** for known errors via `.claude/triage/`"
>
> Any guarantee that doesn't have a citation in this matrix is not allowed. Updates require updating this file FIRST.
