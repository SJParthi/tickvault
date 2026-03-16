# Dhan V2 Super Order — Complete Reference

> **Source**: https://dhanhq.co/docs/v2/super-order/
> **Extracted**: 2026-03-13
> **Related**: `07-orders.md`, `08-annexure-enums.md`

---

## 1. Overview

Entry + Target + Stop Loss + optional Trailing SL in a single order. Available across all segments.

| Method | Endpoint                                     | Description               |
|--------|----------------------------------------------|---------------------------|
| POST   | `/super/orders`                              | Place super order         |
| PUT    | `/super/orders/{order-id}`                   | Modify (any leg)          |
| DELETE | `/super/orders/{order-id}/{order-leg}`       | Cancel specific leg       |
| GET    | `/super/orders`                              | List all super orders     |

**Static IP required.**

---

## 2. Place Super Order

```json
{
    "dhanClientId": "1000000003",
    "correlationId": "123abc678",
    "transactionType": "BUY",
    "exchangeSegment": "NSE_EQ",
    "productType": "CNC",
    "orderType": "LIMIT",
    "securityId": "11536",
    "quantity": 5,
    "price": 1500,
    "targetPrice": 1600,
    "stopLossPrice": 1400,
    "trailingJump": 10
}
```

| Field           | Type   | Description                                |
|-----------------|--------|--------------------------------------------|
| `targetPrice`   | float  | Target exit price                          |
| `stopLossPrice` | float  | Stop loss price                            |
| `trailingJump`  | float  | Price jump for trailing SL (0 = no trail)  |

---

## 3. Modify Super Order

Three leg types: `ENTRY_LEG`, `TARGET_LEG`, `STOP_LOSS_LEG`

- **ENTRY_LEG**: Can modify everything (only when `PENDING` or `PART_TRADED`)
- **TARGET_LEG**: Can only modify `targetPrice`
- **STOP_LOSS_LEG**: Can modify `stopLossPrice` and `trailingJump`

> If `trailingJump` is set to `0`, trailing is cancelled.

---

## 4. Cancel Super Order

```
DELETE /super/orders/{order-id}/{order-leg}
```

- Cancelling `ENTRY_LEG` cancels ALL legs
- Cancelling `TARGET_LEG` or `STOP_LOSS_LEG` individually — that leg cannot be re-added

---

## 5. Super Order List

```
GET /super/orders
```

Response includes `legDetails[]` array with nested target and stop loss legs. Key statuses: `CLOSED` (entry + one exit executed), `TRIGGERED` (target or SL leg triggered).

---

## 6. Rust Structs

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceSuperOrderRequest {
    pub dhan_client_id: String,
    pub correlation_id: Option<String>,
    pub transaction_type: String,
    pub exchange_segment: String,
    pub product_type: String,
    pub order_type: String,
    pub security_id: String,
    pub quantity: i32,
    pub price: f64,
    pub target_price: f64,
    pub stop_loss_price: f64,
    pub trailing_jump: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuperOrderLeg {
    pub order_id: String,
    pub leg_name: String,
    pub transaction_type: String,
    pub remaining_quantity: i32,
    pub triggered_quantity: i32,
    pub price: f64,
    pub order_status: String,
    pub trailing_jump: f64,
}
```
