# Dhan V2 Statements & Trade History — Complete Reference

> **Source**: https://dhanhq.co/docs/v2/statements/
> **Extracted**: 2026-03-13

---

## 1. Overview

| Method | Endpoint                           | Description              |
|--------|------------------------------------|--------------------------|
| GET    | `/ledger?from-date=&to-date=`      | Ledger (credit/debit)    |
| GET    | `/trades/{from-date}/{to-date}/{page}` | Historical trades    |

---

## 2. Ledger Report

```
GET https://api.dhan.co/v2/ledger?from-date=YYYY-MM-DD&to-date=YYYY-MM-DD
Headers: access-token
```

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

> **NOTE**: `debit` and `credit` are **strings**, not floats. Parse them. When one is non-zero, the other is `"0.00"`.

---

## 3. Trade History

```
GET https://api.dhan.co/v2/trades/{from-date}/{to-date}/{page}
Headers: access-token
```

- `page`: 0-indexed pagination. Start with `0`.
- Path params, NOT query params.

```json
[{
    "dhanClientId": "1000000001",
    "orderId": "212212307731",
    "exchangeOrderId": "76036896",
    "exchangeTradeId": "407958",
    "transactionType": "BUY",
    "exchangeSegment": "NSE_EQ",
    "productType": "CNC",
    "orderType": "MARKET",
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
    "exchangeTime": "2022-12-30 10:00:46",
    "drvExpiryDate": "NA",
    "drvOptionType": "NA",
    "drvStrikePrice": 0
}]
```

Key fields: `tradedQuantity`, `tradedPrice`, `exchangeTime` (IST string), all tax/charge breakdowns (`sebiTax`, `stt`, `brokerageCharges`, `serviceTax`, `exchangeTransactionCharges`, `stampDuty`).
