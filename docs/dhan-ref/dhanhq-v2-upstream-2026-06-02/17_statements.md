# Statement (Ledger & Trade History) — DhanHQ v2

> Source: https://dhanhq.co/docs/v2/statements/ · API v2 · Audited 2 Jun 2026

Retrieve trade and ledger details to summarise/analyse trades.

| Method | Path | Use |
|---|---|---|
| GET | `/ledger` | Account debit/credit (ledger) details |
| GET | `/trades/{from-date}/{to-date}/{page}` | Historical trade data (paginated) |

## Ledger Report — `GET /ledger`
All credit/debit transactions for a time interval (query params).
```bash
curl --request GET \
  --url 'https://api.dhan.co/v2/ledger?from-date={YYYY-MM-DD}&to-date={YYYY-MM-DD}' \
  --header 'Accept: application/json' --header 'access-token: {JWT}'
```
**Query params:** `from-date` (req, `YYYY-MM-DD`), `to-date` (req, `YYYY-MM-DD`). No body.

**Response** (array of entries):
```json
{
  "dhanClientId": "1000000001",
  "narration": "FUNDS WITHDRAWAL",
  "voucherdate": "Jun 22, 2022",
  "exchange": "NSE-CAPITAL",
  "voucherdesc": "PAYBNK",
  "vouchernumber": "202200036701",
  "debit": "20000.00",
  "credit": "0.00",
  "runbal": "957.29"
}
```
| Field | Type | Description |
|---|---|---|
| `dhanClientId` | string | User ID |
| `narration` | string | Transaction description |
| `voucherdate` | string | Transaction date |
| `exchange` | string | Exchange info |
| `voucherdesc` | string | Nature of transaction |
| `vouchernumber` | string | System transaction number |
| `debit` | string | Debit amount (only when credit = 0) |
| `credit` | string | Credit amount (only when debit = 0) |
| `runbal` | string | Running balance after transaction |

## Trade History — `GET /trades/{from-date}/{to-date}/{page}`
Detailed trade history for all orders in a time frame. Paginated.
```bash
curl --request GET \
  --url https://api.dhan.co/v2/trades/{from-date}/{to-date}/{page} \
  --header 'Accept: application/json' --header 'access-token: {JWT}'
```
**Path params:** `from-date` (req, `YYYY-MM-DD`), `to-date` (req, `YYYY-MM-DD`), `page` (req, pass `0` as default). No body.

**Response** (array):
```json
[
  {
    "dhanClientId": "1000000001",
    "orderId": "212212307731",
    "exchangeOrderId": "76036896",
    "exchangeTradeId": "407958",
    "transactionType": "BUY",
    "exchangeSegment": "NSE_EQ",
    "productType": "CNC",
    "orderType": "MARKET",
    "tradingSymbol": null,
    "customSymbol": "Tata Motors",
    "securityId": "3456",
    "tradedQuantity": 1,
    "tradedPrice": 390.9,
    "isin": "INE155A01022",
    "instrument": "EQUITY",
    "sebiTax": 0.0004,
    "stt": 0,
    "brokerageCharges": 0,
    "serviceTax": 0.0025,
    "exchangeTransactionCharges": 0.0135,
    "stampDuty": 0,
    "createTime": "NA",
    "updateTime": "NA",
    "exchangeTime": "2022-12-30 10:00:46",
    "drvExpiryDate": "NA",
    "drvOptionType": "NA",
    "drvStrikePrice": 0
  }
]
```
| Field | Type | Description |
|---|---|---|
| `dhanClientId` | string | User ID |
| `orderId` | string | Dhan order id |
| `exchangeOrderId` | string | Exchange order id |
| `exchangeTradeId` | string | Exchange trade id |
| `transactionType` | enum string | `BUY` `SELL` |
| `exchangeSegment` | enum string | See `08_annexure.md` |
| `productType` | enum string | `CNC` `INTRADAY` `MARGIN` `MTF` `CO` `BO` |
| `orderType` | enum string | `LIMIT` `MARKET` `STOP_LOSS` `STOP_LOSS_MARKET` |
| `tradingSymbol` | string | From instrument master |
| `customSymbol` | string | Dhan trading symbol |
| `securityId` | string | From instrument master |
| `tradedQuantity` | int | Quantity executed |
| `tradedPrice` | float | Execution price |
| `isin` | string | Universal scrip ID |
| `instrument` | string | `EQUITY` `DERIVATIVES` |
| `sebiTax` | string | SEBI turnover charges |
| `stt` | string | Securities Transaction Tax |
| `brokerageCharges` | string | Dhan brokerage |
| `serviceTax` | string | Service tax |
| `exchangeTransactionCharges` | string | Exchange transaction charge |
| `stampDuty` | string | Stamp duty |
| `createTime` | string | Order created time |
| `updateTime` | string | Last activity time |
| `exchangeTime` | string | Reached exchange time |
| `drvExpiryDate` | int | F&O contract expiry |
| `drvOptionType` | enum string | `CALL` `PUT` |
| `drvStrikePrice` | float | Option strike |

For enum descriptions, see `08_annexure.md`.
