# Functional Flow — dhan-live-trader

> **Audience:** Anyone. No code knowledge needed.
> **Think of this as:** "What does this system DO?" — explained like a YouTube video.

---

## What Is This System?

Imagine you're a stock trader on the Indian market (NSE). You trade **Options** (F&O) — financial contracts that let you bet on where stock prices will go.

This system does everything a human trader does, but **automatically** and **faster than any human could**:

1. **Watches** thousands of stock prices in real-time
2. **Analyzes** price patterns using math (indicators like RSI, MACD, EMA)
3. **Decides** when to buy or sell based on strategies you define
4. **Executes** trades through the Dhan broker API
5. **Protects** your money with risk controls (max loss limits, kill switch)
6. **Records** everything for analysis later

---

## The Daily Lifecycle (A Normal Trading Day)

### Morning — "Getting Ready" (Before 9:15 AM)

```
┌──────────────────────────────────────────────────────┐
│                   MORNING PREP                        │
│                                                      │
│  1. Wake Up           System starts automatically     │
│                                                      │
│  2. Check Identity    "Am I connecting from the       │
│                        right IP address?"              │
│                        (Dhan requires a fixed IP)      │
│                                                      │
│  3. Login             Generate a login token           │
│                        (like scanning your badge)      │
│                                                      │
│  4. Load Instruments  Download today's stock list      │
│                        (~100K instruments, find F&O)   │
│                                                      │
│  5. Open Connections  Connect to Dhan's live price     │
│                        stream (WebSocket)              │
│                                                      │
│  6. Ready             "I'm watching. Waiting for       │
│                        market open."                   │
└──────────────────────────────────────────────────────┘
```

### Market Hours — "Working" (9:15 AM – 3:30 PM)

```
┌──────────────────────────────────────────────────────┐
│                   LIVE TRADING                        │
│                                                      │
│  Every tick (price change) flows through:             │
│                                                      │
│  Price Arrives                                       │
│       ↓                                              │
│  Parse It (binary → numbers)                         │
│       ↓                                              │
│  Update Candles (1-second price bars)                 │
│       ↓                                              │
│  Run Indicators (RSI, EMA, MACD, etc.)               │
│       ↓                                              │
│  Evaluate Strategy ("Should I buy/sell?")             │
│       ↓                                              │
│  Risk Check ("Can I afford this? Am I within          │
│               my loss limit?")                        │
│       ↓                                              │
│  Place Order (if all checks pass)                    │
│       ↓                                              │
│  Track Order (watch for fill/reject/cancel)           │
│                                                      │
│  This happens THOUSANDS of times per second.          │
└──────────────────────────────────────────────────────┘
```

### After Market — "Wind Down" (After 3:30 PM)

```
┌──────────────────────────────────────────────────────┐
│                   POST-MARKET                         │
│                                                      │
│  1. Fetch historical candles (backfill any gaps)      │
│  2. Save all data to database                        │
│  3. Renew login token for tomorrow                   │
│  4. System stays on (token auto-renews every 24h)     │
└──────────────────────────────────────────────────────┘
```

---

## The Five Jobs This System Does

### Job 1: WATCH — Real-Time Market Data

**What:** Receives live price updates for ~1,500 F&O instruments from Dhan.

**How it works (simplified):**
- Dhan sends tiny binary packets (16–162 bytes each) over WebSocket
- Each packet = one price update for one instrument
- System parses these packets in microseconds (no delay)
- Up to 5 WebSocket connections, 5,000 instruments each

**What you see:** Live prices, volumes, open interest, 5-level order book depth.

---

### Job 2: ANALYZE — Candles & Indicators

**What:** Converts raw price ticks into candles (OHLCV bars) and runs math on them.

**Candles explained:**
```
Every 1 second, the system creates a "candle":

     High ─── $105
      │
      │   ┌───┐
      │   │   │ ← Close: $103
      │   │   │
      │   │   │ ← Open: $101
      │   └───┘
      │
     Low ─── $99

This 1-second candle becomes the base for:
  → 5s, 10s, 15s, 30s candles
  → 1m, 2m, 3m, 5m, 10m, 15m, 30m candles
  → 1h, 2h, 3h, 4h candles
  → 1 day, 1 week, 1 month candles

Total: 21 timeframes — all built automatically.
```

**Indicators explained:**
```
Indicators are math formulas that spot patterns:

  EMA (Exponential Moving Average)  → "Is price trending up or down?"
  RSI (Relative Strength Index)     → "Is the stock overbought or oversold?"
  MACD (Moving Average Convergence) → "Is momentum shifting?"
  Bollinger Bands                   → "Is price near its normal range?"
  ATR (Average True Range)          → "How volatile is this stock?"
  VWAP (Volume-Weighted Avg Price)  → "What's the fair price today?"
  SuperTrend                        → "What's the trend direction?"
  Stochastic                        → "Where is price vs recent range?"

All computed in O(1) — constant time per tick, never slows down.
```

---

### Job 3: DECIDE — Strategy Evaluation

**What:** Checks if conditions are met to buy or sell.

**Example strategy (simplified):**
```
IF:
  - RSI drops below 30          (stock is oversold — might bounce)
  - Price is above 200-EMA      (still in long-term uptrend)
  - MACD histogram is positive  (short-term momentum recovering)
THEN:
  → BUY signal

IF:
  - RSI goes above 70           (stock is overbought — might drop)
  - Price crosses below 20-EMA  (short-term trend broken)
THEN:
  → SELL signal
```

Strategies are defined in TOML config files. You can change them without restarting the system (hot reload).

---

### Job 4: EXECUTE — Order Management

**What:** Places buy/sell orders through Dhan's API.

**The order lifecycle:**
```
Signal arrives ("BUY NIFTY 25000 CE")
       ↓
Risk Check
  ├─ "Do I have enough margin?"
  ├─ "Am I within my daily loss limit?"
  ├─ "Am I within my position size limit?"
  └─ "Is the circuit breaker OK?"
       ↓
Rate Limit Check (max 10 orders/sec, SEBI rule)
       ↓
Place Order (POST to Dhan API)
       ↓
Track via WebSocket (live status updates)
  ├─ TRANSIT → order is being sent
  ├─ PENDING → order is on the exchange
  ├─ TRADED  → order filled! Update P&L
  ├─ REJECTED → something went wrong
  └─ CANCELLED → order was cancelled
```

**Safety:** By default, orders are **paper traded** (simulated). No real money moves until you explicitly enable live trading.

---

### Job 5: PROTECT — Risk Management

**What:** Prevents catastrophic losses.

```
┌─────────────── RISK LAYERS ──────────────────┐
│                                              │
│  Layer 1: PRE-TRADE CHECKS                   │
│    ├─ Max daily loss reached? → HALT         │
│    ├─ Position too large? → REJECT           │
│    └─ Margin insufficient? → REJECT          │
│                                              │
│  Layer 2: LIVE MONITORING                    │
│    ├─ Tick gap detected? → ALERT             │
│    ├─ P&L threshold hit? → AUTO EXIT ALL     │
│    └─ System error? → KILL SWITCH            │
│                                              │
│  Layer 3: EMERGENCY CONTROLS                 │
│    ├─ Kill Switch → stops ALL trading today  │
│    ├─ Exit All → closes ALL positions        │
│    └─ Auto-Halt → freezes order placement    │
│                                              │
│  Layer 4: EXTERNAL (Dhan side)               │
│    ├─ SEBI rate limits enforced              │
│    ├─ Exchange circuit breakers              │
│    └─ Margin calls from broker               │
│                                              │
└──────────────────────────────────────────────┘
```

---

## Key Numbers

| What | Number |
|------|--------|
| Instruments tracked | ~1,500 F&O contracts |
| Candle timeframes | 21 (from 1 second to 1 year) |
| Indicators available | 9 types (SMA, EMA, RSI, MACD, BB, ATR, VWAP, SuperTrend, Stochastic) |
| Max orders per second | 10 (SEBI limit) |
| Max orders per day | 7,000 |
| Token validity | 24 hours (auto-renews at 23h) |
| WebSocket connections | Up to 5 simultaneous |
| Boot time (crash recovery) | ~400ms (fast boot) |
| Boot time (cold start) | 2–5 seconds (slow boot) |

---

## Who Uses This System?

**Parthiban** (the owner) — defines strategies, monitors performance, sets risk limits.

**The system** — does everything else automatically:
- Wakes up → logs in → watches market → trades → protects → sleeps → repeats.

No manual intervention needed during market hours. If something goes wrong, it sends a Telegram alert and halts trading automatically.

---

## One Sentence Summary

> **dhan-live-trader watches 1,500+ stock options in real-time, runs math on every price change, executes trades when conditions are met, and protects your money with multiple safety layers — all automatically, all day, every trading day.**
