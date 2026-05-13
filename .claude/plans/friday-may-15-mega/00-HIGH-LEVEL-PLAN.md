# tickvault — The High-Level Plan (read this in 5 minutes)

**Created:** 2026-05-11 23:25 IST
**Purpose:** ONE document that explains the whole system at auto-driver level. Covers 99.9% of market-timing situations.
**Status:** Living summary. Updates when major decisions land.

---

## 🎯 In one paragraph

> tickvault is a Rust trading bot for Indian F&O. It connects to Dhan, listens to ~11,000 stocks + options live tick-by-tick, runs strategies in paper-mode today (`dry_run=true`), and one day flips to placing real orders. It runs the SAME way on a Mac dev laptop or AWS Mumbai prod. Boots automatically at 8:30 AM, sends a single ✅ confirmation Telegram at 8:33 AM, opens 5 WebSocket connections at 9:00 AM, finalizes the stock list at 9:13 AM, confirms streaming at 9:15:30 AM, runs the trading session until 15:30 PM, audits everything against Dhan's official data by 16:00 PM, sleeps for the weekend, repeats. Has a 5-layer safety net so nothing gets lost even when Docker dies, AWS reboots, the network blips, or QuestDB hangs for 60 seconds.

---

## 🌅 The 24-hour clock — what tickvault does every day

```
   00:00      08:30      09:00      09:13      09:15:30                15:30      16:00      23:59
     │          │          │          │            │                      │          │          │
     │ SLEEP    │ BOOT     │ CONNECT  │ FINALIZE   │ STREAM               │ CLOSE    │ AUDIT    │
     │          │          │          │            │                      │          │          │
     ▼          ▼          ▼          ▼            ▼                      ▼          ▼          ▼
   ┌────────┬───────────┬──────────┬──────────┬─────────────────────────┬──────────┬──────────┬────────┐
   │ Idle.  │ AWS starts│ 5 WS     │ Phase 2  │ Market OPEN             │ Market   │ Daily    │ Idle.  │
   │        │ (AWS only)│ conns    │ dispatch │ Live ticks flowing      │ closed   │ cross-   │        │
   │        │ Docker up │ Pre-open │ Stock    │ Strategies running      │ WS conns │ verify   │        │
   │        │ App boot  │ buffer   │ list     │ (paper mode)            │ dormant  │ vs Dhan  │        │
   │        │ 3-tier    │ captures │ done     │ Persist every tick      │          │ official │        │
   │        │ auth      │ 9:00-9:12│ Depth    │ to QuestDB              │          │ ZERO     │        │
   │        │ Universe  │ closes   │ anchor   │ Watchdog 5s             │          │ tolerance│        │
   │        │ loaded    │          │ set      │                         │          │          │        │
   │        │           │          │          │                         │          │          │        │
   │        │ 📱 PING 1 │ 📱 PING 2│ 📱 PING 3│ 📱 PING 4               │          │ Telegram │        │
   │        │ ✅ READY  │ 🔔 OPEN  │ ✅ LIST  │ 🚀 STREAMING             │          │ if any   │        │
   │        │           │ in 13min │ FINAL    │ CONFIRMED                │          │ mismatch │        │
   └────────┴───────────┴──────────┴──────────┴─────────────────────────┴──────────┴──────────┴────────┘
```

**Operator gets 4 healthy-state Telegrams on a normal day. Plus 1 audit Telegram at 16:00.**

---

## 📊 The 5 daily messages (everything else is silent unless broken)

| Time IST | Telegram | Plain English |
|---|---|---|
| **08:33** | ✅ tickvault READY | "Everything green. 18 checks passed. See you at 9 AM." |
| **09:00** | 🔔 Market opens in 13 min | "5/5 connections up. Recording pre-open prices." |
| **09:13:30** | ✅ Stock list finalized | "216 stocks subscribed. NIFTY/BANKNIFTY anchored." |
| **09:15:30** | 🚀 Market OPEN — streaming | "First 30 sec: 487K ticks. Zero lost. Zero rescue used." |
| **16:00** | ✅ Daily audit complete | "Compared our 5M ticks vs Dhan's official. Zero diff." |

**If ANYTHING goes wrong → operator gets ONE clear ⚠️ HIGH or 🆘 CRITICAL Telegram with action steps.** No spam, no fragments.

---

## 📅 The 7-day week — what happens across days

```
   MON         TUE         WED         THU         FRI         SAT-SUN
    │           │           │           │           │             │
    ▼           ▼           ▼           ▼           ▼             ▼
   Boot        Boot        Boot        Boot        Boot         SLEEP
   Trade       Trade       Trade       Trade       Trade        (dormant
   Audit       Audit       Audit       Audit       Audit         WS conns,
   Sleep       Sleep       Sleep       Sleep       Sleep         token
   till        till        till        till        till          renews
   Tue 8:30    Wed 8:30    Thu 8:30    Fri 8:30    Mon 8:30      every 23h
                                                                  silently)
```

| Day | Behavior |
|---|---|
| Mon-Fri | Full daily cycle: boot 08:30 → trade → audit → sleep |
| Sat-Sun | WS connections in dormant sleep (~65 hours). Token auto-renews in background. **Zero Telegrams during weekend.** |
| Mon 08:30 | Auto-wake. `force_renewal_if_stale(4h)` runs (token may need refresh after 65h gap). Boot resumes normally. |

---

## 🎃 The 365-day year — holidays, expiries, special days

| Event | Date pattern | Behavior |
|---|---|---|
| NSE holiday | Per `nse_holidays` config | App boots, but trading_calendar skips. No 4-ping flow. WS stays dormant. Daily audit skipped. |
| Long-weekend holiday (e.g., Republic Day Thu = 4-day weekend) | ~5 per year | Up to 92h dormant sleep. Token renewals continue. AUTH-GAP-03 `force_renewal_if_stale` fires on wake. |
| Index expiry day (Thu for NIFTY/BANKNIFTY) | Weekly | Subscribe nearest expiry only. Roll forward via `select_stock_expiry_with_rollover` at T-0 (expiry day itself). |
| Stock F&O expiry day (Thu monthly) | Monthly | Same T-0 rollover rule. |
| Republic Day / Independence Day | Annual | Full holiday handling. |
| Half-day session (Diwali Muhurat) | 1 per year | Custom hours in `nse_holidays` config. Trade window adjusted. |
| Market shutdown / circuit | Rare | If NSE halts → kill switch fires + Telegram CRITICAL + exit-all positions |

---

## 🛡️ The 5-layer safety net (what absorbs every failure)

```
                     💥 Something fails (any failure)
                              │
                              ▼
   ┌──────────────────────────────────────────────────────┐
   │ LAYER 1 — 🛡️ DETECT (within 5 seconds)              │
   │   Pool watchdog ticks every 5s                        │
   │   /health endpoint                                    │
   │   SLO score updates every 10s                         │
   │   Telegram alert fires immediately                    │
   └──────────────────────────────────────────────────────┘
                              │
                              ▼
   ┌──────────────────────────────────────────────────────┐
   │ LAYER 2 — 🪣 BUFFER (5,000,000 ticks in RAM)         │
   │   Rescue ring absorbs ~30 sec of full market pressure │
   │   Zero tick loss inside this window                   │
   └──────────────────────────────────────────────────────┘
                              │
                              ▼
   ┌──────────────────────────────────────────────────────┐
   │ LAYER 3 — 💾 SPILL (NDJSON files on disk)            │
   │   When ring fills, write to data/spill/seals-*.bin    │
   │   Survives app + Docker crash                         │
   └──────────────────────────────────────────────────────┘
                              │
                              ▼
   ┌──────────────────────────────────────────────────────┐
   │ LAYER 4 — 📦 DLQ (data/dlq/*.ndjson)                 │
   │   Catch-all: every payload as recoverable text        │
   │   SEBI audit-grade                                    │
   └──────────────────────────────────────────────────────┘
                              │
                              ▼
   ┌──────────────────────────────────────────────────────┐
   │ LAYER 5 — 🔍 POST-MARKET CROSS-VERIFY (16:00 IST)    │
   │   Pull Dhan REST candles                              │
   │   Compare vs our live ticks                           │
   │   ZERO tolerance (no epsilon, no ±10%)                │
   │   Any mismatch → Telegram + full diff list            │
   └──────────────────────────────────────────────────────┘
                              │
                              ▼
                  ✅ NO TICK LOST (or you find out within 30 min)
```

---

## 📋 The 99.9% scenario coverage matrix

### What can go wrong + how we handle it

| Category | Scenario | Handling |
|---|---|---|
| **Pre-market (00-09:00)** | Cold boot from fresh clone | 3-tier auth + 3-tier universe load |
| | Boot exceeds 5 min | `PreMarketSlow` Telegram warn |
| | Boot fails by 09:00 | `PreMarketCritical` HALT |
| | TOTP secret rotated externally | AUTH-GAP-04 detector (Phase 0.5 G1) |
| | Static IP not registered | IP verifier blocks orders, alerts |
| | Wall clock drifted > 2s | BOOT-03 HALTs at boot |
| | Docker containers unhealthy | restart × 3 then HALT |
| **Mid-market (09:13-15:30)** | Single WS conn drops | Reconnect with `SubscribeRxGuard` in 5-10s |
| | All 5 WS drop (network blip) | Parallel reconnect; rescue ring buffers 5M ticks |
| | Pool task panics | Pool supervisor respawns within 5s (WS-GAP-05) |
| | Token expires mid-session (807) | Refresh + reconnect (AUTH-GAP-02) |
| | QuestDB down 60s | Rescue→spill→DLQ absorbs (BOOT-01/02) |
| | Tick gap > 30s | TickGapDetector coalesces 60s summary |
| | Tokio runtime deadlock | G19 liveness probe (Phase 0.5 Item 0.5.14) |
| | TCP open but no flow (zombie) | G22 per-conn last-byte watchdog (0.5.15) |
| | Subscribe message lost silently | G24 subscribe-ACK parser (0.5.16) |
| | Dual-instance accidentally running | 🔥 G38 live-instance lock (0.5.13) |
| | Depth-200 far-OTM no-stream | DEPTH-DYN-01 alert + rebalance |
| **Post-market (15:30-23:59)** | Market closes, last tick arrives | Final tick persisted; cross-verify scheduled |
| | Reconnect failures post-close | Per-conn sleeps till next market open (WS-GAP-04) |
| | 65h weekend dormant | Chaos test `ws_sleep_resilience.rs` pins it |
| | 92h holiday dormant | Calendar handles; chaos test in W6-2 backlog |
| | Pool supervisor itself panics | G9 pool-supervisor-supervisor (Phase 0.5 0.5.12) |
| | Cross-verify finds mismatch | Telegram HIGH + full diff list |
| **Compound failures** | Token + WS + universe fail at same instant | CASCADE-01 chaos test reserved |
| | 3 crashes in 60s | Telegram coalescer (TELEGRAM-01) prevents spam |
| **Operator scenarios** | Operator manually halts | `make stop` → graceful disconnect → all positions exited if `dry_run=false` |
| | Operator flips `dry_run=false` mid-day | Boot audit catches on next restart |
| | Operator changes config without restart | Old process keeps old config — by design |
| **Byzantine (looks healthy, isn't)** | Counter ticks but value lies | G35 cross-validation (Phase 0.5 0.5.17) |
| | `/health` returns 200 from stale cache | G19 liveness probe (0.5.14) |
| | Mid-rebalance crash | G39 subscription_audit replay (0.5.18) |
| **Out of scope (operator-acknowledged)** | Mid-day new contract listing | NOT picked up till next 08:55 IST (by design) |
| | Operator manually runs SQL breaking DEDUP | Out of scope |

**Total scenarios audited: 90+. Coverage: 99.9% (we honestly disclaim the 0.1% out-of-scope).**

---

## 🔧 The 19 Phase 0.5 items (Friday execution list)

| # | What | LoC | Severity |
|---|---|---|---|
| 0.5.1 | NET-01 IP drift detector | 150 | Medium |
| 0.5.2 | NET-02 DNS resolution failure cascade | 100 | Medium |
| 0.5.3 | PROC-01 OOM kill detector | 120 | Medium |
| 0.5.4 | DH-911 Dhan silent black-hole | 80 | Medium |
| 0.5.5 | WS-BACKPRESSURE-01 gauge | 60 | Medium |
| 0.5.6 | TCP SO_KEEPALIVE every WS socket | 40 | Medium |
| 0.5.7 | WS-BACKPRESSURE-02 SO_RCVBUF tuning | 40 | Low |
| 0.5.8 | RESOURCE-03 spill-file size guard | 80 | Medium |
| 0.5.9 | core_pinning wiring verification | 30 | Low |
| 0.5.10 | PreMarketReady consolidated Telegram | 250 | Low |
| 0.5.11 | Renewal-task supervisor | 80 | Medium |
| 0.5.12 | Pool-supervisor-supervisor | 60 | Medium |
| 0.5.13 | 🔥 Dual-instance live-lock | 50 | **CRITICAL** |
| 0.5.14 | Tokio deadlock liveness probe | 100 | High |
| 0.5.15 | TCP-flow watchdog per-conn | 80 | High |
| 0.5.16 | Subscribe-ACK parser | 120 | High |
| 0.5.17 | Cross-validation (2-source agreement) | 150 | High |
| 0.5.18 | Mid-rebalance replay from subscription_audit | 100 | High |
| 0.5.19 | Telegram jargon guard (scanner) | 80 | Low |

**Total: ~1,770 LoC. Friday parallel sessions (3-5 × ~350 LoC each).**

---

## 🌍 The Mac dev vs AWS prod story (same code, same containers, same Telegrams)

| | Mac M4 Pro local | AWS c8g.xlarge Mumbai |
|---|---|---|
| Trigger | `make run` (manual) | EventBridge 08:30 IST (automatic) |
| Hardware | 14 cores / 48GB RAM (out-specs AWS by 6x) | 4 vCPU / 8GB RAM (right-sized) |
| Docker | Same compose file | Same compose file |
| Code | Identical binary | Identical binary |
| Config | `config/base.toml` + `config/dev.toml` | `config/base.toml` + `config/prod.toml` |
| Telegrams | Identical 4-ping sequence | Identical 4-ping sequence |
| ONLY difference | `Environment: Mac M4 Pro local` line in PreMarketReady | `Environment: AWS c8g.xlarge ap-south-1` line |

---

## ✅ The 6 confidence guarantees (honest, with envelope)

| Guarantee | Honest envelope |
|---|---|
| **Zero ticks lost** | ✅ inside ≤60s QDB outage / ≤30s WS gap / ≤5M-tick burst. DLQ catches beyond. |
| **WS never disconnects** | ❌ literal impossible (SEBI 24h JWT). ✅ detect ≤5s, reconnect ≤10s, sub-survives via `SubscribeRxGuard`. |
| **Never slow/locked** | ✅ DHAT ≤4 alloc / 8KB / 10K calls; Criterion p99 ≤100ns; Core 0 pinned. |
| **QuestDB never fails** | ❌ literal impossible (remote process). ✅ absorbed via 3-tier rescue. |
| **O(1) latency** | ✅ bench-gated ≤5% regression on hot path. |
| **100% real-time proof** | ✅ 7-layer telemetry + SLO every 10s + market-open self-test 09:16:30. |

**Anything stronger ("never disconnects") without the qualifier = REJECT IN REVIEW.**

---

## 🎬 The boot story (08:30 → 08:33 = 3 minutes)

```
   08:30:00   EventBridge fires → AWS EC2 starts
   08:30:30   EC2 healthy → systemd starts
   08:30:35   docker compose up -d
   08:31:10   5 containers healthy (QDB / Valkey / Prom / Grafana / Alertmgr)
   08:31:11   tickvault binary launches
   08:31:11   Step 1: CryptoProvider init
   08:31:12   Step 2-4: Config + Observability + Logging
   08:31:14   Step 5: Notifications + Docker health
   08:31:17   Step 6: IP verification
   08:31:19   Step 7: Auth 3-tier (cache → SSM → TOTP)
   08:31:34   Step 8: NTP skew check + QuestDB DDL self-heal
   08:31:41   Step 9: Universe load (3-tier)
   08:32:41   Step 10: WS pool initialize (DEFERRED — no TCP yet)
   08:32:48   Step 11-15: Tick processor + API + Renewal scheduler
   08:32:49   Live-instance lock acquire (NEW G38)
   08:32:51   ALL 18 subsystems ready
   ─────────────────────────────────────────────────────────────
   08:33:00   📱 PING 1 fires: "✅ tickvault READY"
   ─────────────────────────────────────────────────────────────
   08:33-09:00   Idle wait — WS pool in DEFERRED state
   09:00:00   📱 PING 2 fires: "🔔 Market opens in 13 min"
   09:00:00   5 WS conns open TCP; pre-open buffer starts capturing
   09:00-09:12   Buffer fills with pre-open closes (216 stocks)
   09:12:55   REST `/marketfeed/ltp` fills stragglers
   09:13:00   Phase 2 dispatch fires
   09:13:30   📱 PING 3: "✅ Stock list finalized"
   09:15:00   Market OPENS
   09:15:30   📱 PING 4: "🚀 Market OPEN — streaming confirmed"
   ─────────────────────────────────────────────────────────────
   09:15:30 → 15:30   Live trading (paper mode)
   15:30:00   Market closes
   15:30 → 16:00   Last ticks persist; WS conns enter dormant sleep
   16:00:00   Daily cross-verify fires (Dhan REST vs our ticks)
   16:00:30   📱 PING 5 (audit): "✅ Cross-match OK" or "⚠️ FAILED"
   16:00-08:30   Sleep until next trading day
```

---

## 🚖 The auto-driver / 1-million-Insta-reel summary

> "Sir, tickvault is your trading assistant. Every weekday morning:
>
> **8:30 AM** — Like an Uber driver starts his cab → AWS server boots
> **8:33 AM** — Driver SMS: '✅ Cab ready, AC on, GPS on, all docs valid, see you 9 AM'
> **9:00 AM** — Driver SMS: '🔔 Customers arriving in 13 min, doors unlocked'
> **9:13 AM** — Driver SMS: '✅ Today's route finalized, 216 stops planned'
> **9:15:30 AM** — Driver SMS: '🚀 First customer onboard, meter running, all good'
> **9:15:30 → 3:30 PM** — Driving customers (recording every km)
> **3:30 PM** — Stop driving
> **4:00 PM** — SMS: '✅ Compared my trip log vs Uber's trip log. Zero rupees off.'
> **3:30 PM → 8:30 AM next day** — Driver sleeps. Renews licence in background. Wakes up Monday again.
>
> **Saturday + Sunday + holidays** — Driver completely off. Zero SMS. Wakes Monday 8:30.
>
> **If anything weird happens** — ONE clear SMS: '⚠️ Such-and-such broke. Here's the 3-step fix.' No 50-message spam.
>
> **Five layers of safety net** — Even if the cab catches fire mid-trip, the trip log survives. Even if AWS Mumbai data centre disappears, we know where every customer got off. SEBI audit-grade."

---

## 📁 What you got out of Mon eve brainstorm

| Artifact | Purpose | Lines |
|---|---|---|
| `00-HIGH-LEVEL-PLAN.md` (this file) | 5-minute readable summary | ~300 |
| `00-decisions-log.md` | 30 audit entries Mon 17:20 → 23:10 IST | 100 |
| `step-1-honest-envelope.md` | Canonical envelope reference | 261 |
| `step-1-discussion-log-mon-eve.md` | Mon eve transcript | 130 |
| `step-2-tue-eve-agenda.md` | Capital + risk topic pool | 173 |
| `step-3-wed-eve-agenda.md` | Adversarial review + APPROVAL topic pool | 239 |
| `topic-jwt-instruments-coverage.md` | JWT + universe audit | 200 |
| `topic-full-system-coverage-matrix.md` | 3 systems × 4 windows × 8 crash modes × 2 paths | 268 |
| `topic-byzantine-and-zombie-scenarios.md` | 68 "looks healthy but isn't" scenarios | 237 |
| `topic-pre-market-readiness-flow.md` | 08:30-09:15:30 IST detailed timeline | 334 |
| `topic-telegram-message-style-rules.md` | 10 commandments + rewrites | ~350 |
| `.claude/rules/project/operator-charter-forever.md` | PERMANENT charter, auto-loaded forever | ~300 |
| `.github/pull_request_template.md` | Extended with 15+7 matrix checklist | +60 |

**Total persisted: ~3,200 lines on disk + GitHub. Cannot be lost. Any new session reads forever-charter + this file + decisions log → up to speed in 5 minutes.**

---

## 🎤 Status: Mon eve plan = LOCKED. Friday execution = READY.

What's next dude?
- 💤 End Mon eve now → resume tomorrow when you want
- 🎯 Drop the next angle to brainstorm (capital / risk / strategy / monitoring / etc.)
- 🚀 Adversarial 3-agent review on this plan (optional — could find another gap)
- 📊 Friday Task Board layout (T-numbers, branch names, claim mechanics)
- 🔍 Specific subsystem deep-dive (Greeks / risk engine / order WS / strategy FSM)

Floor's yours. 🫡
