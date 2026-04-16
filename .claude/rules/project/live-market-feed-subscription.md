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

### Subscription Strategy (Two-Stage)

**Stage 1 — Priority/ATM-Based:**
| Category | What's subscribed | Mode |
|----------|------------------|------|
| Major index values | NIFTY, BANKNIFTY, SENSEX, MIDCPNIFTY, FINNIFTY | Ticker (IDX_I forced) |
| Major index derivatives | ALL contracts (every expiry, every strike) | Quote/Full |
| Display indices | INDIA VIX, sector indices (~23) | Ticker |
| Stock equities | All NSE_EQ (~206 stocks) | Quote/Full |
| Stock derivatives | Current expiry, ATM ± 10 strikes per underlying | Quote/Full |

**Stage 2 — Progressive Fill:**
- Remaining stock derivatives (further expiries)
- Sorted: nearest expiry first, then by symbol, then by security_id
- Fills remaining capacity up to 25,000 instruments
- Skipped instruments logged with count

### Why NO Dynamic Rebalancing (by design)

The main feed subscribes to ~20,000-25,000 instruments. With this capacity:
- Index derivatives: ALL strikes ALL expiries — ATM shift doesn't matter
- Stock derivatives: ATM ± 10 at boot — 10-strike buffer covers typical intraday drift
- Progressive fill adds more stock derivatives by nearest expiry

**Contrast with depth feed:**
- Depth 20-level = 49 instruments (precise ATM ± 24, needs rebalancing)
- Depth 200-level = 1 instrument (exact ATM, needs rebalancing)
- Main feed = 25,000 instruments (broad coverage, no rebalancing needed)

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
