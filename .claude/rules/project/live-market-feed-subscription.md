# Live Market Feed WebSocket Subscription Rules

> **Authority:** CLAUDE.md > this file.
> **Scope:** Any file touching main WebSocket connection pool, subscription planning, or instrument distribution.
> **Ground truth:** `docs/dhan-ref/03-live-market-feed-websocket.md`, `docs/dhan-ref/08-annexure-enums.md`

## Architecture

### Connection Pool
- **5 connections** (max per Dhan account, independent from depth pools)
- **5,000 instruments per connection** (max per Dhan docs)
- **25,000 total capacity** (5 x 5,000)
- **Distribution:** Round-robin at boot (static, pre-allocated)
- **Endpoint:** `wss://api-feed.dhan.co?version=2&token=<TOKEN>&clientId=<CLIENT_ID>&authType=2`

### Subscription Strategy (Two-Phase)

**Phase 1 — Boot (before 9:00 AM):**
| Category | What's subscribed | Mode |
|----------|------------------|------|
| Major index values | NIFTY, BANKNIFTY, SENSEX, MIDCPNIFTY, FINNIFTY | Ticker (IDX_I forced) |
| Major index derivatives | ALL contracts (every expiry, every strike) | Quote/Full |
| Display indices | INDIA VIX, sector indices (~23) | Ticker |
| Stock equities | All NSE_EQ (~206 stocks) | Quote/Full |
| Stock derivatives | NOT subscribed yet — waiting for pre-market price | — |

**Phase 2 — 9:12 AM (pre-market finalized price available):**
| Category | What's subscribed | Mode |
|----------|------------------|------|
| Stock derivatives | Current expiry only, ATM ± 25 CE + PE + Future | Quote/Full |

ATM is calculated from the **9:08 AM finalized pre-market price** (or the latest
available LTP at 9:12 AM — whichever Dhan sent most recently). Binary search on
sorted option chain finds the nearest strike to the spot price. No median fallback.
If a stock has no pre-market price at 9:12 (didn't trade in pre-market), its F&O
is SKIPPED.

**Fixed for the entire day.** No dynamic resubscription for stocks — ATM ± 25
covers ± 18-62% of price depending on stock, which exceeds the NSE 20% circuit
breaker limit. Mathematically guaranteed sufficient.

**Stage 2 — Progressive Fill:**
- Remaining stock derivatives (further expiries) fill remaining capacity to 25K
- Sorted: nearest expiry first, then by symbol, then by security_id
- Skipped instruments logged with count

### Why NO Dynamic Rebalancing for Stocks (by design)

- ATM ± 25 covers ≥ 18% of stock price (even for highest-priced stocks)
- NSE circuit breaker = 20% — no stock can move beyond that intraday
- Therefore ATM ± 25 is **mathematically guaranteed** to cover any intraday move
- ATM is recalculated fresh every day at 9:12 AM from real pre-market price

**Contrast with depth feed:**
- Depth 20-level = 49 instruments (ATM ± 24, needs rebalancing — narrow band)
- Depth 200-level = 1 instrument (exact ATM, needs rebalancing)
- Main feed stocks = ATM ± 25 (wide band, no rebalancing needed)

### RequestCodes

| Code | Action | Status |
|------|--------|--------|
| 15 | Subscribe Ticker | Used at boot |
| 16 | Unsubscribe Ticker | Implemented, unused operationally |
| 17 | Subscribe Quote | Used at boot |
| 18 | Unsubscribe Quote | Implemented, unused operationally |
| 21 | Subscribe Full | Used at boot |
| 22 | Unsubscribe Full | Implemented, unused operationally |
| 12 | Disconnect | Used on graceful shutdown |

Unsubscribe codes 16/18/22 are built by `subscription_builder.rs` but only used
during graceful shutdown (A5). No live instrument swaps are performed.

### IDX_I Special Case

Indices (segment code 0) are forced to Ticker mode regardless of configured feed
mode. Dhan silently ignores Quote/Full subscriptions for IDX_I. The subscription
builder pre-sorts IDX_I instruments into separate Ticker-mode batches.

## Key Files

| File | Purpose |
|------|---------|
| `crates/core/src/websocket/connection_pool.rs` | Pool of 5 connections, round-robin distribution |
| `crates/core/src/websocket/connection.rs` | Individual WS connection, read loop, reconnect |
| `crates/core/src/instrument/subscription_planner.rs` | Two-stage instrument selection |
| `crates/core/src/websocket/subscription_builder.rs` | JSON message batching (max 100 per msg) |

## What This Prevents

- Subscribing to wrong instruments (ATM filtering ensures relevance)
- Exceeding Dhan limits (5 connections, 5000/conn, 100/msg enforced)
- Numeric SecurityId in JSON (must be string: `"1333"` not `1333`)
- Wrong feed mode for indices (IDX_I forced to Ticker)
- Confusion with depth protocol (main feed = 8-byte header, f32 prices)

## Trigger

This rule activates when editing files matching:
- `crates/core/src/websocket/connection_pool.rs`
- `crates/core/src/websocket/connection.rs`
- `crates/core/src/instrument/subscription_planner.rs`
- `crates/core/src/websocket/subscription_builder.rs`
- Any file containing `WebSocketConnectionPool`, `WebSocketConnection`, `build_subscription_messages`, `build_subscription_plan`, `SubscriptionPlan`, `FeedRequestCode`, `api-feed.dhan.co`
