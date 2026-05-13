# Topic — Dhan Postback Direction Clarification

> **Status:** DRAFT (discussion mode 2026-05-12 → 2026-05-14)
> **Authority:** `dhan-ref/07e-postback.md` + `.claude/rules/dhan/postback.md` > this file.
> **Trigger:** Operator question 2026-05-12 18:00 IST: "this postback will work only from TradingView to Dhan?"

---

## 🚗 Auto-Driver Story

> Sir, "postback" might sound like it works FROM TradingView TO Dhan, but actually the arrow points the OTHER WAY.
>
> **Postback = Dhan → US.** It's Dhan sending US an HTTP POST notification ("your order just filled") to a public URL we provide.
>
> TradingView is a completely separate thing. TradingView alerts can go to OUR server, but that's a different webhook.
>
> Today: clear up which direction does what.

---

## 🎯 THE DIRECT ANSWER

**Postback is from DHAN to US.** Not from TradingView to Dhan.

```
WRONG mental model:
  TradingView ──postback──▶ Dhan
                                ↑
                          [operator imagined this]

CORRECT reality:
  Dhan ──postback──▶ our public URL
   ↑                       ↑
   on every order          we receive
   status change           HTTP POST notification
```

---

## 📋 The 3 different "webhook-style" flows people confuse

### Flow 1 — Dhan Postback (Dhan → us)

Per `dhan-ref/07e-postback.md`:

```
Dhan order system
   │
   │ on every order status change
   │ (TRANSIT/PENDING/REJECTED/CANCELLED/TRADED/EXPIRED)
   ▼
HTTP POST to our publicly-accessible URL
   ▼
Our server receives JSON payload with order details
```

**Configuration:**
- Set up on `web.dhan.co` when generating access token
- ONE URL per access token
- NO `localhost` / `127.0.0.1` (must be publicly accessible)
- Cannot be configured via API (web UI only)

**What it tells us:** order events for our own account.

### Flow 2 — Dhan Order-Update WebSocket (Dhan → us)

Per `dhan-ref/10-live-order-update-websocket.md`:

```
Dhan order system
   │
   │ on every order status change
   │
   ▼
WebSocket message to our subscribed client
   ▼
tickvault receives JSON via persistent WS connection
```

**Key difference from Postback:**
- WebSocket = persistent connection from US to Dhan
- Postback = HTTP POST from Dhan to US (one-way)
- WS doesn't need public URL
- WS works behind NAT / firewall
- Same payload content

### Flow 3 — TradingView Alerts → external webhook (TV → us)

```
TradingView alert fires (e.g., price crossed 100)
   │
   ▼
TV sends HTTP POST to user-configured URL
   ▼
Our server receives the alert
   ▼
We decide to place an order (call Dhan API)
   ▼
Dhan places order on NSE
```

**This is OPTIONAL.** Not built into tickvault today. Some retail traders use this for "alert-to-trade" automation.

---

## 📊 Comparison table

| Property | Dhan Postback | Dhan Order-Update WS | TradingView Alerts |
|---|---|---|---|
| **Direction** | Dhan → us | Dhan → us (via WS) | TV → us |
| **Transport** | HTTPS POST | WebSocket (persistent) | HTTPS POST |
| **What it carries** | Order status updates | Order status updates | Strategy signals |
| **Needs public URL?** | ✅ YES | ❌ NO | ✅ YES |
| **Works behind NAT?** | ❌ NO | ✅ YES | ❌ NO |
| **Setup location** | web.dhan.co UI | API + WS subscribe | TradingView Pine Script alert |
| **Source of truth** | Dhan's order system | Same Dhan order system | TV's chart + alert engine |
| **Used by tickvault?** | ❌ NO (we use WS) | ✅ YES | ❌ NO |
| **Need for SSH tunnel/ngrok?** | YES if behind NAT | NO | YES if behind NAT |

---

## 🎯 Does tickvault need Postback?

**Per our rules (`dhan-postback.md` rule 9):**

> "Prefer Live Order Update WebSocket for tickvault — no public URL needed."

**Answer: NO.** Tickvault uses **Order-Update WebSocket** instead.

### Why WS, not Postback?

| Concern | Postback | Order-Update WS |
|---|---|---|
| Need public URL on AWS / Mac dev | ✅ YES (AWS ALB, port 443 inbound) | ❌ NO |
| Reliability on flaky network | Lower (single HTTPS per event) | Higher (persistent connection retries) |
| Setup friction | Higher (web UI, one-time config) | Lower (API subscribe) |
| Mac dev support | Need ngrok/cloudflare tunnel | Works natively |
| Already in our codebase | NO | ✅ YES — `crates/core/src/websocket/order_update_connection.rs` |

**WS wins for our use case.** Postback NOT NEEDED.

---

## 🎯 Where TradingView FITS in our system (if at all)

**Today: NOT integrated.**

Future possibilities (NOT in current scope):

| Use case | TradingView role |
|---|---|
| Operator's BACKTESTING product | Could use TV charts for visual analysis |
| Alert-to-trade automation | TV sends alert → our server → place Dhan order |
| Live charting dashboard | Embed TV chart widget in our /portal UI |
| Strategy signals | TV Pine Script alerts → our webhook → strategy engine |

**For active tickvault strategy execution:** TV is NOT used. Our own strategy engine reads live aggregation + fires Super Orders directly to Dhan.

---

## 🔬 What Postback contains (FYI for completeness)

If operator EVER wants to use Postback (e.g., as a redundant fill source), per `dhan-ref/07e-postback.md`:

```json
{
  "dhanClientId": "1100000123",
  "orderNo": "112345678",
  "exchOrderNo": "1300000123",
  "correlationId": "myStrategy123",
  "orderStatus": "TRADED",
  "transactionType": "BUY",
  "exchangeSegment": "NSE_EQ",
  "productType": "INTRADAY",
  "orderType": "LIMIT",
  "validity": "DAY",
  "tradingSymbol": "RELIANCE",
  "securityId": "2885",
  "quantity": 10,
  "price": 2845.50,
  "triggerPrice": 0.0,
  "filled_qty": 10,
  "remainingQuantity": 0,
  "averageTradedPrice": 2845.50,
  "exchOrderTime": "2026-05-12 10:15:23",
  "lastUpdatedTime": "2026-05-12 10:15:23",
  "legName": null,
  "boProfitValue": 0.0,
  "boStopLossValue": 0.0
}
```

**Triggers:** TRANSIT, PENDING, REJECTED, CANCELLED, TRADED, EXPIRED, modification, partial fill.

**Same content as Order-Update WS**, just different transport.

---

## 🚨 If operator's question was actually about TradingView integration

If operator was REALLY asking "can TradingView fire signals to Dhan via webhook?" — answer:

**Direct TV → Dhan = NO.** TV alerts hit a user-configured URL; they don't talk to Dhan directly.

**Indirect TV → us → Dhan = YES (if built):**
```
TV alert → POST to our /api/tv-webhook
            ↓
            our server validates + decides
            ↓
            calls Dhan API to place Super Order
            ↓
            tickvault tracks fill via Order-Update WS
```

**This is NOT in current tickvault scope.** If operator wants this, it's a separate feature.

---

## 🛡️ Z+ 7-Layer for Order Update (whether Postback or WS)

Per existing `dhan-ref/10-live-order-update-websocket.md` rules:

| Layer | Mechanism (using WS) |
|---|---|
| L1 DETECT | `tv_order_update_messages_received_total` counter |
| L2 VERIFY | Cross-check with `GET /v2/orders/{id}` after every fill |
| L3 RECONCILE | Every 30s: full order book sync vs local position state |
| L4 PREVENT | WS auth on connect; reconnect with token refresh |
| L5 AUDIT | `order_update_audit` table — every message logged |
| L6 RECOVER | On WS disconnect: reconcile via REST `/v2/orders` |
| L7 COOLDOWN | Between reconnects: per `auth-gap-*` cooldowns |

---

## 🚨 NEW worst-cases for Order Update (W231-W235)

| # | Scenario | Defense |
|---|---|---|
| W231 | Order-Update WS down at moment of fill | REST reconciliation catches within 30s |
| W232 | Operator accidentally enables Postback in addition → duplicates | Tickvault ignores Postback path; only WS path active |
| W233 | TV webhook fired but our /api/tv-webhook not implemented | Not a problem — TV integration is future scope; no current dependency |
| W234 | Fill notification arrives BEFORE position state machine ready | OMS state machine has pending-state for these races |
| W235 | Order-Update WS subscribes to wrong account (multi-account confusion) | Audit logs client_id on every message |

---

## 📊 Total worst-case coverage: 446

```
Prior 441 + NEW Order Update (W231-W235) = 446
```

---

## 🎤 Direct answer to operator

### Q: "This postback will work only from TradingView to Dhan?"

### A: **NO — postback is the OPPOSITE direction.**

| Confusion | Reality |
|---|---|
| TradingView → Dhan (postback)? | ❌ NO — TV doesn't talk to Dhan directly |
| Dhan → us (postback)? | ✅ YES — Dhan sends us HTTP POST on order events |
| Order-Update WS direction? | Dhan → us (via persistent WebSocket) |
| Does tickvault use Postback? | ❌ NO — we use Order-Update WS (no public URL needed) |
| TV integration in tickvault? | ❌ NOT in scope today |

**For our system:** Order-Update WebSocket is the channel for fill notifications. Postback is unnecessary. TradingView is separate (not integrated).

---

## 🚗 Final auto-driver summary

> Sir, three different "webhook-style" things:
>
> 1. **Dhan Postback** = "Dhan rings your doorbell when your order status changes"
>    - Direction: Dhan → YOUR public URL
>    - Need: public URL (operator inputs at web.dhan.co)
>    - We DON'T use this
>
> 2. **Dhan Order-Update WS** = "You hold an open phone line to Dhan; they call you on order events"
>    - Direction: Dhan → us (via persistent WebSocket)
>    - Need: just credentials (no public URL)
>    - We USE this ✅
>
> 3. **TradingView Alerts** = "TV rings YOUR doorbell when chart signal fires"
>    - Direction: TV → YOUR webhook URL
>    - Need: public URL
>    - We DON'T currently integrate — future scope
>
> Postback ≠ TradingView. Postback = Dhan-to-us. We use Order-Update WS instead (no public URL needed).

5 NEW worst-cases W231-W235. **Grand total: 446.**

Discussion mode continues. NO IMPLEMENTATION.
