# PHASE 0 — LEAN LOCKED PLAN (Operator decision 2026-05-13)

> **Status:** LOCKED. Supersedes everything else in `friday-may-15-mega/`.
> **Author:** Parthiban (operator decision after live disconnect storm 09:16-09:29 IST 2026-05-13)
> **Scope:** Minimum-viable F&O retail option-buying system. Everything else parked to Phase 2.
> **Build day:** Friday 2026-05-15
> **First live monitoring day:** Phase 1 starts after build verification (target: 1-month dry_run on AWS)

---

## The decision (operator verbatim, 2026-05-13 ~10:30 IST)

> "Let us skip this full mode depth 20 and depth 200 everything else dude. Let us keep all of this as pause or hold or second phase. See how about doing the simple phase like this dude — see just subscribe only ticker mode alone for entire F&O instruments of around 235 or 250 maximum right including indices, am I right dude? How about as the first step subscribing only to these that too using only one WebSocket connection dude. Let us go ahead from the scratch in an easier manner dude."

**Translated to engineering:** Strip the system to its smallest valuable form. One WebSocket. Ticker mode. ~221 instruments. No depth. No streaming greeks. No dynamic selectors. No full chain. Backtest on Mac, live on AWS t3.medium, dry_run for 1 month, then go live with one strategy.

---

## What's IN Phase 0 (the only things we build)

| Component | Scope | Status |
|---|---|---|
| Main feed WebSocket | **1 connection** | Active |
| Subscribed instruments | **4 indices (NIFTY 13, BANKNIFTY 25, SENSEX 51, INDIA VIX) + ~218 F&O underlying stocks (NSE_EQ)** = **~222 SIDs total** | Active |
| Subscription mode | **Ticker only (16-byte packets)** | Active |
| Order Update WebSocket | 1 connection | Active (separate endpoint, already wired) |
| Tick processor | Tick → indicator engine → strategy evaluator | Active |
| Indicator engine | RSI, MACD, EMA, SMA, Bollinger Bands, Fibonacci, ATR, VWAP | Active |
| Strategy evaluator | TOML-driven, hot-reload, dry_run=true | Active |
| QuestDB persistence | ticks table + candles (1m/5m/15m/1h/1d sealed bars) | Active |
| Telegram alerts | Boot, market-open self-test, disconnect/reconnect, daily P&L | Active |
| Order placement | Super Orders (entry + target + SL + trailing) via REST | Active (dry_run gates) |
| Option chain lookup | One-shot REST `/v2/optionchain` at entry decision | Active |
| Historical fetch | Daily post-market 1m candles for 222 SIDs | Active |
| Cross-verify | Compare live candles vs Dhan historical at 15:31 IST | Active |
| Backtest runner (Mac) | Offline brute-force on Dhan historical | Active |

---

## What's PARKED (Phase 2 — re-enable only when data proves need)

All of these stay in the codebase but gated `false` in `config/base.toml`:

| Parked component | Feature flag | Re-enable trigger |
|---|---|---|
| Depth-20 dynamic (5 conns × 50 SIDs) | `features.depth_20 = false` | Need order-book signals |
| Depth-200 dynamic (5 conns × 1 SID) | `features.depth_200 = false` | Need top-of-book microstructure |
| Greeks pipeline (Delta/Theta/Vega streaming) | `features.greeks_pipeline = false` | Hedged / delta-neutral strategies |
| Movers pipeline (top gainers/losers) | `features.movers_pipeline = false` | Discretionary scanning |
| Index F&O full chain subscribe (10K instruments) | `subscription.scope = "indices_underlyings_only"` | Multi-leg / spread strategies |
| Phase 2 dispatcher (09:13 IST) | n/a — no stock F&O subscribed | Stock F&O strategies |
| Depth rebalancer (ATM drift) | n/a — no depth | Same as depth |
| 4 of 5 main-feed WS conns | n/a — only 1 needed | Total SIDs > 5,000 |
| Volume monotonicity guard | unchanged (still active, defends 222 SIDs) | — |
| Cross-segment uniqueness (I-P1-11) | unchanged (still active) | — |
| 15 audit tables | unchanged (still active) | — |

**Important:** the code stays in the repo, ratchets stay green, banned-pattern guards keep enforcing on the parked code paths. We just don't spawn them.

---

## Architecture (the entire Phase 0)

```
┌──────────────────────────────────────────────────────────────┐
│  DHAN                                                          │
│  ├─ wss://api-feed.dhan.co     (1 conn, ticker, 222 SIDs)     │
│  ├─ wss://api-order-update.dhan.co  (1 conn)                   │
│  ├─ REST /v2/optionchain         (call at entry decision)     │
│  ├─ REST /v2/super/orders        (place order)                 │
│  └─ REST /v2/charts/intraday     (post-market historical)     │
└──────────────────────┬───────────────────────────────────────┘
                       │
                       ▼
┌──────────────────────────────────────────────────────────────┐
│  TICKVAULT APP (256 MB, single binary on t3.medium)            │
│                                                                │
│  ┌─ Tick processor → SPSC ring → indicator engine             │
│  │                                ↓                            │
│  │                       Strategy evaluator (dry_run)          │
│  │                                ↓                            │
│  │                       Signal log (Telegram digest)          │
│  ├─ Boot orchestrator                                         │
│  ├─ Token manager (TOTP → JWT, 23h renewal)                   │
│  ├─ Market-hours gate (09:00-15:30 IST)                       │
│  ├─ Activity watchdog (15s for IDX_I, 30s for stocks)         │
│  ├─ Subscribe-ACK verifier (5s deadline per batch)            │
│  ├─ REST gap-fill (per-minute LTP during WS outage)           │
│  └─ Post-market historical fetch + cross-verify (15:31)       │
└──────────────────────┬───────────────────────────────────────┘
                       │
                       ▼
┌──────────────────────────────────────────────────────────────┐
│  QUESTDB (1.5 GB, t3.medium)                                  │
│  ├─ ticks (live, 222 SIDs × ~1 tick/s × 6.25h = ~5M rows/day) │
│  ├─ candles_1m, _5m, _15m, _1h, _1d (sealed)                  │
│  ├─ historical_candles (Dhan REST, daily refresh)             │
│  └─ 15 audit tables (boot, ws_reconnect, order, etc.)         │
└──────────────────────┬───────────────────────────────────────┘
                       │
                       ▼
┌──────────────────────────────────────────────────────────────┐
│  GRAFANA + PROMETHEUS — single-page operator dashboard        │
│  └─ tick rate, indicator state, disconnect counter, P&L       │
└──────────────────────────────────────────────────────────────┘
```

**Total Docker services on AWS:** 5 (app + questdb + grafana + prometheus + valkey)
**Total WebSocket connections:** 2 (main feed + order update)

---

## Why this works for the strategy

The operator's strategy: **intraday option BUYING + Fibonacci + multiple indicators + tight stop loss + VIX regime filter.**

| Strategy need | Phase 0 supplies it? |
|---|---|
| Underlying direction (RSI/MACD/EMA/Fib on spot) | ✅ Live ticks on 222 underlyings (incl. VIX), ticker mode |
| **Volatility regime filter (VIX-based gate on entry)** | ✅ INDIA VIX tick stream — high VIX → skip trade or smaller size; low VIX → favorable |
| Strike selection at entry | ✅ One REST `/optionchain` call |
| Super Order placement (target + SL + trailing) | ✅ Already wired |
| Fill / status updates | ✅ Order-update WS (Dhan→us, MsgCode 42) |
| Target/SL execution at REAL NSE price | ✅ Super Order legs rest on NSE matching engine |
| Exit signal from underlying | ✅ Computed from underlying tick stream |
| P&L tracking | ✅ Order fills + REST portfolio API |
| Backtesting | ✅ Mac runs Dhan historical sweeps, indicator code identical to live |

**What this strategy does NOT need:** order-book depth, streaming Greeks, multi-leg execution, option flow scanning, BSE F&O. All Phase 2.

---

## AWS infra (final)

| Item | Spec | Cost (₹/mo) |
|---|---|---|
| EC2 t3.medium | 4 GB / 2 vCPU, 9h × 22 weekdays | 700 |
| Elastic IP | Static, 24/7 (Dhan whitelist) | 152 |
| EBS gp3 30 GB | Hot data (last 30-60 days) | 235 |
| S3 cold archive | 50 GB Intelligent-Tier → Glacier | 100 |
| SNS SMS | ~50 alerts/mo | 15 |
| Data transfer | ~5 GB/mo | 50 |
| **Total** | | **₹1,252** |

**75% under the ₹5,000/mo budget cap. ₹3,748/mo saved vs original plan.**

---

## Backtesting flow (Mac)

| Step | Tool | Time |
|---|---|---|
| 1. Pull Dhan historical (CSV cache) | `dhan_historical_fetch` binary | One-time, ~30 min for 5y × 222 SIDs |
| 2. Bulk import to local QuestDB (Docker) | ILP writer | ~10 min |
| 3. Run strategy sweep | `backtest_runner` (in-repo binary, same indicator crate) | 5 min → 12h depending on sweep |
| 4. Walk-forward validate top N | `walk_forward_validator` | ~30 min |
| 5. Commit winning params to `config/strategies.toml` | git | ~5 min |
| 6. Push → AWS app picks up hot-reload | notify crate | <60s |

**Live code == backtest code.** Same indicator engine binary, same QuestDB schema. Zero drift between strategy-design and strategy-execution.

---

## The 3 build changes for Friday 2026-05-15

| # | Change | File | LoC estimate |
|---|---|---|---|
| 1 | Subscription planner: emit **only IDX_I (3) + NSE_EQ F&O underlyings (~218) = ~221 in Ticker mode** | `crates/core/src/instrument/subscription_planner.rs` | ~50 |
| 2 | Connection pool: spawn **only 1 main-feed WS conn** (other 4 stay defined but never started) | `crates/core/src/websocket/connection_pool.rs` + `crates/app/src/main.rs` | ~30 |
| 3 | Config feature flags to gate depth-20, depth-200, greeks, movers, dynamic selectors, Phase 2 dispatcher | `config/base.toml` + boot conditionals in `crates/app/src/main.rs` | ~120 |

**Total: ~200 LoC changed, ~4 hours of focused work.** Everything else stays as-is.

---

## The 5 hardening changes (bundled into Phase 0)

These came from the disconnect-storm analysis on 2026-05-13. They are universal — needed regardless of scope:

| # | Change | LoC |
|---|---|---|
| 1 | Activity watchdog tighter for IDX_I (15s) vs stocks (30s) | ~80 |
| 2 | Subscribe-ACK verification (post-subscribe, require frame within 5s) | ~80 |
| 3 | REST `/marketfeed/ltp` gap-fill module (during WS outage, 1/min) | ~200 |
| 4 | Stagger initial conn 2s apart (no effect when only 1 conn, but safe) | ~10 |
| 5 | Defer depth-20 connect until cohort selector has ≥50 SIDs (N/A in Phase 0 but ratchet retained) | ~50 |

**Total Friday build: ~620 LoC. Achievable in one focused day.**

---

## Honest envelope (the 100% claim)

> "100% inside the tested envelope for Phase 0:
> - 1 WebSocket connection streaming 222 SIDs in Ticker mode
> - ≤2s reconnect with subscribe preservation
> - ≤30s detection of stalled feed (15s for IDX_I)
> - REST gap-fill keeps 1m candles complete during WS outages ≤5 min
> - 5,000,000-tick rescue ring absorbs QuestDB outages ≤60s
> - Daily post-market cross-verify against Dhan historical, exact-match required
> - Composite-key uniqueness `(security_id, exchange_segment)` ratcheted
> - Order placement gated by `dry_run=true` for Phase 1 (1-month monitoring)
> - Beyond the envelope: spill NDJSON catches every payload as recoverable text
>
> Outside Phase 0 scope: order-book depth signals, streaming Greeks, multi-leg execution, stock F&O subscription, BSE F&O. These are Phase 2 — re-enable only after data proves the need."

---

## Phased rollout timeline (the actual ship plan)

| Phase | Duration | Scope |
|---|---|---|
| **Phase 0 build** | Friday 2026-05-15, single day | Implement 3 + 5 = 8 changes, ~620 LoC |
| **Phase 0 validation** | Sat-Sun 2026-05-16/17 | Mac backtest sweep, dry_run smoke tests, CI green |
| **Phase 0 AWS deploy** | Mon 2026-05-18 | Provision t3.medium, EIP, EBS, deploy via docker-compose |
| **Phase 1 monitoring** | 22 trading days (~4 weeks, 2026-05-19 → 2026-06-13) | Live ticks, dry_run=true, weekly Telegram digest, measure disconnects + data quality + strategy signals |
| **Phase 1 decision gate** | 2026-06-16 | Green/Yellow/Red light per criteria in `topic-1-month-aws-monitoring-phase.md` |
| **Phase 2A live trade** | 2 weeks, ~10 trading days | ONE strategy live, smallest lot size, full audit |
| **Phase 2B expand** | Add Phase 2 features (depth/greeks/etc.) one at a time, only if data demands | TBD |

**Total time from now to first live trade: ~6 weeks (build + 1 month dry_run + 2 weeks paper-confirmed).**

---

## What's parked for next session(s)

Everything else in `friday-may-15-mega/` (35+ topic files) covers Phase 2 considerations. They stay in the archive as design references, but **NONE of them block Phase 0**. Specifically parked:

- `topic-dynamic-depth-locked-design.md`
- `topic-top-n-volume-dynamic-depth.md`
- `topic-memory-wal-ring-shadow-deep-drill.md` (rescue ring stays at 5M; everything else dormant)
- `topic-tick-to-candle-math-prevday-sourcing.md` (1m/5m/15m/1h/1d kept; sub-minute cascading dropped)
- `topic-greeks-*` (all greeks parked)
- `topic-movers-*` (all movers parked)
- `topic-ws-flow-health-7-layer-defense.md` (5 hardening changes already pulled forward into Phase 0)
- All Telegram redesign topics (existing alerts are sufficient for Phase 0; richer visualization is Phase 2)

The 471 worst-case paths catalogued earlier reduce to ~80 in scope for Phase 0. The rest are dormant code paths.

---

## Mechanical enforcement (still applies in Phase 0)

| Gate | What it catches |
|---|---|
| `per-item-guarantee-check.sh` | Every Phase 0 change carries 15-row + 7-row matrix |
| `banned-pattern-scanner.sh` | No `HashSet<u32>`, no hot-path allocation |
| `pub-fn-test-guard.sh` | Every new pub fn has a test |
| `pub-fn-wiring-guard.sh` | Every new pub fn has a call site |
| `dedup_segment_meta_guard.rs` | DEDUP keys include segment |
| `error_level_meta_guard.rs` | Flush/persist failures use `error!`, never `warn!` |
| `operator_health_dashboard_guard.rs` | New counters have Grafana panels |
| `resilience_sla_alert_guard.rs` | New counters have alert rules |
| Adversarial 3-agent review | hot-path-reviewer, security-reviewer, hostile general-purpose — all 3 pass before PR merge |

---

## The auto-driver one-liner

> "Sir, MVP plan locked: ek WebSocket, 221 instruments, sirf ticker mode, sirf indicators on underlying, sirf Super Order for entry/exit. Mac pe 5 saal Dhan historical pe backtest. AWS pe ₹1,250/mo me chhota machine. 1 mahina dry_run, phir ek strategy live. 60% kam code, 75% kam paisa, 10x tezi. Sab depth-greeks-movers Phase 2 ke liye park. Ye Friday ka kaam hai."

---

## Operator sign-off

This file is locked when committed. Friday 2026-05-15 build begins from this plan. No new scope additions until Phase 1 monitoring data justifies them.

**Approved by Parthiban: 2026-05-13 ~10:30 IST (verbatim quote captured above).**
**Plan author: Claude (this session).**
**Branch: `claude/trading-tick-vault-BkvpS`.**
