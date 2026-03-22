# Dhan Live Market Feed WebSocket Enforcement

> **Ground truth:** `docs/dhan-ref/03-live-market-feed-websocket.md`
> **Scope:** Any file touching WebSocket binary packet parsing, byte offsets, packet routing, subscription messages, or tick processing.
> **Cross-reference:** `docs/dhan-ref/08-annexure-enums.md` (FeedRequestCode, FeedResponseCode, ExchangeSegment, disconnect codes)

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any packet parser, byte offset, or subscription builder: `Read docs/dhan-ref/03-live-market-feed-websocket.md`.

2. **WebSocket endpoint — exact URL.**
   `wss://api-feed.dhan.co?version=2&token=<ACCESS_TOKEN>&clientId=<CLIENT_ID>&authType=2`
   All 4 query params required: `version=2`, `token`, `clientId`, `authType=2`.

3. **Connection limits.**
   - Max 5 WebSocket connections per user
   - Max 5000 instruments per connection
   - Max 100 instruments per single JSON subscribe message (send multiple messages to reach 5000)

4. **Response header — 8 bytes, every packet.**
   | Byte (0-based) | Type | Description |
   |---|---|---|
   | `0` | u8 | Feed Response Code |
   | `1-2` | u16 LE | Message Length (Dhan docs say i16 but values are non-negative; SDK and our code use unsigned) |
   | `3` | u8 | Exchange Segment (numeric) |
   | `4-7` | u32 LE | Security ID (Dhan docs say i32 but values are non-negative; SDK and our code use unsigned) |

5. **Packet sizes must match exactly.**
   - Ticker (code 2) = **16 bytes**
   - Quote (code 4) = **50 bytes**
   - OI (code 5) = **12 bytes**
   - PrevClose (code 6) = **16 bytes**
   - MarketStatus (code 7) = **8 bytes** (header only, no payload; SDK format `<BHBI>`)
   - Full (code 8) = **162 bytes**
   - Disconnect (code 50) = **10 bytes**

6. **Ticker packet (code 2) — 16 bytes.**
   | Byte (0-based) | Type | Field |
   |---|---|---|
   | `0-7` | header | 8 bytes |
   | `8-11` | f32 LE | LTP |
   | `12-15` | u32 LE | LTT (IST epoch seconds — NOT UTC; subtract 19800 for UTC) |

7. **PrevClose packet (code 6) — 16 bytes.** Arrives on EVERY subscription, any mode.
   | Byte (0-based) | Type | Field |
   |---|---|---|
   | `0-7` | header | 8 bytes |
   | `8-11` | f32 LE | Previous Close Price |
   | `12-15` | u32 LE | Previous Day OI |

8. **Quote packet (code 4) — 50 bytes.**
   | Byte (0-based) | Type | Field |
   |---|---|---|
   | `8-11` | f32 LE | LTP |
   | `12-13` | u16 LE | LTQ |
   | `14-17` | u32 LE | LTT (IST epoch seconds — NOT UTC) |
   | `18-21` | f32 LE | ATP |
   | `22-25` | u32 LE | Volume |
   | `26-29` | u32 LE | Total Sell Qty |
   | `30-33` | u32 LE | Total Buy Qty |
   | `34-37` | f32 LE | Day Open |
   | `38-41` | f32 LE | Day Close (post-market only) |
   | `42-45` | f32 LE | Day High |
   | `46-49` | f32 LE | Day Low |

9. **OI packet (code 5) — 12 bytes.** Separate packet from Quote. Both arrive when subscribed to Quote mode.
   | Byte (0-based) | Type | Field |
   |---|---|---|
   | `8-11` | u32 LE | Open Interest |

10. **Full packet (code 8) — 162 bytes.**
    | Byte (0-based) | Type | Field |
    |---|---|---|
    | `8-11` | f32 LE | LTP |
    | `12-13` | u16 LE | LTQ |
    | `14-17` | u32 LE | LTT (IST epoch seconds — NOT UTC) |
    | `18-21` | f32 LE | ATP |
    | `22-25` | u32 LE | Volume |
    | `26-29` | u32 LE | Total Sell Qty |
    | `30-33` | u32 LE | Total Buy Qty |
    | `34-37` | u32 LE | OI (Derivatives only) |
    | `38-41` | u32 LE | Highest OI (NSE_FNO only) |
    | `42-45` | u32 LE | Lowest OI (NSE_FNO only) |
    | `46-49` | f32 LE | Day Open |
    | `50-53` | f32 LE | Day Close (post-market only) |
    | `54-57` | f32 LE | Day High |
    | `58-61` | f32 LE | Day Low |
    | `62-161` | depth | 5 levels × 20 bytes each |

11. **Market Depth in Full packet — 20 bytes per level × 5 levels.**
    Each level (byte offset within level, 0-based):
    | Offset | Type | Field |
    |---|---|---|
    | `0-3` | u32 LE | Bid Quantity |
    | `4-7` | u32 LE | Ask Quantity |
    | `8-9` | u16 LE | Bid Orders |
    | `10-11` | u16 LE | Ask Orders |
    | `12-15` | f32 LE | Bid Price |
    | `16-19` | f32 LE | Ask Price |
    Parse as: `for level in 0..5 { offset = 62 + (level * 20); ... }`

12. **Disconnect packet (code 50) — 10 bytes.**
    | Byte (0-based) | Type | Field |
    |---|---|---|
    | `8-9` | u16 LE | Disconnect reason code |
    Key code: `805` = >5 connections, oldest killed.

13. **All reads are Little Endian.** Every `from_le_bytes()` call. NEVER `from_be_bytes()`. No exceptions.

14. **Field types must match exactly.**
    - LTP, ATP, prices: `f32` (NOT f64 — that's Full Market Depth)
    - Volume, OI, quantities: `u32` (unsigned — quantities/OI cannot be negative)
    - LTQ: `u16` (unsigned)
    - Orders count: `u16` (unsigned)
    - LTT (timestamps): `u32` (unsigned — IST epoch seconds; subtract 19800 for UTC epoch)
    - Message length: `u16` (unsigned)
    - SecurityId: `u32` (unsigned in header and ParsedTick; Dhan docs say i32 but values are non-negative)
    - Disconnect reason code: `u16` (unsigned)

15. **Subscription messages are JSON with STRING security IDs.**
    - `SecurityId` must serialize as `"1333"` not `1333`
    - `InstrumentCount` must match the actual array length
    - `ExchangeSegment` uses string enum (`"NSE_EQ"`, `"NSE_FNO"`, etc.)

16. **Ping/pong is handled by the WebSocket library.** Do NOT implement manual ping frames. Server pings every 10s, timeout at 40s.

17. **Byte indexing: Dhan docs use 1-based, code uses 0-based.** "Bytes 9-12" in docs = `buffer[8..12]` in Rust. All byte tables in THIS rule file use 0-based indexing.

18. **Signedness**: All integer fields in the binary protocol (quantities, volumes, OI, timestamps, message lengths, security IDs) are non-negative. The Python SDK uses unsigned types (`H`=u16, `I`=u32). Our Rust code follows this convention with `u16`/`u32`.

## What This Prevents

- Wrong byte offset → reading price from volume field → silent data corruption
- Wrong packet size check → truncated reads → garbage data
- Wrong field type (f64 vs f32) → wrong price values
- Big-endian reads → completely garbled values
- Numeric SecurityId in JSON → Dhan rejects subscription → no data
- Missing response code handler → unrouted packets → lost ticks
- Wrong depth offset → bid/ask swap → catastrophic trading decisions
- Manual ping → duplicate pings → connection instability

## Trigger

This rule activates when editing files matching:
- `crates/core/src/parser/*.rs`
- `crates/core/src/websocket/*.rs`
- `crates/core/src/pipeline/tick_processor.rs`
- `crates/core/src/pipeline/candle_aggregator.rs`
- `crates/common/src/constants.rs`
- `crates/common/src/tick_types.rs`
- `crates/storage/src/tick_persistence.rs`
- `crates/core/tests/websocket_protocol_e2e.rs`
- `crates/core/tests/parser_pipeline.rs`
- `crates/core/tests/snapshot_parser.rs`
- `crates/core/benches/tick_parser.rs`
- Any file containing `parse_ticker_packet`, `parse_quote_packet`, `parse_full_packet`, `parse_oi_packet`, `parse_prev_close_packet`, `parse_disconnect_packet`, `PacketHeader`, `ResponseHeader`, `TICKER_PACKET_SIZE`, `QUOTE_PACKET_SIZE`, `FULL_PACKET_SIZE`, `FeedResponseCode`, `SubscribeRequest`, `api-feed.dhan.co`, `MarketDepthLevel`
