# Dhan V2 Portfolio & Positions — Complete Reference

> **Source**: https://dhanhq.co/docs/v2/portfolio/
> **Extracted**: 2026-03-13
> **Related**: `08-annexure-enums.md`

---

## 1. Overview

| Method | Endpoint              | Description                              |
|--------|-----------------------|------------------------------------------|
| GET    | `/holdings`           | Demat holdings (T1 + delivered)          |
| GET    | `/positions`          | Open positions for the day               |
| POST   | `/positions/convert`  | Convert intraday ↔ delivery              |
| DELETE | `/positions`          | Exit all positions + cancel all orders   |

---

## 2. Holdings

```
GET https://api.dhan.co/v2/holdings
Headers: access-token
```

```json
[{
    "exchange": "ALL",
    "tradingSymbol": "HDFC",
    "securityId": "1330",
    "isin": "INE001A01036",
    "totalQty": 1000,
    "dpQty": 1000,
    "t1Qty": 0,
    "mtf_tq_qty": 0,
    "mtf_qty": 0,
    "availableQty": 1000,
    "collateralQty": 0,
    "avgCostPrice": 2655.0,
    "lastTradedPrice": 2780.5
}]
```

| Field           | Type   | Description                               |
|-----------------|--------|-------------------------------------------|
| `totalQty`        | int    | Total holdings                            |
| `dpQty`           | int    | Delivered in demat                        |
| `t1Qty`           | int    | Pending delivery (T+1)                    |
| `mtf_tq_qty`      | int    | MTF T+1 quantity (**snake_case**, not camelCase) |
| `mtf_qty`         | int    | MTF (Margin Trading Facility) quantity (**snake_case**) |
| `availableQty`    | int    | Available for sell/pledge                 |
| `collateralQty`   | int    | Pledged as collateral                     |
| `avgCostPrice`    | float  | Average buy price                         |
| `lastTradedPrice` | float  | Current market price (LTP)                |

---

## 3. Positions

```
GET https://api.dhan.co/v2/positions
Headers: access-token
```

```json
[{
    "dhanClientId": "1000000009",
    "tradingSymbol": "TCS",
    "securityId": "11536",
    "positionType": "LONG",
    "exchangeSegment": "NSE_EQ",
    "productType": "CNC",
    "buyAvg": 3345.8,
    "buyQty": 40,
    "costPrice": 3215.0,
    "sellAvg": 0.0,
    "sellQty": 0,
    "netQty": 40,
    "realizedProfit": 0.0,
    "unrealizedProfit": 6122.0,
    "rbiReferenceRate": 0.0,
    "multiplier": 1,
    "carryForwardBuyQty": 0,
    "carryForwardSellQty": 0,
    "carryForwardBuyValue": 0.0,
    "carryForwardSellValue": 0.0,
    "dayBuyQty": 40,
    "daySellQty": 0,
    "dayBuyValue": 133832.0,
    "daySellValue": 0.0,
    "drvExpiryDate": "0001-01-01",
    "drvOptionType": null,
    "drvStrikePrice": 0.0,
    "crossCurrency": false
}]
```

| Field                  | Type   | Description                                    |
|------------------------|--------|------------------------------------------------|
| `positionType`         | string | `LONG`, `SHORT`, `CLOSED`                      |
| `buyAvg` / `sellAvg`   | float  | Average price (mark to market)                 |
| `costPrice`            | float  | Actual cost price                              |
| `netQty`               | int    | buyQty − sellQty                               |
| `realizedProfit`       | float  | Booked P&L                                     |
| `unrealizedProfit`     | float  | Open P&L                                       |
| `carryForward*`        | —      | F&O carried positions from previous sessions   |
| `day*`                 | —      | Today's intraday activity                      |
| `rbiReferenceRate`     | float  | RBI reference rate (currency pairs)            |
| `multiplier`           | int    | For currency/commodity contracts               |

---

## 4. Convert Position

```
POST https://api.dhan.co/v2/positions/convert
Headers: access-token
```

```json
{
    "dhanClientId": "1000000009",
    "fromProductType": "INTRADAY",
    "exchangeSegment": "NSE_EQ",
    "positionType": "LONG",
    "securityId": "11536",
    "tradingSymbol": "",
    "convertQty": "40",
    "toProductType": "CNC"
}
```

Response: `202 Accepted`

> **SDK Note**: The Python SDK sends `convertQty` as an integer, while the official docs show it as a string `"40"`. The Dhan API likely accepts both. Our implementation should use the string format per official documentation.

> **Note**: The Traders Control API uses `"DELIVERY"` as a product type value (in the `productType` array for P&L exit), which is different from `"CNC"` used in order placement. These refer to the same concept but use different string values depending on the API.

---

## 5. Exit All Positions

```
DELETE https://api.dhan.co/v2/positions
Headers: access-token
```

Exits all open positions. Response: `{ "status": "SUCCESS", "message": "All orders and positions exited successfully" }`

> **Ambiguity in Dhan docs**: The API description says "only squares off open positions and does not cancel pending orders", but the v2.5 release notes say "close all open positions and open orders", and the response message mentions "All orders and positions". Treat conservatively: assume it MAY cancel pending orders too.

---

## 6. Rust Structs

```rust
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Holding {
    pub exchange: String,
    pub trading_symbol: String,
    pub security_id: String,
    pub isin: String,
    pub total_qty: i32,
    pub dp_qty: i32,
    pub t1_qty: i32,
    #[serde(rename = "mtf_tq_qty")]  // NOTE: snake_case — inconsistent with other fields
    pub mtf_tq_qty: i32,
    #[serde(rename = "mtf_qty")]     // NOTE: snake_case — inconsistent with other fields
    pub mtf_qty: i32,
    pub available_qty: i32,
    pub collateral_qty: i32,
    pub avg_cost_price: f64,
    pub last_traded_price: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub dhan_client_id: String,
    pub trading_symbol: String,
    pub security_id: String,
    pub position_type: String,
    pub exchange_segment: String,
    pub product_type: String,
    pub buy_avg: f64,
    pub buy_qty: i32,
    pub cost_price: f64,
    pub sell_avg: f64,
    pub sell_qty: i32,
    pub net_qty: i32,
    pub realized_profit: f64,
    pub unrealized_profit: f64,
    pub rbi_reference_rate: f64,
    pub multiplier: i32,
    pub carry_forward_buy_qty: i32,
    pub carry_forward_sell_qty: i32,
    pub carry_forward_buy_value: f64,
    pub carry_forward_sell_value: f64,
    pub day_buy_qty: i32,
    pub day_sell_qty: i32,
    pub day_buy_value: f64,
    pub day_sell_value: f64,
    pub drv_expiry_date: Option<String>,
    pub drv_option_type: Option<String>,
    pub drv_strike_price: Option<f64>,
    pub cross_currency: bool,
}
```
