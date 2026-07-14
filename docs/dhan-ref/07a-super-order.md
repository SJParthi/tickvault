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

> **API Typo**: The Dhan API returns `totalQuatity` (missing 'n' in Quantity) in Super Order leg details, similar to the `availabelBalance` typo in the Funds API. Use `#[serde(rename = "totalQuatity")]` in Rust structs.

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

---

## 2026-07-14 Upstream Update (runner-crawled live pages)

**Evidence tier: Verified-live.** Raw HTML of `https://dhanhq.co/docs/v2/super-order/`
(runs 1–3, sha256 `4a04b343` content-identical, latest 2026-07-14T07:57:53Z) + portal markdown
exports (place/modify/cancel super-order, 2026-07-14T08:05:03–14Z). Full manifest:
`00-COVERAGE-MANIFEST.md`.

1. **`trailingJump` is `required` in the live place table** ("trailingJump required | float |
   Price Jump by which Stop Loss should be trailed") — pass `0` for no trail; the field itself
   is required, not optional.
2. **Trail cancellation is WIDER than the =0 arm:** live modify table verbatim — "If trailing
   jump is **not added** or passed as 0, it will be cancelled" (ENTRY_LEG or STOP_LOSS_LEG
   modify). OMITTING `trailingJump` on a modify ALSO cancels the trail — load-bearing for any
   future modify caller: always re-send `trailingJump`.
3. **Single-trail-only on the v2 API surface (BOTH doc surfaces):** exactly ONE trailing field
   exists — `trailingJump`, the SL trail (×8 on the classic page; the portal place/modify
   exports agree); the TARGET_LEG modify body carries only `targetPrice`; no second trail
   field, no "Super Trail" text anywhere. The dhan.co support-page "Super Trail" claim (dual
   SL + target trailing — `verification-2026-07-13.md` §4 flag 6) is a PLATFORM feature claim
   (Secondary tier); the v2 API exposes SL-trail only as of 2026-07-14. This note records the
   API-surface fact and deliberately does NOT encode the platform claim.
4. Consistency spot-check: the classic page's "Cancelling main order ID cancels all legs. If
   particular target or stop loss leg is cancelled, then the same cannot be added again." is
   ABSENT from the portal cancel page (an omission, not a contradiction — the classic note
   stands).
