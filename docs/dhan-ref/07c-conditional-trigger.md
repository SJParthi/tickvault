# Dhan V2 Conditional Trigger — Complete Reference

> **Source**: https://dhanhq.co/docs/v2/conditional-trigger/
> **Extracted**: 2026-03-13
> **Related**: `08-annexure-enums.md` (Sections on Comparison Type, Indicator Name, Operator, Status)

---

## 1. Overview

Place orders triggered by price or technical indicator conditions. Only Equities and Indices supported.

| Method | Endpoint                       | Description            |
|--------|--------------------------------|------------------------|
| POST   | `/alerts/orders`               | Create trigger         |
| PUT    | `/alerts/orders/{alertId}`     | Modify trigger         |
| DELETE | `/alerts/orders/{alertId}`     | Delete trigger         |
| GET    | `/alerts/orders/{alertId}`     | Get trigger by ID      |
| GET    | `/alerts/orders`               | Get all triggers       |

---

## 2. Place Conditional Trigger

```json
{
    "dhanClientId": "123456789",
    "condition": {
        "comparisonType": "TECHNICAL_WITH_VALUE",
        "exchangeSegment": "NSE_EQ",
        "securityId": "12345",
        "indicatorName": "SMA_5",
        "timeFrame": "DAY",
        "operator": "CROSSING_UP",
        "comparingValue": 250,
        "expDate": "2026-08-24",
        "frequency": "ONCE",
        "userNote": "Price crossing SMA"
    },
    "orders": [{
        "transactionType": "BUY",
        "exchangeSegment": "NSE_EQ",
        "productType": "CNC",
        "orderType": "LIMIT",
        "securityId": "12345",
        "quantity": 10,
        "validity": "DAY",
        "price": "250.00",
        "discQuantity": "0",
        "triggerPrice": "0"
    }]
}
```

### Comparison Types (from 08-annexure)

| Type                        | What it compares                           | Required fields                                    |
|-----------------------------|--------------------------------------------|----------------------------------------------------|
| `TECHNICAL_WITH_VALUE`      | Indicator vs fixed number                  | `indicatorName`, `operator`, `timeFrame`, `comparingValue` |
| `TECHNICAL_WITH_INDICATOR`  | Indicator vs another indicator             | `indicatorName`, `operator`, `timeFrame`, `comparingIndicatorName` |
| `TECHNICAL_WITH_CLOSE`      | Indicator vs closing price                 | `indicatorName`, `operator`, `timeFrame`           |
| `PRICE_WITH_VALUE`          | Market price vs fixed value                | `operator`, `comparingValue`                       |

### Available Indicators

SMA (5/10/20/50/100/200), EMA (5/10/20/50/100/200), BB_UPPER, BB_LOWER, RSI_14, ATR_14, STOCHASTIC, STOCHRSI_14, MACD_26, MACD_12, MACD_HIST

### Operators

`CROSSING_UP`, `CROSSING_DOWN`, `CROSSING_ANY_SIDE`, `GREATER_THAN`, `LESS_THAN`, `GREATER_THAN_EQUAL`, `LESS_THAN_EQUAL`, `EQUAL`, `NOT_EQUAL`

### Timeframes

`DAY`, `ONE_MIN`, `FIVE_MIN`, `FIFTEEN_MIN`

---

## 3. Response

```json
{ "alertId": "12345", "alertStatus": "ACTIVE" }
```

Status: `ACTIVE`, `TRIGGERED`, `EXPIRED`, `CANCELLED`

---

## 4. Notes

1. **Equities and Indices only** — no F&O or commodity support.
2. **`expDate` default = 1 year** from creation.
3. **`frequency: "ONCE"`** — triggers once then deactivates.
4. **Multiple orders** can be attached to a single condition.
5. **Postback** updates sent to webhook URL when triggered.

---

## 2026-07-14 Upstream Update (runner-crawled live page)

**Evidence tier: Verified-live.** Raw HTML of `https://dhanhq.co/docs/v2/conditional-trigger/`
(runs 1–3, sha256 `f3ddbe28` content-identical, latest 2026-07-14T07:58:00Z). Full manifest:
`00-COVERAGE-MANIFEST.md`.

1. **Timeframe enum-literal vs example split:** the live param tables list "**DATE** ONE_MIN
   FIVE_MIN FIFTEEN_MIN" while the Sample Value column and every JSON example use
   `"timeFrame": "DAY"`. "DATE" is the literal enum text but almost certainly a doc typo —
   use `DAY` (matches the repo list + the wire examples).
2. **Field-name case split:** the param tables say `condition.timeframe` (lowercase f); the
   JSON examples use `timeFrame`. The repo JSON (`timeFrame`) matches the wire examples.
3. **UTC-Z timestamps (live-only):** get-by-ID/get-all responses carry
   `createdTime`/`triggeredTime` in ISO-8601 UTC `Z` format ("2019-08-24T14:15:22Z") — the
   ONLY UTC timestamps in the trading-API family (everything else is IST strings) — plus a
   `lastPrice` field. Any future consumer must convert; do not assume IST here.
