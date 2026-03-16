# Dhan V2 Option Chain REST API — Complete Reference

> **Source**: https://dhanhq.co/docs/v2/option-chain/
> **Extracted**: 2026-03-13
> **Related**: `08-annexure-enums.md`, `09-instrument-master.md`

---

## 1. Overview

Full option chain for any underlying across NSE, BSE, MCX. Gives OI, greeks, volume, bid/ask, LTP, IV for all strikes.

| Method | Endpoint                    | Description                     |
|--------|-----------------------------|---------------------------------|
| POST   | `/optionchain`              | Full option chain               |
| POST   | `/optionchain/expirylist`   | Active expiry dates             |

**Rate limit: 1 unique request every 3 seconds** (stricter than standard Data APIs). Multiple different underlyings/expiries can be fetched concurrently within the 3s window.

**Requires both `access-token` AND `client-id` headers.**

---

## 2. Option Chain

```
POST https://api.dhan.co/v2/optionchain
Headers: access-token, client-id, Content-Type: application/json
```

### Request
```json
{
    "UnderlyingScrip": 13,
    "UnderlyingSeg": "IDX_I",
    "Expiry": "2024-10-31"
}
```

| Field            | Type        | Required | Description                                      |
|------------------|-------------|----------|--------------------------------------------------|
| `UnderlyingScrip`| int         | Yes      | SecurityId of underlying (from instrument master) |
| `UnderlyingSeg`  | enum string | Yes      | Exchange segment of underlying                    |
| `Expiry`         | string      | Yes      | Expiry date `YYYY-MM-DD` (from expiry list API)   |

### Response
```json
{
    "data": {
        "last_price": 25642.8,
        "oc": {
            "25650.000000": {
                "ce": {
                    "average_price": 146.99,
                    "greeks": { "delta": 0.53871, "theta": -15.1539, "gamma": 0.00132, "vega": 12.18593 },
                    "implied_volatility": 9.789,
                    "last_price": 134,
                    "oi": 3786445,
                    "previous_close_price": 244.85,
                    "previous_oi": 402220,
                    "previous_volume": 31931705,
                    "security_id": 42528,
                    "top_ask_price": 134,
                    "top_ask_quantity": 1365,
                    "top_bid_price": 133.55,
                    "top_bid_quantity": 1625,
                    "volume": 117567970
                },
                "pe": { ... }
            }
        }
    },
    "status": "success"
}
```

### CE/PE Fields

| Field                  | Type  | Description                                        |
|------------------------|-------|----------------------------------------------------|
| `average_price`        | float | VWAP for the day                                   |
| `greeks.delta`         | float | Change per ₹1 move in underlying                  |
| `greeks.theta`         | float | Time decay per day                                 |
| `greeks.gamma`         | float | Rate of delta change                               |
| `greeks.vega`          | float | Change per 1% IV move                              |
| `implied_volatility`   | float | IV of this strike                                  |
| `last_price`           | float | LTP                                                |
| `oi`                   | int   | Current Open Interest                              |
| `previous_close_price` | float | Previous day close                                 |
| `previous_oi`          | int   | Previous day OI                                    |
| `previous_volume`      | int   | Previous day volume                                |
| `security_id`          | int   | SecurityId of this option contract                 |
| `top_ask_price`        | float | Best ask                                           |
| `top_ask_quantity`     | int   | Quantity at best ask                               |
| `top_bid_price`        | float | Best bid                                           |
| `top_bid_quantity`     | int   | Quantity at best bid                               |
| `volume`               | int   | Today's volume                                     |

---

## 3. Expiry List

```
POST https://api.dhan.co/v2/optionchain/expirylist
Headers: access-token, client-id, Content-Type: application/json
```

### Request
```json
{ "UnderlyingScrip": 13, "UnderlyingSeg": "IDX_I" }
```

### Response
```json
{ "data": ["2024-10-17", "2024-10-24", "2024-10-31", ...], "status": "success" }
```

---

## 4. Rust Structs

```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Serialize)]
pub struct OptionChainRequest {
    #[serde(rename = "UnderlyingScrip")]
    pub underlying_scrip: u64,
    #[serde(rename = "UnderlyingSeg")]
    pub underlying_seg: String,
    #[serde(rename = "Expiry")]
    pub expiry: String,
}

#[derive(Debug, Deserialize)]
pub struct OptionChainResponse {
    pub data: OptionChainData,
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct OptionChainData {
    pub last_price: f64,
    pub oc: HashMap<String, StrikeData>,  // key = strike price as string
}

#[derive(Debug, Deserialize)]
pub struct StrikeData {
    pub ce: Option<OptionData>,
    pub pe: Option<OptionData>,
}

#[derive(Debug, Deserialize)]
pub struct OptionData {
    pub average_price: f64,
    pub greeks: Greeks,
    pub implied_volatility: f64,
    pub last_price: f64,
    pub oi: i64,
    pub previous_close_price: f64,
    pub previous_oi: i64,
    pub previous_volume: i64,
    pub security_id: u64,
    pub top_ask_price: f64,
    pub top_ask_quantity: i64,
    pub top_bid_price: f64,
    pub top_bid_quantity: i64,
    pub volume: i64,
}

#[derive(Debug, Deserialize)]
pub struct Greeks {
    pub delta: f64,
    pub theta: f64,
    pub gamma: f64,
    pub vega: f64,
}

#[derive(Debug, Deserialize)]
pub struct ExpiryListResponse {
    pub data: Vec<String>,
    pub status: String,
}
```

---

## 5. Critical Notes

1. **`client-id` header required** — same as Market Quote API.
2. **Rate limit: 1 req / 3 seconds** — much slower than other Data APIs (5/sec). OI updates slowly, hence the limit.
3. **Strike keys are strings** with decimal (`"25650.000000"`). Parse as f64.
4. **CE or PE may be None** — deep OTM strikes might have no data on one side.
5. **`security_id` in response** — gives you the SecurityId of each option contract directly, no instrument master lookup needed for subscriptions.
6. **Expiry dates from expiry list API** — don't guess expiry dates, fetch them.
