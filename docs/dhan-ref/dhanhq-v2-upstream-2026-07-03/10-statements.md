# DhanHQ API v2 — Statements (Ledger + Trade History)

> **Source:** Official DhanHQ documentation (docs.dhanhq.co) — captured from DhanHQ's own "Export .md for LLMs" file, generated 2026-06-30.
> **Pages covered in this file:**
> - https://docs.dhanhq.co/api/v2/statements
> - https://docs.dhanhq.co/api/v2/guides/statements
> - https://docs.dhanhq.co/api/v2/statements/get-ledger-report
> - https://docs.dhanhq.co/api/v2/statements/get-trade-history


---

## Statement

Statement

This set of APIs retreives all your trade and ledger details to help you summarise and analyse your trades.

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/ledger` | Retrieve Trading Account debit and credit details |
| GET | `/trades/` | Retrieve historical trade data |

---

## Statement

Retrieve trading account ledger reports and historical trade data

Retrieve your trade and ledger details to help summarise and analyse your trades.

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/ledger` | Retrieve Trading Account debit and credit details |
| GET | `/trades/{from}/{to}/{page}` | Retrieve historical trade data |

---

## Ledger Report

Retrieve Trading Account Ledger Report with all credit and debit transaction details for a particular time interval.

```bash
curl --request GET \
  --url 'https://api.dhan.co/v2/ledger?from-date={YYYY-MM-DD}&to-date={YYYY-MM-DD}' \
  --header 'Accept: application/json' \
  --header 'access-token: {JWT}'
```

**Query Parameters**

| Field | Description |
|-------|-------------|
| `from-date`* | Start date in format `YYYY-MM-DD` |
| `to-date`* | End date in format `YYYY-MM-DD` |

**Response**

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

---

## Trade History

Retrieve detailed trade history for all orders for a particular time frame. Response is paginated — pass page number as path parameter.

```bash
curl --request GET \
  --url https://api.dhan.co/v2/trades/{from-date}/{to-date}/{page} \
  --header 'Accept: application/json' \
  --header 'access-token: {JWT}'
```

**Path Parameters**

| Field | Description |
|-------|-------------|
| `from-date`* | Start date in format `YYYY-MM-DD` |
| `to-date`* | End date in format `YYYY-MM-DD` |
| `page`* | Page number (pass `0` as default) |

**Response**

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


For description of enum values, refer to the [Annexure](/api/v2/guides/annexure).


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| dhanClientId | string | No | User specific identification |
| narration | string | No | Description of the ledger transaction |
| voucherdate | string | No | Transaction Date |
| exchange | string | No | Exchange information for the transaction |
| voucherdesc | string | No | Nature of transaction |
| vouchernumber | string | No | System generated transaction number |
| debit | string | No | Debit amount (only when credit returns 0) |
| credit | string | No | Credit amount (only when debit returns 0) |
| runbal | string | No | Running Balance post transaction |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| dhanClientId | string | No | User specific identification |
| orderId | string | No | Order specific identification by Dhan |
| exchangeOrderId | string | No | Order specific identification by exchange |
| exchangeTradeId | string | No | Trade specific identification by exchange |
| transactionType | enum | No | or Values: `BUY`, `SELL`. |
| exchangeSegment | enum | No | Exchange Segment ([Annexure](/api/v2/guides/annexure#exchange-segment)) |
| productType | enum | No | Values: `CNC`, `INTRADAY`, `MARGIN`, `MTF`. |
| orderType | enum | No | Values: `LIMIT`, `MARKET`, `STOP_LOSS`, `STOP_LOSS_MARKET`. |
| tradingSymbol | string | No | Symbol ([Instruments](/api/v2/guides/instruments)) |
| customSymbol | string | No | Trading Symbol as per Dhan |
| securityId | string | No | Exchange standard ID |
| tradedQuantity | int | No | Number of shares executed |
| tradedPrice | float | No | Price at which trade is executed |
| isin | string | No | Universal standard ID |
| instrument | string | No | or Values: `EQUITY`, `DERIVATIVES`. |
| sebiTax | string | No | SEBI Turnover Charges |
| stt | string | No | Securities Transactions Tax |
| brokerageCharges | string | No | Brokerage charges ([pricing](https://dhan.co/pricing/)) |
| serviceTax | string | No | Applicable Service Tax |
| exchangeTransactionCharges | string | No | Exchange Transaction Charge |
| stampDuty | string | No | Stamp Duty Charges |
| createTime | string | No | Time at which the order is created |
| updateTime | string | No | Time at which the last activity happened |
| exchangeTime | string | No | Time at which order reached exchange |
| drvExpiryDate | int | No | Expiry date for F&O contracts |
| drvOptionType | enum | No | or Values: `CALL`, `PUT`. |
| drvStrikePrice | float | No | Strike Price for Options |

---

## Ledger Report

`GET` https://api.dhan.co/v2/ledger

Retrieve Trading Account Ledger Report with all credit and debit transaction details.




### Query Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| from-date | string (date) | No | Start date in YYYY-MM-DD format |
| to-date | string (date) | No | End date in YYYY-MM-DD format |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| dhanClientId | string | No | User specific identification generated by Dhan |
| narration | string | No | Description of transaction |
| voucherdate | string | No | Date of ledger entry |
| exchange | string | No | Exchange for the transaction |
| voucherdesc | string | No | Nature of transaction |
| vouchernumber | string | No | Voucher number for the transaction |
| debit | string | No | Debit amount for the entry |
| credit | string | No | Credit amount for the entry |
| runbal | string | No | Running balance after the entry |


### Status Codes

| Code | Description |
|------|-------------|
| 200 | Ledger entries |
| 401 | Unauthorized |


### Example Request

```bash
curl -X GET https://api.dhan.co/v2/ledger \
  -H "access-token: <your-access-token>"
```

---

## Trade History

`GET` https://api.dhan.co/v2/trades/{from-date}/{to-date}/{page-number}

Retrieve detailed trade history for all orders for a given time frame. Response is paginated.

Users can retrieve their detailed trade history for all orders for a particular time frame. User needs to add header parameters along with page number as the response is paginated.


### Path Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| from-date | string | Yes | Start date in YYYY-MM-DD format |
| to-date | string | Yes | End date in YYYY-MM-DD format |
| page-number | integer | Yes | Page number (pass 0 as default) |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| dhanClientId | string | No | User specific identification generated by Dhan |
| orderId | string | No | Order specific identification generated by Dhan |
| exchangeOrderId | string | No | Order specific identification generated by exchange |
| exchangeTradeId | string | No | Trade specific identification generated by exchange |
| transactionType | enum string | No | The trading side of transaction Values: `BUY`, `SELL`. |
| exchangeSegment | enum string | No | Exchange Segment (see [Annexure](/api/v2/guides/annexure#exchange-segment)) |
| productType | enum string | No | Product type of trade Values: `CNC`, `INTRADAY`, `MARGIN`, `MTF`. |
| orderType | enum string | No | Order Type Values: `LIMIT`, `MARKET`, `STOP_LOSS`, `STOP_LOSS_MARKET`. |
| customSymbol | string | No | Custom trading symbol |
| securityId | string | No | Exchange standard ID for each scrip. refer here |
| tradedQuantity | int | No | Number of shares executed |
| tradedPrice | float | No | Price at which trade is executed |
| isin | string | No | ISIN of the security |
| instrument | string | No | Instrument type |
| sebiTax | float | No | SEBI Tax |
| stt | float | No | Securities Transaction Tax (STT) |
| brokerageCharges | float | No | Brokerage charges |
| serviceTax | float | No | Service tax |
| exchangeTransactionCharges | float | No | Exchange transaction charges |
| stampDuty | float | No | Stamp duty |
| createTime | string | No | Time at which the order is created |
| updateTime | string | No | Time at which the last activity happened |
| exchangeTime | string | No | Time at which order reached at exchange |
| drvExpiryDate | string | No | For F&O, expiry date of contract |
| drvOptionType | enum string | No | Type of Option Values: `CALL`, `PUT`. |
| drvStrikePrice | float | No | For Options, Strike Price |


### Status Codes

| Code | Description |
|------|-------------|
| 200 | Paginated trade history |
| 401 | Unauthorized |


### Example Request

```bash
curl -X GET https://api.dhan.co/v2/trades/{from-date}/{to-date}/{page-number} \
  -H "access-token: <your-access-token>"
```
