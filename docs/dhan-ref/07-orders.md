# Dhan V2 Orders — Complete Reference

> **Source**: https://dhanhq.co/docs/v2/orders/
> **Extracted**: 2026-03-13
> **Related**: `08-annexure-enums.md`, `07a-super-order.md`, `07b-forever-order.md`, `07c-conditional-trigger.md`

---

## 1. Overview

| Method | Endpoint                               | Description                    |
|--------|----------------------------------------|--------------------------------|
| POST   | `/orders`                              | Place new order                |
| PUT    | `/orders/{order-id}`                   | Modify pending order           |
| DELETE | `/orders/{order-id}`                   | Cancel pending order           |
| POST   | `/orders/slicing`                      | Slice over freeze limit (F&O)  |
| GET    | `/orders`                              | Order book (all today's orders)|
| GET    | `/orders/{order-id}`                   | Single order by ID             |
| GET    | `/orders/external/{correlation-id}`    | Order by correlation ID        |
| GET    | `/trades`                              | Trade book (all today's trades)|
| GET    | `/trades/{order-id}`                   | Trades of specific order       |

> **Static IP required** for Place/Modify/Cancel (SEBI mandate since v2.4).

---

## 2. Place Order

```
POST https://api.dhan.co/v2/orders
Headers: access-token, Content-Type: application/json
```

```json
{
    "dhanClientId": "1000000003",
    "correlationId": "123abc678",
    "transactionType": "BUY",
    "exchangeSegment": "NSE_EQ",
    "productType": "INTRADAY",
    "orderType": "MARKET",
    "validity": "DAY",
    "securityId": "11536",
    "quantity": 5,
    "disclosedQuantity": "",
    "price": "",
    "triggerPrice": "",
    "afterMarketOrder": false,
    "amoTime": ""
}
```

| Field               | Type        | Required     | Description                                        |
|---------------------|-------------|--------------|----------------------------------------------------|
| `dhanClientId`      | string      | Yes          | Dhan Client ID                                     |
| `correlationId`     | string      | No           | User tracking ID (max 30 chars, `[a-zA-Z0-9 _-]`) |
| `transactionType`   | enum string | Yes          | `BUY` or `SELL`                                    |
| `exchangeSegment`   | enum string | Yes          | See 08-annexure Section 1                          |
| `productType`       | enum string | Yes          | `CNC`, `INTRADAY`, `MARGIN`, `MTF`                |
| `orderType`         | enum string | Yes          | `LIMIT`, `MARKET`, `STOP_LOSS`, `STOP_LOSS_MARKET` |
| `validity`          | enum string | Yes          | `DAY`, `IOC`                                       |
| `securityId`        | string      | Yes          | From instrument master                             |
| `quantity`          | int         | Yes          | Number of shares                                   |
| `disclosedQuantity` | int         | No           | Visible qty (>30% of quantity)                     |
| `price`             | float       | Yes          | Order price (0 for MARKET)                         |
| `triggerPrice`      | float       | Conditional  | For SL/SLM orders                                  |
| `afterMarketOrder`  | boolean     | Conditional  | AMO flag                                           |
| `amoTime`           | enum string | Conditional  | `PRE_OPEN`, `OPEN`, `OPEN_30`, `OPEN_60`           |
| `boProfitValue`     | float       | No           | Bracket order target profit value. Only when `productType` is `BO`. |
| `boStopLossValue`   | float       | No           | Bracket order stop loss value. Only when `productType` is `BO`.     |

> **Bracket Order fields** (optional): `boProfitValue` (f64) and `boStopLossValue` (f64) enable bracket order behavior through the standard order endpoint. Only used when `productType` is `BO`.

**Response**: `{ "orderId": "...", "orderStatus": "PENDING" }`

---

## 3. Modify Order

```
PUT https://api.dhan.co/v2/orders/{order-id}
```

```json
{
    "dhanClientId": "1000000009",
    "orderId": "112111182045",
    "orderType": "LIMIT",
    "quantity": 40,
    "price": 3345.8,
    "disclosedQuantity": 10,
    "triggerPrice": "",
    "validity": "DAY"
}
```

> **NOTE**: `quantity` = total order quantity (NOT remaining). Max 25 modifications per order.

> **SDK Note**: The Python SDK includes a `legName` field in the modify order request, which is needed for modifying specific legs of Bracket/Cover orders.

---

## 4. Cancel Order

```
DELETE https://api.dhan.co/v2/orders/{order-id}
```

Response: `{ "orderId": "...", "orderStatus": "CANCELLED" }`

---

## 5. Order Slicing

```
POST https://api.dhan.co/v2/orders/slicing
```

Same request body as Place Order. Automatically splits quantity into multiple legs when over F&O freeze limit.

---

## 6. Order Book

```
GET https://api.dhan.co/v2/orders
```

Returns array of all orders for the day. Key response fields: `orderId`, `correlationId`, `orderStatus`, `quantity`, `filledQty`, `remainingQuantity`, `averageTradedPrice`, `price`, `triggerPrice`, `omsErrorCode`, `omsErrorDescription`.

**Order Status values**: `TRANSIT`, `PENDING`, `REJECTED`, `CANCELLED`, `PART_TRADED`, `TRADED`, `EXPIRED`

---

## 7. Trade Book

```
GET https://api.dhan.co/v2/trades
```

Returns array of all executed trades. Key fields: `orderId`, `exchangeOrderId`, `exchangeTradeId`, `tradedQuantity`, `tradedPrice`, `exchangeTime`.

```
GET https://api.dhan.co/v2/trades/{order-id}
```

Returns trades for a specific order (useful for partial fills).

---

## 8. Rust Structs

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceOrderRequest {
    pub dhan_client_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    pub transaction_type: String,       // BUY, SELL
    pub exchange_segment: String,       // NSE_EQ, NSE_FNO, etc.
    pub product_type: String,           // CNC, INTRADAY, MARGIN, MTF
    pub order_type: String,             // LIMIT, MARKET, STOP_LOSS, STOP_LOSS_MARKET
    pub validity: String,               // DAY, IOC
    pub security_id: String,
    pub quantity: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disclosed_quantity: Option<i32>,
    pub price: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_market_order: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amo_time: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderResponse {
    pub order_id: String,
    pub order_status: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderBookEntry {
    pub dhan_client_id: String,
    pub order_id: String,
    pub correlation_id: Option<String>,
    pub order_status: String,
    pub transaction_type: String,
    pub exchange_segment: String,
    pub product_type: String,
    pub order_type: String,
    pub validity: String,
    pub security_id: String,
    pub quantity: i32,
    pub disclosed_quantity: i32,
    pub price: f64,
    pub trigger_price: f64,
    pub after_market_order: bool,
    pub create_time: String,
    pub update_time: String,
    pub exchange_time: Option<String>,
    pub remaining_quantity: i32,
    pub average_traded_price: f64,
    pub filled_qty: i32,
    pub oms_error_code: Option<String>,
    pub oms_error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeEntry {
    pub dhan_client_id: String,
    pub order_id: String,
    pub exchange_order_id: String,
    pub exchange_trade_id: String,
    pub transaction_type: String,
    pub exchange_segment: String,
    pub product_type: String,
    pub order_type: String,
    pub security_id: String,
    pub traded_quantity: i32,
    pub traded_price: f64,
    pub create_time: String,
    pub exchange_time: String,
}
```

---

## 9. Critical Notes

1. **Static IP mandatory** for place/modify/cancel since v2.4.
2. **Rate limit: 10 orders/sec**, 250/min, 500/hr, 5000/day. 25 modifications per order.
3. **`quantity` in modify = TOTAL order qty**, not remaining qty.
4. **`correlationId`** — set this on every order for tracking. Max 30 chars.
5. **Timestamps are IST strings** (`YYYY-MM-DD HH:MM:SS`), not epoch.
6. **`PART_TRADED`** status — check `filledQty` and `remainingQuantity`.
7. **Order slicing** — use for F&O when quantity exceeds exchange freeze limit.
