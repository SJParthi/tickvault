# Topic — Missed-Tick Target/SL Execution Risk (the CRITICAL question)

> **Status:** DRAFT (discussion mode 2026-05-12 → 2026-05-14)
> **Authority:** Honest market microstructure > this file.
> **Trigger:** Operator question 2026-05-12 17:15 IST — the most CRITICAL question yet about live trading.

---

## 🎯 First — confirming the backtest point

**Operator confirmed:** "for backtesting we use traded data from Dhan + Groww, we don't need NSE precise."

✅ **ACCEPTED.** That decision is locked. Backtesting uses broker-traded data (close enough for strategy R&D). No further debate.

---

## 🚨 NOW the BIG live-trading question

### Operator's exact scenario:

```
09:17 IST  Operator enters trade at LTP = 100 (long position)
09:18 IST  Operator sets target profit = 133
   ↓
   Within the 09:18 minute:
   - LIVE Dhan WS stream shows max LTP = 125 (what we saw)
   - Dhan REST historical (post-fill) shows max high = 135 (server-side complete)
   - NSE actual high (truth) = 135
   - Target was 133
   - 133 is BETWEEN our seen 125 and actual 135
   ↓
   Question: Did target trigger?
```

**THIS IS THE MOST DANGEROUS QUESTION IN LIVE TRADING.**

---

## 🚗 Auto-Driver Story

> Sir, imagine your security guard at the entrance is watching cars. He's instructed: "when a red car passes, alert me."
>
> A red car ZIPS through at 70 km/h. Your guard BLINKS. Doesn't see it.
>
> Did the red car pass? **YES** (camera footage shows it).
> Did your guard see it? **NO** (he blinked).
>
> Did your alarm trigger? **DEPENDS:**
> - If alarm is wired to GUARD'S EYES → NO (guard blinked)
> - If alarm is wired to MAIN GATE CAMERA → YES (camera saw it)

**Our trading system has the same choice:**

| Option | Wired to | Missed-tick risk |
|---|---|---|
| **Strategy reads live LTP and fires market orders** | Our guard's eyes (tickvault) | ❌ **VULNERABLE** — we miss the 135 tick → never trigger |
| **Resting LIMIT order on exchange** | Main gate camera (NSE matching engine) | ✅ **SAFE** — NSE matches at 133, we just get the fill notification |

---

## 🔬 The TECHNICAL REALITY

### Path 1 — Client-side triggered order (DANGEROUS)

```
Strategy reads tickvault state:
  current_LTP = 125  (we missed the 135 tick)
  target = 133
  → 125 < 133 → DON'T trigger
   ↓
   Meanwhile NSE: price hit 135 (actual market)
   Position holds; eventually price drops back below 133
   Target NEVER triggers
   We missed our exit ❌
```

**This is what operator is worried about. And it's REAL.**

### Path 2 — Exchange-side resting order (SAFE)

```
At entry (09:17 IST), we placed:
  - Buy market at 100 (entry) ✅
  - Sell LIMIT at 133 (target) ← rests on exchange
  - Sell SL-M at 95 (stop loss) ← rests on exchange
   ↓
At 09:18:23, NSE price prints 135
NSE matching engine sees: "limit sell at 133 in book"
NSE matches: sells our position at 133 ✅
   ↓
Dhan's order-update WS notifies us:
  "Order filled at 133"
   ↓
tickvault gets fill via OrderUpdateMessage (LegNo=2 TARGET_LEG)
   ↓
Our P&L: +₹33 per unit ✅
```

**THIS IS THE CORRECT APPROACH.** Target/SL ALWAYS rest on exchange.

---

## 🚨 The critical distinction

| Order type | Lives where | Missed-tick safe? |
|---|---|---|
| **LIMIT order** (price specified) | NSE order book | ✅ YES — NSE matches |
| **MARKET order** (place now) | One-shot, not resting | N/A (immediate fire) |
| **Bracket Order (BO)** entry + target + SL | All 3 legs rest on exchange | ✅ YES |
| **Super Order** entry + target + SL + trailing | All rest on exchange (per `dhan-ref/07a-super-order.md`) | ✅ YES |
| **Forever Order (GTT)** target reachable later | Rests on exchange | ✅ YES |
| **Manual "watch LTP and fire market order"** | Client-side | ❌ NO — vulnerable to missed ticks |

**Dhan supports all of the above.** Operator picks per strategy.

---

## 🎯 The DEFENSE for our trading system

### Defense 1 — ALWAYS use exchange-side resting orders for target/SL

When operator's strategy places an order with target + SL, our OMS MUST use one of:
- **Bracket Order** (`/v2/orders` with `productType=BO`) — per `dhan-ref/07a-super-order.md`
- **Super Order** (`/v2/super/orders`) — operator's primary choice today
- **Forever Order** (`/v2/forever/orders`) — for GTT (good-till-triggered) scenarios

NEVER let strategy do "client-side watch + market order" pattern.

### Defense 2 — Post-market audit of missed-trigger scenarios

Daily post-market task:
1. For each open position from yesterday: read our live high/low for that minute
2. Compare against Dhan historical high/low for same minute
3. If historical_high > our_live_high AND target was within that range → ALERT

```sql
-- Audit query (runs daily)
SELECT
  position.security_id,
  position.target_price,
  live_candles_1m.high AS live_high,
  historical_candles.high AS historical_high,
  CASE
    WHEN historical_candles.high >= position.target_price
     AND live_candles_1m.high < position.target_price
    THEN 'MISSED TARGET'
    ELSE 'OK'
  END AS verdict
FROM open_positions position
JOIN live_candles_1m ON ...
JOIN historical_candles ON ...
WHERE verdict = 'MISSED TARGET';
```

If any row returned → Telegram CRITICAL.

### Defense 3 — Order routing pre-flight check

Before placing any order, OMS verifies:
- IF order has target_price OR sl_price field → MUST use BO / Super / Forever
- IF strategy attempts client-side trigger → REFUSE in OMS layer

Mechanical check: banned-pattern scanner in strategy code.

### Defense 4 — Audit trail of exchange-side vs client-side decisions

```sql
CREATE TABLE order_routing_audit (
  ts                  TIMESTAMP,
  order_id            STRING,
  security_id         INT,
  has_target          BOOLEAN,
  has_sl              BOOLEAN,
  routing             SYMBOL,  -- 'exchange_side' / 'client_side'
  reason              STRING
);
```

Every order placement logs which path. SEBI audit + operator monitoring.

---

## 📊 Math: what operator's strategy SHOULD look like

### WRONG (vulnerable to missed ticks):

```rust
// DANGEROUS — client-side triggered
async fn watch_and_exit() {
    loop {
        let ltp = current_bar.live_close;  // What WE see
        if ltp >= 133 {
            // Place market sell — but we may have ALREADY missed the spike
            place_market_sell();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
```

### RIGHT (exchange-side rests):

```rust
// SAFE — exchange-side resting order
async fn enter_with_bracket() {
    // At entry: place ALL THREE legs at once
    let entry_order = SuperOrderRequest {
        security_id: 41735,
        transaction_type: "BUY",
        order_type: "MARKET",       // Entry: market buy
        quantity: 75,
        target_price: 133.0,         // ← rests on exchange after entry fills
        stop_loss_price: 95.0,       // ← rests on exchange after entry fills
        trailing_jump: 0.0,
    };

    api_client.place_super_order(entry_order).await?;

    // Now: NSE matches target/SL. We just listen for order-update WS notification.
}
```

**One API call. NSE matches. We just wait for fill.**

---

## 🚨 What happens TODAY in tickvault?

Per `dhan-ref/07a-super-order.md` and our OMS plan, super orders ARE supported.

**Question for operator:** is your STRATEGY designed to use super orders / BO, or is it doing client-side LTP watching?

If strategy uses BO/Super → **safe by construction**
If strategy is custom client-side → **VULNERABLE to missed-tick scenario**

This is a critical design choice the operator must lock.

---

## 📋 Recommendation

### Lock the rule:

> **EVERY trade with target/SL MUST use exchange-side resting orders (BO, Super, or Forever). NO client-side LTP-watch + market-order patterns allowed in tickvault.**

Mechanical enforcement:
- Banned-pattern in `crates/trading/src/strategy/*.rs` rejecting `place_market_sell()` after a `target` check
- OMS layer refuses orders with target_price unless wrapped in BO/Super/Forever
- Ratchet test: `test_no_client_side_target_pattern`

### Add audit:

`order_routing_audit` table per Section "Defense 4" above.

### Add post-market verification:

Daily missed-trigger detection per Section "Defense 2" above.

---

## 🛡️ Z+ 7-Layer for missed-tick target execution

| Layer | Mechanism |
|---|---|
| L1 DETECT | `tv_client_side_trigger_attempts_total` counter (should always be 0) |
| L2 VERIFY | Per-order routing pre-flight (BO/Super/Forever required when target set) |
| L3 RECONCILE | Daily post-market missed-trigger audit query |
| L4 PREVENT | OMS layer refuses non-exchange-side targets; banned-pattern in strategy |
| L5 AUDIT | `order_routing_audit` table (SEBI 5y) |
| L6 RECOVER | If missed trigger detected: alert + manual operator intervention |
| L7 COOLDOWN | N/A |

---

## 🚨 NEW worst-cases for missed-tick risk (W211-W215)

| # | Scenario | Defense |
|---|---|---|
| W211 | Operator's strategy uses client-side LTP watch → misses target | Mechanical: OMS refuses; banned-pattern catches |
| W212 | Dhan order-update WS drops the fill notification | OMS reconciliation: query Dhan order book on reconnect |
| W213 | NSE matches order BUT price spike was momentary (mean-reverted) | Operator accepts — that's how markets work; resting order = correct semantics |
| W214 | Position has target=133, but historical shows market only hit 132.95 | NSE didn't actually reach target; no missed trigger |
| W215 | Multiple targets on same position (scaled exits) | Bracket order doesn't support; use multiple Super Orders or Forever Orders |

---

## 📊 Total worst-case coverage: 426

| Plan | Count |
|---|---|
| Prior 421 | 421 |
| NEW missed-tick (W211-W215) | 5 |
| **GRAND TOTAL** | **~426** |

---

## 🎤 Direct answer to operator

### Q: "If live shows high=125 and historical shows 135 and target=133, does target trigger?"

### A: **Depends on order type:**

| Order type | Does target trigger? |
|---|---|
| Resting LIMIT / Bracket / Super / Forever Order | ✅ **YES** — NSE matches at 133, regardless of what tickvault saw |
| Client-side strategy ("watch LTP, fire market") | ❌ **NO** — we missed the spike, never triggered |

### THE RULE FOR TICKVAULT:

**ALWAYS use Super Orders or Bracket Orders.** NEVER let strategy do client-side LTP watching for targets.

Then operator's scenario is SAFE: the 133 target executes at NSE, we just receive the fill notification via order-update WS, our P&L = +₹33 per unit, done.

---

## 🚗 Final auto-driver summary

> Sir, your concern is valid AND has a clean fix:
>
> 1. **DON'T** let your strategy "watch the price and click sell when it hits 133" — you'll miss spikes
> 2. **DO** place a "limit sell at 133" order at NSE when you enter the trade — NSE will match it AUTOMATICALLY when price hits 133, regardless of whether YOU saw the 133 tick
>
> The exchange is more reliable than our tick stream. Let the exchange do the matching. Tickvault just listens for "yes, your order filled at 133" notification.
>
> Super Orders / Bracket Orders / Forever Orders on Dhan — all three rest on the exchange and guarantee this.
>
> **Rule:** every target/SL = resting order. Zero client-side triggers. Operator's missed-tick scenario becomes IMPOSSIBLE.

5 NEW worst-cases W211-W215. **Grand total: 426.**

Discussion mode continues. NO IMPLEMENTATION.
