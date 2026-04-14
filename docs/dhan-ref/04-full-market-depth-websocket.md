# Dhan V2 Full Market Depth WebSocket — Complete Reference

> **Source**: https://dhanhq.co/docs/v2/full-market-depth/
> **Extracted**: 2026-03-13
> **Related**: `08-annexure-enums.md`, `09-instrument-master.md`, `03-live-market-feed-websocket.md`

---

## 1. Overview

Full Market Depth provides 20-level and 200-level order book data via WebSocket. Binary responses, JSON requests.

- **Only NSE Equity and Derivatives** segments supported
- Separate WebSocket endpoints from Live Market Feed
- Bid and Ask packets arrive **separately** (not combined)

---

## 2. Establishing Connection

### 20-Level Endpoint

```
wss://depth-api-feed.dhan.co/twentydepth?token=<TOKEN>&clientId=<CLIENT_ID>&authType=2
```

### 200-Level Endpoint

```
wss://full-depth-api.dhan.co/?token=<TOKEN>&clientId=<CLIENT_ID>&authType=2
```

> **SDK verified (2026-04-06):** Dhan API (Python SDK ref) (fulldepth.py) uses `wss://full-depth-api.dhan.co/` (root path, no `/twohundreddepth`). Our code now matches the SDK. Earlier Dhan documentation referenced `/twohundreddepth` but the Dhan docs are the ground truth for what works in production.

| Parameter   | Required | Value                          |
|-------------|----------|--------------------------------|
| `token`     | Yes      | Access Token                   |
| `clientId`  | Yes      | Dhan Client ID                 |
| `authType`  | Yes      | `2` (always)                   |

> **Connection limits (confirmed by Dhan team, 2026-04-06):**
> The limit of **5 connections** applies to EACH WebSocket type **independently**:
> - Live Market Feed: 5 connections (separate pool)
> - 20-level Depth: 5 connections (separate pool)
> - 200-level Depth: 5 connections (separate pool)
>
> These are NOT a shared 5-connection cap. Depth connections remain connectable
> after 3:30 PM (post market close) — they simply won't return data.

---

## 3. Subscribing to Instruments

### 20-Level: Up to 50 instruments per connection

```json
{
    "RequestCode": 23,
    "InstrumentCount": 1,
    "InstrumentList": [
        {
            "ExchangeSegment": "NSE_EQ",
            "SecurityId": "1333"
        }
    ]
}
```

### 200-Level: Only 1 instrument per connection

```json
{
    "RequestCode": 23,
    "ExchangeSegment": "NSE_EQ",
    "SecurityId": "1333"
}
```

> **NOTE**: 200-level has a different JSON structure — no `InstrumentList` array, fields are flat.

---

## 4. Keeping Connection Alive

- Server pings every **10 seconds**
- Client must respond within **40 seconds** or connection drops
- Same ping-pong mechanism as Live Market Feed

---

## 5. Response Structure — 20-Level

### 5.1 Response Header (12 bytes)

| Byte Position | Type   | Size | Description                                                |
|---------------|--------|------|------------------------------------------------------------|
| `1-2`         | int16  | 2    | Message Length                                              |
| `3`           | byte   | 1    | Feed Response Code: `41`=Bid(Buy), `51`=Ask(Sell)          |
| `4`           | byte   | 1    | Exchange Segment (numeric — see 08-annexure Section 1)     |
| `5-8`         | int32  | 4    | Security ID                                                |
| `9-12`        | uint32 | 4    | Message Sequence (ignore)                                  |

> **DIFFERENT from Live Market Feed header**: 12 bytes (not 8), byte order is different (msg_length first), bytes 9-12 = sequence number.

### 5.2 Depth Packet (20 levels × 16 bytes = 320 bytes)

| Byte Position | Type         | Size | Description                              |
|---------------|-------------|------|------------------------------------------|
| `0-12`        | array       | 12   | Response Header                          |
| `13-332`      | Depth (20×16B) | 320  | 20 levels of depth data                 |

Each of the 20 levels (16 bytes per level):

| Byte Offset | Type    | Size | Description       |
|-------------|---------|------|-------------------|
| `1-8`       | float64 | 8    | Price             |
| `9-12`      | uint32  | 4    | Quantity          |
| `13-16`     | uint32  | 4    | No. of Orders     |

> **NOTE**: Price is `float64` (8 bytes, double precision), NOT `float32` like Live Market Feed. This is a key difference.

---

## 6. Response Structure — 200-Level

### 6.1 Response Header (12 bytes)

| Byte Position | Type   | Size | Description                                                |
|---------------|--------|------|------------------------------------------------------------|
| `1-2`         | int16  | 2    | Message Length                                              |
| `3`           | byte   | 1    | Feed Response Code: `41`=Bid(Buy), `51`=Ask(Sell)          |
| `4`           | byte   | 1    | Exchange Segment                                           |
| `5-8`         | int32  | 4    | Security ID                                                |
| `9-12`        | uint32 | 4    | **No. of Rows** (how many levels to read)                  |

> **CRITICAL DIFFERENCE from 20-level**: Bytes 9-12 = **row count** (not sequence number). This tells you how many of the 200 levels actually have data.

> **CRITICAL**: For 200-level depth, bytes 8-11 of the header contain the **row count** (number of active levels), NOT a sequence number. The actual packet size is `12 + (row_count × 16)` bytes, which may be LESS than the maximum 3212 bytes. Parsers MUST read the row count and parse only that many levels. Demanding the full 3212 bytes will cause parse failures on real-world data where the order book has fewer than 200 active levels.

### 6.2 Depth Packet (up to 200 levels × 16 bytes = 3200 bytes)

Same per-level structure as 20-level:

| Byte Offset | Type    | Size | Description       |
|-------------|---------|------|-------------------|
| `1-8`       | float64 | 8    | Price             |
| `9-12`      | uint32  | 4    | Quantity          |
| `13-16`     | uint32  | 4    | No. of Orders     |

---

## 7. Packet Stacking

When multiple instruments are subscribed (20-level only), packets are **stacked sequentially** in a single WebSocket message:

```
[Instrument1 Bid][Instrument1 Ask][Instrument2 Bid][Instrument2 Ask]...
```

Split packets by message length from the header.

---

## 8. Feed Disconnect

```json
{ "RequestCode": 12 }
```

Server disconnect packet: 14 bytes total = 12-byte header (with feed code 50) + 2-byte disconnect reason code (i16 LE at bytes 12-13, 0-based). This differs from the Live Market Feed disconnect (10 bytes: 8-byte header + 2-byte code at bytes 8-9). Per Dhan docs, the header remains 12 bytes with code 50, followed by an int16 disconnection message code. See 08-annexure Section 11 for disconnect reason codes. Note: Python SDK `fulldepth.py` has a bug — uses `<hBBiI>` (12 bytes) but tries to access index `[5]` (needs `<hBBiIH>` = 14 bytes).

---

## 9. Rust Struct Definitions

```rust
// ─── Full Market Depth Response Header (12 bytes) ───

#[derive(Debug, Clone)]
pub struct DepthResponseHeader {
    pub message_length: u16,           // bytes 1-2, uint16 LE (Dhan docs say int16; values always non-negative)
    pub response_code: u8,             // byte 3 (41=Bid, 51=Ask)
    pub exchange_segment: u8,          // byte 4 (numeric enum)
    pub security_id: u32,              // bytes 5-8, uint32 LE (Dhan docs say int32; values always non-negative)
    pub seq_or_rows: u32,              // bytes 9-12: sequence (20-lvl) OR row count (200-lvl)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepthSide {
    Bid = 41,   // Buy side
    Ask = 51,   // Sell side
}

impl DepthSide {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            41 => Some(Self::Bid),
            51 => Some(Self::Ask),
            _  => None,
        }
    }
}

// ─── Depth Level (16 bytes) ───
// NOTE: price is f64 (double), not f32 like Live Market Feed

#[derive(Debug, Clone, Copy, Default)]
pub struct FullDepthLevel {
    pub price: f64,            // 8 bytes, float64 LE
    pub quantity: u32,         // 4 bytes, uint32 LE
    pub orders: u32,           // 4 bytes, uint32 LE
}

// ─── 20-Level Depth Packet ───

#[derive(Debug, Clone)]
pub struct TwentyDepthPacket {
    pub header: DepthResponseHeader,
    pub side: DepthSide,
    pub levels: [FullDepthLevel; 20],
}

// ─── 200-Level Depth Packet ───

#[derive(Debug, Clone)]
pub struct TwoHundredDepthPacket {
    pub header: DepthResponseHeader,
    pub side: DepthSide,
    pub row_count: u32,                    // from header bytes 9-12
    pub levels: Vec<FullDepthLevel>,       // Vec because actual count varies
}
```

---

## 10. Critical Implementation Notes

1. **Header is 12 bytes, NOT 8** — different from Live Market Feed's 8-byte header. Don't reuse the same parser.

2. **Bytes 9-12 meaning changes by depth level**:
   - 20-level: sequence number (ignore it)
   - 200-level: **row count** (how many levels have actual data)

3. **Price is float64 (8 bytes)**, not float32 (4 bytes) — this is different from Live Market Feed which uses float32 for prices.

4. **Bid and Ask arrive as separate packets** — response code `41` for Bid (buy side), `51` for Ask (sell side). You need both to build the full order book.

5. **200-level: 1 instrument per connection** — if you need depth for 5 instruments, open 5 WebSocket connections.

6. **20-level: max 50 instruments per connection** — packets stack sequentially, split by message length.

7. **Only NSE segments supported** — NSE_EQ and NSE_FNO. No BSE, MCX, or currency.

8. **Response header byte order differs from Live Market Feed** — message length is bytes 1-2 (first), response code is byte 3. In Live Market Feed, response code is byte 1.

9. **Disconnect code `805`** — same as Live Market Feed, >5 connections kills the oldest.

10. **SDK Bug**: The Dhan API (Python SDK ref)'s `fulldepth.py` uses `subscribe_code + 1` for unsubscribe, which yields RequestCode 24 for depth. The correct unsubscribe code per the Dhan Annexure is **25**, not 24. Our implementation correctly uses 25.

---

## 11. Comparison: Live Market Feed vs Full Market Depth

| Aspect                    | Live Market Feed               | Full Market Depth (20-lvl)     | Full Market Depth (200-lvl)    |
|---------------------------|-------------------------------|-------------------------------|-------------------------------|
| Endpoint                  | `wss://api-feed.dhan.co`     | `wss://depth-api-feed.dhan.co/twentydepth` | `wss://full-depth-api.dhan.co/` |
| Header size               | 8 bytes                       | 12 bytes                      | 12 bytes                      |
| Header byte 1             | Response code                 | Message length (low byte)     | Message length (low byte)     |
| Depth levels              | 5 (Full mode only)            | 20                            | Up to 200                     |
| Price type                | float32 (4 bytes)             | float64 (8 bytes)             | float64 (8 bytes)             |
| Bid/Ask in same packet?   | Yes (interleaved in depth)    | No (separate packets)         | No (separate packets)         |
| Max instruments            | 5000                          | 50                            | 1                             |
| Bytes 9-12                | Payload data                  | Sequence number               | Row count                     |
| Segments supported        | All                           | NSE only                      | NSE only                      |
