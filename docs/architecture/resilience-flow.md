# Resilience Flow — Zero Human Intervention

> **Last updated:** 2026-03-22
> **Context:** This document describes how dhan-live-trader survives every crash
> scenario automatically. From boot to market close, no human intervention is needed.

---

## 1. Morning Boot Sequence (What Happens When You Start)

```
┌─────────────────────────────────────────────────────────────┐
│                    APP START (cargo run or systemd)          │
└────────────────────────────┬────────────────────────────────┘
                             │
                    ┌────────▼────────┐
                    │ CryptoProvider   │  ← Must be first (TLS)
                    │ Config Load      │  ← base.toml + local.toml
                    │ Observability    │  ← Prometheus metrics init
                    │ Logging Setup    │  ← tracing + file writer
                    └────────┬────────┘
                             │
               ┌─────────────▼──────────────┐
               │  Is it market hours (9-16)  │
               │  AND valid token cache?     │
               └──────┬──────────────┬──────┘
                      │ YES          │ NO
              ┌───────▼───────┐  ┌───▼───────────────┐
              │  FAST BOOT    │  │  SLOW BOOT        │
              │  (~400ms)     │  │  (~5-30 seconds)   │
              └───────┬───────┘  └───┬───────────────┘
                      │              │
                      │         Full sequential:
                      │         SSM → TOTP → JWT →
                      │         CSV download → parse →
                      │         QuestDB DDL → ...
                      │              │
              ┌───────▼──────────────▼──────┐
              │  Token loaded (cache or SSM) │
              │  Instruments loaded (rkyv)   │
              │  WebSocket pool created      │
              └───────────────┬─────────────┘
                              │
              ┌───────────────▼─────────────┐
              │  5 WebSocket connections      │
              │  (staggered 10s apart)        │
              │  Each subscribes 100 inst/msg │
              └───────────────┬─────────────┘
                              │
              ┌───────────────▼─────────────┐
              │  ✅ FIRST TICK RECEIVED      │
              │  Pipeline: parse → dedup →   │
              │  validate → broadcast →      │
              │  QuestDB persist             │
              └─────────────────────────────┘

    Background (non-blocking):
    ├── Docker infra check + QuestDB DDL
    ├── Token renewal scheduler (23h cycle)
    ├── Historical candle fetch (cold path)
    ├── IP monitoring (every 5 min)
    └── Notification service init
```

### Fast Boot Path (Market Hours + Valid Cache)
| Step | Action | Time |
|------|--------|------|
| 1 | CryptoProvider + Config | ~1ms |
| 2 | Observability + Logging | ~5ms |
| 3 | Token cache load | ~2ms |
| 4 | Instrument rkyv cache load | ~5-15ms |
| 5 | WebSocket pool create + connect | ~400ms |
| 6 | **First tick received** | **~500ms total** |
| 7+ | Background: QuestDB DDL, SSM, renewal | Non-blocking |

### Slow Boot Path (Pre-Market / No Cache)
| Step | Action | Time |
|------|--------|------|
| 1-2 | Config + Logging | ~5ms |
| 3 | Docker infra ensure | 0-90s |
| 4 | AWS SSM → TOTP → JWT | ~2-5s |
| 5 | Instrument CSV download + parse | ~3-10s |
| 6 | QuestDB DDL | ~1-2s |
| 7 | WebSocket connect | ~400ms |
| 8 | **First tick received** | **~10-30s total** |

---

## 2. Normal Operation (What Runs During the Day)

```
┌──────────────────────────────────────────────────────────┐
│                  NORMAL TRADING DAY                        │
│                                                            │
│  ┌─────────┐    ┌──────────┐    ┌───────────┐            │
│  │ 5 x WS  │───▶│ Pipeline │───▶│ QuestDB   │            │
│  │ Conns   │    │ Parse +  │    │ Tick Store │            │
│  │ (Dhan)  │    │ Dedup    │    │           │            │
│  └─────────┘    └────┬─────┘    └───────────┘            │
│                      │                                     │
│                      ▼                                     │
│              ┌──────────────┐                              │
│              │ Broadcast    │                              │
│              │ (65K SPSC)   │                              │
│              └──┬────┬──┬──┘                              │
│                 │    │  │                                   │
│    ┌────────────▼┐ ┌▼──▼────────┐  ┌───────────────┐     │
│    │ Candle     │ │ Tick        │  │ Risk Engine   │     │
│    │ Aggregator │ │ Persistence │  │ Gap Tracker   │     │
│    └────────────┘ │ (+ Buffer)  │  └───────────────┘     │
│                   └─────────────┘                          │
│                                                            │
│  Monitoring (continuous):                                  │
│  ├── Prometheus scrapes every 15s                          │
│  ├── Tick gap detection per-tick (30s warn, 120s error)   │
│  ├── Token renewal at 23h mark                             │
│  ├── IP verification every 5 min                           │
│  └── Grafana dashboards auto-refresh 5s                    │
│                                                            │
│  Market Close (15:30 IST):                                 │
│  ├── WebSocket connections gracefully closed                │
│  ├── Historical candle re-fetch + cross-verify             │
│  ├── Telegram: "Market close — pipeline stopped"           │
│  └── App shutdown at 16:00 IST                             │
└──────────────────────────────────────────────────────────┘
```

---

## 3. Crash Scenarios — What Happens & How It Recovers

### Scenario 1: WebSocket Disconnects (Network / Dhan Server)

```
WS Disconnect
    │
    ▼
ConnectionState → Reconnecting
    │
    ▼
Exponential Backoff: 500ms → 1s → 2s → 4s → ... → 60s cap
    │
    ├─── reconnect_max_attempts = 0 (INFINITE during market hours)
    │    Never gives up. CRITICAL alert every 10 failures.
    │
    ├─── Non-reconnectable code (808, 809, 810)?
    │    → Stop + CRITICAL alert
    │
    ├─── Token expired (807)?
    │    → Wait for token renewal (poll 5s, max 60s) → reconnect
    │
    └─── Success?
         → Resubscribe (cached messages) → Resume ticks
         → Log: "WebSocket reconnected — gap may exist"
         → Post-market historical fetch backfills gap
```

**Detection:** Immediate (WS error)
**Recovery:** Automatic, 500ms-60s per attempt, infinite retries
**Data Impact:** Ticks missed during disconnect. Gap filled post-market.
**Alert:** Telegram after 10 consecutive failures

### Scenario 2: QuestDB Crashes

```
QuestDB Down
    │
    ▼
Tick write fails → sender = None
    │
    ▼
┌─────────────────────────────────┐
│  RING BUFFER activated          │
│  Capacity: 300K ticks (~5 min)  │
│  New ticks → push to buffer    │
│  (no data loss)                 │
└────────────┬────────────────────┘
             │
    Every append_tick():
    Try reconnect (1s → 2s → 4s, 3 attempts)
             │
             ├─── QuestDB still down?
             │    → Keep buffering (up to 5 min)
             │    → If buffer full: drop oldest, ERROR alert
             │
             └─── QuestDB back?
                  → Drain buffer (oldest-first, batched)
                  → Set recovery_flag = true
                  → Spawn gap integrity check (SAMPLE BY 1m)
                  → Resume normal writes
```

**Detection:** Immediate (write failure)
**Recovery:** Automatic reconnect + buffer drain
**Data Impact:** Zero loss for outages < 5 min. Oldest ticks dropped if > 5 min.
**Alert:** ERROR on storage failure, CRITICAL if buffer overflows

### Scenario 3: App Process Crashes

```
App Crash (OOM / panic / SIGKILL)
    │
    ▼
systemd detects: no WATCHDOG ping for 60s
  OR process exit detected immediately
    │
    ▼
systemd: Restart=always, RestartSec=3
    │
    ▼
App restarts (3 seconds later)
    │
    ▼
FAST BOOT (market hours + token cache):
    Token from data/cache/dlt-token-cache (~2ms)
    Instruments from rkyv binary cache (~5ms)
    WebSocket reconnect (~400ms)
    │
    ▼
✅ Back online in ~3.5 seconds
    Post-market fetch fills any gap
```

**Detection:** systemd watchdog (max 60s) or immediate on exit
**Recovery:** Auto-restart in 3s + fast boot ~500ms
**Data Impact:** ~4s of ticks missed. Gap filled post-market.
**Alert:** Prometheus `ApplicationDown` after 1 min

### Scenario 4: Docker Daemon Crash

```
Docker Daemon Crash
    │
    ▼
ALL 8 containers stop
    │
    ▼
Docker auto-restarts (OS-level service)
    │
    ▼
Containers restart: restart: unless-stopped
    QuestDB: ~15s to healthy
    Valkey: ~5s
    Prometheus: ~10s
    Grafana: ~15s (depends on Loki)
    Loki: ~10s (/ready healthcheck)
    Alloy: ~10s (depends on Loki)
    │
    ▼
App (systemd) detects QuestDB healthy
    WebSocket reconnects (infinite retries)
    Ring buffer holds ticks during QuestDB startup
    │
    ▼
✅ Full system recovered
```

**Detection:** Prometheus `TargetDown` on all services (2 min)
**Recovery:** Docker auto-restart all containers
**Data Impact:** Ticks buffered in ring buffer during restart
**Alert:** ServiceFlapping alert + individual TargetDown alerts

### Scenario 5: Token Expires Mid-Session

```
Token expires (24h JWT)
    │
    ▼
Dhan sends disconnect code 807 (AccessTokenExpired)
    │
    ▼
WebSocket layer: wait_for_valid_token()
    Polls every 5s, max 60s
    │
    ▼
Token renewal task (background):
    arc-swap atomic swap of new token
    │
    ▼
WebSocket reconnects with fresh token
    │
    ▼
✅ Sub-second interruption
```

**Detection:** Disconnect code 807 (immediate)
**Recovery:** Automatic token renewal + reconnect
**Data Impact:** Sub-second, negligible
**Alert:** Telegram on renewal success/failure

### Scenario 6: Full Network Outage

```
Network dies (ISP / AWS)
    │
    ▼
All WS connections: ReadTimeout (40s)
    │
    ▼
Reconnect loop: infinite retries, 60s cap
    Each attempt fails (no network)
    │
    ├── Ticks: not arriving (no WS)
    ├── QuestDB: local, still works (Docker network)
    ├── Ring buffer: empty (no ticks to buffer)
    │
    ▼
Network returns
    │
    ▼
Next reconnect attempt succeeds
    Subscriptions restored
    │
    ▼
✅ Back online. Post-market fetch fills gap.
```

**Detection:** WS timeout (40s) + reconnect failures
**Recovery:** Automatic when network returns
**Data Impact:** All ticks during outage missed. Gap filled post-market.
**Alert:** CRITICAL after 10 failures (if Telegram reachable via mobile data)

---

## 4. Alert Paths — How Failures Reach Your Phone

```
┌─────────────────────────────────────────────────────────┐
│                    THREE ALERT PATHS                      │
│                                                           │
│  Path 1: Direct Telegram (fastest, <1s)                  │
│  ─────────────────────────────────────                   │
│  Rust app → NotificationService → HTTP POST → Telegram   │
│  Events: AuthFailed, TokenRenewalFailed, WS Disconnected │
│          InstrumentBuildFailed, RiskHalt, IpMismatch     │
│                                                           │
│  Path 2: Prometheus → Grafana → Telegram (1-5 min)      │
│  ─────────────────────────────────────────────           │
│  App metrics → Prometheus scrape → Alert rules →          │
│  Grafana unified alerting → dlt-telegram contact point   │
│  Alerts: TargetDown, AppDown, PipelineStopped,           │
│          NoTicks, HighParseErrors, ServiceFlapping        │
│                                                           │
│  Path 3: Loki ERROR logs → Grafana → Telegram (1-3 min) │
│  ─────────────────────────────────────────────           │
│  error!() → file → Alloy → Loki → Grafana alert rule →  │
│  dlt-telegram contact point                               │
│  Covers: tick gaps, QuestDB failures, panics, any ERROR  │
└─────────────────────────────────────────────────────────┘
```

### Alert Timing Summary

| Failure | Path | Time to Alert |
|---------|------|---------------|
| App crash | Path 2 (Prometheus) | ~1 min |
| WS disconnect | Path 1 (Direct) | <1s |
| Token failure | Path 1 (Direct) | <1s |
| Tick gap >120s | Path 3 (Loki ERROR) | ~2 min |
| QuestDB write fail | Path 2 (Prometheus) | ~3 min |
| Container restart | Path 2 (Prometheus) | ~2-5 min |
| IP mismatch | Path 1 (Direct) | <1s |
| Parse errors | Path 2 (Prometheus) | ~3 min |
| Full network outage | Path 1 (if mobile) | <1s |

---

## 5. Data Protection Layers

```
Layer 1: SPSC Channel (65K capacity)
    WS → Pipeline → Broadcast
    Prevents: slow consumer blocking WS reads

Layer 2: Ring Buffer (300K ticks, ~5 min)
    Pipeline → Buffer → QuestDB
    Prevents: tick loss during QuestDB outages

Layer 3: QuestDB DEDUP UPSERT
    (ts, security_id, segment) dedup key
    Prevents: duplicate ticks on WS reconnect

Layer 4: Post-Market Historical Backfill
    Dhan REST API → 1-min candles → QuestDB
    Fills: any gaps from WS disconnects

Layer 5: Gap Integrity Check
    After QuestDB recovery: SAMPLE BY 1m query
    Detects: silent data holes → ERROR alert
```

---

## 6. Pre-Market Checklist (Automated)

The app performs these checks at boot (no manual steps needed):

1. **Token validity** — cache or fresh from SSM
2. **Data plan active** — `dataPlan == "Active"` from Dhan profile
3. **Derivative segment** — `activeSegment` contains "Derivative"
4. **Token expiry** — > 4 hours remaining
5. **Static IP** — verified against Dhan API
6. **Instrument master** — downloaded + validated (row count > 0)
7. **QuestDB tables** — idempotent DDL (CREATE IF NOT EXISTS)
8. **Docker services** — auto-started if not running

If any check fails → CRITICAL Telegram alert + HALT.

---

## 7. Summary: What Can Go Wrong Tomorrow

| Scenario | Automated? | Your Action |
|----------|------------|-------------|
| Normal boot + trading | ✅ Fully automated | None |
| WS disconnects | ✅ Infinite reconnect | Check Telegram |
| QuestDB crashes | ✅ Ring buffer + reconnect | Check Telegram |
| App crashes | ✅ systemd restart | Check Telegram |
| Token expires | ✅ Auto-renewal | None |
| Network outage | ✅ Auto-reconnect | Check Telegram |
| Docker restarts | ✅ unless-stopped | Check Telegram |
| Data gaps | ✅ Post-market backfill | Check Grafana after close |

**Bottom line:** Start the app, go to the gym. Everything is automated.
Your only job is to check Telegram if it buzzes.
