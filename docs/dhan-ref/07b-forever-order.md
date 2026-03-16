# Dhan V2 Forever Order — Complete Reference

> **Source**: https://dhanhq.co/docs/v2/forever/
> **Extracted**: 2026-03-13

---

## 1. Overview

Good Till Triggered (GTT) orders. Two types: `SINGLE` (one trigger) and `OCO` (two triggers — whichever hits first).

| Method | Endpoint                        | Description          |
|--------|---------------------------------|----------------------|
| POST   | `/forever/orders`               | Create               |
| PUT    | `/forever/orders/{order-id}`    | Modify               |
| DELETE | `/forever/orders/{order-id}`    | Delete               |
| GET    | `/forever/all`                  | List all forever orders |

**Static IP required.** Product types: `CNC`, `MTF` only.

---

## 2. Create Forever Order

```json
{
    "dhanClientId": "1000000132",
    "correlationId": "",
    "orderFlag": "OCO",
    "transactionType": "BUY",
    "exchangeSegment": "NSE_EQ",
    "productType": "CNC",
    "orderType": "LIMIT",
    "validity": "DAY",
    "securityId": "1333",
    "quantity": 5,
    "disclosedQuantity": 1,
    "price": 1428,
    "triggerPrice": 1427,
    "price1": 1420,
    "triggerPrice1": 1419,
    "quantity1": 10
}
```

| Field          | Required     | Description                           |
|----------------|--------------|---------------------------------------|
| `orderFlag`    | Yes          | `SINGLE` or `OCO`                     |
| `price`        | Yes          | First leg price                       |
| `triggerPrice` | Yes          | First leg trigger                     |
| `price1`       | OCO only     | Second leg (target) price             |
| `triggerPrice1` | OCO only    | Second leg trigger                    |
| `quantity1`    | OCO only     | Second leg quantity                   |

---

## 3. Modify

`legName`: `TARGET_LEG` (single/first OCO leg), `STOP_LOSS_LEG` (second OCO leg)

---

## 4. Status Values

`TRANSIT`, `PENDING`, `REJECTED`, `CANCELLED`, `TRADED`, `EXPIRED`, `CONFIRM`

> `CONFIRM` = forever order is active and waiting for trigger.
