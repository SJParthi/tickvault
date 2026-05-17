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
│  ├─ 15 audit tables (boot, ws_reconnect, order, etc.)         │
│  └─ Built-in web UI at port 9000 = the operator dashboard     │
└──────────────────────────────────────────────────────────────┘
```

**Total Docker services on AWS:** 2 (Tickvault app + QuestDB)
**Total WebSocket connections:** 2 (main feed + order update)
**Dashboard:** QuestDB built-in UI at port 9000 (SQL queries against live tables)
**Alerting:** Telegram via teloxide direct from app — no Grafana, no Prometheus, no Alertmanager
**Cache:** in-app `arc-swap` for token + `papaya` HashMap for instruments — no Valkey
**Gateway:** AWS ALB free tier or direct port — no Traefik

---

## Phase 0 memory budget on t3.medium (4 GB total)

| Component | RAM | Reason |
|---|---|---|
| **QuestDB** | **1.5 GB** | Minute-boundary candle seal bursts + 5M tick/day ingestion + query cache |
| **Tickvault app** | **1.0 GB** | 5M-tick rescue ring + today/yesterday sealed bars in RAM + indicator state for 222 SIDs + future strategy headroom |
| **OS + FS cache** | **500 MB** | Linux page cache, kernel TCP buffers, tracing log writes, Docker daemon |
| **Total used** | **3.0 GB** | |
| **Headroom (hard floor)** | **1.0 GB** | OOM safety margin (kswapd needs ≥1 GB free) |

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

## The 7 hardening changes (bundled into Phase 0)

These came from the disconnect-storm analysis on 2026-05-13. They are universal — needed regardless of scope:

| # | Change | LoC |
|---|---|---|
| 1 | Activity watchdog tighter for IDX_I (15s) vs stocks (30s) | ~80 |
| 2 | Subscribe-ACK verification (post-subscribe, require frame within 5s) | ~80 |
| 3 | **Disconnect gap-fill via `/v2/charts/intraday`** (seal-then-fetch, 5s buffer after bar boundary, DEDUP UPSERT into `candles_1m` + RAM bar cache, `gap_fill_audit` table, multi-minute support, market-close cutoff). **REPLACES original `/marketfeed/ltp` gap-fill — LTP has no per-minute OHLCV.** | ~450 |
| 4 | Stagger initial conn 2s apart (no effect when only 1 conn, but safe) | ~10 |
| 5 | Defer depth-20 connect until cohort selector has ≥50 SIDs (N/A in Phase 0 but ratchet retained) | ~50 |
| 6 | **Disconnect-chain 7-layer observability** (5 Prom counters + 2 gauges + 2 audit tables + 5 typed Telegram variants + 6 ratchet tests + Grafana panels) | already counted in #3 |
| 7 | **Seal-then-fetch scheduler invariants** (constants `GAP_FILL_POST_SEAL_BUFFER_SECS=5`, `GAP_FILL_FETCH_TIMEOUT_SECS=30`, `GAP_FILL_MAX_CONCURRENT_FETCHES=5`, `GAP_FILL_RETRY_ATTEMPTS=3`, `GAP_FILL_RETRY_BACKOFF_SECS=[2,5,10]` + ratchets) | already counted in #3 |

### Seal-then-fetch rule (mechanical invariant, 2026-05-13 lock)

| Disconnect time | Bar to refill | Earliest legal fetch |
|---|---|---|
| 09:33:03 | 09:33 | 09:34:05 (bar_end + 5s buffer) |
| 09:33:03 (3-min outage to 09:36:00) | 09:33, 09:34, 09:35 | each at `bar_end + 5s` |
| Outage spans 15:29 → 15:31 | 15:29 bar only | 15:30:05 (do NOT fetch 15:30 — market closed mid-bar) |

**Why 5s buffer:** Dhan ingestion lag + clock skew safety (±2s per BOOT-03) + round-trip overhead. Asking earlier = half-cooked bar = wrong data.

### Why NOT `/marketfeed/ltp` (operator-rejected 2026-05-13)

| Reason | Detail |
|---|---|
| No proper timestamp | LTP is "current price now" — no per-minute resolution |
| Can't reconstruct OHLC | One snapshot ≠ open/high/low/close of a 1m bar |
| Missing volume per bar | Only cumulative day volume |
| Wrong tool for the job | LTP is for "what's the price now", not "what was the bar" |

### `gap_fill_audit` table schema

```sql
CREATE TABLE IF NOT EXISTS gap_fill_audit (
  ts TIMESTAMP,                 -- minute bar start (IST nanos)
  trading_date_ist STRING,
  bar_minute STRING,            -- "09:33"
  trigger_event STRING,         -- ws_disconnect | manual | scheduler_catchup
  sids_requested INT,
  sids_completed INT,
  sids_failed INT,
  duration_ms LONG,
  result STRING                 -- success | partial | failed
) DEDUP UPSERT KEYS(trading_date_ist, bar_minute, trigger_event);
```

### 8. Dual-gate market-hours fix (the 15:29:59.586 skipped-tick bug, operator-locked 2026-05-13)

**Bug observed yesterday:** tick with `exchange_timestamp = 15:29:59.586` was REJECTED because local wall-clock had advanced to `15:30:00.100` by the time the gate evaluated. The 15:29 bar's true close was lost.

**Root cause:** market-hours gate evaluates `is_within_market_hours_ist(now())` — checks LOCAL clock instead of the tick's stamped time.

**Fix — two timestamps, two gates:**

| Gate | Source | Boundary | Purpose |
|---|---|---|---|
| **G1 Exchange Gate** (the truth) | `tick.exchange_timestamp_ist` | `[09:15:00.000, 15:30:00.000)` exclusive on close | Decides if tick belongs to session |
| **G2 Wall-Clock Gate** (wait window) | local `now_ist()` | open until **15:31:00 IST** (60s grace) | Decides when to close socket / seal final bar |

**Constants pinned:**
- `MARKET_OPEN_IST_NANOS = 09:15:00.000_000_000`
- `MARKET_CLOSE_IST_NANOS = 15:30:00.000_000_000` (exclusive)
- `WS_GRACE_AFTER_CLOSE_SECS = 60`
- `BAR_FINAL_SEAL_OFFSET_SECS = 60` (15:29 bar seals at 15:31:00)
- `LATE_TICK_ANOMALY_THRESHOLD_MS = 30_000`

**Why 60s grace, not 5s:** Dhan ingestion + network at close-of-day spike can delay last ticks up to ~45s. 60s safely covers; matches T+1-minute industry trade-reporting tail.

**Banned-pattern hook addition:** scanner rejects any `is_within_market_hours.*now\(\)` — must use `tick.exchange_timestamp_ist`.

**Ratchet tests (mandatory):**
- `test_tick_with_exchange_ts_15_29_59_586_accepted_even_if_local_recv_15_30_00_100`
- `test_tick_with_exchange_ts_15_30_00_000_rejected`
- `test_ws_socket_stays_open_until_15_31_00`
- `test_final_bar_seals_at_15_31_00_not_15_30_00`
- `test_market_gate_does_not_call_local_now` (source-scan)

**New Telegram event:** `LastTickAfterBoundary` (Info) fires if any tick arrives with `exchange_ts ≥ 15:30:00.000` — should be zero; informational only.

**New audit table:** `last_tick_audit` — per-SID last-tick exchange_ts at each minute seal, for forensic queries.

**LoC: ~210 across 8 files.**

**Total Friday build: ~1,330 LoC. Still achievable in one focused day.**

### 9. Pre-open equilibrium → 09:15 candle.open wiring (operator-locked 2026-05-13)

**Bug observed:** today we treat the **first WS tick after 09:15:00** as the OPEN price for the 09:15 1m candle. WRONG — NSE's OFFICIAL OPEN is the pre-open call-auction equilibrium price frozen at **09:08:00 IST**. The first post-09:15 trade is just the first POST-OPEN trade, not the OPEN.

**Concrete impact:** ₹2-5 silent drift per stock per day on the daily candle's OPEN field. Compounds over backtests. Strategies that use gap-up/gap-down logic see different signals than NSE truth.

**NSE pre-open mechanics:**

| Phase | IST window | Outcome |
|---|---|---|
| Order entry | 09:00:00 – 09:07:30 | Buy/sell orders submitted |
| Call-auction matching | 09:07:30 – 09:08:00 | NSE computes single equilibrium price |
| Buffer / freeze | 09:08:00 – 09:15:00 | Price locked; this IS the official open |
| Continuous trading | 09:15:00 onwards | First matched trade is just first post-open trade |

**The fix — per-instrument-class open-price source:**

| Class | 09:15 candle.open source | Fallback chain |
|---|---|---|
| IDX_I NIFTY (13) / BANKNIFTY (25) | Pre-open buffer last slot | First WS tick + `OPEN-PRICE-WARN` |
| IDX_I SENSEX (51) — BSE | BSE pre-open (not in our NSE buffer) | First WS tick + cross-verify flag |
| IDX_I INDIA VIX | NO pre-open (option-chain-derived) | First WS tick (acceptable) |
| NSE_EQ (218 F&O stocks) | Pre-open buffer last slot | REST `/v2/marketfeed/quote` `day_open` → first WS tick + warn |

**Mechanical flow at 09:15:00.000 IST:**
- Aggregator initializes `candle_1m[09:15].open` from buffer (or fallback chain)
- After 09:15:00.001 ticks update HIGH / LOW / CLOSE / VOLUME only — OPEN is FROZEN
- 09:16:05 IST: fetch Dhan `/v2/charts/intraday` 09:15 bar across all 222 SIDs; compare our `open` vs Dhan's `open`; mismatch → Telegram CRITICAL `OpenPriceMismatchVsDhan`

**Constants pinned:**
- `PREOPEN_FREEZE_TIME_IST = 09:08:00.000`
- `PREOPEN_BUFFER_READ_TIME_IST = 09:14:59.900` (read just before seal)
- `OPEN_PRICE_REST_FALLBACK_TIME_IST = 09:14:55.000`
- `OPEN_PRICE_CROSS_CHECK_TIME_IST = 09:16:05.000`

**New audit table `open_price_audit`** — per-SID daily row with `(our_open, dhan_open, source, mismatch_pct, result)`. DEDUP UPSERT KEYS `(trading_date_ist, security_id, exchange_segment)`.

**3 new typed Telegram events:**
- `OpenPriceFromPreopenBuffer` (Info, daily 09:16:55 summary)
- `OpenPriceFallbackToFirstTick` (High, per-SID buffer-empty path)
- `OpenPriceMismatchVsDhan` (Critical, 09:16:05 cross-check failure)

**8 ratchet tests:** open uses buffer for NSE_EQ + NIFTY/BANKNIFTY, falls back to REST then first-tick, OPEN field frozen after 09:15:00.000, first tick only updates close/high/low, Dhan cross-check accuracy, audit DEDUP key invariant.

**LoC: ~700 across 4 files.**

**Total Friday build: ~2,030 LoC across 18 changes.**

### 10. Tightened detector timings (operator-locked 2026-05-13)

| Detector / Action | Old | NEW (locked) | Floor reason |
|---|---:|---:|---|
| First reconnect attempt | 1-2s | **0 ms (instant)** | TCP reconnect fires same-tick as `Err` |
| Subscribe re-send after reconnect | varies | **0 ms** | `SubscribeRxGuard` already holds channel |
| **IDX_I activity watchdog** | 15s | **3 s** | IDX_I 1-3 ticks/sec; 3s = 3-9 missed ticks |
| **NSE_EQ activity watchdog** | 30s | **10 s** | 0.5-2 ticks/sec; 10s = ≥5 missed |
| **VIX activity watchdog** | — | **30 s** | ~1 tick/10s normal |
| **Subscribe-ACK first frame** | 5s | **2 s** | Dhan typically <500ms |
| **WS handshake deadline** | 30s | **5 s** | AWS direct path ≤5s |
| **Order placement REST timeout** | default | **5 s** | Dhan REST p99 ~800ms |

### 11. Daily SEBI 24h JWT renewal observability (operator-locked 2026-05-13)

Every day at ~T-1h before expiry, token manager force-renews. Operator MUST see the renewal succeed (positive ping) and audit row written.

- New typed Telegram: `JwtRenewalCompleted` (Info — daily once)
- New audit table: `auth_renewal_audit` with `(trading_date_ist, renewal_ts, old_token_expiry, new_token_expiry, source = valkey|ssm|totp, latency_ms, result)`
- DEDUP UPSERT KEYS `(trading_date_ist, renewal_ts)`
- `tv_token_renewals_total{source,result}` Prom counter
- Ratchet: `test_jwt_renewal_audit_row_written_on_success`

**LoC: ~150.**

### 12. Static IP boot check (operator-locked 2026-05-13)

At boot, before any order-path code spawns:
- Call `GET /v2/ip/getIP` (free, non-rate-limited)
- Verify `ordersAllowed == true`
- HALT + Telegram CRITICAL if false
- Compare `detectedIP` vs SSM-stored `expected_primary_ip` — mismatch = static IP changed externally

**LoC: ~80.**

### 13. Dual-instance lock (RESILIENCE-01, operator-locked 2026-05-13)

Refuse to boot if another tickvault instance is already running against the same Dhan client_id (would fragment WS budget + static-IP conflict).

- QuestDB `live_instance_lock` table: `(client_id, host_id, boot_ts)` with DEDUP UPSERT KEYS `(client_id)`
- At boot: SELECT existing row; if `boot_ts > now() - 60s` → refuse start + Telegram CRITICAL `DualInstanceDetected`
- Lock heartbeat updates `boot_ts` every 30s

**LoC: ~150.**

### 14. Orphan position 15:25 IST watchdog (operator-locked 2026-05-13)

Strategy is intraday option-buying — NO overnight positions allowed.

- Tokio task fires at 15:25:00 IST
- Query `/v2/positions` for any `netQty != 0`
- For each open position: cancel pending Super Order legs + place market exit
- Telegram CRITICAL `OrphanPositionAutoClosed` with position details
- New audit row in `orphan_position_audit` table

**LoC: ~200.**

### 15. NaN / division-by-zero guards in indicator engine (operator-locked 2026-05-13)

Every yata/custom indicator wrapped — RSI with no down-moves, MACD with zero stddev, BB with zero variance, etc. all return NaN → poison downstream signals.

- Wrap each indicator update with `validate_finite()` that returns `Option<f64>`
- On NaN/Inf: reset indicator state for that SID + Telegram High `IndicatorNaNReset { sid, indicator }`
- Counter `tv_indicator_nan_resets_total{indicator,sid}`

**LoC: ~150.**

### 16. Order placement 5s REST timeout + retry policy (operator-locked 2026-05-13)

- Reqwest client built with `.timeout(Duration::from_secs(5))`
- DH-904 (rate limit) → exponential backoff 10s→20s→40s→80s, then give-up + CRITICAL
- DH-905 (input err), DH-906 (order err) → NEVER retry, log + alert operator
- DH-908 (server err) → retry once with 2s backoff
- DH-909 (network err) → retry 3 times with backoff

**LoC: ~100.**

### 17. Stop-loss-leg-cancelled detection (operator-locked 2026-05-13)

If operator manually cancels SL leg on Dhan UI, position becomes naked.

- Order-update WS handler: detect `Source=N` (Normal, not API) + `LegNo=2` (SL leg) + `status=CANCELLED`
- Within 30s: place fresh STOP_LOSS_MARKET order at the original SL price
- Telegram CRITICAL `NakedPositionDetectedSLReplaced`
- New audit row in `sl_replacement_audit`

**LoC: ~180.**

### 18. Boot-success Telegram positive ping (operator-locked 2026-05-13)

False-OK Rule 11: operator must SEE green before market opens.

- At 09:14:55 IST (just before pre-open buffer read), assemble status: `subscribed=N/222, token_ok, qdb_ok, ip_ok, preopen_buffer_filled=N/218`
- Send ONCE per trading day as `BootReadyConfirmation` (Info)
- If ANY check fails, send `BootDegraded` (Critical) instead with diagnostic

**LoC: ~80.**

### 19. End-of-day Telegram digest (operator-locked 2026-05-13)

Operator's "did today work?" question gets one daily answer.

- Fires at 15:31:30 IST after final bar seal
- Payload: P&L (realized/unrealized), signals fired, orders placed, fills, disconnects count, gap-fills count, cross-verify result (pass/fail), top 3 errors
- Telegram `DailyDigest` (Info if all green, High if any cross-verify mismatch, Critical if open positions remain)

**LoC: ~150.**

### 20. Tick-level integrity guards (operator-locked 2026-05-13)

- Reject `price ≤ 0` or `price > 10_000_000` → counter `tv_ticks_rejected_invalid_price_total{reason}` + audit row
- Reject `oi < 0` → same
- Aggregator validation at bar seal: `assert high >= low && open ∈ [low,high] && close ∈ [low,high]` — violation marks `corrupt=true` + Telegram + skip strategy decisions on that bar
- Counter `tv_aggregator_corrupt_bars_total{sid}`

**LoC: ~160.**

### 21. Self-trade prevention (operator-locked 2026-05-13)

SEBI rule: no wash trades. Within single account:

- Refuse SELL on a SID if we placed BUY in last 60s for same SID+ProductType (or vice versa)
- New constant `SELF_TRADE_COOLDOWN_SECS = 60`
- Telegram High `SelfTradePrevented` with both order details
- Audit row in `self_trade_audit` table

**LoC: ~100.**

### 22. GIFT NIFTY — DEFERRED TO PHASE 2 (operator-locked 2026-05-13, dropped same day)

**Decision:** drop from Phase 0. Re-evaluate during Phase 1 monitoring window or Phase 2A planning.

**Why dropped:**
- Dhan support unverified — adds Thu pre-flight risk to Friday build
- Per-segment market-hours table (`HashMap<ExchangeSegment, MarketHoursSchedule>`) adds ~150 LoC of complexity
- AWS schedule rewrite (08:30-17:30 → 06:30-15:45 IST) was reversible noise
- Overnight close can be inferred from morning gap on NIFTY itself once live — marginal alpha
- Lean MVP principle: ship 222 SIDs Friday, prove the architecture, then add GIFT in a clean PR after Phase 1 data validates the need

**What gets reverted:**
- AWS schedule stays at **08:30-17:30 IST Mon-Fri** (original plan)
- Single market-hours window `[09:15:00.000, 15:30:00.000)` for all 222 SIDs (no per-segment table)
- NSE_IFSC segment NOT added to `ExchangeSegment` enum
- No overnight close fetch
- Total SIDs: **222** (was 223)

**Re-evaluation trigger:** Phase 1 monitoring (22 trading days) shows operator wants gap-direction signal that NIFTY's own pre-open buffer doesn't already provide.

**LoC saved: ~580. Friday + Saturday build now: ~4,170 LoC across 22 items.**

---

### 22-OLD (PARKED — original GIFT NIFTY design, kept for future Phase 2 reference)

GIFT NIFTY = NIFTY 50 futures on NSE IX (GIFT City), USD-denominated, ~21h/day across 2 sessions.

**Architecture changes:**

| # | Sub-item | LoC |
|---|---|---:|
| 22a | **Per-segment market-hours table** `HashMap<ExchangeSegment, MarketHoursSchedule>` (replaces single `[09:15, 15:30)` constant) | ~150 |
| 22b | **NSE_IFSC segment** added to `ExchangeSegment` enum + binary parser (if Dhan supports) | ~50 |
| 22c | **GIFT NIFTY SecurityId** added to subscription planner, Ticker mode | ~30 |
| 22d | **AWS EventBridge schedule** revised to **06:30 - 15:45 IST Mon-Fri** (was 08:30-17:30) | ~20 (Terraform) |
| 22e | **Overnight-close REST fetch at boot** — fetch GIFT NIFTY 02:45 IST close via `/v2/charts/intraday`; store in `gift_nifty_overnight_audit` | ~150 |
| 22f | **09:15 open source** for GIFT NIFTY = last continuous tick before 09:15 (no pre-open buffer concept) | ~30 |
| 22g | **6 ratchet tests** — NSE_IFSC schedule, overnight fetch, 09:15 open source, 02:45 close cutoff, per-segment gate, GIFT skip on cross-verify if not in Dhan historical | ~150 |

**Pre-flight verification mandatory before Friday:**

| Check | Owner | Deadline |
|---|---|---|
| GIFT NIFTY in Dhan instrument master CSV — get SecurityId + segment code | operator | Thu 2026-05-14 |
| Test subscribe + first frame on Mac dev | operator | Thu |
| Verify `/v2/charts/intraday` returns GIFT NIFTY bars | operator | Thu |
| If unsupported → defer to Phase 2 OR build separate NSE IX feed (separate plan) | operator | Thu |

**Schedule choice locked: Option B — extend AWS to 06:30-15:45 IST. Captures Session 1 (overlaps NSE) + overnight close fetched once at boot via REST. Skips Session 2 (17:00-02:45 IST). AWS cost delta: +~₹20/mo.**

**LoC: ~580 across 7 sub-items. [PARKED — see decision above; not part of Phase 0 Friday build.]**

**Total Friday + Saturday build (AFTER GIFT NIFTY DROP): ~4,170 LoC across 22 top-level items. Approximately 1.5-2 focused days.**

### 23. Prev-close mode mix — fix Ticker-only blind spot (operator-locked 2026-05-13)

**Bug:** Phase 0 was Ticker-only for all 222 SIDs. Per Dhan Ticket #5525125 binary protocol:

| Segment | Ticker | Quote | Full | Code 6 standalone |
|---|:---:|:---:|:---:|:---:|
| IDX_I (NIFTY/BANKNIFTY/SENSEX/VIX) | ❌ no field | ❌ Dhan rejects on IDX_I | ❌ Dhan rejects on IDX_I | ✅ bytes 8-11 f32 (auto-fires) |
| **NSE_EQ (218 F&O stocks)** | ❌ no field | ✅ **bytes 38-41 f32 (`close`=prev day)** | ✅ bytes 50-53 | ❌ never sent |
| NSE_FNO | ❌ no field | ✅ bytes 38-41 | ✅ bytes 50-53 | ❌ never sent |

**Impact:** 218 stocks would have NO prev_close in Phase 0. Gap detection / % change / risk filter all broken on stocks.

**Fix — MIXED-MODE subscription:**

| Segment | NEW Phase 0 mode | Source of prev_close |
|---|---|---|
| IDX_I (4 SIDs) | Ticker | Code 6 packet (auto-fires) |
| **NSE_EQ (218 SIDs)** | **Quote (50-byte)** | bytes 38-41 of every Quote packet |
| NSE_IFSC GIFT NIFTY (1 SID) | Ticker (default; verify Thu) | TBD |

**Bandwidth impact:** ~78 MB/day (Ticker only) → ~250 MB/day (mixed). Trivial on t3.medium. AWS egress unchanged (we receive, not send).

**Belt-and-suspenders — REST boot-time prev_close fetch:**

- `/v2/charts/historical?interval=1d` for previous trading day, all 222 SIDs
- One-shot at 06:30 IST boot
- ~222 calls @ 5/sec = ~45s
- Populates RAM `prev_close_cache` BEFORE market opens
- Source field: `PrevCloseSource::{ Code6Packet, QuotePacket, RestHistorical, Fallback }`
- New Telegram `PrevCloseMissingAtMarketOpen` (Critical) at 09:14:55 IST if any SID lacks prev_close → skip strategy on that SID

**Constants pinned:**
- `PREV_CLOSE_BOOT_FETCH_TIME_IST = 06:30:00`
- `PREV_CLOSE_FALLBACK_DEADLINE_IST = 09:14:55`

**Banned-pattern hook:** Ticker subscription on NSE_EQ → REJECT at commit (anti-regression).

**Audit table** `prev_close_audit` extended with `source` column. DEDUP UPSERT KEYS `(trading_date_ist, security_id, exchange_segment)`.

**Ratchet tests (mandatory):**
- `test_prev_close_source_for_idx_i_is_code_6_packet`
- `test_prev_close_source_for_nse_eq_is_quote_bytes_38_41`
- `test_rest_historical_fetch_populates_cache_at_boot`
- `test_prev_close_missing_at_market_open_fires_critical_telegram`
- `test_banned_pattern_rejects_ticker_mode_on_nse_eq`

**LoC: ~630 across 6 sub-items.**

**Total Friday + Saturday build (FINAL, after GIFT NIFTY drop): ~4,170 LoC across 22 top-level items (item 22 = GIFT NIFTY, PARKED). ~1.5-2 focused days.**

---

## Phase 0 FINAL summary (after GIFT NIFTY drop, 2026-05-13 final lock)

| Metric | Value |
|---|---|
| Top-level items | **22** (item 22 PARKED, items 1-21 + 23 = 22 active) |
| Total LoC | **~4,170** |
| Friday + Saturday | **1.5-2 focused days** |
| Total SIDs subscribed | **222** (3 IDX_I + INDIA VIX + 218 NSE_EQ F&O underlyings) |
| Mode mix | IDX_I Ticker + NSE_EQ Quote |
| AWS schedule | **08:30-17:30 IST Mon-Fri** (original — GIFT-driven revision reverted) |
| AWS cost | **~₹1,252/mo** (original — 75% under cap) |
| Market hours | **Single window** `[09:15:00.000, 15:30:00.000)` with 60s wall-clock grace |

---

### 24. Order Update WebSocket — 7-layer observability (operator-locked 2026-05-13)

`wss://api-order-update.dhan.co` is a separate WS endpoint streaming JSON order events (MsgCode 42). Today it's connected but has zero disconnect-class observability — same gap main-feed had before this session. Phase 0 closes that gap.

**Components:**

| Layer | Spec |
|---|---|
| L1 Prom counters | `tv_order_update_disconnects_total{reason}`, `tv_order_update_reconnect_attempts_total`, `tv_order_update_messages_received_total{msg_code}`, `tv_order_update_source_filter_total{source}` |
| L2 Prom gauges | `tv_order_update_connected` (0/1), `tv_order_update_last_message_age_secs` |
| L3 Tracing spans | `#[instrument]` on connect, read_loop, parse_message, route_by_source |
| L4 Loki logs | `error!(code=ORDER-UPDATE-WS-01, ...)` on disconnect; `info!(code=ORDER-UPDATE-RECV-01, ...)` on every message |
| L5 Telegram | `OrderUpdateWsDisconnected` (High in-market, Low off-hours), `OrderUpdateWsReconnected` (Info), `OrderUpdateActivityWatchdogTrip` (High) |
| L6 Grafana | panel cites `increase(tv_order_update_disconnects_total[5m])` + message rate panel |
| L7 Audit | `order_update_ws_audit` table with `(ts, event, attempt_num, reason_code, duration_ms)` — DEDUP UPSERT KEYS `(ts, event)` |

**Activity watchdog:** Dhan goes silent on idle accounts. Watchdog fires `OrderUpdateActivityWatchdogTrip` if no message in 4h (per `live-order-update.md` rule); during dry_run this will fire frequently — that's expected because we don't place orders.

**Source filter (Source=P vs Source=N):**
- `Source=P` = our API orders → process
- `Source=N` = orders placed on Dhan web/app → log to `external_order_audit` table + Telegram `ExternalOrderDetected` (High) so operator knows
- Anti-conflict: if `Source=N` order modifies a SID we have a strategy position on → CRITICAL alert

**Ratchet tests (mandatory):**
- `test_order_update_disconnect_writes_audit_row`
- `test_order_update_source_p_routes_to_strategy_tracker`
- `test_order_update_source_n_routes_to_external_audit`
- `test_order_update_activity_watchdog_4h_threshold`
- `test_order_update_reconnect_with_subscribe_rx_guard_preservation`
- `test_msg_code_42_required_for_auth_message`

**LoC: ~350 across 4 files.**

---

### 25. P&L tracker scaffolding (operator-locked 2026-05-13)

In-memory P&L state. In dry_run, every "would-be order" runs through tracker and logs hypothetical P&L; in live mode (Phase 2A) the same code path uses real fills.

**State shape:**

```rust
pub struct PnlState {
    realized_today: f64,           // sum of all closed positions' P&L
    unrealized_open: HashMap<(u32, ExchangeSegment), OpenPosition>,
    daily_total: f64,              // realized + sum(unrealized)
    daily_loss_floor: f64,         // operator-configured kill trigger
    daily_profit_target: Option<f64>,
    high_water_mark: f64,
    drawdown_from_hwm: f64,
}

pub struct OpenPosition {
    entry_price: f64,
    entry_ts: i64,
    quantity: u32,
    lot_size: u32,
    side: Side,
    correlation_id: String,
    sl_price: f64,
    target_price: f64,
}
```

**Hot-path math** (called on every tick for SIDs we have open positions on):

```rust
fn mark_to_market(&mut self, sid: u32, segment: ExchangeSegment, ltp: f64) {
    if let Some(pos) = self.unrealized_open.get_mut(&(sid, segment)) {
        let pnl = (ltp - pos.entry_price) * pos.quantity as f64 * pos.lot_size as f64
                * match pos.side { Side::Long => 1.0, Side::Short => -1.0 };
        // O(1), zero allocation
    }
}
```

**Dry-run behavior:** every strategy signal computes hypothetical entry/SL/target, registers as `unrealized_open` row, marks-to-market on every tick, logs to `dry_run_pnl_audit` table at 15:31:30 IST EOD.

**Live behavior (Phase 2A):** flip `dry_run = false` config flag — same code path, real fills update `realized_today`.

**EOD audit table** `pnl_audit`:
```sql
CREATE TABLE IF NOT EXISTS pnl_audit (
  ts TIMESTAMP,
  trading_date_ist STRING,
  realized DOUBLE,
  unrealized DOUBLE,
  daily_total DOUBLE,
  high_water_mark DOUBLE,
  drawdown_pct DOUBLE,
  mode STRING                    -- dry_run | live
) DEDUP UPSERT KEYS(trading_date_ist, mode);
```

**Telegram variants:**
- `DailyPnlSnapshot` (Info, daily EOD)
- `DailyLossFloorBreached` (Critical — fires once when daily_total < daily_loss_floor; in live mode triggers kill switch)
- `DailyProfitTargetHit` (High — operator-info)
- `DrawdownFromHwmExceeded` (High — 5% drawdown threshold)

**Ratchet tests:**
- `test_pnl_mark_to_market_is_o1_per_tick`
- `test_dry_run_pnl_audit_writes_eod_snapshot`
- `test_daily_loss_floor_breach_fires_critical`
- `test_pnl_state_resets_at_ist_midnight`
- `test_high_water_mark_monotonic`

**LoC: ~250 across 3 files.**

---

### 26. Signal + decision audit tables (operator-locked 2026-05-13)

Phase 1 monitoring (22 trading days) needs DATA to prove the strategy works. Today: strategy generates signals but no persistent audit. Fix:

**Two audit tables:**

```sql
CREATE TABLE IF NOT EXISTS signal_audit (
  ts TIMESTAMP,                        -- signal fire time
  trading_date_ist STRING,
  security_id INT,
  exchange_segment SYMBOL,
  strategy_id STRING,
  indicator_state STRING,              -- JSON snapshot: rsi=58.3, macd_hist=2.1, ...
  spot_price DOUBLE,                   -- LTP at signal time
  would_be_action STRING,              -- BUY_CE | BUY_PE | EXIT | HOLD
  would_be_strike DOUBLE,
  would_be_entry_price DOUBLE,         -- from /optionchain
  would_be_sl_price DOUBLE,
  would_be_target_price DOUBLE,
  reason STRING                        -- text explanation
) DEDUP UPSERT KEYS(ts, security_id, strategy_id);

CREATE TABLE IF NOT EXISTS decision_audit (
  ts TIMESTAMP,
  trading_date_ist STRING,
  security_id INT,
  exchange_segment SYMBOL,
  strategy_id STRING,
  no_signal_reason STRING              -- "vix_too_high" | "stale_tick" | "outside_session" | "indicator_not_ready" | "self_trade_cooldown"
) DEDUP UPSERT KEYS(ts, security_id, strategy_id, no_signal_reason);
```

**Sampling rule for `decision_audit`:** writing every "no signal" decision = ~5M rows/day. Instead: sample 1 row per minute per SID per strategy (~10K rows/day across 218 stocks × 1 strategy × 60 min × 7.5h = ~98K — still manageable).

**`signal_audit` writes ONLY on signal fire** — naturally bounded to actual signals.

**Phase 1 monitoring queries** these tables answer:
- "How many BUY_CE signals fired in 22 days?"
- "What was avg `spot_price` at entry vs the next 30m price?"
- "Which strategy fires most often?"
- "What's the top no_signal_reason? (tells us where strategy is being too strict)"

**Ratchet tests:**
- `test_signal_audit_dedup_includes_strategy_id`
- `test_decision_audit_sampled_at_one_per_minute_per_sid_strategy`
- `test_dry_run_writes_to_signal_audit_even_without_orders`

**LoC: ~200 across 2 files.**

---

### 27. Phase 0 documentation bundle (operator-locked 2026-05-13)

8 markdown files. Total ~10,000 words. Sunday afternoon work after code lands.

| # | File | Purpose | Words |
|---|---|---|---:|
| 27a | `docs/phases/phase-0-readme.md` | Architecture overview for any new operator / Claude session | ~1,500 |
| 27b | `docs/runbooks/aws-deployment.md` | Terraform setup, EC2 t3.medium provision, EIP attach, EBS gp3 30GB, IAM role, EventBridge cron 08:30-17:30 IST, security group rules | ~2,000 |
| 27c | `docs/runbooks/operator-daily-startup.md` | 08:30 IST checklist: check Telegram for BootReadyConfirmation, glance Grafana dashboard, confirm 222/222 SIDs subscribed, confirm prev_close populated | ~800 |
| 27d | `docs/runbooks/troubleshooting.md` | "What to do when X fails" — one section per ErrorCode (BOOT-01, WS-GAP-01, PHASE2-01, AUTH-GAP-01, PREVCLOSE-01, GAP-FILL-01, OPEN-PRICE-WARN, SELF-TRADE-01, etc.) | ~2,500 |
| 27e | `docs/runbooks/kill-switch.md` | When/how to activate emergency halt: Phase 2A only; manual flag flip in config + Telegram notify; SEBI implications | ~600 |
| 27f | `docs/runbooks/backtest-runner.md` | Mac-side workflow: fetch Dhan historical → import QuestDB → sweep strategy params → walk-forward validate → commit winning params | ~1,200 |
| 27g | `docs/runbooks/phase-1-monitoring-rubric.md` | 22-day dry_run pass/fail criteria: disconnect rate < 5/day, gap-fill success > 99%, cross-verify match > 99.5%, signal fire rate within expected range, no NaN resets, no orphan positions | ~800 |
| 27h | `.claude/rules/project/phase-0-architecture.md` | Auto-loaded rule for future Claude Code sessions: "Phase 0 is mixed-mode (IDX_I Ticker + NSE_EQ Quote), single-window market hours [09:15, 15:30) + 60s grace, 222 SIDs, dual-gate, seal-then-fetch gap-fill, etc. Do NOT add depth/greeks/movers — they're Phase 2." | ~600 |

**Total: ~10,000 words across 8 files.**

**Mechanical enforcement:** new banned-pattern hook checks `.claude/plans/active-plan-phase-0*.md` exists during Friday build; if not, plan-verify fails.

**Documentation ratchets** (yes, even docs get ratchets):
- `test_phase_0_readme_mentions_all_22_active_items`
- `test_troubleshooting_runbook_has_section_for_every_error_code_in_phase_0`
- `test_aws_deployment_runbook_lists_all_terraform_resources`

---

**Total Friday + Saturday + Sunday build (FINAL FINAL): ~4,970 LoC across 26 top-level items + ~10,000 words across 8 docs.**

## Phase 0 — TRUE FINAL summary (after items 24-27 added)

| Metric | Value |
|---|---|
| **Top-level items** | **26** (22 active + 1 PARKED-GIFT + 3 new = items 24/25/26 + 27 docs) |
| **Total LoC** | **~4,970** |
| **Total docs** | **8 files, ~10,000 words** |
| **Total SIDs** | **222** |
| **Mode mix** | IDX_I Ticker + NSE_EQ Quote |
| **AWS** | t3.medium, 08:30-17:30 IST Mon-Fri, EIP, gp3 30GB |
| **AWS cost** | ~₹1,252/mo (75% under cap) |
| **Effort** | Friday 8h + Saturday 9h + Sunday AM 4h code + PM 6h docs = ~27 focused hours over 3 days |
| **AWS deploy** | Monday 2026-05-18 |
| **Phase 1 monitoring** | Mon 2026-05-19 → Mon 2026-06-13 (22 trading days) |

---

### 28. Cross-verify mismatch — Dhan historical OVERWRITES local (RAM + DB), operator-locked 2026-05-13

**The mechanical rule:**

> **Dhan historical is the SOURCE OF TRUTH at cross-verify time. Any mismatch found by cross-verify (09:16:05 AM IST for the 09:15 bar, or 15:31 IST EOD for all bars) results in: (a) DEDUP UPSERT overwrite of `candles_1m` row with Dhan's version, (b) overwrite of in-memory RAM bar cache with Dhan's version, (c) Telegram CRITICAL `BarMismatchCorrectedFromHistorical`, (d) audit row in `bar_correction_audit`.**

**Two trigger paths:**

| When | What's checked | Source of truth |
|---|---|---|
| **09:16:05 AM IST** | The 09:15 1m bar across all 222 SIDs (open-price correctness focus) | Dhan `/v2/charts/intraday` |
| **15:31 IST EOD** | ALL 1m bars of the trading day across all 222 SIDs | Dhan `/v2/charts/intraday` |

**Correction flow per mismatched SID:**

```
For each SID where local.candle_1m[ts] != dhan_historical.candle_1m[ts]:
  1. DEDUP UPSERT into candles_1m with Dhan's (open, high, low, close, volume)
     - DEDUP KEYS (security_id, exchange_segment, ts) already exist
     - source column set to "dhan_historical_overwrite"
  2. Update RAM bar_cache: papaya::HashMap<(security_id, segment, ts), Bar>
     - Atomic replace — concurrent readers see either old or new, never torn
  3. Write to bar_correction_audit:
     (ts_corrected, security_id, segment, bar_ts, field, old_value, new_value, source_of_correction)
  4. Telegram CRITICAL BarMismatchCorrectedFromHistorical with up to 10 sample deltas
  5. Counter tv_bar_corrections_total{field} increment
```

**`bar_correction_audit` table schema:**

```sql
CREATE TABLE IF NOT EXISTS bar_correction_audit (
  ts_corrected TIMESTAMP,             -- when correction was applied
  trading_date_ist STRING,
  bar_ts TIMESTAMP,                   -- the affected bar minute (e.g. 09:15)
  security_id INT,
  exchange_segment SYMBOL,
  field STRING,                       -- open | high | low | close | volume
  old_value DOUBLE,
  new_value DOUBLE,
  source_of_correction STRING,        -- "cross_verify_09_16" | "cross_verify_eod" | "gap_fill"
  cross_check_id STRING
) DEDUP UPSERT KEYS(trading_date_ist, bar_ts, security_id, exchange_segment, field, source_of_correction);
```

**Strategy decision gate for the 09:15 bar (anti-stale-signal):**

The 09:15 bar seals at 09:16:00.005 IST; cross-verify completes around 09:16:50 IST. To prevent strategy firing on an unverified 09:15 bar that may be wrong:

- Constant `STRATEGY_GATE_FOR_09_15_BAR_UNTIL_IST = 09:16:55` (5s safety margin after cross-verify)
- Strategy evaluator MUST skip signals based on the 09:15 bar until 09:16:55 IST
- After 09:16:55: strategy reads the (possibly corrected) bar and decides
- For all other bars (09:16, 09:17, ...): no gate — strategy fires immediately on bar seal

**Why this rule is safe:**

| Concern | Resolution |
|---|---|
| Strategy misses early signal | 09:15 bar gate is only 55 seconds. Pre-09:15 underlying ticks already captured. Other entry timeframes (5m/15m/1h) unaffected. |
| Dhan historical itself is wrong | Two cross-verify points (09:16 + 15:31) catch mistake. EOD cross-verify hits ALL bars including 09:15 — if 09:16 overwrite was wrong, 15:31 re-overwrites with the consensus. |
| Race: correction happens DURING strategy read | papaya HashMap is lock-free; reader sees consistent snapshot (atomic pointer swap). DB DEDUP UPSERT is ACID per QuestDB. |
| Concurrent corrections on same bar | DEDUP UPSERT KEYS include `source_of_correction` — both 09:16 and EOD corrections preserved as separate audit rows; last write wins in `candles_1m`. |

**Constants pinned:**

| Constant | Value |
|---|---|
| `BAR_CORRECTION_LATENCY_BUDGET_SECS` | 50 (cross-verify must finish within 50s after bar seal at 09:16:05) |
| `STRATEGY_GATE_FOR_09_15_BAR_UNTIL_IST` | 09:16:55 |
| `MAX_BAR_CORRECTIONS_PER_DAY_THRESHOLD` | 5 (above this, alert operator that something systematic is wrong) |

**New Telegram events:**

| Event | Severity | Trigger |
|---|---|---|
| `BarMismatchCorrectedFromHistorical` | Critical | Any single bar overwritten — payload: SID list, field deltas, count |
| `BarCorrectionRateAbove5PerDay` | High | More than 5 corrections in a single trading day — systemic issue |
| `CrossVerifyMissingDhanHistorical` | High | Dhan REST returned 0 rows for an SID at cross-verify — can't compare; flag for next day |

**Ratchet tests (mandatory):**

| Test | Pins |
|---|---|
| `test_bar_correction_overwrites_candles_1m_via_dedup_upsert` | DB write path |
| `test_bar_correction_overwrites_ram_bar_cache` | RAM cache update |
| `test_bar_correction_audit_row_written_per_corrected_field` | Per-field forensic granularity |
| `test_strategy_gate_blocks_09_15_signals_until_09_16_55_ist` | Anti-stale-signal |
| `test_bar_correction_concurrent_writes_dont_tear_data` | loom concurrency |
| `test_more_than_5_corrections_per_day_fires_high_telegram` | Systemic alert |
| `test_eod_cross_verify_also_uses_overwrite_path` | Both trigger points share path |

**LoC: ~200 across 3 files.**

**Total Friday + Saturday + Sunday build: ~5,170 LoC across 27 active items + 8 docs.**

---

### 29. QuestDB schema changes (operator-locked 2026-05-13, REVISED late-evening for 6-TF + no-1s + no-matview lock)

**Timeframe set LOCKED:** `1m`, `3m`, `5m`, `15m`, `1h`, `1d` — exactly 6 candle base tables.
- **NO `candles_1s` table** — wasteful and never read on hot path. Aggregator computes 1m directly from ticks in RAM.
- **NO materialized views** — every candle TF is its own base table written by the in-RAM aggregator. Matviews are never used. Reason: hot path is RAM-only (per `aws-budget.md` rule 12); matviews give zero benefit and create offset/lag traps.

**Existing tables — delta:**
- `ticks`: NO new columns (prev_close from Quote bytes 38-41 stored in `prev_close_audit`, not per-tick)
- `candles_1m`/`_3m`/`_5m`/`_15m`/`_1h`/`_1d`: **ADD COLUMN `source SYMBOL`** (`live_aggregated` | `dhan_historical_overwrite` | `gap_fill_backfill`)
- `candles_1h` uses **ALIGN TO CALENDAR WITH OFFSET '00:15'** so buckets match Dhan `/charts/intraday?interval=60` (09:15 IST market-open aligned). All other TFs use `'00:00'` IST midnight alignment.
- All via idempotent `ALTER ADD COLUMN IF NOT EXISTS` at boot per schema self-heal pattern

**15 NEW audit tables** (all with DEDUP UPSERT KEYS, created at boot if missing):

| Table | DEDUP KEYS |
|---|---|
| `gap_fill_audit` | `(trading_date_ist, bar_minute, trigger_event)` |
| `last_tick_audit` | `(trading_date_ist, security_id, exchange_segment, bar_minute)` |
| `open_price_audit` | `(trading_date_ist, security_id, exchange_segment)` |
| `prev_close_audit` (with `source` col) | `(trading_date_ist, security_id, exchange_segment)` |
| `auth_renewal_audit` | `(trading_date_ist, renewal_ts)` |
| `live_instance_lock` | `(client_id)` |
| `orphan_position_audit` | `(trading_date_ist, security_id, exchange_segment)` |
| `sl_replacement_audit` | `(trading_date_ist, original_order_id)` |
| `external_order_audit` | `(ts, exchange_order_id)` |
| `order_update_ws_audit` | `(ts, event)` |
| `pnl_audit` | `(trading_date_ist, mode)` |
| `signal_audit` | `(ts, security_id, strategy_id)` |
| `decision_audit` | `(ts, security_id, strategy_id, no_signal_reason)` |
| `bar_correction_audit` | `(trading_date_ist, bar_ts, security_id, exchange_segment, field, source_of_correction)` |
| `self_trade_audit` | `(trading_date_ist, security_id, exchange_segment, ts)` |

**Mat views removed entirely:** Wave-5 matviews `candles_1m/5m/15m/30m/1h/2h/3h/4h/1d` and `candles_1s` base table are **DROPPED** in Phase 0 boot. Replaced by 6 native base tables (1m/3m/5m/15m/1h/1d) written directly by the in-RAM aggregator. `movers_1s`/`_1m`/`option_movers` matviews stay DROPPED (no use case in Phase 0; reintroduce only when derivatives come back).

**6-table column contract (identical schema for all 6 candle tables — folds Z+ items M8 + L2):**

```
segment              SYMBOL
security_id          LONG
open / high / low / close   DOUBLE
volume               LONG
oi                   LONG       -- always 0 for NSE_EQ in Phase 0 (no derivatives subscribed)
tick_count           LONG       -- Z+ M8: was INT; LONG avoids 2.1B i32 wrap on multi-month re-aggregation
volume_cum_day_at_end        LONG
prev_day_close               DOUBLE
prev_day_oi                  LONG
phase                SYMBOL CAPACITY 16   -- Z+ M1/H2: values include live, market_open_partial, boot_partial, eod
close_pct_from_prev_day      DOUBLE     -- item 30 (Wave-5 naming kept)
oi_pct_from_prev_day         DOUBLE     -- NULL in Phase 0; downstream must COALESCE (Z+ C4)
volume_pct_from_prev_day     DOUBLE
source               SYMBOL CAPACITY 8    -- Z+ L2: live_aggregated | dhan_historical_overwrite | gap_fill_backfill
ts                   TIMESTAMP    (designated, IST)
```

**Bucket boundary rule LOCKED (Z+ C2):**
- `BUCKET_BOUNDARY_RULE = "[start, end)"` (half-open right) — a tick at exactly 09:16:00.000 lands in the 09:16 bucket, NOT the 09:15 bucket
- Constant: `crates/common/src/constants.rs::BUCKET_BOUNDARY_RULE`

**Close-of-session forced seal LOCKED (Z+ C1):**
- `MARKET_CLOSE_FORCED_SEAL_IST = "15:30:00"` — every TF (including 1h's 15:15-15:30 partial bar) gets force-sealed at 15:30:00 IST exchange-ts, NOT at its nominal bar-end (would be 16:15 for 1h)
- Applies to all 6 TFs

**Alignment offset constants LOCKED (Z+ L1):**
- `CANDLE_1H_ALIGN_OFFSET = "00:15"` — NSE market-open aligned, matches Dhan `/charts/intraday?interval=60`
- `CANDLE_DEFAULT_ALIGN_OFFSET = "00:00"` — IST midnight, used by 1m/3m/5m/15m/1d

**Boot ordering constraint LOCKED (Z+ H4):**
- All 24 ALTER ADD COLUMN statements MUST complete BEFORE the in-RAM aggregator + ILP-writer task spawns
- Boot sequence: QuestDB ready → DDL block → ALTER block → instrument master load → aggregator spawn → WebSocket subscribe

**DEDUP UPSERT KEYS (6 candle tables):** `(ts, security_id, segment)` — identical across all 6. **No `timeframe` column** because each TF lives in its own table; the table name IS the timeframe.

**REST boot fetch time correction:** `PREV_CLOSE_BOOT_FETCH_AT_APP_BOOT = true` (~08:31 IST, post AWS instance start at 08:30 + 30-60s QuestDB readiness probe). Was incorrectly `06:30 IST` before — AWS instance not up at 06:30. Must complete by `PREV_CLOSE_FALLBACK_DEADLINE_IST = 09:14:55`.

**LoC: ~250** (DDL + 15 schema ratchet tests).

**Total build (after item 29): ~5,420 LoC across 28 active items + 8 docs.**

---

### 30. Per-minute % change from prev_close stored in candle tables (operator-locked 2026-05-13, REVISED late-evening to 6 TFs + Wave-5 column name)

Every sealed bar (1m/3m/5m/15m/1h/1d) gets `close_pct_from_prev_day` computed at seal time and stored as a column. Strategy hot-path reads it directly from RAM — zero math on every tick. Dashboard SQL is one column. Backtest reads identical-shape rows.

**Column name:** `close_pct_from_prev_day` (Wave-5 §K-L12 naming, kept to avoid fragmenting the codebase). Sister columns `oi_pct_from_prev_day` and `volume_pct_from_prev_day` are present in the schema (NULL in Phase 0 — no derivatives, equity Quote packet has no OI).

**Schema delta (ALTER ADD COLUMN IF NOT EXISTS, idempotent at boot):**

| Table | New columns |
|---|---|
| `candles_1m` | `close_pct_from_prev_day DOUBLE`, `oi_pct_from_prev_day DOUBLE`, `volume_pct_from_prev_day DOUBLE` |
| `candles_3m` | same |
| `candles_5m` | same |
| `candles_15m` | same |
| `candles_1h` | same |
| `candles_1d` | same |

**DEDUP UPSERT KEYS unchanged** — `(ts, security_id, segment)` already uniquely identifies each row in each table. The 3 pct fields are new attributes on the existing row, NOT new identity dimensions.

**Prev-close source (two-source routing, Dhan Ticket #5525125 confirmed):**

| Segment | Source of prev_close | Bytes |
|---|---|---|
| IDX_I (NIFTY, BANKNIFTY, SENSEX, INDIA VIX — 4 SIDs in Ticker mode) | Standalone PrevClose packet (code 6, 16 bytes) | `buf[8..12]` f32 LE — auto-fires once per session at subscribe |
| NSE_EQ (218 F&O stocks in Quote mode) | `close` field of Quote packet (NOT today's running close — Dhan-mislabeled, actually previous session's close) | `buf[38..42]` f32 LE — read on first Quote tick, cache, ignore subsequent |

**Boot-time cache loader** (PREVCLOSE-04 / Wave-5 §K-L13): `populate_prev_day_cache_at_boot` reads `previous_close` table from QuestDB (7-day lookback) so RAM cache is warm BEFORE first tick. Live packets overwrite stale cache within seconds of subscribe.

**Cache primitive LOCKED (Z+ H6 + H8 + M9):**
- `prev_close_cache: Arc<papaya::HashMap<(u32, ExchangeSegment), f64>>` — composite key per I-P1-11 (no `HashMap<u32, _>`), lock-free read via `papaya`, NOT `Mutex` / `RwLock`
- Existing buggy `index_prev_close_cache: HashMap<u32, f32>` at `tick_processor.rs:537` REPLACED with composite key
- Banned-pattern hook category 5 explicitly scans this site

**Per-tick aggregator bucket structure LOCKED (Z+ M9):**
- `aggregator_state: Arc<papaya::HashMap<(u32, ExchangeSegment), [Bucket; 6]>>` — fixed-array indexed by `TimeframeIdx as usize`, not 6 hash probes per tick
- `TimeframeIdx`: `M1=0, M3=1, M5=2, M15=3, H1=4, D1=5`

**ILP writer fan-out LOCKED (Z+ C5):**
- Per-SID seal writes via bounded `mpsc::channel(SEAL_BURST_CHANNEL_CAPACITY = 2048)` to a dedicated ILP-writer tokio task
- Reused `questdb_rs::Buffer` (no per-row allocation)
- Backpressure: rescue ring overflow path per `aws-budget.md` rule (5M-tick capacity)
- Criterion bench `bench_seal_burst_892_rows_p99_under_5ms` enforces 5ms budget at 09:30:00 IST worst-case fan-out

**Prev-close bounds guard LOCKED (Z+ H9):**
- BEFORE writing prev_close to RAM cache OR `previous_close` ILP table: reject if `!is_finite() OR <= 0.0 OR > 10_000_000.0`
- Applies to both IDX_I code-6 packet path and NSE_EQ first-Quote-tick path

**NSE_EQ first-tick poisoning guard LOCKED (Z+ C3):**
- `NSE_EQ_FIRST_TICK_PREV_CLOSE_DRIFT_LIMIT = 10.0` (10× drift from boot-loaded QuestDB value)
- If first-tick `close` field is `< 1e-9` OR `> 10×` the boot-loader value: REJECT (do not lock cache); wait for next tick
- Counter: `tv_nse_eq_first_tick_rejected_total{reason="zero"|"implausible"}`

**Cross-verify recompute LOCKED (Z+ H5):**
- When item 28 cross-verify corrects a bar's `close`, it ALSO revalidates `prev_close` against Dhan historical's previous-trading-day `_1d.close`
- If prev_close drifted: overwrite cache + `previous_close` table + recompute pct

**1d self-referential lookup LOCKED (Z+ H7 + M4):**
- `candles_1d` pct at seal time reads ONLY from `prev_close_cache` (RAM); NO QuestDB SELECT during business hours
- Yesterday's `_1d.close` rehydrated into the same cache at boot via `populate_prev_day_cache_at_boot`

**EOD `previous_close` write window LOCKED (Z+ H3 + M6):**
- Write window: `[15:30:30, 15:31:30]` IST — single edge-trigger fire
- DEDUP UPSERT KEYS `(trading_date_ist, security_id, exchange_segment)` guarantees idempotency
- Writer spawned exactly once from main.rs BEFORE any parallel task; ratchet `test_prev_close_writer_spawned_exactly_once_at_boot`

**INDIA VIX code-6 verification LOCKED (Z+ H1):**
- Pre-Friday smoke test: subscribe VIX (security_id TBD from CSV) alone, verify code-6 packet receipt within 60s
- If verified: continue with same IDX_I routing as NIFTY/BANKNIFTY/SENSEX
- If NOT verified: fallback to QuestDB `previous_close` row only; flag `VIX_PREV_CLOSE_SOURCE = "questdb_fallback"` in the cache entry
- Smoke test ratchet wired into CI; result captured in `vix_code6_smoke_audit` table

**DDL error escalation LOCKED (Z+ M7):**
- `ensure_previous_close_table` DDL failures upgrade `warn!` → `error!(code = ErrorCode::PrevCloseDdl04.code_str(), ...)` to route through Telegram per `error_level_meta_guard.rs` Rule 5

**Computation:**

```
close_pct_from_prev_day = (bar.close - prev_close_cache[(sid, segment)]) / prev_close * 100.0
```

| Trigger | Action |
|---|---|
| Bar seal (every 1m/3m/5m/15m/1h/1d boundary) | Compute + ILP write into candle row |
| `prev_close_cache` miss for that SID | Write NULL + Telegram `PctFromPrevCloseSkipped` (Low — should be impossible after 09:14:55 cutoff) |
| Cross-verify overwrite (item 28) corrects bar.close | Re-compute pct using corrected close + same cached prev_close |
| Daily candle (`candles_1d`) | Self-referential — `prev_close = yesterday's candles_1d.close` lookup at session end |
| `prev_close < 1e-9` (degenerate edge) | Write NULL + log Info — avoids div-by-zero |

**Constants pinned:**
- `PCT_FROM_PREV_CLOSE_DEGENERATE_THRESHOLD = 1e-9`
- `CANDLE_TIMEFRAMES_LOCKED = ["1m", "3m", "5m", "15m", "1h", "1d"]` (6 entries — adding/removing fails build)

**Ratchet tests (mandatory — 44 total: 17 base + 27 from Z+ review item 31):**

Base 17 (pct + routing + dedup invariants):
- `test_pct_computed_at_bar_seal_for_1m`
- `test_pct_computed_for_all_6_timeframes`
- `test_pct_recomputed_after_cross_verify_overwrite_item_28`
- `test_pct_skipped_when_prev_close_below_degenerate_threshold`
- `test_pct_skipped_when_prev_close_cache_miss`
- `test_alter_add_pct_columns_idempotent_on_boot`
- `test_candles_1d_pct_uses_yesterday_close_self_referential`
- `test_candle_dedup_key_is_ts_security_id_segment_no_timeframe_column`
- `test_idx_i_prev_close_routes_through_code6_packet`
- `test_nse_eq_prev_close_routes_through_quote_close_field_bytes_38_41`
- `test_nse_eq_first_seen_only_writes_cache_once_per_day`
- `test_india_vix_uses_idx_i_code6_routing`
- `test_no_candles_1s_base_table_in_ddl`
- `test_no_materialized_views_in_storage_crate` (greps for `CREATE MATERIALIZED VIEW` — must return zero hits)
- `test_no_db_select_in_indicator_strategy_risk_hot_path` (banned-pattern category extension)
- `test_exactly_six_candle_base_tables_exist`
- `test_candle_1h_uses_0915_ist_offset_for_dhan_parity`

Z+ CRITICAL fixes (5):
- `test_candles_1h_final_bar_seals_at_15_30_not_16_15` (C1)
- `test_tick_at_exact_boundary_falls_into_next_bucket` (C2)
- `test_nse_eq_first_tick_rejects_zero_or_implausible_value` (C3)
- `test_no_strategy_or_dashboard_query_references_oi_pct_without_coalesce` (C4)
- `test_minute_boundary_burst_does_not_block_ws_read_loop` + Criterion `bench_seal_burst_892_rows_p99_under_5ms` (C5)

Z+ HIGH fixes (9):
- `test_vix_code6_smoke_or_fallback_path` (H1)
- `test_partial_boot_bars_marked_boot_partial` (H2)
- `test_previous_close_eod_writes_exactly_once_per_day` (H3)
- `test_alter_block_completes_before_aggregator_spawn` (H4)
- `test_cross_verify_revalidates_prev_close_not_just_bar_close` (H5)
- `test_prev_close_cache_uses_papaya_not_mutex` (H6)
- `test_candles_1d_self_referential_does_not_select_at_seal_time` (H7)
- `test_index_prev_close_cache_uses_composite_key_not_u32_alone` (H8)
- `test_prev_close_rejects_nan_inf_zero_and_extreme` (H9)

Z+ MEDIUM fixes (9):
- `test_3m_bucket_spanning_market_open_either_empty_or_phase_marked` (M1)
- `test_quote_packet_sequence_number_field_exists_or_dedup_key_uses_tick_count` (M2)
- `test_per_sid_seal_emission_watchdog_alert_wired` (M3)
- (M4 subsumed by H7)
- `test_vix_pct_used_only_by_regime_filter_never_by_ranker` (M5)
- `test_prev_close_writer_spawned_exactly_once_at_boot` (M6)
- `test_prev_close_ddl_failure_logs_at_error_level_with_code` (M7)
- `test_candle_tick_count_column_is_long_not_int` (M8)
- `test_aggregator_bucket_lookup_uses_fixed_array_indexed_by_tf_idx` (M9)

Z+ LOW fixes (4):
- `test_candle_1h_align_offset_constant_pinned_at_0015` (L1)
- `test_source_column_symbol_capacity_pinned_at_8` (L2)
- `test_drop_matview_idempotent_when_view_absent_or_present` (L3)
- (L4 subsumed by M8)

**LoC: ~480 (6 ALTER × 3 cols + bar-seal computation × 6 timeframes + cross-verify recompute hook + daily-candle lookup + Telegram + 17 ratchets + matview cleanup at boot).**

---

### 31. Z+ adversarial review — 5 CRITICAL + 9 HIGH fixes (operator-locked 2026-05-13 late-evening)

**Authority:** `topic-Z-PLUS-DEFENSE-6TF-LOCK.md` (27 findings from 3 adversarial agents).

**5 CRITICAL fixes (must merge before Friday):**

| # | Fix | New constant / API | Ratchet |
|---|---|---|---|
| C1 | Force-seal every TF at 15:30:00 IST exchange-ts (1h's 15:15-16:15 bucket would otherwise wait until 16:15) | `MARKET_CLOSE_FORCED_SEAL_IST = "15:30:00"` | `test_candles_1h_final_bar_seals_at_15_30_not_16_15` |
| C2 | Pin bucket boundary rule `[start, end)` (half-open right) so tick at 09:16:00.000 lands in 09:16 bucket | `BUCKET_BOUNDARY_RULE = "[start, end)"` | `test_tick_at_exact_boundary_falls_into_next_bucket` |
| C3 | NSE_EQ first-Quote-tick prev_close: reject if `< 1e-9` OR `> 10× boot-loaded QuestDB value`; wait for next tick instead of locking cache | `NSE_EQ_FIRST_TICK_PREV_CLOSE_DRIFT_LIMIT = 10.0` | `test_nse_eq_first_tick_rejects_zero_or_implausible_value` |
| C4 | `oi_pct_from_prev_day` is NULL in Phase 0 — block any strategy/dashboard query that references it without `COALESCE` | dashboard JSON scan + banned-pattern category | `test_no_strategy_or_dashboard_query_references_oi_pct_without_coalesce` |
| C5 | ILP fan-out at 09:30:00 IST (1m+3m+5m+15m co-fire = 892 rows in <1ms): dedicated tokio task + bounded `mpsc::channel(2048)` + reused `questdb_rs::Buffer` + rescue-ring overflow | `SEAL_BURST_CHANNEL_CAPACITY = 2048` | `test_minute_boundary_burst_does_not_block_ws_read_loop` + Criterion `bench_seal_burst_892_rows_p99_under_5ms` |

**9 HIGH fixes:**

| # | Fix | Ratchet |
|---|---|---|
| H1 | INDIA VIX code-6 emission UNVERIFIED — pre-Friday smoke test subscribes VIX alone, verifies code-6 receipt within 60s; fallback to QuestDB `previous_close` row if absent | `test_vix_code6_smoke_or_fallback_path` |
| H2 | Boot mid-day (e.g. 11:30 IST) → bar `open` is wrong (first tick after boot, not true 11:15 open). Stamp `phase = "boot_partial"`; backtest filters on it | `test_partial_boot_bars_marked_boot_partial` |
| H3 | `previous_close` EOD write window pinned `[15:30:30, 15:31:30]` IST with DEDUP UPSERT + edge-trigger single-fire guard | `test_previous_close_eod_writes_exactly_once_per_day` |
| H4 | ALTER block (24 ALTER ADD COLUMN) MUST complete BEFORE first ILP-writer spawn — boot ordering constraint | `test_alter_block_completes_before_aggregator_spawn` |
| H5 | Cross-verify revalidates prev_close (not just bar.close) against Dhan historical's previous-day `_1d.close`; overwrite if drifted | `test_cross_verify_revalidates_prev_close_not_just_bar_close` |
| H6 | `prev_close_cache` primitive PINNED as `Arc<papaya::HashMap<(u32, ExchangeSegment), f64>>` (lock-free read; no Mutex/RwLock) | `test_prev_close_cache_uses_papaya_not_mutex` |
| H7 | `candles_1d` self-referential prev_close at seal time MUST read from RAM cache only (no QuestDB SELECT) | `test_candles_1d_self_referential_does_not_select_at_seal_time` |
| H8 | `index_prev_close_cache` migrated from `HashMap<u32, f32>` to composite `HashMap<(u32, ExchangeSegment), f64>` (I-P1-11 compliance) | banned-pattern category 5 explicit guard on `tick_processor.rs:537` |
| H9 | Pre-ILP bounds guard on `prev_close` f32: reject `!is_finite()` OR `<= 0.0` OR `> 10_000_000.0` BEFORE writing to QuestDB or RAM cache | `test_prev_close_rejects_nan_inf_zero_and_extreme` |

**9 MEDIUM fixes (folded into other items):**

| # | Fix | Owner |
|---|---|---|
| M1 | 3m bucket 09:12-09:15 spans pre-open boundary → empty (G1 gate) OR `phase = "market_open_partial"` | item 8 (G1 dual-gate) |
| M2 | Verify sequence number on Quote packets; if absent, switch tick DEDUP key to `(ts, security_id, segment, tick_count_in_minute)` | item 29 schema |
| M3 | Per-SID seal-emission watchdog Prometheus counter `tv_aggregator_seals_per_sid_total{sid,tf}` + alert `tv-aggregator-seal-stopped-per-sid` for 5m during market hours | item 11 (timing audit) |
| M4 | 1d pct computation reads RAM cache only (subsumed by H7) | H7 |
| M5 | Source-scan guard: `test_vix_pct_used_only_by_regime_filter_never_by_ranker` | item 31 |
| M6 | `prev_close_persist::spawn_global` race: spawn writer exactly once from main.rs BEFORE any parallel task; ratchet `test_prev_close_writer_spawned_exactly_once_at_boot` | item 31 |
| M7 | DDL errors upgrade `warn!` → `error!(code = ErrorCode::PrevCloseDdl04.code_str(), ...)` to route through Telegram | item 31 |
| M8 | `tick_count` column type `INT` → `LONG` on all 6 candle tables (i32 wrap at 2.1B for re-aggregation) | item 29 schema |
| M9 | Per-tick aggregator bucket lookup: `Arc<papaya::HashMap<(u32, ExchangeSegment), [Bucket; 6]>>` indexed by `TimeframeIdx as usize` (fixed-array, not 6 hash probes) | item 31 |

**4 LOW fixes:**

| # | Fix | Owner |
|---|---|---|
| L1 | `CANDLE_1H_ALIGN_OFFSET = "00:15"` + `CANDLE_DEFAULT_ALIGN_OFFSET = "00:00"` constants in `crates/common/src/constants.rs` | item 31 |
| L2 | `source SYMBOL CAPACITY 8` (avoid default 256) | item 29 DDL |
| L3 | `test_drop_matview_idempotent_when_view_absent_or_present` | item 31 |
| L4 | `tick_count LONG` (subsumed by M8) | M8 |

**Honest 100% claim wording (verbatim, mandated):**

> "100% inside the tested envelope, with ratcheted regression coverage:
> ≤60s QuestDB outage absorbed by rescue→spill→DLQ;
> ≤5,000,000-tick ring buffer (`TICK_BUFFER_CAPACITY`);
> bench-gated O(1) hot path; composite-key uniqueness;
> chaos-tested 65h weekend sleep/wake;
> 27 hostile findings from 3 adversarial agents all closed with named ratchets (this item).
> Beyond the envelope, DLQ NDJSON catches every payload as recoverable text."

**LoC: ~620 (5 CRITICAL fixes ~300 + 9 HIGH fixes ~200 + 9 MEDIUM ~80 + 4 LOW ~40 + 27 ratchet tests).**

**Total Friday + Saturday + Sunday build: ~6,520 LoC across 30 active items + 9 docs.**

---

## Phase 0 progress checkboxes (operator-locked checkbox defense protocol 2026-05-13)

**Mandatory rule:** Every Claude Code session MUST check these boxes at session start. Only mark `- [x]` when ALL 4 criteria met: (1) code merged, (2) ratchet tests green in CI, (3) 7-layer observability wired, (4) PR closed.

- [x] **Item 1** — Subscription planner mode mix (IDX_I Ticker + NSE_EQ Quote) — `SubscriptionScope::IndicesUnderlyingsOnly` in `crates/common/src/config.rs:1143`; planner branches at `subscription_planner.rs:247/265/298`; 13+ ratchet tests under `tests` module (line 4570 onward) incl. `test_indices_underlyings_only_planner_emits_zero_derivatives`, `_nse_eq_uses_quote_mode`, `_emits_all_three_idx_i_full_chain`, `_lower_bound_floor`, `test_phase_0_idx_i_symbols_contain_required_four`, `test_india_vix_security_id_constant_pinned_at_21`.
- [x] **Item 2** — Connection pool: 1 main-feed WS conn (others dormant) — `effective_main_feed_pool_size(IndicesUnderlyingsOnly, _) → 1` in `crates/common/src/config.rs:1192`; consumed at boot in `crates/app/src/main.rs:6480`; ratchets `test_effective_main_feed_pool_size_under_phase_0_returns_one` + `test_phase_0_main_feed_connection_count_constant_pinned_at_1`.
- [x] **Item 3** — Config feature flags (depth-20/200/greeks/movers/Phase 2 dispatcher all `false` under Phase 0 scope) — scope-aware spawn helpers in `crates/app/src/phase2_recovery.rs::should_spawn_phase2_scheduler/_depth_dynamic_pipeline/_greeks_pipeline` return `false` for `IndicesUnderlyingsOnly` **regardless of flag value**; ratchets `test_should_spawn_*_skipped_under_phase_0_regardless_of_flag` (lines 407/457/479). Movers pipeline already DELETED (`main.rs:1456` confirms).
- [x] **Item 4** — Watchdog timings: IDX_I 3s / NSE_EQ 10s / VIX 30s / Subscribe-ACK 2s / first reconnect 0ms — constants `WATCHDOG_THRESHOLD_IDX_I_SECS=3 / _NSE_EQ_SECS=10 / _VIX_SECS=30` in `crates/core/src/websocket/activity_watchdog.rs:79/87/93`; scope-aware helper `effective_main_feed_watchdog_threshold_secs` in `crates/app/src/phase2_recovery.rs:242`; boot consumes effective at `crates/app/src/main.rs:6425`; ratchets at `activity_watchdog.rs:496-507` + `phase2_recovery.rs:506-549`.
- [x] **Item 5** — SubscribeRxGuard preservation (existing, verify wired) — type defined `crates/core/src/websocket/connection.rs:145`, `Drop` reinstall at line 178, acquire at line 937; 2 ratchet tests at lines 4378-4400 (`test_subscribe_rx_guard_reinstalls_on_drop`, `test_subscribe_rx_guard_survives_many_cycles`). Originally shipped in PR #337 (2026-04-24).
- [x] **Item 6** — Stagger init conns 2s — `connection_stagger_ms = 2000` default in `crates/common/src/config.rs:1908`; ratchet `test_connection_stagger_ms_default_pinned_at_2000` at `crates/common/src/config.rs:2644`. Under Phase 0 (1 conn) the stagger is a no-op; under legacy 5-conn scopes it provides the 2s spacing.
- [x] **Item 7** — Defer depth-20 connect (ratchet retained for Phase 2) — `should_spawn_depth_dynamic_pipeline(IndicesUnderlyingsOnly, _) → false` in `crates/app/src/phase2_recovery.rs:197`; ratchet `test_should_spawn_depth_dynamic_pipeline_skipped_under_phase_0_regardless_of_flag` at `phase2_recovery.rs:457` retained for Phase 2 re-enable.
- [x] **Item 8** — Gap-fill scheduler `/charts/intraday` seal-then-fetch — series PR-A through PR-D9 (#668, #670, #671, #672, #673, #674, #675, #676, #677, #678, #679, #680, #681); scheduler at `crates/core/src/historical/gap_fill_scheduler.rs::run_gap_fill_scheduler` + `execute_bar_for_all_instruments`; per-bar parallelism via `tokio::task::JoinSet` + `Arc<tokio::sync::Semaphore>` capped at `GAP_FILL_MAX_CONCURRENT_FETCHES = 5` (matches Dhan Data API hard limit); per-conn precision via `DisconnectResolvedEvent.subscribed: Arc<Vec<(u32, u8)>>`; `outage_start_secs` refinement via shared `LastSeenLttCache` (PR-D8); Lagged(n) catchup audit + Telegram (PR-D9). 39 ratchet tests in `gap_fill_scheduler::tests` + 12 in `last_seen_ltt_cache::tests` + 41 in `grafana_dashboard_snapshot_filter_guard` (PR-D5 panel-pinning). 6 counters + 1 histogram + 2 Grafana panels + 1 Critical alert (`GapFillBarsFailedHigh`).
- [x] **Item 9** — Gap-fill writes go to `historical_candles` (NOT `candles_1m`) — locked design decision per `live-feed-purity.md` ("Historical data lives in two places only: `historical_candles` table — Dhan REST `/charts/historical` results"). PR-B' #670 architectural lock; existing DEDUP key `(ts, security_id, timeframe, segment)` already I-P1-11 compliant; `historical_candles` table created at boot via `candle_persistence.rs::ensure_candle_table_dedup_keys`; cross-verify (`cross_verify.rs`) already READS this table. `gap_fill_audit` table DDL + writer shipped in PR-A, wired at boot via `ensure_gap_fill_audit_table` in `crates/app/src/main.rs` (PR-D5 fixed silent table-not-found bug); per-bar audit row written in `execute_bar_for_all_instruments`; Lagged catchup audit row added in PR-D9. Audit row schema: `(ts, trading_date_ist, bar_minute, trigger_event, sids_requested, sids_completed, sids_failed, duration_ms, result)` with `trigger_event` SYMBOL in `{ws_disconnect, manual, scheduler_catchup}`.
- [ ] **Item 10** — Disconnect-chain 7-layer observability (5 counters + 2 gauges + 2 audit + 5 Telegram + ratchets)
- [ ] **Item 11** — Dual-gate market-hours fix (G1 exchange_ts + G2 wall-clock 60s grace)
- [ ] **Item 12** — `last_tick_audit` + `LastTickAfterBoundary` Telegram + banned-pattern hook
- [x] **Item 13** — Pre-open buffer → 09:15 candle.open wiring (IDX_I + NSE_EQ) — merged via PR #619 (`open-price source-selection primitives`); see `crates/common/src/open_price_source.rs` + `commit c2b1201`.
- [x] **Item 14** — REST `/marketfeed/quote.day_open` fallback at 09:14:55 IST — merged at `commit 6e9b4ce` (PR-14 ratchets bumped test-count baseline 9396→9423 in `6a1d35a`).
- [ ] **Item 15** — 09:16:05 IST cross-check vs Dhan `/charts/intraday` 09:15 bar
- [ ] **Item 16** — `open_price_audit` + 3 Telegram variants + 8 ratchets
- [ ] **Item 17** — SEBI 24h JWT daily renewal observability + `auth_renewal_audit`
- [ ] **Item 18** — Static IP boot check (`ordersAllowed=true`)
- [ ] **Item 19** — Dual-instance lock (RESILIENCE-01) via `live_instance_lock` table
- [ ] **Item 20** — Orphan position 15:25 IST watchdog
- [ ] **Item 21** — NaN / div-by-zero indicator guards
- [ ] **Item 22a** — Order placement 5s REST timeout + DH-904 retry policy
- [ ] **Item 22b** — Stop-loss-leg-cancelled auto-replace + `sl_replacement_audit`
- [ ] **Item 22c** — Boot-success Telegram positive ping at 09:14:55 IST
- [ ] **Item 22d** — End-of-day Telegram digest at 15:31:30 IST
- [ ] **Item 22e** — Tick-level integrity guards (price/oi/high≥low)
- [ ] **Item 22f** — Self-trade prevention 60s cooldown + `self_trade_audit`
- [ ] ~~Item 22~~ — GIFT NIFTY (PARKED for Phase 2, do NOT implement)
- [ ] **Item 23** — Prev-close mode mix + REST boot fetch + `PrevCloseMissingAtMarketOpen` + banned-pattern hook
- [ ] **Item 24** — Order Update WS 7-layer observability + `order_update_ws_audit` + Source filter
- [ ] **Item 25** — P&L tracker scaffolding (dry-run hypothetical) + `pnl_audit`
- [ ] **Item 26** — `signal_audit` + `decision_audit` tables (sampled)
- [ ] **Item 27a** — `docs/phases/phase-0-readme.md`
- [ ] **Item 27b** — `docs/runbooks/aws-deployment.md`
- [ ] **Item 27c** — `docs/runbooks/operator-daily-startup.md`
- [ ] **Item 27d** — `docs/runbooks/troubleshooting.md`
- [ ] **Item 27e** — `docs/runbooks/kill-switch.md`
- [ ] **Item 27f** — `docs/runbooks/backtest-runner.md`
- [ ] **Item 27g** — `docs/runbooks/phase-1-monitoring-rubric.md`
- [ ] **Item 27h** — `.claude/rules/project/phase-0-architecture.md`
- [ ] **Item 28** — Cross-verify mismatch DEDUP UPSERT overwrite + 09:15 strategy gate + `bar_correction_audit`
- [ ] **Item 29** — QuestDB schema changes (candles source column + 15 new audit tables + 15 schema ratchets)
- [ ] **Item 30** — `pct_from_prev_close` column on all 5 candle tables + bar-seal computation + cross-verify recompute hook + 8 ratchets (DEDUP KEYS unchanged: `(security_id, exchange_segment, ts, timeframe)`)

**Total active items to check off: 36 boxes (28 active items, of which 27 = items 1-21,23-28; item 22 PARKED; item 27 has 8 sub-doc boxes; item 29 added).**

**Session-start protocol:** new `.claude/hooks/phase-0-progress-check.sh` runs at every Claude Code session start; parses this section; prints `Phase 0 progress: N/36 complete` + next pending item.

**Pre-push gate:** `bash .claude/hooks/plan-verify.sh` rejects any push that has unchecked `- [ ]` items remaining IF the PR description claims "Phase 0 complete".

---

## NOT IN PHASE 0 — Phase B (Backtest brute-force) discussion PARKED 2026-05-13

The Phase B (Mac-side brute-force backtest using `/charts/intraday` + `/charts/rollingoption` expired options data, 5-year history, extreme permutation engine) discussion was opened and parked tonight. Operator decision: **finish Phase 0 LIVE phase first, then move to Phase B.**

Phase B will be its own LOCKED plan file (`topic-PHASE-B-BACKTEST-LOCKED.md`) when we re-open the topic post-Phase-0. Do NOT mix Phase B work into Phase 0 build (anti-scope-creep).

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
