# Funds & Margin — DhanHQ v2

> Source: https://dhanhq.co/docs/v2/funds/ · API v2 · Audited 2 Jun 2026

Fund availability and margin requirements for your trading account.

| Method | Path | Use |
|---|---|---|
| POST | `/margincalculator` | Margin requirement for one order |
| POST | `/margincalculator/multi` | Margin for multiple orders |
| GET | `/fundlimit` | Available fund limits |

## Margin Calculator — Single Order — `POST /margincalculator`
Get span/exposure/var margins, brokerage, leverage and available margin for an order before placing it.
```bash
curl --request POST --url https://api.dhan.co/v2/margincalculator \
  --header 'Accept: application/json' --header 'Content-Type: application/json' \
  --header 'access-token: ' --data '{Request JSON}'
```
**Request**
```json
{
  "dhanClientId": "1000000132",
  "exchangeSegment": "NSE_EQ",
  "transactionType": "BUY",
  "quantity": 5,
  "productType": "CNC",
  "securityId": "1333",
  "price": 1428,
  "triggerPrice": 1427
}
```
| Field | Type | Description |
|---|---|---|
| `dhanClientId` (req) | string | User ID |
| `exchangeSegment` (req) | enum string | `NSE_EQ` `NSE_FNO` `BSE_EQ` `BSE_FNO` `MCX_COMM` |
| `transactionType` (req) | enum string | `BUY` `SELL` |
| `quantity` (req) | int | Order quantity |
| `productType` (req) | enum string | `CNC` `INTRADAY` `MARGIN` `MTF` |
| `securityId` (req) | string | From instrument master |
| `price` (req) | float | Order price |
| `triggerPrice` (cond.) | float | For SL-M & SL-L |

**Response**
```json
{
  "totalMargin": 2800.00, "spanMargin": 1200.00, "exposureMargin": 1003.00,
  "availableBalance": 10500.00, "variableMargin": 1000.00,
  "insufficientBalance": 0.00, "brokerage": 20.00, "leverage": "4.00"
}
```
| Field | Type | Description |
|---|---|---|
| `totalMargin` | float | Total margin required |
| `spanMargin` | float | SPAN margin |
| `exposureMargin` | float | Exposure margin |
| `availableBalance` | float | Available account balance |
| `variableMargin` | float | VAR / variable margin |
| `insufficientBalance` | float | Available − Total (shortfall) |
| `brokerage` | float | Brokerage for the order |
| `leverage` | string | Margin leverage per product type |

## Margin Calculator — Multi Order — `POST /margincalculator/multi`
Margins for multiple scrips in one request (span, exposure, equity, F&O, commodity). **Values are indicative and valid only for the current session.**
**Request**
```json
{
  "includePosition": true,
  "includeOrders": true,
  "scripts": [
    {
      "exchangeSegment": "NSE_EQ",
      "transactionType": "BUY",
      "quantity": 100,
      "productType": "CNC",
      "securityId": "12345",
      "price": 250.50
    }
  ]
}
```
| Field | Type | Description |
|---|---|---|
| `includePosition` | boolean | Include existing positions |
| `includeOrders` | boolean | Include open orders |
| `scripts` | array | Scrips to calculate for |
| `scripts[].exchangeSegment` | string | e.g. `NSE_EQ`, `NSE_FNO` |
| `scripts[].transactionType` | string | `BUY` / `SELL` |
| `scripts[].quantity` | int | Quantity |
| `scripts[].productType` | string | `CNC` `INTRADAY` `MARGIN` `MTF` |
| `scripts[].securityId` | string | From instrument master |
| `scripts[].price` | float | Order price |
| `scripts[].triggerPrice` | number | If applicable |

> The OpenAPI/sample body field for the array is `scripList` in one example and `scripts` in another; the documented request structure uses `scripts` / `includeOrders`. Verify field names empirically.

**Response**
```json
{
  "total_margin": "150000.00", "span_margin": "50000.00", "exposure_margin": "30000.00",
  "equity_margin": "70000.00", "fo_margin": "0.00", "commodity_margin": "0.00",
  "currency": "INR", "hedge_benefit": ""
}
```

## Fund Limit — `GET /fundlimit`
Balance, margin utilised, collateral, etc. No body.
```bash
curl --request GET --url https://api.dhan.co/v2/fundlimit \
  --header 'Content-Type: application/json' --header 'access-token: JWT'
```
**Response**
```json
{
  "dhanClientId": "1000000009",
  "availabelBalance": 98440.0,
  "sodLimit": 113642,
  "collateralAmount": 0.0,
  "receiveableAmount": 0.0,
  "utilizedAmount": 15202.0,
  "blockedPayoutAmount": 0.0,
  "withdrawableBalance": 98310.0
}
```
| Field | Type | Description |
|---|---|---|
| `dhanClientId` | string | User ID |
| `availabelBalance` | float | Available to trade *(note: field is spelled `availabelBalance`)* |
| `sodLimit` | float | Start-of-day balance |
| `collateralAmount` | float | Amount against collateral |
| `receiveableAmount` | float | Amount against selling deliveries |
| `utilizedAmount` | float | Amount utilised today |
| `blockedPayoutAmount` | float | Blocked against payout request |
| `withdrawableBalance` | float | Withdrawable to bank |

For enum descriptions, see `08_annexure.md`.
