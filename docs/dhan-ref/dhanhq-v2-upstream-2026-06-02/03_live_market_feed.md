# Live Market Feed — DhanHQ v2

> Source: https://dhanhq.co/docs/v2/live-market-feed/ · API v2 · Audited 2 Jun 2026

Real-time, **tick-by-tick** market data over WebSocket. **Requests are JSON; responses are binary, little-endian.** You need a WebSocket library plus a binary parser. The DhanHQ Python library can quick-start this (see `15_python_sdk_dhanhq.md`).

- **Up to 5 WebSocket connections per user**, **5000 instruments per connection**.
- **Max 100 instruments per subscription JSON message** (send multiple messages to reach 5000).

## Establishing connection
```
wss://api-feed.dhan.co?version=2&token=eyxxxxx&clientId=100xxxxxxx&authType=2
```
| Field | Description |
|---|---|
| `version` (req) | `2` for DhanHQ v2 |
| `token` (req) | Access token |
| `clientId` (req) | User ID |
| `authType` (req) | `2` by default |

## Adding instruments (subscribe)
```json
{
  "RequestCode": 15,
  "InstrumentCount": 2,
  "InstrumentList": [
    { "ExchangeSegment": "NSE_EQ", "SecurityId": "1333" },
    { "ExchangeSegment": "BSE_EQ", "SecurityId": "532540" }
  ]
}
```
| Field | Type | Description |
|---|---|---|
| `RequestCode` (req) | int | Data-mode code — see Feed Request Code in `08_annexure.md` (15 Ticker, 17 Quote, 21 Full…) |
| `InstrumentCount` (req) | int | Instruments in this message (≤100) |
| `InstrumentList.ExchangeSegment` (req) | enum string | See `08_annexure.md` |
| `InstrumentList.SecurityId` (req) | string | From instrument master (`09_instruments.md`) |

## Keeping connection alive
Server sends a **ping every 10 s**; the WS library auto-sends pong. **If the client is silent >40 s, the server closes the connection** and you must reconnect.

## Market data modes
Three modes: **Ticker**, **Quote**, **Full**. All responses = [Response Header](#response-header) + payload, distinguished by Feed Response Code.

**Endianness:** data is **Little Endian**. Define endianness explicitly if your host is Big Endian.

### Response header (8 bytes — common to all feed packets)
| Bytes | Type | Size | Description |
|---|---|---|---|
| `1` | byte | 1 | Feed Response Code (see `08_annexure.md`) |
| `2-3` | int16 | 2 | Message length of entire payload |
| `4` | byte | 1 | Exchange Segment |
| `5-8` | int32 | 4 | Security ID |

### Ticker packet — response code `2`
LTP + Last Traded Time.
| Bytes | Type | Size | Description |
|---|---|---|---|
| `0-8` | header (code 2) | 8 | |
| `9-12` | float32 | 4 | Last Traded Price |
| `13-16` | int32 | 4 | Last Trade Time (EPOCH) |

#### Prev close — response code `6`
Sent automatically on any subscribe, for day-on-day comparison.
| Bytes | Type | Size | Description |
|---|---|---|---|
| `0-8` | header (code 6) | 8 | |
| `9-12` | float32 | 4 | Previous-day close price |
| `13-16` | int32 | 4 | Previous-day Open Interest |

### Quote packet — response code `4`
| Bytes | Type | Size | Description |
|---|---|---|---|
| `0-8` | header (code 4) | 8 | |
| `9-12` | float32 | 4 | Last Traded Price |
| `13-14` | int16 | 2 | Last Traded Quantity |
| `15-18` | int32 | 4 | Last Trade Time (EPOCH) |
| `19-22` | float32 | 4 | Average Trade Price |
| `23-26` | int32 | 4 | Volume |
| `27-30` | int32 | 4 | Total Sell Quantity |
| `31-34` | int32 | 4 | Total Buy Quantity |
| `35-38` | float32 | 4 | Day Open |
| `39-42` | float32 | 4 | Day Close (post-close only) |
| `43-46` | float32 | 4 | Day High |
| `47-50` | float32 | 4 | Day Low |

#### OI data — response code `5`
Sent alongside Quote for derivative contracts.
| Bytes | Type | Size | Description |
|---|---|---|---|
| `0-8` | header (code 5) | 8 | |
| `9-12` | int32 | 4 | Open Interest |

### Full packet — response code `8`
Complete trade data + market depth + OI in one packet.
| Bytes | Type | Size | Description |
|---|---|---|---|
| `0-8` | header (code 8) | 8 | |
| `9-12` | float32 | 4 | Last Traded Price |
| `13-14` | int16 | 2 | Last Traded Quantity |
| `15-18` | int32 | 4 | Last Trade Time (EPOCH) |
| `19-22` | float32 | 4 | Average Trade Price |
| `23-26` | int32 | 4 | Volume |
| `27-30` | int32 | 4 | Total Sell Quantity |
| `31-34` | int32 | 4 | Total Buy Quantity |
| `35-38` | int32 | 4 | Open Interest (derivatives) |
| `39-42` | int32 | 4 | Highest OI for the day (NSE_FNO only) |
| `43-46` | int32 | 4 | Lowest OI for the day (NSE_FNO only) |
| `47-50` | float32 | 4 | Day Open |
| `51-54` | float32 | 4 | Day Close (post-close only) |
| `55-58` | float32 | 4 | Day High |
| `59-62` | float32 | 4 | Day Low |
| `63-162` | Market-depth block | 100 | 5 packets × 20 bytes |

**5-level depth sub-packet (20 bytes each):**
| Bytes | Type | Size | Description |
|---|---|---|---|
| `1-4` | int32 | 4 | Bid Quantity |
| `5-8` | int32 | 4 | Ask Quantity |
| `9-10` | int16 | 2 | No. of Bid Orders |
| `11-12` | int16 | 2 | No. of Ask Orders |
| `13-16` | float32 | 4 | Bid Price |
| `17-20` | float32 | 4 | Ask Price |

## Feed disconnect
Client request to disconnect:
```json
{ "RequestCode": 12 }
```
On a server-side disconnect you get a disconnect packet (response code `50`):
| Bytes | Type | Size | Description |
|---|---|---|---|
| `0-8` | header (code 50) | 8 | |
| `9-10` | int16 | 2 | Disconnection reason code (see Data API errors in `08_annexure.md`) |

- If more than 5 WebSockets are established, the **oldest** socket is disconnected with code `805` for each additional connection.
