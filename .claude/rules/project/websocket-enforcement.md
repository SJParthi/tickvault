---
paths:
  - "crates/core/src/websocket/**/*.rs"
  - "crates/core/src/parser/**/*.rs"
  - "crates/core/src/pipeline/tick_processor.rs"
  - "crates/storage/src/tick_persistence.rs"
  - "crates/storage/src/deep_depth_persistence.rs"
---

# WebSocket Enforcement Rules

> **Ground truth:** `docs/architecture/websocket-complete-reference.md`
> **Dhan refs:** `docs/dhan-ref/03-live-market-feed-websocket.md`, `docs/dhan-ref/04-full-market-depth-websocket.md`, `docs/dhan-ref/10-live-order-update-websocket.md`
> **Scope:** Any file touching WebSocket connections, binary parsing, subscription building, depth processing, or tick persistence.

## Mechanical Rules

1. **Read the reference first.** Before modifying ANY WebSocket-related code: `Read docs/architecture/websocket-complete-reference.md`. This is mandatory, not optional.

2. **Also read the relevant Dhan ground truth:**
   - Main feed changes → `Read docs/dhan-ref/03-live-market-feed-websocket.md`
   - Depth changes → `Read docs/dhan-ref/04-full-market-depth-websocket.md`
   - Order update changes → `Read docs/dhan-ref/10-live-order-update-websocket.md`
   - Enum/code changes → `Read docs/dhan-ref/08-annexure-enums.md`

3. **Four WebSocket types — NEVER confuse their protocols.**
   - Main feed: 8-byte header, byte 0 = response code, prices are f32
   - Depth (20 + 200): 12-byte header, bytes 0-1 = message length, prices are f64
   - Order update: JSON (not binary at all)
   - Getting header byte order wrong = garbage data for every field

4. **Depth has NO timestamp.** Only `received_at_nanos` (local clock). Never reference `exchange_timestamp` or `LTT` in depth code.

5. **200-level bytes 8-11 = row count, NOT sequence.** 20-level bytes 8-11 = sequence (ignore). Mixing these up = parsing garbage levels.

6. **Depth bid/ask arrive SEPARATELY.** Code 41 = Bid, 51 = Ask. Two packets per update. Never assume combined.

7. **200-level: 1 instrument per connection.** 20-level: up to 50. Main feed: up to 5,000.

8. **Connection pools are independent.** 5 main + 5 depth-20 + 5 depth-200 = 15 total available. Using main feed slots does NOT reduce depth slots.

9. **All binary reads are Little Endian.** `from_le_bytes()` always. Never big endian.

10. **SecurityId: u32 in binary, STRING in JSON.** Binary header bytes 4-7 = u32. JSON subscribe = `"1333"` (quoted string).

11. **Order update field names differ from REST.** Single-char codes (`C`=CNC, `B`=Buy, etc.), PascalCase keys, `MsgCode: 42`.

12. **Unsubscribe depth = code 25, NOT 24.** Dhan SDK has a bug here (subscribe_code + 1 = 24). Our code correctly uses 25.

## Gaps to Track

Any change to WebSocket code must check the open gaps in `docs/architecture/websocket-complete-reference.md` Section 10.2 and not introduce regressions.

Key open gaps:
- `WebSocketDisconnected` event not wired for main feed
- No Telegram for depth rebalancer ATM changes

Resolved gaps:
- Telegram now fires on first data frame (not just subscription) — fixed 2026-04-09
- Depth connections stay connected 24/7 (no market-hours sleep) — fixed 2026-04-09
- Depth metrics labeled per underlying (not shared gauge) — fixed 2026-04-09

## What This Prevents

- f32 parser on f64 depth prices (or vice versa)
- 8-byte header parser on 12-byte depth header
- 20-level header semantics on 200-level (sequence vs row count)
- Missing Little Endian reads
- Numeric SecurityId in JSON subscribe
- Binary parser on JSON order update
- REST field names on WS order data
- Depth code confused with main feed code

## Trigger

This rule activates when editing files matching the paths above, or any file containing:
`WebSocketConnection`, `WebSocketConnectionPool`, `DepthConnection`, `OrderUpdateConnection`, `parse_ticker_packet`, `parse_quote_packet`, `parse_full_packet`, `parse_oi_packet`, `parse_prev_close_packet`, `parse_disconnect_packet`, `parse_twenty_depth_packet`, `parse_two_hundred_depth_packet`, `parse_deep_depth_header`, `dispatch_deep_depth_frame`, `run_twenty_depth_connection`, `run_two_hundred_depth_connection`, `run_order_update_connection`, `build_subscription_messages`, `SubscribeRequest`, `InstrumentSubscription`, `api-feed.dhan.co`, `depth-api-feed.dhan.co`, `full-depth-api.dhan.co`, `api-order-update.dhan.co`
