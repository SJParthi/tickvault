# Live Order Update — DhanHQ v2

> Source: https://dhanhq.co/docs/v2/order-update/ · API v2 · Audited 2 Jun 2026

Real-time updates for **all** your orders (any platform they were placed from), via WebSocket. **Messages are JSON** (not binary). You get status, traded price, quantity and full order details.

## Establishing connection
```
wss://api-order-update.dhan.co
```
On connect, send an authorisation message.

### For Individual
Receives updates for all orders in your account, regardless of placement platform.
```json
{
  "LoginReq": { "MsgCode": 42, "ClientId": "1000000001", "Token": "JWT" },
  "UserType": "SELF"
}
```
| Field | Type | Description |
|---|---|---|
| `LoginReq` (req) | object | Client ID + access token |
| `MsgCode` (req) | int | `42` by default |
| `ClientId` (req) | string | User ID |
| `Token` (req) | string | Access token |
| `UserType` (req) | string | `SELF` for individuals |

### For Partners
Receives updates for all users on the partner platform (needs the Partner Login module).
```json
{
  "LoginReq": { "MsgCode": 42, "ClientId": "partner_id" },
  "UserType": "PARTNER",
  "Secret": "partner_secret"
}
```
| Field | Type | Description |
|---|---|---|
| `LoginReq` (req) | object | Client ID (partner_id) |
| `MsgCode` (req) | int | `42` by default |
| `ClientId` (req) | string | `partner_id` |
| `UserType` (req) | string | `PARTNER` |
| `Secret` (req) | string | `partner_secret` |

## Order Update message
Wrapped as `{ "Data": { ... }, "Type": "order_alert" }`.
```json
{
  "Data": {
    "Exchange": "NSE", "Segment": "E", "Source": "N",
    "SecurityId": "14366", "ClientId": "1000000001",
    "ExchOrderNo": "1400000000404591", "OrderNo": "1124091136546",
    "Product": "C", "TxnType": "B", "OrderType": "LMT", "Validity": "DAY",
    "DiscQuantity": 1, "DiscQtyRem": 1, "RemainingQuantity": 1,
    "Quantity": 1, "TradedQty": 0, "Price": 13, "TriggerPrice": 0,
    "TradedPrice": 0, "AvgTradedPrice": 0,
    "OffMktFlag": "0",
    "OrderDateTime": "2024-09-11 14:39:29", "ExchOrderTime": "2024-09-11 14:39:29",
    "LastUpdatedTime": "2024-09-11 14:39:29",
    "Remarks": "Super Order", "MktType": "NL", "ReasonDescription": "CONFIRMED",
    "LegNo": 1, "Instrument": "EQUITY", "Symbol": "IDEA", "ProductName": "CNC",
    "Status": "Cancelled", "LotSize": 1, "ExpiryDate": "0001-01-01 00:00:00",
    "OptType": "XX", "DisplayName": "Vodafone Idea", "Isin": "INE669E01016",
    "Series": "EQ", "GoodTillDaysDate": "2024-09-11", "RefLtp": 13.21,
    "TickSize": 0.01, "AlgoId": "0", "Multiplier": 1, "CorrelationId": ""
  },
  "Type": "order_alert"
}
```

**Fields**
| Field | Type | Description |
|---|---|---|
| `Exchange` | string | Exchange the order is placed in |
| `Segment` | string | Order segment |
| `Source` | string | Platform; `P` for API orders |
| `SecurityId` | string | From instrument master |
| `ClientId` | string | User ID |
| `ExchOrderNo` | string | Exchange order id |
| `OrderNo` | string | Dhan order id |
| `Product` | enum string | `C` CNC, `I` INTRADAY, `M` MARGIN, `F` MTF, `V` CO, `B` BO |
| `TxnType` | enum string | `B` Buy / `S` Sell |
| `OrderType` | enum string | `LMT` Limit, `MKT` Market, `SL` Stop-Loss, `SLM` Stop-Loss Market |
| `Validity` | enum string | `DAY` / `IOC` |
| `DiscQuantity` | int | Disclosed (visible) qty |
| `DiscQtyRem` | int | Disclosed qty pending |
| `RemainingQuantity` | int | Qty pending for execution |
| `Quantity` | int | Total order qty |
| `TradedQty` | int | Executed qty |
| `Price` | float | Order price |
| `TriggerPrice` | float | For SL-M, SL-L, CO, BO |
| `TradedPrice` | float | Trade execution price |
| `AvgTradedPrice` | float | Avg (differs from TradedPrice on partial fills) |
| `AlgoOrdNo` | float | Entry-leg order no. tracking target/SL (BO/CO) |
| `OffMktFlag` | string | `1` if AMO else `0` |
| `OrderDateTime` | string | Received by Dhan |
| `ExchOrderTime` | string | Placed on exchange |
| `LastUpdatedTime` | string | Last modify/trade time |
| `Remarks` | string | Free text; `Super Order` if part of a super order |
| `MktType` | string | `NL` normal; `AU`/`A1`/`A2` auction |
| `ReasonDescription` | string | Order rejection reason |
| `LegNo` | int | `1` Entry, `2` Stop-Loss, `3` Target |
| `Instrument` | string | See `08_annexure.md` |
| `Symbol` | string | From instrument master |
| `ProductName` | string | See `08_annexure.md` |
| `Status` | enum string | `TRANSIT` `PENDING` `REJECTED` `CANCELLED` `TRADED` `EXPIRED` |
| `LotSize` | int | For derivatives |
| `StrikePrice` | float | Option strike |
| `ExpiryDate` | string | Contract expiry |
| `OptType` | string | `CE`/`PE` (`XX` if N/A) |
| `DisplayName` | string | Instrument display name |
| `Isin` | string | ISIN |
| `Series` | string | Exchange series |
| `GoodTillDaysDate` | string | Validity for Forever Orders |
| `RefLtp` | float | LTP at update time |
| `TickSize` | float | Instrument tick size |
| `AlgoId` | string | Exchange id for special order types |
| `Multiplier` | int | For commodity/currency contracts |
| `CorrelationId` | string | Your tracking id; max 30 chars; allowed `[^a-zA-Z0-9 _-]` |

> `CorrelationId` and the order-level `Remarks` were added in v2.2.
For enum descriptions, see `08_annexure.md`.
