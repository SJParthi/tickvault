# Dhan Full Market Depth WebSocket Enforcement

> **Ground truth:** `docs/dhan-ref/04-full-market-depth-websocket.md`
> **Scope:** Any file touching 20-level or 200-level depth WebSocket parsing, subscription, or connection.
> **Cross-reference:** `docs/dhan-ref/08-annexure-enums.md` (ExchangeSegment, disconnect codes), `docs/dhan-ref/03-live-market-feed-websocket.md` (comparison)

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any depth parser, subscription builder, or depth connection: `Read docs/dhan-ref/04-full-market-depth-websocket.md`.

2. **Two SEPARATE endpoints. Never mix them.**
   - 20-level: `wss://depth-api-feed.dhan.co/twentydepth?token=<TOKEN>&clientId=<CLIENT_ID>&authType=2`
   - 200-level: `wss://full-depth-api.dhan.co/?token=<TOKEN>&clientId=<CLIENT_ID>&authType=2` (SDK uses root path, no `/twohundreddepth`)

3. **Connection limits are INDEPENDENT per WebSocket type (confirmed by Dhan 2026-04-06).**
   - Live Market Feed: 5 connections (separate pool)
   - 20-level Depth: 5 connections (separate pool)
   - 200-level Depth: 5 connections (separate pool)
   - These are NOT a shared cap. Depth connections are connectable even post 3:30 PM (no data returned).

4. **Header is 12 bytes, NOT 8.** Different from Live Market Feed's 8-byte header. Do NOT reuse the same header parser.
   | Byte (0-based) | Type | Field |
   |---|---|---|
   | `0-1` | i16 LE | Message Length |
   | `2` | u8 | Response Code: `41`=Bid, `51`=Ask |
   | `3` | u8 | Exchange Segment (numeric) |
   | `4-7` | i32 LE | Security ID |
   | `8-11` | u32 LE | Sequence (20-lvl) OR Row Count (200-lvl) |

4. **Header byte order differs from Live Market Feed.**
   - Live Feed: byte 0 = response code, bytes 1-2 = message length
   - Full Depth: bytes 0-1 = message length, byte 2 = response code
   - Getting this wrong = parsing garbage.

5. **Bid/Ask arrive as SEPARATE packets.** Response code `41`=Bid (Buy side), `51`=Ask (Sell side). NOT combined like Live Market Feed Full packet. You need both to build the full order book.

6. **Depth prices are f64 (8 bytes), NOT f32.** This is DIFFERENT from Live Market Feed which uses f32 for prices.
   Each level = 16 bytes:
   | Byte Offset | Type | Field |
   |---|---|---|
   | `0-7` | f64 LE | Price |
   | `8-11` | u32 LE | Quantity |
   | `12-15` | u32 LE | Number of Orders |

7. **20-level packet = 332 bytes total.** 12 header + 320 depth (20 levels × 16 bytes).

8. **200-level max = 3212 bytes total.** 12 header + up to 3200 depth (200 levels × 16 bytes). Actual size depends on row count from header bytes 8-11.

9. **Bytes 8-11 meaning changes by depth type.**
   - 20-level: Sequence number (ignore it — informational only)
   - 200-level: **Row count** — determines how many levels actually have data. The packet size is variable: `12 + (row_count × 16)`. Parse ONLY this many levels. Do NOT demand the full 3212 bytes. Validate `row_count <= 200` before parsing.
   - Getting this wrong on 200-level = parsing N garbage levels.

10. **200-level = 1 instrument per connection.** Different JSON structure — no `InstrumentList` array, fields are flat:
    ```json
    { "RequestCode": 23, "ExchangeSegment": "NSE_EQ", "SecurityId": "1333" }
    ```

11. **20-level = up to 50 instruments per connection.** Same JSON structure as Live Market Feed:
    ```json
    { "RequestCode": 23, "InstrumentCount": 1, "InstrumentList": [{ "ExchangeSegment": "NSE_EQ", "SecurityId": "1333" }] }
    ```

12. **RequestCode = 23 for BOTH subscribe and depth types.** Unsubscribe = 25.

13. **Only NSE segments valid.** NSE_EQ and NSE_FNO only. BSE, MCX, Currency are NOT available for Full Market Depth. Reject at subscription build time — do not send and wait for error.

14. **Packet stacking (20-level only).** When multiple instruments subscribed, packets stack sequentially in one WebSocket message: `[Inst1 Bid][Inst1 Ask][Inst2 Bid][Inst2 Ask]...`. Split by message length from header.

15. **All reads are Little Endian.** See `dhan-live-market-feed.md` rule 13.

16. **Disconnect: code 50, reason in bytes 12-13 (0-based), i16 LE.** The depth disconnect packet is 14 bytes total: 12-byte depth header + 2-byte disconnect reason code at bytes 12-13. This is DIFFERENT from the Live Market Feed disconnect (10 bytes: 8-byte header + 2-byte code at bytes 8-9). The Python SDK's `fulldepth.py` has a bug: it uses `<hBBiI>` (12 bytes, 5 fields) but accesses index `[5]` which causes IndexError — the correct format should be `<hBBiIH>` (14 bytes, 6 fields) where the trailing `H` (u16) at bytes 12-13 is the disconnect reason code. Per Dhan docs: header is 12 bytes with code 50, followed by int16 disconnect message code.

17. **Ping/pong: let WebSocket library handle it.** See `dhan-live-market-feed.md` rule 16.

## What This Prevents

- f32 parse on f64 depth price → garbled prices → wrong trading decisions
- 8-byte header parser on 12-byte header → every field offset wrong → total garbage
- 20-level header semantics applied to 200-level → parse N garbage levels
- BSE/MCX subscription → silent failure, no data
- Multiple instruments on 200-level connection → undefined behavior
- Bid/Ask not separated → order book assembled wrong → dangerous decisions

## Trigger

This rule activates when editing files matching:
- `crates/core/src/parser/market_depth.rs`
- `crates/core/src/parser/deep_depth.rs`
- `crates/core/src/parser/types.rs` (DepthLevel, DepthSide)
- `crates/core/src/websocket/connection_pool.rs`
- `crates/core/src/websocket/subscription_builder.rs`
- `crates/core/src/instrument/validation.rs` (NSE-only check)
- `crates/common/src/tick_types.rs` (depth price type)
- Any file containing `DepthLevel`, `DepthPacket`, `DepthSide`, `DepthResponseHeader`, `TwentyDepthPacket`, `TwoHundredDepthPacket`, `FullDepthLevel`, `parse_depth`, `parse_market_depth`, `twentydepth`, `twohundreddepth`, `depth-api-feed.dhan.co`, `full-depth-api.dhan.co`
