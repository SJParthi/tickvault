# Portfolio and Positions — DhanHQ v2

> Source: https://dhanhq.co/docs/v2/portfolio/ · API v2 · Audited 2 Jun 2026

Retrieve holdings and positions, convert positions, and exit all.

| Method | Path | Use |
|---|---|---|
| GET | `/holdings` | Demat holdings |
| GET | `/positions` | Open positions |
| POST | `/positions/convert` | Convert intraday ↔ delivery |
| DELETE | `/positions` | Exit all positions |

## Holdings — `GET /holdings`
T1 + delivered quantities from previous sessions. No body.
```bash
curl --request GET --url https://api.dhan.co/v2/holdings \
  --header 'Content-Type: application/json' --header 'access-token: JWT'
```
**Response** (array):
```json
[
  {
    "exchange": "ALL", "tradingSymbol": "HDFC", "securityId": "1330",
    "isin": "INE001A01036", "totalQty": 1000, "dpQty": 1000, "t1Qty": 0,
    "availableQty": 1000, "collateralQty": 0, "avgCostPrice": 2655.0
  }
]
```
| Field | Type | Description |
|---|---|---|
| `exchange` | enum string | Exchange |
| `tradingSymbol` | string | Trading symbol |
| `securityId` | string | From instrument master |
| `isin` | string | Universal scrip ID |
| `totalQty` | int | Total quantity |
| `dpQty` | int | Delivered in demat |
| `t1Qty` | int | Pending delivery in demat |
| `availableQty` | int | Available for transaction |
| `collateralQty` | int | Placed as collateral |
| `avgCostPrice` | float | Average buy price |

## Positions — `GET /positions`
All open positions for the day, incl. F&O carry-forward. No body.
```bash
curl --request GET --url https://api.dhan.co/v2/positions \
  --header 'Content-Type: application/json' --header 'access-token: JWT'
```
**Response** (array):
```json
[
  {
    "dhanClientId": "1000000009", "tradingSymbol": "TCS", "securityId": "11536",
    "positionType": "LONG", "exchangeSegment": "NSE_EQ", "productType": "CNC",
    "buyAvg": 3345.8, "buyQty": 40, "costPrice": 3215.0, "sellAvg": 0.0, "sellQty": 0,
    "netQty": 40, "realizedProfit": 0.0, "unrealizedProfit": 6122.0,
    "rbiReferenceRate": 1.0, "multiplier": 1,
    "carryForwardBuyQty": 0, "carryForwardSellQty": 0,
    "carryForwardBuyValue": 0.0, "carryForwardSellValue": 0.0,
    "dayBuyQty": 40, "daySellQty": 0, "dayBuyValue": 133832.0, "daySellValue": 0.0,
    "drvExpiryDate": "0001-01-01", "drvOptionType": null, "drvStrikePrice": 0.0,
    "crossCurrency": false
  }
]
```
| Field | Type | Description |
|---|---|---|
| `dhanClientId` | string | User ID |
| `tradingSymbol` | string | Trading symbol |
| `securityId` | string | From instrument master |
| `positionType` | enum string | `LONG` `SHORT` `CLOSED` |
| `exchangeSegment` | enum string | `NSE_EQ` `NSE_FNO` `NSE_CURRENCY` `BSE_EQ` `BSE_FNO` `BSE_CURRENCY` `MCX_COMM` |
| `productType` | enum string | `CNC` `INTRADAY` `MARGIN` `MTF` `CO` `BO` |
| `buyAvg` | float | Avg buy price (MTM) |
| `buyQty` | int | Total bought |
| `costPrice` | int | Actual cost price |
| `sellAvg` | float | Avg sell price (MTM) |
| `sellQty` | int | Total sold |
| `netQty` | int | buyQty − sellQty |
| `realizedProfit` | float | Booked P&L |
| `unrealizedProfit` | float | Open-position P&L |
| `rbiReferenceRate` | float | RBI reference rate (forex) |
| `multiplier` | int | Currency F&O multiplier |
| `carryForwardBuyQty` | int | CF F&O long qty |
| `carryForwardSellQty` | int | CF F&O short qty |
| `carryForwardBuyValue` | float | CF F&O long value |
| `carryForwardSellValue` | float | CF F&O short value |
| `dayBuyQty` | int | Bought today |
| `daySellQty` | int | Sold today |
| `dayBuyValue` | float | Value bought today |
| `daySellValue` | float | Value sold today |
| `drvExpiryDate` | string | F&O contract expiry |
| `drvOptionType` | enum string | `CALL` `PUT` |
| `drvStrikePrice` | float | Option strike |
| `crossCurrency` | boolean | Non-INR currency pair? |

## Convert Position — `POST /positions/convert`
Convert intraday ↔ delivery.
```bash
curl --request POST --url https://api.dhan.co/v2/positions/convert \
  --header 'Accept: application/json' --header 'Content-Type: application/json' \
  --header 'access-token: JWT' --data '{}'
```
**Request**
```json
{
  "dhanClientId": "1000000009",
  "fromProductType": "INTRADAY",
  "exchangeSegment": "NSE_EQ",
  "positionType": "LONG",
  "securityId": "11536",
  "tradingSymbol": "",
  "convertQty": "40",
  "toProductType": "CNC"
}
```
| Field | Type | Description |
|---|---|---|
| `dhanClientId` | string | User ID |
| `fromProductType` | enum string | `CNC` `INTRADAY` `MARGIN` `CO` `BO` |
| `exchangeSegment` | enum string | See `08_annexure.md` |
| `positionType` | enum string | `LONG` `SHORT` `CLOSED` |
| `securityId` | string | From instrument master |
| `tradingSymbol` | string | Trading symbol |
| `convertQty` | int | Shares to convert |
| `toProductType` | enum string | `CNC` `INTRADAY` `MARGIN` `CO` `BO` |

**Response:** `202 Accepted`.

## Exit All Positions — `DELETE /positions`
Exit all active positions and cancel all open orders for the day. No body.
```bash
curl --request DELETE --url https://api.dhan.co/v2/positions \
  --header 'Accept: application/json' --header 'access-token: '
```
**Response**
```json
{ "status": "SUCCESS", "message": "All orders and positions exited successfully" }
```
`status` enum: `SUCCESS` / `ERROR`.

For enum descriptions, see `08_annexure.md`.
