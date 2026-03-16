# Dhan V2 Market Quote REST API — Complete Reference

> **Source**: https://dhanhq.co/docs/v2/market-quote/
> **Extracted**: 2026-03-13
> **Related**: `08-annexure-enums.md`, `09-instrument-master.md`

---

## 1. Overview

REST-based snapshot market data. Three modes: LTP, OHLC, and Full Quote (with depth). Up to **1000 instruments** per request, rate limit **1 request/second**.

**All three endpoints require both `access-token` AND `client-id` headers.**

| Method | Endpoint             | Data                                |
|--------|----------------------|-------------------------------------|
| POST   | `/marketfeed/ltp`    | Last Traded Price only              |
| POST   | `/marketfeed/ohlc`   | LTP + OHLC                         |
| POST   | `/marketfeed/quote`  | LTP + OHLC + Depth + OI + Volume   |

---

## 2. Common Headers

```
access-token: JWT
client-id: 1000000001
Content-Type: application/json
Accept: application/json
```

> **NOTE**: `client-id` header is required for Market Quote APIs. This is an extra header not needed by most other APIs.

---

## 3. Request Structure (same for all three)

```json
{
    "NSE_EQ": [11536],
    "NSE_FNO": [49081, 49082]
}
```

Keys are exchange segment string enums. Values are arrays of SecurityId integers.

---

## 4. Ticker Data (`/marketfeed/ltp`)

```
POST https://api.dhan.co/v2/marketfeed/ltp
```

**Response:**
```json
{
    "data": {
        "NSE_EQ": {
            "11536": {
                "last_price": 4520
            }
        }
    },
    "status": "success"
}
```

| Field        | Type  | Description |
|--------------|-------|-------------|
| `last_price` | float | LTP         |

---

## 5. OHLC Data (`/marketfeed/ohlc`)

```
POST https://api.dhan.co/v2/marketfeed/ohlc
```

**Response:**
```json
{
    "data": {
        "NSE_EQ": {
            "11536": {
                "last_price": 4525.55,
                "ohlc": {
                    "open": 4521.45,
                    "close": 4507.85,
                    "high": 4530,
                    "low": 4500
                }
            }
        }
    },
    "status": "success"
}
```

| Field        | Type  | Description                 |
|--------------|-------|-----------------------------|
| `last_price` | float | LTP                         |
| `ohlc.open`  | float | Day open                    |
| `ohlc.close` | float | Previous close              |
| `ohlc.high`  | float | Day high                    |
| `ohlc.low`   | float | Day low                     |

---

## 6. Full Quote / Market Depth (`/marketfeed/quote`)

```
POST https://api.dhan.co/v2/marketfeed/quote
```

**Response:**
```json
{
    "data": {
        "NSE_FNO": {
            "49081": {
                "average_price": 0,
                "buy_quantity": 1825,
                "sell_quantity": 0,
                "depth": {
                    "buy": [
                        { "quantity": 1800, "orders": 1, "price": 77 },
                        { "quantity": 25, "orders": 1, "price": 50 },
                        ...
                    ],
                    "sell": [
                        { "quantity": 0, "orders": 0, "price": 0 },
                        ...
                    ]
                },
                "last_price": 368.15,
                "last_quantity": 0,
                "last_trade_time": "01/01/1980 00:00:00",
                "lower_circuit_limit": 48.25,
                "upper_circuit_limit": 510.85,
                "net_change": 0,
                "ohlc": { "open": 0, "close": 368.15, "high": 0, "low": 0 },
                "oi": 0,
                "oi_day_high": 0,
                "oi_day_low": 0,
                "volume": 0
            }
        }
    },
    "status": "success"
}
```

| Field                  | Type   | Description                                      |
|------------------------|--------|--------------------------------------------------|
| `last_price`           | float  | LTP                                              |
| `last_quantity`        | int    | Last traded quantity                              |
| `last_trade_time`      | string | Last trade time (string format, NOT epoch)        |
| `average_price`        | float  | VWAP of the day                                  |
| `buy_quantity`         | int    | Total buy order quantity pending                 |
| `sell_quantity`        | int    | Total sell order quantity pending                 |
| `volume`              | int    | Total traded volume                               |
| `net_change`          | float  | Absolute change from prev close                   |
| `lower_circuit_limit` | float  | Lower circuit                                     |
| `upper_circuit_limit` | float  | Upper circuit                                     |
| `oi`                  | int    | Open Interest (Derivatives)                       |
| `oi_day_high`         | int    | Highest OI today (NSE_FNO only)                   |
| `oi_day_low`          | int    | Lowest OI today (NSE_FNO only)                    |
| `ohlc.*`              | float  | Day OHLC                                          |
| `depth.buy[].price`   | float  | Bid price at depth level                          |
| `depth.buy[].quantity` | int   | Bid quantity                                      |
| `depth.buy[].orders`  | int    | Number of buy orders                              |
| `depth.sell[].*`      | —      | Same structure for ask side                       |

---

## 7. Rust Struct Definitions

```rust
use serde::Deserialize;
use std::collections::HashMap;

// ─── Request ───
// Request body is: HashMap<String, Vec<u64>>
// e.g., { "NSE_EQ": [11536], "NSE_FNO": [49081] }

pub type MarketQuoteRequest = HashMap<String, Vec<u64>>;

// ─── LTP Response ───

#[derive(Debug, Deserialize)]
pub struct LtpResponse {
    pub data: HashMap<String, HashMap<String, LtpData>>,
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct LtpData {
    pub last_price: f64,
}

// ─── OHLC Response ───

#[derive(Debug, Deserialize)]
pub struct OhlcResponse {
    pub data: HashMap<String, HashMap<String, OhlcData>>,
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct OhlcData {
    pub last_price: f64,
    pub ohlc: OhlcValues,
}

#[derive(Debug, Deserialize)]
pub struct OhlcValues {
    pub open: f64,
    pub close: f64,
    pub high: f64,
    pub low: f64,
}

// ─── Full Quote Response ───

#[derive(Debug, Deserialize)]
pub struct QuoteResponse {
    pub data: HashMap<String, HashMap<String, QuoteData>>,
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct QuoteData {
    pub last_price: f64,
    pub last_quantity: i32,
    pub last_trade_time: String,
    pub average_price: f64,
    pub buy_quantity: i64,
    pub sell_quantity: i64,
    pub volume: i64,
    pub net_change: f64,
    pub lower_circuit_limit: f64,
    pub upper_circuit_limit: f64,
    pub oi: i64,
    pub oi_day_high: i64,
    pub oi_day_low: i64,
    pub ohlc: OhlcValues,
    pub depth: DepthData,
}

#[derive(Debug, Deserialize)]
pub struct DepthData {
    pub buy: Vec<DepthLevel>,
    pub sell: Vec<DepthLevel>,
}

#[derive(Debug, Deserialize)]
pub struct DepthLevel {
    pub quantity: i64,
    pub orders: i32,
    pub price: f64,
}
```

---

## 8. Critical Implementation Notes

1. **`client-id` header required** — unlike most other APIs that only need `access-token`. Missing this returns auth error.

2. **Rate limit: 1 request per second** — much stricter than other Data APIs (5/sec). But unlimited per minute/hour/day.

3. **Up to 1000 instruments per request** — efficient for bulk snapshots.

4. **`last_trade_time` is a string**, NOT epoch — format varies (seen `DD/MM/YYYY HH:MM:SS`). May show `01/01/1980 00:00:00` for untraded instruments.

5. **This is REST, not WebSocket** — use for point-in-time snapshots. For continuous data, use Live Market Feed WebSocket.

6. **Response is nested HashMap** — `data[segment][security_id]`. SecurityId is a string key in response even though request sends integers.

7. **Depth is 5 levels** — same as Live Market Feed Full mode, but in JSON not binary.

8. **Use LTP mode when you only need price** — saves bandwidth and parsing overhead vs full quote.

9. **Response field naming is snake_case** — `last_price`, `buy_quantity`, `oi_day_high` etc. This is DIFFERENT from most other Dhan APIs which use camelCase. The Rust structs above use explicit snake_case field names (no `rename_all` needed since Rust is already snake_case).

10. **Go SDK uses int32 for quantities/volumes** — `buy_quantity`, `sell_quantity`, `volume`, `oi`, `last_quantity` are int32 in the Go SDK. Our Rust structs use i64 for volume/quantity fields as a safety margin against overflow on high-volume instruments.
