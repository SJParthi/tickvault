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
