# Topic — Pre-Market Readiness Flow (08:30 → 09:15:30 IST)

**Created:** 2026-05-11 22:45 IST
**Trigger:** Operator: "AWS starts 08:30, JWT acquired, instruments loaded, WS connects — all done BEFORE pre-market with confidence + confirmation + guarantee + assurance. Same flow Mac local AND AWS."
**Mode:** Brainstorm + design (no implementation yet)
**Status:** PERSISTED — extends Phase 0.5 Item 0.5.10 (PreMarketReady consolidator)

---

## 🎯 The 45-minute golden window: 08:30 → 09:15:30 IST

```
   08:30 IST          09:00 IST           09:13 IST       09:15:30 IST
   AWS auto-start     Pre-market open     Phase 2 fire    Market streaming
   (or Mac manual)                                        confirmation
       │                   │                   │                   │
       ▼                   ▼                   ▼                   ▼
   ┌─────────────────┬─────────────────┬─────────────────┬─────────────────┐
   │ 30 min runway   │ 13 min pre-open │ 2 min Phase 2   │ Market live     │
   │ (BOOT WINDOW)   │ (BUFFER WINDOW) │ (DISPATCH)      │ (TRADING)       │
   │                 │                 │                 │                 │
   │ Telegram:       │ Telegram:       │ Telegram:       │ Telegram:       │
   │ PreMarketReady  │ MarketOpening   │ Phase2Complete  │ MarketOpen      │
   │ at ~08:33 IST   │ at 09:00 IST    │ + Depth anchor  │ Streaming       │
   │                 │                 │ at 09:13:30 IST │ Confirmation    │
   │                 │                 │                 │ at 09:15:30 IST │
   └─────────────────┴─────────────────┴─────────────────┴─────────────────┘
```

**Operator confidence checkpoints: 4 Telegram pings total.** Not 6 fragmented, not 1 generic.

---

## 📱 PART 1 — The 4 Pre-Market Telegram pings (NEW design)

### Ping 1 — `PreMarketReady` at ~08:33 IST (NEW — replaces 6 fragments)

Single consolidated message replacing today's `Auth OK + Instruments OK + Boot complete + Order Update WS connected + tickvault started + (5× WS connected)`:

```
✅ tickvault PRE-MARKET READY

Environment: AWS c8g.xlarge ap-south-1 (or Mac M4 Pro local)
Boot duration: 2m 47s
Mode: LIVE  |  dry_run = true  (paper trading; no real orders)

──────── Subsystems ────────
✅ AWS EC2 / host         : online, EIP X.X.X.X
✅ Docker containers      : 5/5 healthy (QDB, Valkey, Prom, Grafana, Alertmgr)
✅ Auth                    : Dhan JWT acquired (Layer 1 cache hit / Layer 2 SSM / Layer 3 TOTP)
                            Token expiry: 2026-05-12 23:30 IST (24h headroom)
✅ Instruments             : 97,422 derivatives + 218 underlyings loaded (rkyv cache, 11ms)
✅ Universe                : 11,034 instruments planned for indices_only_all_expiries
✅ QuestDB                 : connected, schema self-heal verified, DEDUP applied
✅ Valkey                  : connected, AOF enabled
✅ Pool watchdog           : armed, 5s tick
✅ Phase 2 scheduler       : armed for 09:13:00 IST
✅ Pool supervisor         : armed (5s respawn)
✅ Token renewal scheduler : armed (23h refresh)
✅ Telegram coalescer      : armed, 60s bucket
✅ /health endpoint        : 200 OK, last_tick_age=0 (DEFERRED state)
✅ Live instance lock      : acquired (no other instance detected)
✅ Core pinning            : 4 cores assigned (WS / parser / ILP / other)
✅ NTP skew                : 0.04s (within ±2s envelope)
✅ Static IP              : registered with Dhan (ordersAllowed=true)

──────── Pre-market plan ────────
🕘 08:33 IST  Pre-market READY (this message)
🕘 09:00 IST  WS pool opens 5 conns + IDX_I / NSE_EQ pre-open buffer starts
🕘 09:13 IST  Phase 2 dispatch (stock F&O subscribe via 09:12 closes)
🕘 09:15 IST  Market opens — live ticks flow
🕘 09:15:30   Market open streaming confirmation
🕘 15:30 IST  Market close
🕘 16:00 IST  Post-market cross-verify (zero-tolerance Dhan REST vs live)

Dashboards: Grafana http://X.X.X.X:3000  |  QuestDB :9000  |  Prometheus :9090

🟢 ALL GREEN. Ready for 09:00 IST.
```

**Severity:** `Info` (LOW). Single ping, full state.

### Ping 2 — `MarketOpening` at 09:00 IST (NEW — informational)

```
🔔 tickvault — Market opening in 13 minutes

✅ WS pool: 5/5 main feed conns ESTABLISHED
✅ Pre-open buffer: capturing 09:00 → 09:12 closes
✅ Token expiry: 24h+ headroom (safe through session)
🕘 Phase 2 dispatch armed for 09:13:00 IST

Pre-open captured so far: 0 of 216 F&O stocks
(captures progressively over next 12 min)
```

**Severity:** `Info` (LOW). Confirms WS handshake survived 08:33 → 09:00 idle wait.

### Ping 3 — `Phase2Complete + MarketOpenDepthAnchor` at 09:13:30 IST (EXISTS)

Two messages already today, sub-second apart:

```
✅ Phase 2 complete
Added 216 F&O stock subscriptions
Total active: 11,034 instruments

Pre-open buffer entries: 216 (100%)
Stocks skipped (no price): 0
Stocks skipped (no expiry): 0

Depth-20 added: 2 underlyings (NIFTY, BANKNIFTY)
Depth-200 added: 4 contracts (NIFTY CE/PE, BANKNIFTY CE/PE)
```

```
📍 Market open depth anchor — NIFTY
Anchor close (09:12 IST): 22,684.50
ATM strike: 22,700 CE / 22,700 PE
Expiry: 2026-05-15 (current weekly)
Strikes subscribed: ATM ± 24 = 49 instruments
```

(One depth-anchor message per index — 2 today: NIFTY + BANKNIFTY.)

### Ping 4 — `MarketOpenStreamingConfirmation` at 09:15:30 IST (EXISTS)

```
🚀 tickvault — Market OPEN, streaming confirmed

⏱  Time: 09:15:30 IST  (30s after market open)

──────── Live counts ────────
✅ Main feed conns active: 5/5
✅ Depth-20 conns active: 4/4
✅ Depth-200 conns active: 4/4 (NIFTY + BANKNIFTY CE/PE)
✅ Order Update WS active: 1/1

──────── First-30-second stats ────────
Ticks received: 487,213
Ticks persisted: 487,213 (rescue ring used: 0)
Unique securities seen: 8,442 / 11,034
Earliest tick lag from market open: 0.34s

🟢 ALL SYSTEMS HEALTHY. Session live.
```

**Severity:** `Info` (LOW). Single ping confirms market-open-to-streaming chain works.

---

## 📊 PART 2 — Per-step boot timeline (08:30 → 08:33 IST)

| Time (IST) | Step | Component | Worst-case duration | Failure handling |
|---|---|---|---|---|
| 08:30:00 | EventBridge fires | AWS EC2 | 0s | Retry: next-minute restart |
| 08:30:05 | EC2 boot begins | AWS | ~30s | EventBridge auto-retry |
| 08:30:35 | systemd start | OS | ~5s | If fails → manual intervention |
| 08:30:40 | docker compose up -d | Docker | ~30s | restart: unless-stopped |
| 08:31:10 | Containers healthy (5/5) | Docker | ~30s | retry × 3 then HALT |
| 08:31:10 | tickvault binary start | App | ~1s | systemd restart |
| 08:31:11 | Step 1: CryptoProvider init | App | <1s | HALT if fails |
| 08:31:11 | Step 2: Config load (figment + TOML) | App | <1s | HALT if fails |
| 08:31:11 | Step 3: Observability init (Prom, OTEL, tracing) | App | ~1s | Degraded if partial |
| 08:31:12 | Step 4: Logging init (errors.jsonl rotation) | App | <1s | HALT if log dir unwritable |
| 08:31:12 | Step 5: Notification (Telegram + SNS) | App | ~2s | Degraded if Telegram down |
| 08:31:14 | Step 5b: Docker health check from app | App | ~3s | HALT if any container missing |
| 08:31:17 | Step 6: IP verification | App | ~2s | HALT if `ordersAllowed=false` |
| 08:31:19 | Step 7: Auth 3-tier | App | ~5-15s | 3-tier fallback; HALT if all fail |
| 08:31:34 | Step 8a: NTP skew check (BOOT-03) | App | ~2s | HALT if skew > 2s |
| 08:31:36 | Step 8b: QuestDB DDL + schema self-heal | App | ~5s | retry × 3 then HALT |
| 08:31:41 | Step 9: Universe load (3-tier) | App | ~5-60s | 3-tier fallback; HALT if all fail |
| 08:32:41 | Step 10: WS pool initialize (5 conns) | App | ~2s | conns in DEFERRED state (no TCP yet) |
| 08:32:43 | Step 11: Tick processor + aggregator spawn | App | ~1s | HALT if spawn fails |
| 08:32:44 | Step 12: Historical candles (cold path, background) | App | bg | not blocking |
| 08:32:44 | Step 13: Order update WS connect | App | ~3s | retry × 5 then degraded |
| 08:32:47 | Step 14: API server (port 3001) | App | ~1s | HALT if port busy |
| 08:32:48 | Step 15: Token renewal scheduler armed | App | <1s | n/a |
| 08:32:48 | Live-instance-lock acquire (NEW G38) | App | ~1s | HALT if other instance < 60s old |
| 08:32:49 | Core pinning verification (Item 0.5.9) | App | <1s | Warn if not pinned |
| 08:32:49 | Subscription audit / replay (NEW 0.5.18) | App | ~2s | n/a |
| 08:32:51 | All subsystems ready signal | App | <1s | n/a |
| **08:33:00** | **PreMarketReady Telegram fires** | App | <1s | If any step degraded → `PreMarketDegraded` instead |

**Budget: 3 min boot. Slack: 27 min before 09:00 IST.** If boot takes longer than 5 min → `PreMarketSlow` Telegram warn. If boot fails by 08:55 IST → `PreMarketDegraded` Telegram CRITICAL. If still failing at 09:00 IST → `PreMarketCritical` + halt.

---

## 🎬 PART 3 — Mac local vs AWS prod (identical experience)

| Step | AWS c8g.xlarge | Mac M4 Pro 14c/48GB | Difference |
|---|---|---|---|
| Trigger | EventBridge 08:30 IST | Operator runs `make run` | Manual vs automatic |
| EC2 boot | 30s | n/a | AWS only |
| Docker compose | 30s | 30s | Same |
| Binary load | 1s | 1s | Same |
| 15 boot steps | 90-120s | 30-60s (faster CPU) | Mac is faster |
| Telegram pings | Same content | Same content | **IDENTICAL** |
| Dashboards | EC2 IP | localhost | URL only differs |

**Common-runtime principle:** the `PreMarketReady` Telegram message field `Environment:` is the ONLY thing that differs — `AWS c8g.xlarge ap-south-1` vs `Mac M4 Pro local`. Everything else identical. Operator gets the same confidence on both.

---

## 🚨 PART 4 — Degraded path Telegram (the "something's wrong" message)

If any subsystem fails by 08:55 IST, replace `PreMarketReady` with:

```
⚠️ [HIGH] tickvault PRE-MARKET DEGRADED

Boot duration: 4m 23s
Started: 08:30:35 IST  →  Failed at: 08:34:58 IST

──────── Failed subsystems ────────
❌ Auth                    : Layer 3 TOTP gen failed (INVALID_TOTP after 5 retries)
                            Possible cause: TOTP secret rotated externally (G1/AUTH-GAP-04)
                            Runbook: .claude/rules/dhan/authentication.md

✅ Other 17 subsystems     : healthy

──────── Recovery actions ────────
🔄 Auto-retry × 3 every 60s through 08:55 IST
🆘 If still failed at 09:00 IST → PreMarketCritical fires + HALT

Operator action: SSH to host → check SSM TOTP secret → restart app
```

**Severity:** `High`. Telegram pages operator.

If failure persists at 09:00 IST → `PreMarketCritical` (Severity Critical, Telegram + SMS):

```
🆘 [CRITICAL] tickvault PRE-MARKET CRITICAL — NO TRADING TODAY

Time: 09:00:01 IST
Failed for: 25 minutes
Halted at boot Step 7 (Auth 3-tier)

ACTION REQUIRED:
1. SSH: ssh ec2-user@<EIP>
2. Logs: tail -f /var/log/tickvault/errors.jsonl
3. Verify SSM secret: aws ssm get-parameter --name /tickvault/prod/dhan/totp_secret
4. If valid → bounce process: sudo systemctl restart tickvault
5. If invalid → regenerate from Dhan web portal → push to SSM

No live trading until PreMarketReady fires.
Paper-mode (dry_run=true) continues unaffected.
```

---

## 📊 PART 5 — Confidence checkpoints (the operator's "is everything ready?" answer)

| Time | Operator sees | What it means | If MISSING |
|---|---|---|---|
| ~08:33 | `PreMarketReady` Telegram | All 18 subsystems green | Worry: something's wrong, check by 08:55 |
| 09:00 | `MarketOpening` Telegram | WS handshake survived idle wait | Worry: WS broke between 08:33 and 09:00 |
| 09:13:30 | `Phase2Complete` + `DepthAnchor`(s) | Stock F&O subscribed | Worry: Phase 2 didn't fire / empty plan |
| 09:15:30 | `MarketOpenStreamingConfirmation` | Live ticks flowing for 30s, no rescue used | Worry: tick stream silent |

**Total operator-attention messages on a healthy boot: 4 pings.**
(Down from 6+ fragmented today.)

**If anything degraded: replace with degraded variants — operator gets one HIGH/CRITICAL alert.**

---

## ✅ PART 6 — Updated Phase 0.5 item list (ALL 6 NEW additions adopted)

Per operator's "add everything dude":

| Item | What | LoC | Severity | Closes gap |
|---|---|---|---|---|
| 0.5.1 | NET-01 IP drift detector | ~150 | Med | G16 |
| 0.5.2 | NET-02 DNS resolution failure cascade | ~100 | Med | G17 |
| 0.5.3 | PROC-01 OOM kill detector | ~120 | Med | G15 |
| 0.5.4 | DH-911 silent black-hole | ~80 | Med | G18 |
| 0.5.5 | WS-BACKPRESSURE-01 gauge | ~60 | Med | G10 |
| 0.5.6 | TCP SO_KEEPALIVE | ~40 | Med | G11 |
| 0.5.7 | WS-BACKPRESSURE-02 SO_RCVBUF | ~40 | Low | G12 |
| 0.5.8 | RESOURCE-03 spill-size guard | ~80 | Med | G14 |
| 0.5.9 | core_pinning wiring verify | ~30 | Low | n/a |
| 0.5.10 | PreMarketReady consolidator | ~250 (raised from 200 — adds MarketOpening ping + 18-checkpoint state) | Low | Multi |
| **0.5.11 NEW** | Renewal-task supervisor | ~80 | Med | G3 |
| **0.5.12 NEW** | Pool-supervisor-supervisor | ~60 | Med | G9 |
| **0.5.13 NEW** | 🔥 Dual-instance live-lock | ~50 | **CRITICAL** | G38 |
| **0.5.14 NEW** | Tokio deadlock liveness probe | ~100 | High | G19 |
| **0.5.15 NEW** | TCP-flow watchdog per-conn | ~80 | High | G22 |
| **0.5.16 NEW** | Subscribe-ACK parser | ~120 | High | G24 |
| **0.5.17 NEW** | Cross-validation (2-source) | ~150 | High | G35 |
| **0.5.18 NEW** | Mid-rebalance replay from subscription_audit | ~100 | High | G39 |

**Total Phase 0.5: 18 items, ~1,690 LoC. Doable in Friday parallel sessions (3-5 sessions × 350 LoC each).**

---

## 🚖 Auto-driver / dumb-kid summary

> "Sir, today the bot sends 6 small SMS at boot — 'door opened', 'lights on', 'AC on', etc. You said you want ONE clear SMS at 8:33 AM saying:
>
> ✅ Shop FULLY OPEN. AC at 22°C. All staff in. Coffee brewing. Stock list ready. Ready for customers in 27 minutes.
>
> That's `PreMarketReady` — one Telegram with 18 green check-marks.
>
> Then at 9:00 AM: 🔔 'Customers arriving in 13 min, doors unlocked, register armed.' = `MarketOpening`.
>
> Then at 9:13 AM: ✅ 'Phase 2 done — stock list finalized.' = `Phase2Complete + DepthAnchor`.
>
> Then at 9:15:30 AM: 🚀 'First customer served, register ticking, all good.' = `MarketOpenStreamingConfirmation`.
>
> Four pings total on a healthy day. Same flow whether you're testing on your Mac or running on AWS. Same checklist. Same confidence."

---

## 🎤 Confirmed locked decisions (from this turn)

1. ✅ ALL 6 Phase 0.5 additions (0.5.13–0.5.18) adopted
2. ✅ `PreMarketReady` Telegram format locked (18-checkpoint consolidated message)
3. ✅ 4-ping schedule locked: 08:33 / 09:00 / 09:13:30 / 09:15:30 IST
4. ✅ Degraded + Critical variants locked (replaces healthy message if subsystem fails)
5. ✅ Mac local = AWS prod identical Telegram experience (only `Environment:` field differs)
6. ✅ Boot budget: 3 min target, 5 min warn, 25 min critical halt at 09:00 IST

---

## 📌 What's NOT in this file (carried forward to other topic files)

- Per-Telegram message Rust impl details → Friday tasks T05-10 / T05-NN
- Failure handling for AWS EC2 boot before tickvault even starts → AWS budget rule
- SMS via AWS SNS as Telegram backup → already in design (notification/sns_client.rs)
- Operator's manual `make stop` mid-boot → graceful shutdown path already exists

These are all already-known and don't need new brainstorm.
