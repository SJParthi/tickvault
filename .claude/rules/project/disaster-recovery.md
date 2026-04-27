# Disaster Recovery — F&O Subscription Boot Modes & Worst-Case Scenarios

> **Authority:** CLAUDE.md > this file.
> **Scope:** Every boot path, every failure mode, every recovery primitive
> for the F&O WebSocket subscription system.
> **Created:** 2026-04-25 (branch `claude/build-fno-universe-Tlb9d`).

## Purpose

Every disaster — fresh clone mid-market, Docker volume deleted, QuestDB lost,
all 5 WS connections dropped, token expired, network blip — MUST have a
deterministic recovery path that brings the F&O universe back to full
24,324-instrument subscription within ≤ 60 seconds.

This document is the single source of truth for those recovery paths.

## The 4 Boot Modes

| Mode | Time window IST | Spot source for stocks | RTO to full sub | Telegram event |
|---|---|---|---|---|
| **PreMarket** | 00:00 – 09:00 | Pre-open buffer (captures 09:00–09:12 closes) | At 09:13:00 | `Phase2Complete` |
| **MidPreMarket** | 09:00 – 09:13 | Partial pre-open buffer + REST `/marketfeed/ltp` at 09:12:55 | At 09:13:00 | `Phase2Complete` |
| **MidMarket** | 09:13 – 15:30 | **Live cash-equity ticks** (primary) → REST stragglers → QuestDB previous close | ~30 seconds | `MidMarketBootComplete` |
| **PostMarket** | 15:30 – next 09:00 | QuestDB previous close (no live data expected) | At boot | (no event) |

Detected by pure function `detect_boot_mode(now_ist_secs_of_day)` in
`crates/core/src/instrument/boot_mode.rs`. 21 unit tests cover every minute
boundary including the 4 transition points.

## Source Priority Hierarchy (cold start)

| State | Primary | Fallback 1 | Fallback 2 | Fallback 3 |
|---|---|---|---|---|
| Instrument master | rkyv binary cache (disk, 10ms load) | QuestDB `derivative_contracts` | Dhan CSV download (60s) | S3 backup |
| Auth token | Valkey cache | AWS SSM | TOTP regeneration | HALT + Telegram |
| Stock spot price (Mode A/B) | Pre-open buffer | REST `/marketfeed/ltp` | QuestDB previous close | Skip stock |
| Stock spot price (Mode C) | **Live cash-equity tick** | REST `/marketfeed/ltp` | QuestDB previous close | Skip stock |
| Index spot price | Live IDX_I WS tick | REST `/marketfeed/ltp` | Pre-open buffer (09:00–09:12 only) | QuestDB previous close |
| Universe build status | Valkey cache | QuestDB `instrument_build_metadata` | Force rebuild from CSV | — |

## Disaster Scenarios Catalogue (11 covered)

### 1. Fresh clone + Docker fresh + boot at 11:30 IST (the canonical worst case)

| Step | Action | Wall-clock |
|---|---|---|
| Detection | `detect_boot_mode()` returns `MidMarket` | T+0s |
| Universe load | rkyv binary cache (disk) → 10ms | T+1s |
| Auth | Valkey miss → SSM read → JWT cache → 2s | T+3s |
| QuestDB DDL | Idempotent `CREATE TABLE IF NOT EXISTS` + `ALTER ADD COLUMN IF NOT EXISTS` | T+5s |
| Boot subscribe | IDX_I (3) + display (26) + cash equities (216) + index F&O full chain (2,037) = 2,282 instruments | T+8s |
| Live tick warmup | First IDX_I + cash equity ticks arrive | T+10s |
| **Live-tick ATM resolver** | Polls `SharedSpotPrices` at 5/10/15/20/25s; exits early when all 216 stocks resolved | T+15s — T+30s |
| REST straggler fallback | For stocks still missing → `/v2/marketfeed/ltp` (1 batch, 200ms) | T+27s |
| QuestDB last-resort | For stocks REST also missed → previous close | T+28s |
| Stock F&O subscribe | 22,042 instruments in 221 batches (max 100/msg per WS-GAP-02) | T+30s |
| Telegram | `MidMarketBootComplete { live_tick: 214, rest: 1, quest_db: 1, skipped: 0, total: 24310 }` | T+30s |
| **Result** | Fully subscribed 24,310 instruments | **~30s** |

### 2. Docker deleted mid-market, restart

Same recovery path as #1. The rkyv binary cache survives if the data volume
is intact; if not, fall through to CSV download (~60s adds 30s to RTO).

### 3. QuestDB lost (volume deleted)

| Step | Action |
|---|---|
| Detection | DDL execution against empty schema succeeds (no rows) |
| Universe rebuild | CSV downloader runs: download (~5s) + parse + persist + cache write (~30s) |
| Subscribe | Same as #1 |
| **RTO** | ~60s |

### 4. Valkey lost

| Step | Action |
|---|---|
| Detection | Token cache miss on read |
| Recovery | Falls through to AWS SSM (already-fresh token) |
| **Impact** | Zero — trading continues, cache repopulates on next renewal |

### 5. Single WS connection drops mid-day (REWRITTEN — Wave 2 Item 5/6)

| Step | Action |
|---|---|
| Detection | Pool watchdog (5s tick) sees state ≠ Connected |
| Recovery | Reconnect with `SubscribeRxGuard` (PR #337). **Wave 2:** if 3 consecutive failures occur post-15:30 IST, the per-connection task does NOT exit — it sleeps until the next NSE market open via `TradingCalendar::secs_until_next_market_open` (WS-GAP-04). Was a `return false` → process-restart-required in the legacy code. |
| **RTO** | ~5–10s in-market; up to ~65h overnight (Fri 16:00 → Mon 09:00 sleep). |

### 6. All 5 WS connections drop (network blip / RST flood) (REWRITTEN — Wave 2 Item 5/6)

| Step | Action |
|---|---|
| Detection | Watchdog sees `active_count = 0` → Telegram CRITICAL |
| Recovery | All 5 reconnect in parallel; `SubscribeRxGuard` restores subscriptions. **Wave 2:** if all reconnects exhaust the post-close gate, pool stays alive in dormant sleep instead of giving up. Pool supervisor (`respawn_dead_connections_loop`) re-spawns any panicked task within 5s (WS-GAP-05). |
| Spot freshness | If buffer stale (Mode C), live-tick resolver re-runs to confirm ATM |
| **RTO** | ~30s in-market; pool re-converges at next market open if event happens after 15:30 IST. |

### 7. Auth token expired mid-market (DH-901 / DataAPI-807) (REWRITTEN — Wave 2 Item 5.4)

| Step | Action |
|---|---|
| Detection | `DisconnectCode::AccessTokenExpired` (807) routed by Dhan |
| Recovery | Token refresh (Valkey → SSM → TOTP) → reconnect with fresh JWT (AUTH-GAP-02). **Wave 2:** on wake-from-sleep, `TokenManager::force_renewal_if_stale(threshold_secs = 14400)` proactively renews if token has < 4h validity, BEFORE the post-sleep reconnect attempt (AUTH-GAP-03). Prevents the legacy "wake → reconnect → 807 → token refresh → reconnect → success" 30-second cascade. |
| **RTO** | ~15s in-market; ~5s on wake (token already fresh). |

### 8. Network blip (RST flood like 2026-04-24 incident) (REWRITTEN — Wave 2 Item 5)

| Step | Action |
|---|---|
| Detection | Connection read-loop `Err` + watchdog 5s tick |
| Recovery | Existing exponential backoff + `SubscribeRxGuard` preserves the subscription channel across reconnects. **Wave 2:** post-close streak no longer terminates the task — it transitions to sleep mode with `tv_ws_post_close_sleep_total{feed="main"}` increment + `WebSocketSleepEntered` notification (Severity::Low). |
| **RTO** | ~10–30s in-market. |

### 12. Boot-time QuestDB readiness race (NEW — Wave 2 Item 7)

| Step | Action |
|---|---|
| Detection | Boot probe `wait_for_questdb_ready(BOOT_DEADLINE_SECS=60)` polls QuestDB `/exec` endpoint with `SELECT 1`. Escalating logs: DEBUG @5s, INFO @10s, WARN @20s, ERROR `BOOT-01` @30s (with Telegram alert), CRITICAL `BOOT-02` @60s (HALT). |
| Recovery | If QuestDB green within 60s → boot continues. If not → app halts. Operator runs `make doctor` + `docker ps` to fix the underlying issue, then restarts. The rescue ring buffers ticks in the meantime. |
| **RTO** | ~10s warm path (QuestDB already up); ~60s on cold start; HALT after 60s. |

### 13. Synthetic / regulatory audit reconstruction (NEW — Wave 2 Item 9)

| Step | Action |
|---|---|
| Trigger | "Why was BANKNIFTY 47000 swapped to 47200 at 11:23:45 IST on 2026-05-15?" |
| Recovery | Query `depth_rebalance_audit` for ts range. Query `phase2_audit` for that day's 09:13 outcome. Query `ws_reconnect_audit` for any churn around the swap. Query `boot_audit` for that day's boot timeline. Query `selftest_audit` for `make doctor` history. |
| SEBI retention | All 6 audit tables retained 90d hot (QuestDB) → S3 IT (90–365d) → Glacier Deep Archive (≥1y up to 5y). `order_audit` is the strict 5y SEBI-mandate table. |
| **Cost** | ~₹333/mo for up to 500GB cold archive (covered by `aws-budget.md`). |

### 9. Dhan REST `/marketfeed/ltp` returns 805 (too many connections)

| Step | Action |
|---|---|
| Detection | Per-call HTTP status check |
| Recovery | 60s STOP_ALL pause per `dhan-annexure-enums` rule 12 → retry once → fall back to QuestDB previous close |
| **RTO** | ~60s |

### 10. F&O stock has no LTP anywhere (REST empty + no QuestDB history)

| Step | Action |
|---|---|
| Detection | `Phase2Failed` event with diagnostic when plan is empty |
| Recovery | Skip that stock for the day, log + Telegram, continue with rest |
| **Impact** | Continuous — no global halt, just one stock missing |

### 11. Capacity overflow (computed plan > 25,000)

| Step | Action |
|---|---|
| Detection | Pre-subscribe assertion `total ≤ MAX_TOTAL_SUBSCRIPTIONS` (= 25,000) |
| Recovery | Build fails fast, Telegram CRITICAL, operator manual intervention |
| Warning threshold | `MAX_TOTAL_SUBSCRIPTIONS_TARGET = 24_500` — Telegram WARN before hitting hard cap |

## Idempotency Guarantees

| Guarantee | Mechanism |
|---|---|
| Re-subscribing same security_id on same conn | Dhan ignores duplicates |
| Tick dedup across restarts | QuestDB DEDUP key `(security_id, exchange_segment, ts, sequence_number)` (STORAGE-GAP-01) |
| Order dedup | Valkey UUID v4 idempotency key (OMS-GAP-05) |
| Universe persist | DEDUP `(security_id, underlying_symbol, exchange_segment)` (I-P1-05) |
| Boot mode detection | Pure function of `now()` — same time → same mode every time |
| ATM computation | Pure function `(spot, sorted_strikes) → strike` — deterministic |
| Capacity hard cap | Compile/boot-time assertion → fails fast on regression |

## Real-Time Observability Checks

| Check | Frequency | Action on Fail |
|---|---|---|
| Active WS connections gauge | 5s (pool watchdog) | Telegram alert if `active < 5` |
| Capacity utilization gauge | Once at Phase 2 dispatch | Telegram WARN if `total > 24_500` |
| `tv_instrument_registry_cross_segment_collisions` | At boot + on rebuild | Existing telemetry |
| Subscription audit log | Every subscribe message | QuestDB `subscription_audit_log` |
| Live tick freshness per stock | 5s during Mode C resolver | Mark straggler if no tick in 25s |
| `/health` endpoint counters | On-demand | Verifies main_feed_active, depth_active |
| Phase 2 outcome | Once per trading day | `Phase2Complete` / `Phase2Failed` Telegram |
| Mid-market boot outcome | Once per Mode C boot | `MidMarketBootComplete` Telegram |

## What This Document Does NOT Cover

- Order management failures (see `OMS-GAP-*`)
- Risk engine halts (see `RISK-GAP-*`)
- Greeks pipeline failures (separate runbook)
- Strategy-layer failures (out of scope — strategies are dry-run)

## Cross-Refs

- `live-market-feed-subscription.md` — subscription scope details
- `depth-subscription.md` — depth-specific recovery (NIFTY + BANKNIFTY only)
- `security-id-uniqueness.md` — composite-key invariant (I-P1-11)
- `audit-findings-2026-04-17.md` — historical recovery lessons
- `observability-architecture.md` — error classification + Telegram routing

## Trigger

This rule auto-loads when editing:
- `crates/core/src/instrument/boot_mode.rs`
- `crates/core/src/instrument/live_tick_atm_resolver.rs`
- `crates/core/src/instrument/subscription_planner.rs`
- `crates/app/src/main.rs` (boot sequence)
- Any file containing `BootMode`, `MidMarketBootComplete`, `detect_boot_mode`,
  `resolve_stock_atm_from_live_ticks`, `MAX_TOTAL_SUBSCRIPTIONS_TARGET`,
  `STOCK_OPTION_ATM_STRIKES_EACH_SIDE`
