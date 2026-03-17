# Dhan V2 Funds & Margin — Complete Reference

> **Source**: https://dhanhq.co/docs/v2/funds/
> **Extracted**: 2026-03-13

---

## 1. Overview

| Method | Endpoint                  | Description                          |
|--------|---------------------------|--------------------------------------|
| POST   | `/margincalculator`       | Margin for single order              |
| POST   | `/margincalculator/multi` | Margin for multiple orders           |
| GET    | `/fundlimit`              | Account balance & limits             |

---

## 2. Single Order Margin Calculator

```
POST https://api.dhan.co/v2/margincalculator
Headers: access-token
```

### Request
```json
{
    "dhanClientId": "1000000132",
    "exchangeSegment": "NSE_EQ",
    "transactionType": "BUY",
    "quantity": 5,
    "productType": "CNC",
    "securityId": "1333",
    "price": 1428,
    "triggerPrice": 1427
}
```

### Response
```json
{
    "totalMargin": 2800.00,
    "spanMargin": 1200.00,
    "exposureMargin": 1003.00,
    "availableBalance": 10500.00,
    "variableMargin": 1000.00,
    "insufficientBalance": 0.00,
    "brokerage": 20.00,
    "leverage": "4.00"
}
```

| Field                | Type   | Description                                    |
|----------------------|--------|------------------------------------------------|
| `totalMargin`        | float  | Total margin needed                            |
| `spanMargin`         | float  | SPAN margin                                    |
| `exposureMargin`     | float  | Exposure margin                                |
| `availableBalance`   | float  | Available in account                           |
| `variableMargin`     | float  | VAR margin                                     |
| `insufficientBalance`| float  | Shortfall (0 if sufficient)                    |
| `brokerage`          | float  | Brokerage charges                              |
| `leverage`           | string | Margin leverage multiplier                     |

---

## 3. Multi Order Margin Calculator

```
POST https://api.dhan.co/v2/margincalculator/multi
Headers: access-token
```

```json
{
    "includePosition": true,
    "includeOrders": true,
    "scripts": [{
        "exchangeSegment": "NSE_EQ",
        "transactionType": "BUY",
        "quantity": 100,
        "productType": "CNC",
        "securityId": "12345",
        "price": 250.50
    }]
}
```

### Response

> **NOTE**: Multi-margin response uses **snake_case** field names — different from single-margin (camelCase).

```json
{
    "total_margin": "150000.00",
    "span_margin": "50000.00",
    "exposure_margin": "45000.00",
    "equity_margin": "0.00",
    "fo_margin": "150000.00",
    "commodity_margin": "0.00",
    "currency": "0.00",
    "hedge_benefit": "5000.00"
}
```

| Field              | Type   | Description                               |
|--------------------|--------|-------------------------------------------|
| `total_margin`     | string | Total margin required (all legs combined) |
| `span_margin`      | string | SPAN margin component                    |
| `exposure_margin`  | string | Exposure margin component                |
| `equity_margin`    | string | Equity segment margin                    |
| `fo_margin`        | string | F&O segment margin                       |
| `commodity_margin` | string | Commodity segment margin                 |
| `currency`         | string | Currency segment margin                  |
| `hedge_benefit`    | string | Margin reduction from hedged positions   |

> **NOTE**: All multi-margin response values are **strings** (e.g., `"150000.00"`), unlike single-margin which returns floats.

> **Note**: Dhan's own documentation shows inconsistent field names for multi-margin requests: `includeOrders` vs `includeOrder` (singular/plural), and `scripts` vs `scripList`. Our implementation uses `includeOrders` (plural) and `scripts`. Verify with live API testing.

---

## 4. Fund Limit

```
GET https://api.dhan.co/v2/fundlimit
Headers: access-token
```

```json
{
    "dhanClientId": "1000000009",
    "availabelBalance": 98440.0,
    "sodLimit": 113642,
    "collateralAmount": 0.0,
    "receiveableAmount": 0.0,
    "utilizedAmount": 15202.0,
    "blockedPayoutAmount": 0.0,
    "withdrawableBalance": 98310.0
}
```

| Field                 | Type   | Description                              |
|-----------------------|--------|------------------------------------------|
| `availabelBalance`    | float  | Available to trade (**note: typo in API**)|
| `sodLimit`            | float  | Start of day balance                     |
| `collateralAmount`    | float  | Collateral value                         |
| `receiveableAmount`   | float  | Receivable from sell deliveries          |
| `utilizedAmount`      | float  | Used today                               |
| `blockedPayoutAmount` | float  | Blocked for payout                       |
| `withdrawableBalance` | float  | Can withdraw to bank                     |

> **NOTE**: `availabelBalance` is a typo in Dhan's API (missing 'l'). Use this exact field name in your struct. Do NOT "fix" it to `availableBalance`.

---

## 5. Rust Struct Definitions

```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─── Single Margin Calculator Request ───

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarginCalculatorRequest {
    pub dhan_client_id: String,
    pub exchange_segment: String,
    pub transaction_type: String,
    pub quantity: i32,
    pub product_type: String,
    pub security_id: String,        // STRING, not integer
    pub price: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_price: Option<f64>,
}

// ─── Single Margin Calculator Response ───

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarginCalculatorResponse {
    pub total_margin: f64,
    pub span_margin: f64,
    pub exposure_margin: f64,
    pub available_balance: f64,    // NOTE: different spelling from fundlimit!
    pub variable_margin: f64,
    pub insufficient_balance: f64,
    pub brokerage: f64,
    pub leverage: String,          // STRING "4.00", NOT float
}

// ─── Multi Margin Calculator Request ───

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MultiMarginRequest {
    pub include_position: bool,
    pub include_orders: bool,
    pub scripts: Vec<MarginScript>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarginScript {
    pub exchange_segment: String,
    pub transaction_type: String,
    pub quantity: i32,
    pub product_type: String,
    pub security_id: String,
    pub price: f64,
}

// ─── Multi Margin Response (snake_case!) ───

#[derive(Debug, Deserialize)]
pub struct MultiMarginResponse {
    pub total_margin: String,        // STRING, not float
    pub span_margin: String,
    pub exposure_margin: String,
    pub equity_margin: String,
    pub fo_margin: String,
    pub commodity_margin: String,
    pub currency: String,
    pub hedge_benefit: String,
}

// ─── Fund Limit Response ───

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FundLimitResponse {
    pub dhan_client_id: String,
    #[serde(rename = "availabelBalance")]  // Dhan API typo — do NOT fix
    pub availabel_balance: f64,
    pub sod_limit: f64,
    pub collateral_amount: f64,
    pub receiveable_amount: f64,
    pub utilized_amount: f64,
    pub blocked_payout_amount: f64,
    pub withdrawable_balance: f64,
}
```

---

## 6. Critical Implementation Notes

1. **`availabelBalance` typo** — Dhan's API response spells it wrong. Use exact field name. Do NOT "fix" it.

2. **`leverage` is a STRING** — `"4.00"`, not a float. Parse explicitly if needed for calculations.

3. **Single vs Multi response naming** — Single margin uses camelCase (`totalMargin`), multi uses snake_case (`total_margin`). Different serde attributes needed.

4. **Single vs Multi response types** — Single margin returns floats, multi returns strings. Different deserialization needed.

5. **Margin values are indicative and session-scoped** — valid only for the current trading session. Recalculate before each order.

6. **Pre-trade margin check** — call margin calculator before placing orders. If `insufficientBalance > 0`, do NOT place the order.

7. **`availableBalance` vs `availabelBalance`** — Single margin response uses `availableBalance` (correct spelling). Fund limit uses `availabelBalance` (typo). These are DIFFERENT fields in different responses with different spellings.
