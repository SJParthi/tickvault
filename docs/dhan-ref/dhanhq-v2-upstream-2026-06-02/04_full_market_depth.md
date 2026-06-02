# Full Market Depth (20 & 200 level) — DhanHQ v2

> Source: https://dhanhq.co/docs/v2/full-market-depth/ · API v2 · Audited 2 Jun 2026

Level-3 market depth up to **20 levels** and **200 levels**, streamed real-time over WebSocket — useful for detecting demand/supply zones beyond the standard 5-level depth. **Only NSE Equity and Derivatives segments are enabled.** Requests are JSON; responses are binary, little-endian.

## Establishing connection
- **20 level:** `wss://depth-api-feed.dhan.co/twentydepth?token=eyxxxxx&clientId=100xxxxxxx&authType=2`
- **200 level:** `wss://full-depth-api.dhan.co/twohundreddepth?token=eyxxxxx&clientId=100xxxxxxx&authType=2`

| Field | Description |
|---|---|
| `token` (req) | Access token |
| `clientId` (req) | User ID |
| `authType` (req) | `2` by default |

## Adding instruments (subscribe)

### 20 level — up to 50 instruments per connection
All 50 can go in one message, or split across messages.
```json
{
  "RequestCode": 23,
  "InstrumentCount": 1,
  "InstrumentList": [ { "ExchangeSegment": "NSE_EQ", "SecurityId": "1333" } ]
}
```
| Field | Type | Description |
|---|---|---|
| `RequestCode` (req) | int | `23` for Full Market Depth |
| `InstrumentCount` (req) | int | Instruments in this message |
| `InstrumentList.ExchangeSegment` (req) | enum string | See `08_annexure.md` |
| `InstrumentList.SecurityId` (req) | string | From instrument master |

### 200 level — only 1 instrument per connection
```json
{ "RequestCode": 23, "ExchangeSegment": "NSE_EQ", "SecurityId": "1333" }
```
| Field | Type | Description |
|---|---|---|
| `RequestCode` (req) | int | `23` |
| `ExchangeSegment` (req) | enum string | See `08_annexure.md` |
| `SecurityId` (req) | string | From instrument master |

## Keeping connection alive
Same ping/pong as the live feed: server pings every 10 s; >40 s of client silence → disconnect.

## Response structure
Binary packets = [Response Header](#response-header) + payload, keyed by Feed Response Code.

### 20 level

#### Response header (12 bytes) — note order differs from the live-feed header
| Bytes | Type | Size | Description |
|---|---|---|---|
| `1-2` | int16 | 2 | Message length of entire payload |
| `3` | byte | 1 | Feed Response Code |
| `4` | byte | 1 | Exchange Segment |
| `5-8` | int32 | 4 | Security ID |
| `9-12` | uint32 | 4 | Message sequence (ignore) |

#### Depth packet
Bid (Buy) and Ask (Sell) arrive as **separate** packets, each with **20 entries × 16 bytes = 320 bytes**.
| Bytes | Type | Size | Description |
|---|---|---|---|
| `0-12` | header | 12 | `41` = Bid (Buy), `51` = Ask (Sell) |
| `13-332` | depth structure | 320 | 20 × 16-byte entries |

### 200 level

#### Response header (12 bytes)
| Bytes | Type | Size | Description |
|---|---|---|---|
| `1-2` | int16 | 2 | Message length of entire payload |
| `3` | byte | 1 | Feed Response Code |
| `4` | byte | 1 | Exchange Segment |
| `5-8` | int32 | 4 | Security ID |
| `9-12` | uint32 | 4 | **No. of rows** to read for the response |

#### Depth packet
**200 entries × 16 bytes = 3200 bytes** per packet.
| Bytes | Type | Size | Description |
|---|---|---|---|
| `0-12` | header | 12 | `41` = Bid (Buy), `51` = Ask (Sell) |
| `13-3212` | depth structure | 3200 | 200 × 16-byte entries |

### 16-byte depth entry (both 20 & 200 level)
| Bytes | Type | Size | Description |
|---|---|---|---|
| `1-8` | **float64** | 8 | Price |
| `9-12` | uint32 | 4 | Quantity |
| `13-16` | uint32 | 4 | No. of Orders |

**Packet stacking:** when several instruments are subscribed (20-level), packets are concatenated in one message in order: instrument-1 Bid, instrument-1 Ask, instrument-2 Bid, instrument-2 Ask, … Split by the length field (header bytes 1-2) to decode.

## Feed disconnect
Client request:
```json
{ "RequestCode": 12 }
```
Server disconnect packet (response code `50`):
| Bytes | Type | Size | Description |
|---|---|---|---|
| `0-12` | header (code 50) | — | |
| `13-14` | int16 | 2 | Disconnection reason code (see `08_annexure.md`) |

- More than 5 WebSockets → the **oldest** is dropped with code `805` per additional connection.
