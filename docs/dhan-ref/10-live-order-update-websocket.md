# Dhan V2 Live Order Update WebSocket — Complete Reference

> **Source**: https://dhanhq.co/docs/v2/order-update/
> **Extracted**: 2026-03-13
> **Related**: `08-annexure-enums.md` (enums), `02-authentication.md`

---

## 1. Overview

Real-time order updates via WebSocket. **JSON messages** (not binary — unlike market feed). Receives updates for ALL orders on your account regardless of which platform placed them.

---

## 2. Establishing Connection

### Endpoint

```
wss://api-order-update.dhan.co
```

### Auth Message — Individual

```json
{
    "LoginReq": {
        "MsgCode": 42,
        "ClientId": "1000000001",
        "Token": "JWT"
    },
    "UserType": "SELF"
}
```

### Auth Message — Partner

```json
{
    "LoginReq": {
        "MsgCode": 42,
        "ClientId": "partner_id"
    },
    "UserType": "PARTNER",
    "Secret": "partner_secret"
}
```

| Field      | Type   | Description                           |
|------------|--------|---------------------------------------|
| `MsgCode`  | int    | Always `42`                           |
| `ClientId` | string | Dhan Client ID (or partner_id)        |
| `Token`    | string | Access Token (individual only)        |
| `UserType` | string | `SELF` or `PARTNER`                   |
| `Secret`   | string | Partner secret (partner only)         |

---

## 3. Order Update Message Structure

```json
{
    "Data": {
        "Exchange": "NSE",
        "Segment": "E",
        "Source": "N",
        "SecurityId": "14366",
        "ClientId": "1000000001",
        "ExchOrderNo": "1400000000404591",
        "OrderNo": "1124091136546",
        "Product": "C",
        "TxnType": "B",
        "OrderType": "LMT",
        "Validity": "DAY",
        "DiscQuantity": 1,
        "DiscQtyRem": 1,
        "RemainingQuantity": 1,
        "Quantity": 1,
        "TradedQty": 0,
        "Price": 13,
        "TriggerPrice": 0,
        "TradedPrice": 0,
        "AvgTradedPrice": 0,
        "OffMktFlag": "0",
        "OrderDateTime": "2024-09-11 14:39:29",
        "ExchOrderTime": "2024-09-11 14:39:29",
        "LastUpdatedTime": "2024-09-11 14:39:29",
        "ReasonDescription": "CONFIRMED",
        "LegNo": 1,
        "Instrument": "EQUITY",
        "Symbol": "IDEA",
        "ProductName": "CNC",
        "Status": "Cancelled",
        "LotSize": 1,
        "ExpiryDate": "0001-01-01 00:00:00",
        "OptType": "XX",
        "DisplayName": "Vodafone Idea",
        "Isin": "INE669E01016",
        "CorrelationId": "",
        "Remarks": ""
    },
    "Type": "order_alert"
}
```

---

## 4. Field Reference

### Core Order Fields

| Field              | Type   | Description                                                    |
|--------------------|--------|----------------------------------------------------------------|
| `Exchange`         | string | `NSE`, `BSE`, `MCX`                                           |
| `Segment`          | string | `E`=Equity, `D`=Derivatives, `C`=Currency, `M`=Commodity      |
| `Source`           | string | `P`=API, `N`=Normal (Dhan web/app)                            |
| `SecurityId`       | string | Exchange standard ID                                           |
| `ClientId`         | string | Dhan Client ID                                                 |
| `ExchOrderNo`      | string | Order ID from exchange                                         |
| `OrderNo`          | string | Order ID from Dhan                                             |

### Transaction Details

| Field              | Type   | Description                                                    |
|--------------------|--------|----------------------------------------------------------------|
| `Product`          | string | `C`=CNC, `I`=INTRADAY, `M`=MARGIN, `F`=MTF, `V`=CO, `B`=BO  |
| `TxnType`          | string | `B`=Buy, `S`=Sell                                              |
| `OrderType`        | string | `LMT`, `MKT`, `SL`, `SLM`                                     |
| `Validity`         | string | `DAY`, `IOC`                                                   |

### Quantity & Price

| Field              | Type   | Description                                                    |
|--------------------|--------|----------------------------------------------------------------|
| `Quantity`         | int    | Total order quantity                                           |
| `TradedQty`        | int    | Executed quantity                                              |
| `RemainingQuantity`| int    | Pending quantity                                               |
| `DiscQuantity`     | int    | Disclosed quantity                                             |
| `Price`            | float  | Order price                                                    |
| `TriggerPrice`     | float  | Trigger price (SL/CO/BO)                                       |
| `TradedPrice`      | float  | Execution price                                                |
| `AvgTradedPrice`   | float  | Average price (differs from TradedPrice on partial fills)      |

### Timestamps

| Field              | Type   | Description                                                    |
|--------------------|--------|----------------------------------------------------------------|
| `OrderDateTime`    | string | When Dhan received the order (`YYYY-MM-DD HH:MM:SS` **IST**)  |
| `ExchOrderTime`    | string | When order hit exchange                                        |
| `LastUpdatedTime`  | string | Last modification or trade time                                |

> **NOTE**: These timestamps are **IST strings**, NOT epoch. Different from WebSocket feed LTT.

### Status & Metadata

| Field              | Type   | Description                                                    |
|--------------------|--------|----------------------------------------------------------------|
| `Status`           | string | `TRANSIT`, `PENDING`, `REJECTED`, `CANCELLED`, `TRADED`, `EXPIRED` |
| `ReasonDescription`| string | Rejection reason or `CONFIRMED`                                |
| `LegNo`            | int    | `1`=Entry, `2`=Stop Loss, `3`=Target (BO/CO)                  |
| `OffMktFlag`       | string | `1`=AMO, `0`=normal                                           |
| `CorrelationId`    | string | User-provided tracking ID (max 30 chars)                       |
| `Remarks`          | string | `Super Order` if part of super order, else user remarks        |

### Instrument Info

| Field              | Type   | Description                                                    |
|--------------------|--------|----------------------------------------------------------------|
| `Instrument`       | string | `EQUITY`, `FUTIDX`, `OPTIDX`, etc.                            |
| `Symbol`           | string | Trading symbol                                                 |
| `DisplayName`      | string | Human-readable name                                            |
| `Isin`             | string | ISIN code                                                      |
| `LotSize`          | int    | Lot size for derivatives                                       |
| `StrikePrice`      | float  | Strike price (options only)                                    |
| `ExpiryDate`       | string | Expiry date                                                    |
| `OptType`          | string | `CE`, `PE`, or `XX` (non-option)                               |

---

## 5. Rust Struct Definitions

```rust
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct OrderUpdateMessage {
    #[serde(rename = "Data")]
    pub data: OrderUpdateData,
    #[serde(rename = "Type")]
    pub msg_type: String,         // "order_alert"
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct OrderUpdateData {
    pub exchange: String,
    pub segment: String,
    pub source: String,
    pub security_id: String,
    pub client_id: String,
    pub exch_order_no: String,
    pub order_no: String,
    pub product: String,
    pub txn_type: String,
    pub order_type: String,
    pub validity: String,
    pub disc_quantity: i32,
    pub disc_qty_rem: i32,
    pub remaining_quantity: i32,
    pub quantity: i32,
    pub traded_qty: i32,
    pub price: f64,
    pub trigger_price: f64,
    pub traded_price: f64,
    pub avg_traded_price: f64,
    pub off_mkt_flag: String,
    pub order_date_time: String,      // IST string, NOT epoch
    pub exch_order_time: String,
    pub last_updated_time: String,
    pub reason_description: String,
    pub leg_no: i32,
    pub instrument: String,
    pub symbol: String,
    pub product_name: String,
    pub status: String,
    pub lot_size: i32,
    pub display_name: String,
    pub isin: String,
    pub correlation_id: Option<String>,
    pub remarks: Option<String>,
}

// ─── Auth Request ───

#[derive(Debug, serde::Serialize)]
pub struct OrderUpdateAuth {
    #[serde(rename = "LoginReq")]
    pub login_req: LoginReq,
    #[serde(rename = "UserType")]
    pub user_type: String,      // "SELF"
}

#[derive(Debug, serde::Serialize)]
pub struct LoginReq {
    #[serde(rename = "MsgCode")]
    pub msg_code: u32,          // Always 42
    #[serde(rename = "ClientId")]
    pub client_id: String,
    #[serde(rename = "Token")]
    pub token: String,
}
```

---

## 6. Critical Implementation Notes

1. **JSON, not binary** — unlike market feed WebSockets. Parse with serde_json directly.

2. **Timestamps are IST strings** (`YYYY-MM-DD HH:MM:SS`), NOT epoch. No +5:30 conversion needed — they're already IST.

3. **Receives ALL account orders** — not just API orders. Web, mobile, partner platform orders all stream here.

4. **`Source: "P"`** = API-placed order. Use this to filter your own orders.

5. **`Status` tracks the lifecycle**: TRANSIT → PENDING → TRADED/REJECTED/CANCELLED. Partial fills show as `PART_TRADED` in some cases.

6. **`CorrelationId`** — set this when placing orders to track them through updates. Max 30 chars, alphanumeric + underscore + hyphen.

7. **`Remarks: "Super Order"`** — indicates the order belongs to a Super Order group.

8. **For dhan-live-trader monitoring phase**: Connect this WebSocket alongside market feed. Log all order updates for debugging without executing any orders.
