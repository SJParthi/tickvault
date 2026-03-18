# Dhan API V2 Complete Reference

> **Source:** DhanHQ Python SDK v2.2.0 (github.com/dhan-oss/DhanHQ-py) + dhanhq.co/docs/v2
> **Extracted:** 2026-03-08
> **Base URL:** `https://api.dhan.co/v2`
> **Auth Base URL:** `https://auth.dhan.co`

---

## 1. HTTP Configuration

### Base URL

```
https://api.dhan.co/v2
```

### Standard Headers (All REST Endpoints)

| Header | Value |
|--------|-------|
| `access-token` | `<JWT access token>` |
| `client-id` | `<Dhan client ID>` |
| `Content-Type` | `application/json` |
| `Accept` | `application/json` |

### Request Defaults

- Timeout: 60 seconds
- All payloads: JSON
- `dhanClientId` injected into every request payload automatically by SDK
- SSL: enabled by default (optional `disable_ssl` flag)
- HTTP methods: GET, POST, PUT, DELETE

### Response Format (Standardized)

```json
{
    "status": "success" | "failure",
    "remarks": "<error details or empty string>",
    "data": "<response payload or empty string>"
}
```

HTTP status 200-299 = success. All other statuses = failure.

---

## 2. Authentication

### Method 1: OAuth Consent Flow (Browser-Based)

**Step 1 — Generate Consent**

```
POST https://auth.dhan.co/app/generate-consent?client_id=<client_id>
Headers:
  app_id: <app_id>
  app_secret: <app_secret>

Response:
{
    "status": "success",
    "data": {
        "consentAppId": "<consent_app_id>"
    }
}
```

**Step 2 — Open Browser Login**

```
URL: https://auth.dhan.co/login/consentApp-login?consentAppId=<consentAppId>
```

User logs in via browser. After login, a `tokenId` is generated.

**Step 3 — Consume Token ID**

```
GET https://auth.dhan.co/app/consumeApp-consent?tokenId=<tokenId>
Headers:
  app_id: <app_id>
  app_secret: <app_secret>

Response: Contains access token and related details.
```

### Method 2: PIN/TOTP Token Generation (Headless)

```
POST https://auth.dhan.co/app/generateAccessToken?dhanClientId=<client_id>&pin=<pin>&totp=<totp>

Response:
{
    "status": "success",
    "data": {
        "accessToken": "<JWT access token>"
    }
}
```

**TOTP Parameters:**
- Algorithm: SHA-1
- Digits: 6
- Period: 30 seconds
- Secret: Base32-encoded

### Method 3: Token Renewal

```
GET https://api.dhan.co/v2/RenewToken
Headers:
  access-token: <current_access_token>
  dhanClientId: <client_id>
  (standard Content-Type and Accept headers)

Response:
{
    "status": "success",
    "data": {
        "access_token": "<new JWT>",
        "token_type": "Bearer",
        "expires_in": 86400
    }
}
```

**Token Properties:**
- Validity: 24 hours (86400 seconds)
- Refresh window: 23 hours after issuance (1 hour before expiry)

### User Profile Validation

```
GET https://api.dhan.co/v2/profile
Headers:
  access-token: <access_token>
  dhanClientId: <client_id>

Purpose: Verify token validity and account setup.
```

### IP Whitelisting

```
POST /ip/setIP
Body: { "ip": "<ip_address>", "ipFlag": "PRIMARY" | "SECONDARY" }

PUT /ip/modifyIP
Body: { "ip": "<ip_address>", "ipFlag": "PRIMARY" | "SECONDARY" }

GET /ip/getIP
Response: List of configured IPs with modification dates.
```

---

## 3. Orders

### Place Order

```
POST /orders
Headers: Standard (access-token, client-id, Content-Type, Accept)

Request Body:
{
    "dhanClientId": "<client_id>",
    "transactionType": "BUY" | "SELL",
    "exchangeSegment": "NSE_EQ" | "NSE_FNO" | "BSE_EQ" | "BSE_FNO" | "MCX_COMM" | "NSE_CURRENCY",
    "productType": "CNC" | "INTRADAY" | "MARGIN" | "MTF" | "CO" | "BO",
    "orderType": "LIMIT" | "MARKET" | "STOP_LOSS" | "STOP_LOSS_MARKET",
    "validity": "DAY" | "IOC",
    "securityId": "<security_id_string>",
    "quantity": <int>,
    "price": <float>,
    "triggerPrice": <float>,            // default: 0
    "disclosedQuantity": <int>,         // default: 0
    "afterMarketOrder": <bool>,         // default: false
    "amoTime": "OPEN" | "OPEN_30" | "OPEN_60",  // AMO timing
    "boProfitValue": <float>,           // bracket order profit target
    "boStopLossValue": <float>,         // bracket order stop-loss
    "correlationId": "<uuid>",          // idempotency key / tag
    "shouldSlice": <bool>               // enable order slicing
}

Response:
{
    "orderId": "1234567890",
    "orderStatus": "TRANSIT",
    "correlationId": "<echoed-back>"
}
```

### Place Slice Order

```
POST /orders/slicing
Headers: Standard
Body: Same as Place Order
```

### Modify Order

```
PUT /orders/{orderId}
Headers: Standard

Request Body:
{
    "dhanClientId": "<client_id>",
    "orderId": "<order_id>",
    "orderType": "LIMIT" | "MARKET" | "STOP_LOSS" | "STOP_LOSS_MARKET",
    "legName": "<leg_name>",
    "quantity": <int>,
    "price": <float>,
    "triggerPrice": <float>,
    "disclosedQuantity": <int>,
    "validity": "DAY" | "IOC"
}
```

### Cancel Order

```
DELETE /orders/{orderId}
Headers: Standard
No body required.
```

### Get Order by ID

```
GET /orders/{orderId}
Headers: Standard

Response: Full order object (see Order Response Fields below).
```

### Get Order by Correlation ID

```
GET /orders/external/{correlationId}
Headers: Standard
```

### Get All Orders (Today)

```
GET /orders
Headers: Standard

Response: Array of order objects.
```

### Order Response Fields

```json
{
    "orderId": "1234567890",
    "correlationId": "uuid-from-request",
    "orderStatus": "TRANSIT" | "PENDING" | "CONFIRMED" | "TRADED" | "CANCELLED" | "REJECTED" | "EXPIRED",
    "transactionType": "BUY" | "SELL",
    "exchangeSegment": "NSE_FNO",
    "productType": "INTRADAY",
    "orderType": "LIMIT",
    "validity": "DAY",
    "securityId": "52432",
    "quantity": 50,
    "price": 245.50,
    "triggerPrice": 0.0,
    "tradedQuantity": 50,
    "tradedPrice": 245.50,
    "remainingQuantity": 0,
    "filledQty": 50,
    "averageTradePrice": 245.50,
    "exchangeOrderId": "NSE-1234567",
    "exchangeTime": "2026-03-15T10:30:45",
    "createTime": "2026-03-15T10:30:44",
    "updateTime": "2026-03-15T10:30:45",
    "rejectionReason": "",
    "tag": ""
}
```

---

## 4. Forever Orders (GTT)

### Place Forever Order

```
POST /forever/orders
Headers: Standard

Request Body:
{
    "orderFlag": "SINGLE" | "OCO",
    "transactionType": "BUY" | "SELL",
    "exchangeSegment": "<segment>",
    "productType": "<product_type>",
    "orderType": "LIMIT" | "MARKET",
    "validity": "DAY" | "IOC",
    "tradingSymbol": "<symbol>",
    "securityId": "<security_id>",
    "quantity": <int>,
    "disclosedQuantity": <int>,
    "price": <float>,
    "triggerPrice": <float>,
    "price1": <float>,              // OCO second leg price
    "triggerPrice1": <float>,       // OCO second leg trigger
    "quantity1": <int>,             // OCO second leg quantity
    "correlationId": "<tag>"
}
```

### Modify Forever Order

```
PUT /forever/orders/{orderId}
Headers: Standard
Body: Same fields as place order with legName specification.
```

### Get All Forever Orders

```
GET /forever/orders
Headers: Standard
```

### Cancel Forever Order

```
DELETE /forever/orders/{orderId}
Headers: Standard
```

---

## 5. Super Orders

### Place Super Order

```
POST /super/orders
Headers: Standard

Request Body:
{
    "transactionType": "BUY" | "SELL",
    "exchangeSegment": "<segment>",
    "productType": "<product_type>",
    "orderType": "LIMIT" | "MARKET",
    "securityId": "<security_id>",
    "quantity": <int>,
    "price": <float>,
    "targetPrice": <float>,         // required: at least one of target/stopLoss
    "stopLossPrice": <float>,       // required: at least one of target/stopLoss
    "trailingJump": <float>,        // trailing stop-loss
    "correlationId": "<tag>"
}

Validation:
- At least one of targetPrice or stopLossPrice required
- BUY: targetPrice > price, stopLossPrice < price
- SELL: targetPrice < price, stopLossPrice > price
```

### Modify Super Order

```
PUT /super/orders/{orderId}
Headers: Standard

Request Body:
{
    "orderId": "<order_id>",
    "legName": "ENTRY_LEG" | "TARGET_LEG" | "STOP_LOSS_LEG",
    "orderType": "<type>",          // ENTRY_LEG only
    "quantity": <int>,              // ENTRY_LEG only
    "price": <float>,              // ENTRY_LEG only
    "targetPrice": <float>,        // ENTRY_LEG or TARGET_LEG
    "stopLossPrice": <float>,      // ENTRY_LEG or STOP_LOSS_LEG
    "trailingJump": <float>        // ENTRY_LEG or STOP_LOSS_LEG
}
```

### Get All Super Orders

```
GET /super/orders
Headers: Standard
```

### Cancel Super Order

```
DELETE /super/orders/{orderId}/{orderLeg}
Headers: Standard

orderLeg: "ENTRY_LEG" | "TARGET_LEG" | "STOP_LOSS_LEG"
```

---

## 6. Portfolio

### Get Holdings

```
GET /holdings
Headers: Standard

Response: Array of holding objects with ISIN, quantity, avg price, etc.
```

### Get Positions

```
GET /positions
Headers: Standard

Response: Array of position objects for current day.
```

### Convert Position

```
POST /positions/convert
Headers: Standard

Request Body:
{
    "fromProductType": "CNC" | "INTRADAY" | "MARGIN" | ...,
    "exchangeSegment": "<segment>",
    "positionType": "LONG" | "SHORT",
    "securityId": "<security_id>",
    "convertQty": <int>,
    "toProductType": "CNC" | "INTRADAY" | "MARGIN" | ...
}
```

---

## 7. Market Quote (REST API)

### LTP (Last Traded Price)

```
POST /marketfeed/ltp
Headers: Standard

Request Body:
{
    "NSE_EQ": [11536, 2885],
    "NSE_FNO": [49081, 49082],
    "IDX_I": [13]
}

Keys: exchange segment strings
Values: arrays of security ID integers

Response: LTP data per instrument.
```

### OHLC

```
POST /marketfeed/ohlc
Headers: Standard

Request Body: Same format as LTP.

Response: Open, High, Low, Close + LTP per instrument.
```

### Full Quote

```
POST /marketfeed/quote
Headers: Standard

Request Body: Same format as LTP.

Response: Full packet including market depth, last trade, circuit limits,
          OHLC, OI, and volume data per instrument.
```

---

## 8. Historical Data

### Intraday Minute Candles

```
POST /charts/intraday
Headers: Standard

Request Body:
{
    "securityId": "13",
    "exchangeSegment": "IDX_I",
    "instrument": "INDEX",
    "interval": "1",                // 1, 5, 15, 25, or 60
    "fromDate": "2026-03-10",      // YYYY-MM-DD
    "toDate": "2026-03-15",        // YYYY-MM-DD
    "oi": false                     // include open interest (optional)
}

Constraints:
- Maximum 5 trading days of data per request
- Interval values: 1, 5, 15, 25, 60 (minutes)

Response: Array of OHLCV candles with timestamps.
```

### Historical Daily Candles

```
POST /charts/historical
Headers: Standard

Request Body:
{
    "securityId": "13",
    "exchangeSegment": "IDX_I",
    "instrument": "INDEX",
    "fromDate": "2025-01-01",
    "toDate": "2026-03-15",
    "expiryCode": 0,               // 0, 1, 2, or 3
    "oi": false                     // include open interest (optional)
}

expiryCode values:
  0 = Near month (current expiry)
  1 = Next month
  2 = Far month
  3 = Far-far month

Response: Array of daily OHLCV candles.
```

### Expired Options (Rolling)

```
POST /charts/rollingoption
Headers: Standard

Request Body:
{
    "securityId": "<security_id>",
    "exchangeSegment": "<segment>",
    "instrument": "OPTIDX" | "OPTSTK",
    "fromDate": "2026-01-01",
    "toDate": "2026-03-15",
    "expiryFlag": "WEEK" | "MONTH",
    "expiryCode": 0,               // 0-3
    "strike": "ATM" | "ATM+1" | "ATM-1" | ...,
    "drvOptionType": "CALL" | "PUT",
    "requiredData": ["open", "high", "low", "close", "iv", "volume", "strike", "oi", "spot"],
    "interval": 1                   // 1, 5, 15, 25, or 60
}
```

---

## 9. Option Chain

### Get Option Chain

```
POST /optionchain
Headers: Standard

Request Body:
{
    "UnderlyingScrip": 13,          // security ID (integer)
    "UnderlyingSeg": "IDX_I",         // exchange segment
    "Expiry": "2026-03-27"          // expiry date string
}

Response: OI, Greeks, Volume, LTP, Best Bid/Ask, IV across all strikes.
```

### Get Expiry List

```
POST /optionchain/expirylist
Headers: Standard

Request Body:
{
    "UnderlyingScrip": 13,
    "UnderlyingSeg": "IDX_I"
}

Response: List of expiry dates for the underlying instrument.
```

---

## 10. Funds

### Get Fund Limits

```
GET /fundlimit
Headers: Standard

Response: Balance, margin utilized, collateral, and all account info.
```

### Margin Calculator

```
POST /margincalculator
Headers: Standard

Request Body:
{
    "securityId": "<security_id>",
    "exchangeSegment": "NSE_EQ",
    "transactionType": "BUY" | "SELL",
    "quantity": <int>,
    "productType": "CNC" | "INTRADAY" | ...,
    "price": <float>,
    "triggerPrice": <float>         // optional, default: 0
}

Response: Margin required for the specified trade.
```

---

## 11. Statements

### Get Trades (Today)

```
GET /trades
GET /trades/{orderId}              // trades for specific order
Headers: Standard

Response: Array of trade objects.
```

### Trade History (Date Range)

```
GET /trades/{fromDate}/{toDate}/{pageNumber}
Headers: Standard

Parameters:
  fromDate: YYYY-MM-DD string
  toDate: YYYY-MM-DD string
  pageNumber: integer (default: 0)

Response: Paginated trade history.
```

### Ledger Report

```
GET /ledger?from-date={fromDate}&to-date={toDate}
Headers: Standard

Parameters:
  from-date: YYYY-MM-DD string
  to-date: YYYY-MM-DD string

Response: Ledger details for date range.
```

---

## 12. eDIS (Electronic Delivery Instruction Slip)

### Generate T-PIN

```
GET /edis/tpin
Headers: Standard

Response: T-PIN sent to registered mobile.
```

### Submit eDIS Form

```
POST /edis/form
Headers: Standard

Request Body:
{
    "isin": "<ISIN>",
    "qty": <int>,
    "exchange": "<exchange>",
    "segment": "EQ",
    "bulk": false
}

Response: { "edisFormHtml": "<HTML form content>" }
```

### Inquire eDIS Status

```
GET /edis/inquire/{isin}
Headers: Standard

Response: eDIS status for the given ISIN.
```

---

## 13. Trader Controls

### Set Kill Switch

```
POST /killswitch?killSwitchStatus=ACTIVATE|DEACTIVATE
Headers: Standard
Body: {}

Response:
{
    "status": "success" | "failure",
    "remarks": "<message>",
    "data": "<response_data>"
}
```

### Get Kill Switch Status

```
GET /killswitch
Headers: Standard
```

---

## 14. Instruments List

### CSV Download (No Authentication Required)

**Primary (Detailed):**
```
GET https://images.dhan.co/api-data/api-scrip-master-detailed.csv
```
- ~40 MB, ~276,000 rows
- Updated daily by Dhan
- No auth headers needed

**Fallback (Compact):**
```
GET https://images.dhan.co/api-data/api-scrip-master.csv
```
- Smaller file, fewer columns, same security IDs

### CSV Columns (Detailed)

```
EXCH_ID, SEGMENT, SECURITY_ID, INSTRUMENT, UNDERLYING_SECURITY_ID,
UNDERLYING_SYMBOL, SYMBOL_NAME, DISPLAY_NAME, SERIES, LOT_SIZE,
SM_EXPIRY_DATE, STRIKE_PRICE, OPTION_TYPE, TICK_SIZE, EXPIRY_FLAG
```

### Exchange/Segment Filtering

Relevant segments: (NSE, I), (NSE, E), (NSE, D), (BSE, I), (BSE, D)

---

## 15. Live Market Feed WebSocket

### Connection

```
URL: wss://api-feed.dhan.co?version=2&token=<access_token>&clientId=<client_id>&authType=2
Protocol: Binary frames, Little Endian
Max connections: 5 per account
Max instruments/connection: 5,000
Max instruments total: 25,000
Max instruments/message: 100 (batched)
Server ping interval: 10 seconds
Server timeout: 40 seconds without pong
Client pong: Automatic (tokio-tungstenite / websockets handles it)
```

### Exchange Segment Codes (Binary Wire Protocol)

| Code | Segment |
|------|---------|
| 0 | IDX_I (Indices) |
| 1 | NSE_EQ (NSE Equity) |
| 2 | NSE_FNO (NSE F&O) |
| 3 | NSE_CURRENCY |
| 4 | BSE_EQ |
| 5 | MCX_COMM |
| 7 | BSE_CURRENCY |
| 8 | BSE_FNO |

Note: Codes 6 is unused/skipped.

### Subscribe (JSON Text Frame)

```json
{
    "RequestCode": 15,
    "InstrumentCount": 3,
    "InstrumentList": [
        { "ExchangeSegment": "IDX_I", "SecurityId": "13" },
        { "ExchangeSegment": "NSE_EQ", "SecurityId": "2885" },
        { "ExchangeSegment": "NSE_FNO", "SecurityId": "52432" }
    ]
}
```

**Subscribe Request Codes:**

| Code | Mode | Description |
|------|------|-------------|
| 15 | Ticker | LTP + LTT (16 bytes) |
| 17 | Quote | Extended price data (50 bytes) |
| 21 | Full | Quote + OI + 5-level depth (162 bytes) |

**Unsubscribe Request Codes (subscribe_code + 1):**

| Code | Mode |
|------|------|
| 16 | Unsubscribe Ticker |
| 18 | Unsubscribe Quote |
| 22 | Unsubscribe Full |

**Disconnect Request Code:** 12

**Rules:**
- SecurityId is always a STRING in JSON (not numeric)
- Max 100 instruments per message
- For >100 instruments, send multiple batched messages

### Binary Response Header (8 bytes)

```
Offset 0:     Response Code (u8)
Offset 1-2:   Message Length (u16 LE)
Offset 3:     Exchange Segment Code (u8)
Offset 4-7:   Security ID (i32 LE)
```

### Response Code 1 — Index Ticker (16 bytes)

Same structure as Response Code 2.

### Response Code 2 — Ticker (16 bytes)

Format: `<BHBIfI`

| Offset | Size | Field | Type |
|--------|------|-------|------|
| 0 | 1 | Response code | u8 |
| 1-2 | 2 | Message length | u16 LE |
| 3 | 1 | Exchange segment | u8 |
| 4-7 | 4 | Security ID | i32 LE |
| 8-11 | 4 | LTP | f32 LE |
| 12-15 | 4 | Last Trade Time | u32 LE (epoch) |

**Price note:** Prices are float32 in rupees. No division by 100 needed (SDK-verified).

### Response Code 4 — Quote (50 bytes)

Format: `<BHBIfHIfIIIffff`

| Offset | Size | Field | Type |
|--------|------|-------|------|
| 0-7 | 8 | Header | (as above) |
| 8-11 | 4 | LTP | f32 LE |
| 12-13 | 2 | LTQ (Last Traded Qty) | u16 LE |
| 14-17 | 4 | LTT (Last Trade Time) | u32 LE |
| 18-21 | 4 | Avg Trade Price | f32 LE |
| 22-25 | 4 | Volume | u32 LE |
| 26-29 | 4 | Total Buy Qty | u32 LE |
| 30-33 | 4 | Total Sell Qty | u32 LE |
| 34-37 | 4 | Open | f32 LE |
| 38-41 | 4 | High | f32 LE |
| 42-45 | 4 | Low | f32 LE |
| 46-49 | 4 | Close (prev) | f32 LE |

### Response Code 5 — OI Data (12 bytes)

| Offset | Size | Field | Type |
|--------|------|-------|------|
| 0-7 | 8 | Header | (as above) |
| 8-11 | 4 | Open Interest | u32 LE |

### Response Code 6 — Previous Close (16 bytes)

| Offset | Size | Field | Type |
|--------|------|-------|------|
| 0-7 | 8 | Header | (as above) |
| 8-11 | 4 | Previous Close Price | f32 LE |
| 12-15 | 4 | Previous OI | u32 LE |

Sent once per instrument on subscription or market open.

### Response Code 7 — Market Status (8 bytes)

Header only. Market status encoded in the segment byte or a separate field.

| Status Code | Meaning |
|-------------|---------|
| 0 | Market Closed |
| 1 | Pre-Open |
| 2 | Market Open |
| 3 | Post-Close |

### Response Code 8 — Full Packet (162 bytes)

Format: `<BHBIfHIfIIIIIIffff100s`

| Offset | Size | Field | Type |
|--------|------|-------|------|
| 0-7 | 8 | Header | (as above) |
| 8-49 | 42 | Quote fields | (same as code 4) |
| 50-53 | 4 | OI | u32 LE |
| 54-57 | 4 | OI Day High | u32 LE |
| 58-61 | 4 | OI Day Low | u32 LE |
| 62-161 | 100 | Market Depth (5 levels) | (see below) |

**5-Level Market Depth (100 bytes, 20 bytes per level):**

Each level format: `<IIHHff`

| Offset | Size | Field | Type |
|--------|------|-------|------|
| 0-3 | 4 | Bid Quantity | i32 LE |
| 4-7 | 4 | Ask Quantity | i32 LE |
| 8-9 | 2 | Bid Orders | i16 LE |
| 10-11 | 2 | Ask Orders | i16 LE |
| 12-15 | 4 | Bid Price | f32 LE |
| 16-19 | 4 | Ask Price | f32 LE |

### Response Code 50 — Disconnect (10 bytes)

| Offset | Size | Field | Type |
|--------|------|-------|------|
| 0-7 | 8 | Header | (as above) |
| 8-9 | 2 | Disconnect Code | u16 LE |

### Disconnect Codes (Annexure Section 11 — 12 codes)

| Code | Meaning | Reconnectable? | Token Refresh? |
|------|---------|----------------|----------------|
| 800 | Internal server error | Yes | No |
| 804 | Instruments exceed limit | No | No |
| 805 | Exceeded active WebSocket connections | No | No |
| 806 | Data API subscription required | No | No |
| 807 | Access token expired | Yes | Yes |
| 808 | Authentication failed | No | No |
| 809 | Access token invalid | No | No |
| 810 | Client ID invalid | No | No |
| 811 | Invalid expiry date | No | No |
| 812 | Invalid date format | No | No |
| 813 | Invalid security ID | No | No |
| 814 | Invalid request | No | No |

---

## 16. 20-Level Depth WebSocket

### Connection

```
URL: wss://depth-api-feed.dhan.co/twentydepth?token=<access_token>&clientId=<client_id>&authType=2
Max instruments per connection: 50
Segments supported: NSE_EQ (1), NSE_FNO (2) ONLY
```

### Subscription

```json
{
    "RequestCode": 23,
    "InstrumentCount": 1,
    "InstrumentList": [
        { "ExchangeSegment": "NSE_EQ", "SecurityId": "2885" }
    ]
}
```

**Unsubscribe:** RequestCode = 25 (Dhan Annexure: UnsubscribeFullDepth = 25, NOT 24)
**Disconnect:** RequestCode = 12

### Binary Packet — 12-Byte Header

| Offset | Size | Field | Type |
|--------|------|-------|------|
| 0-1 | 2 | Message Length | u16 LE |
| 2 | 1 | Feed Response Code | u8 (41=Bid, 51=Ask, 50=Disconnect) |
| 3 | 1 | Exchange Segment | u8 |
| 4-7 | 4 | Security ID | i32 LE |
| 8-11 | 4 | Number of Rows | u32 LE |

### Depth Body (per level = 16 bytes)

Format: `<dII`

| Offset | Size | Field | Type |
|--------|------|-------|------|
| 0-7 | 8 | Price | f64 LE (double, rupees) |
| 8-11 | 4 | Quantity | u32 LE |
| 12-15 | 4 | Orders | u32 LE |

**20 levels x 16 bytes = 320 bytes body**
**Total per-side packet: 12 (header) + 320 (body) = 332 bytes**

Bid and Ask arrive as SEPARATE packets (feed codes 41 and 51).

### Disconnect Codes

Same as standard market feed (805-809).

---

## 17. 200-Level Depth WebSocket

### Connection

```
URL: wss://full-depth-api.dhan.co/?token=<access_token>&clientId=<client_id>&authType=2
Max instruments per connection: 1 (CRITICAL LIMITATION)
Segments supported: NSE_EQ and NSE_FNO ONLY
Subscribe: RequestCode = 23
Unsubscribe: RequestCode = 25
```

### Packet Format

Identical to 20-level depth, but with 200 levels per side.

**200 levels x 16 bytes = 3,200 bytes body**
**Total per-side packet: 12 (header) + 3,200 (body) = 3,212 bytes**

---

## 18. Order Update WebSocket

### Connection

```
URL: wss://api-order-update.dhan.co
Protocol: JSON (NOT binary)
```

### Authentication (Send After Connect)

```json
{
    "LoginReq": {
        "MsgCode": 42,
        "ClientId": "<client_id>",
        "Token": "<access_token>"
    },
    "UserType": "SELF"
}
```

No subscription needed. All order updates are pushed automatically after login.

### Order Update Message Format

```json
{
    "Data": {
        "Exchange": "NSE",
        "Segment": "D",
        "SecurityId": "52432",
        "ClientId": "1000000001",
        "OrderNo": "1234567890",
        "ExchOrderNo": "NSE-1234567",
        "Product": "I",
        "TxnType": "B",
        "OrderType": "LMT",
        "Validity": "DAY",
        "Quantity": 50,
        "TradedQty": 50,
        "RemainingQuantity": 0,
        "Price": 245.50,
        "TriggerPrice": 0.0,
        "TradedPrice": 245.50,
        "AvgTradedPrice": 245.50,
        "Status": "TRADED",
        "Symbol": "NIFTY",
        "DisplayName": "NIFTY 27MAR 24500 CE",
        "CorrelationId": "uuid-123",
        "Remarks": "",
        "ReasonDescription": "CONFIRMED",
        "OrderDateTime": "2026-03-15 10:30:45",
        "ExchOrderTime": "2026-03-15 10:30:45",
        "LastUpdatedTime": "2026-03-15 10:30:46",
        "Instrument": "OPTIDX",
        "LotSize": 50,
        "StrikePrice": 24500.0,
        "ExpiryDate": "2026-03-27",
        "OptType": "CE",
        "Isin": "",
        "DiscQuantity": 0,
        "DiscQtyRem": 0,
        "LegNo": 0,
        "ProductName": "INTRADAY",
        "refLtp": 245.0,
        "tickSize": 0.05
    }
}
```

**Message type field:** `"order_alert"` identifies order messages.

**Field name convention:** PascalCase in WebSocket (NOT camelCase like REST).

---

## 19. Annexure — All Enums, Codes & Constants

### Exchange Segment Enum

| String (REST/JSON) | Binary Code (Wire) | Description |
|--------------------|--------------------|-------------|
| IDX_I | 0 | Indices |
| NSE_EQ | 1 | NSE Equity |
| NSE_FNO | 2 | NSE Futures & Options |
| NSE_CURRENCY | 3 | NSE Currency Derivatives |
| BSE_EQ | 4 | BSE Equity |
| MCX_COMM | 5 | MCX Commodities |
| BSE_CURRENCY | 7 | BSE Currency |
| BSE_FNO | 8 | BSE Futures & Options |

### Transaction Type Enum

| Value | Meaning |
|-------|---------|
| BUY | Buy order |
| SELL | Sell order |

### Product Type Enum

| Value | Meaning | Description |
|-------|---------|-------------|
| CNC | Cash & Carry | Delivery-based equity holding |
| INTRADAY | Intraday | Squared off same day |
| MARGIN | Margin | Leveraged delivery |
| MTF | Margin Trading Facility | Broker-funded leverage |
| CO | Cover Order | With stop-loss |
| BO | Bracket Order | With target + stop-loss |

Note: SDK uses `INTRA` internally but REST API uses `INTRADAY`.

### Order Type Enum

| Value | Meaning |
|-------|---------|
| LIMIT | Limit order |
| MARKET | Market order |
| STOP_LOSS | SL order (trigger then limit) |
| STOP_LOSS_MARKET | SL-M order (trigger then market) |

Note: SDK uses `SL` and `SLM` as shorthand.

### Validity Enum

| Value | Meaning |
|-------|---------|
| DAY | Valid for the trading day |
| IOC | Immediate or Cancel |

### Order Status Enum

| Value | Meaning |
|-------|---------|
| TRANSIT | Order in transit to exchange |
| PENDING | Pending at exchange |
| CONFIRMED | Order confirmed/open |
| TRADED | Fully executed |
| CANCELLED | Cancelled by user or system |
| REJECTED | Rejected by exchange or RMS |
| EXPIRED | Expired at end of day |

### Order Update WebSocket Shorthand Codes

| Field | WebSocket Code | REST Equivalent |
|-------|---------------|-----------------|
| TxnType | B | BUY |
| TxnType | S | SELL |
| Product | I | INTRADAY |
| Product | C | CNC |
| Product | M | MARGIN |
| OrderType | LMT | LIMIT |
| OrderType | MKT | MARKET |
| OrderType | SL | STOP_LOSS |
| OrderType | SLM | STOP_LOSS_MARKET |
| Segment | D | Derivatives (FNO) |
| Segment | E | Equity |
| Segment | I | Index |

### Instrument Types (CSV)

| Value | Description |
|-------|-------------|
| INDEX | Index instrument |
| EQUITY | Equity stock |
| FUTIDX | Index future |
| FUTSTK | Stock future |
| OPTIDX | Index option |
| OPTSTK | Stock option |
| FUTCUR | Currency future |
| OPTCUR | Currency option |
| FUTCOM | Commodity future |
| OPTCOM | Commodity option |

### Feed Request Codes

| Code | Type |
|------|------|
| 12 | Disconnect |
| 15 | Subscribe Ticker |
| 16 | Unsubscribe Ticker |
| 17 | Subscribe Quote |
| 18 | Unsubscribe Quote |
| 21 | Subscribe Full |
| 22 | Unsubscribe Full |
| 23 | Subscribe 20/200 Depth |
| 24 | Unsubscribe 20/200 Depth |
| 42 | Order Update Login |

### Binary Response Codes

| Code | Packet Type | Size (bytes) |
|------|-------------|--------------|
| 1 | Index Ticker | 16 |
| 2 | Ticker | 16 |
| 4 | Quote | 50 |
| 5 | OI Data | 12 |
| 6 | Previous Close | 16 |
| 7 | Market Status | 8 |
| 8 | Full Packet | 162 |
| 41 | Deep Depth Bid | 332 (20-lvl) / 3212 (200-lvl) |
| 50 | Disconnect | 10 |
| 51 | Deep Depth Ask | 332 (20-lvl) / 3212 (200-lvl) |

### Disconnect Codes (Annexure Section 11)

| Code | Meaning | Action |
|------|---------|--------|
| 800 | Internal server error | Reconnect with backoff |
| 804 | Instruments exceed limit | Reduce instrument count |
| 805 | Exceeded active connections (5 max) | Reduce connections |
| 806 | Data API subscription required | User needs active subscription |
| 807 | Access token expired | Refresh token, reconnect |
| 808 | Authentication failed | Check token/credentials |
| 809 | Access token invalid | Re-authenticate |
| 810 | Client ID invalid | Check SSM credentials |
| 811 | Invalid expiry date | Fix request |
| 812 | Invalid date format | Fix request |
| 813 | Invalid security ID | Fix request |
| 814 | Invalid request | Fix request |

### Market Status Codes

| Code | Status |
|------|--------|
| 0 | Market Closed |
| 1 | Pre-Open |
| 2 | Market Open |
| 3 | Post-Close |

### REST API Error Codes

| HTTP Status | Error Code | Meaning |
|-------------|-----------|---------|
| 400 | DH-901 | Invalid request parameters |
| 400 | DH-902 | Missing required fields |
| 400 | DH-903 | Invalid order type |
| 400 | DH-904 | Invalid product type |
| 400 | DH-905 | Invalid exchange segment |
| 401 | DH-100 | Authentication failed |
| 401 | DH-101 | Token expired |
| 401 | DH-102 | Invalid access token |
| 403 | DH-200 | Insufficient permissions |
| 429 | DH-300 | Rate limit exceeded (10 orders/sec SEBI) |
| 500 | DH-500 | Internal server error |
| 502 | DH-501 | Exchange connectivity issue |
| 503 | DH-502 | Service temporarily unavailable |

### AMO (After Market Order) Timing

| Value | Meaning |
|-------|---------|
| OPEN | Execute at market open |
| OPEN_30 | Execute 30 mins after open |
| OPEN_60 | Execute 60 mins after open |

### Forever Order Flags

| Value | Meaning |
|-------|---------|
| SINGLE | Single trigger order |
| OCO | One Cancels Other (two-leg) |

### Super Order Leg Names

| Value | Purpose |
|-------|---------|
| ENTRY_LEG | Entry order leg |
| TARGET_LEG | Profit target leg |
| STOP_LOSS_LEG | Stop-loss leg |

### Position Types

| Value | Meaning |
|-------|---------|
| LONG | Long position |
| SHORT | Short position |

### Kill Switch Actions

| Value | Meaning |
|-------|---------|
| ACTIVATE | Disable trading for current session |
| DEACTIVATE | Re-enable trading |

### Historical Data Intervals (Minutes)

| Value | Period |
|-------|--------|
| 1 | 1-minute candles |
| 5 | 5-minute candles |
| 15 | 15-minute candles |
| 25 | 25-minute candles |
| 60 | 1-hour candles |

### Historical Data Expiry Codes

| Code | Meaning |
|------|---------|
| 0 | Near month (current expiry) |
| 1 | Next month |
| 2 | Far month |
| 3 | Far-far month |

### Rolling Option Expiry Flags

| Value | Meaning |
|-------|---------|
| WEEK | Weekly expiry |
| MONTH | Monthly expiry |

### Rolling Option Strike Types

| Value | Meaning |
|-------|---------|
| ATM | At the money |
| ATM+1 | 1 strike above ATM |
| ATM-1 | 1 strike below ATM |
| ATM+N | N strikes above ATM |
| ATM-N | N strikes below ATM |

---

## 20. Rate Limits & Constraints

| Constraint | Value |
|------------|-------|
| Max orders per second | 10 (SEBI mandate) |
| HTTP timeout | 60 seconds |
| Max WebSocket connections | 5 per account |
| Max instruments per WS connection | 5,000 |
| Max total WS subscriptions | 25,000 |
| Max instruments per subscribe message | 100 |
| Max instruments per 20-depth connection | 50 |
| Max instruments per 200-depth connection | 1 |
| Token validity | 24 hours |
| Server ping interval | 10 seconds |
| Server ping timeout | 40 seconds |
| Intraday data range | Max 5 trading days per request |

---

## 21. Complete Endpoint Summary

| # | Method | Path | Purpose |
|---|--------|------|---------|
| 1 | POST | `/orders` | Place order |
| 2 | POST | `/orders/slicing` | Place slice order |
| 3 | PUT | `/orders/{orderId}` | Modify order |
| 4 | DELETE | `/orders/{orderId}` | Cancel order |
| 5 | GET | `/orders` | Get all orders today |
| 6 | GET | `/orders/{orderId}` | Get order by ID |
| 7 | GET | `/orders/external/{correlationId}` | Get order by correlation ID |
| 8 | POST | `/forever/orders` | Place forever order |
| 9 | PUT | `/forever/orders/{orderId}` | Modify forever order |
| 10 | GET | `/forever/orders` | Get all forever orders |
| 11 | DELETE | `/forever/orders/{orderId}` | Cancel forever order |
| 12 | POST | `/super/orders` | Place super order |
| 13 | PUT | `/super/orders/{orderId}` | Modify super order |
| 14 | GET | `/super/orders` | Get all super orders |
| 15 | DELETE | `/super/orders/{orderId}/{orderLeg}` | Cancel super order leg |
| 16 | GET | `/holdings` | Get holdings |
| 17 | GET | `/positions` | Get positions |
| 18 | POST | `/positions/convert` | Convert position |
| 19 | POST | `/marketfeed/ltp` | Get LTP |
| 20 | POST | `/marketfeed/ohlc` | Get OHLC |
| 21 | POST | `/marketfeed/quote` | Get full quote |
| 22 | POST | `/charts/intraday` | Get intraday candles |
| 23 | POST | `/charts/historical` | Get daily candles |
| 24 | POST | `/charts/rollingoption` | Get expired options data |
| 25 | POST | `/optionchain` | Get option chain |
| 26 | POST | `/optionchain/expirylist` | Get expiry list |
| 27 | GET | `/fundlimit` | Get fund limits |
| 28 | POST | `/margincalculator` | Calculate margin |
| 29 | GET | `/trades` | Get trades today |
| 30 | GET | `/trades/{orderId}` | Get trades for order |
| 31 | GET | `/trades/{from}/{to}/{page}` | Get trade history |
| 32 | GET | `/ledger?from-date=X&to-date=Y` | Get ledger |
| 33 | GET | `/edis/tpin` | Generate T-PIN |
| 34 | POST | `/edis/form` | Submit eDIS form |
| 35 | GET | `/edis/inquire/{isin}` | Check eDIS status |
| 36 | POST | `/killswitch?killSwitchStatus=X` | Set kill switch |
| 37 | GET | `/killswitch` | Get kill switch status |
| 38 | GET | `/profile` | Get user profile |
| 39 | GET | `/RenewToken` | Renew access token |
| 40 | POST | `/ip/setIP` | Set IP whitelist |
| 41 | PUT | `/ip/modifyIP` | Modify IP whitelist |
| 42 | GET | `/ip/getIP` | Get IP whitelist |

### Auth Endpoints (Different Base URL: https://auth.dhan.co)

| # | Method | Path | Purpose |
|---|--------|------|---------|
| 43 | POST | `/app/generate-consent` | Generate OAuth consent |
| 44 | GET | `/app/consumeApp-consent` | Consume token ID |
| 45 | POST | `/app/generateAccessToken` | Generate token (PIN/TOTP) |

### Instrument Download (No Auth: https://images.dhan.co)

| # | Method | Path | Purpose |
|---|--------|------|---------|
| 46 | GET | `/api-data/api-scrip-master-detailed.csv` | Detailed instrument CSV |
| 47 | GET | `/api-data/api-scrip-master.csv` | Compact instrument CSV |

### WebSocket Endpoints

| # | URL | Protocol | Purpose |
|---|-----|----------|---------|
| 48 | `wss://api-feed.dhan.co` | Binary | Live market feed |
| 49 | `wss://depth-api-feed.dhan.co/twentydepth` | Binary | 20-level depth |
| 50 | `wss://full-depth-api.dhan.co/` | Binary | 200-level depth |
| 51 | `wss://api-order-update.dhan.co` | JSON | Order updates |
