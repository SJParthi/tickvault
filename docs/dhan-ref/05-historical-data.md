# Dhan V2 Historical Data REST API — Complete Reference

> **Source**: https://dhanhq.co/docs/v2/historical-data/
> **Extracted**: 2026-03-13
> **Related**: `08-annexure-enums.md` (enums), `09-instrument-master.md` (SecurityId lookup)

---

## 1. Overview

Historical candle data (OHLCV + OI) via REST API. Two endpoints: daily and intraday.

| Method | Endpoint            | Description                    |
|--------|---------------------|--------------------------------|
| POST   | `/charts/historical`| Daily OHLC candles             |
| POST   | `/charts/intraday`  | 1/5/15/25/60 min OHLC candles  |

---

## 2. Daily Historical Data

Data available back to inception date of any scrip.

```
POST https://api.dhan.co/v2/charts/historical
Headers: Content-Type: application/json, access-token: JWT
```

### Request

```json
{
    "securityId": "1333",
    "exchangeSegment": "NSE_EQ",
    "instrument": "EQUITY",
    "expiryCode": 0,
    "oi": false,
    "fromDate": "2022-01-08",
    "toDate": "2022-02-08"
}
```

| Field             | Type        | Required | Description                                              |
|-------------------|-------------|----------|----------------------------------------------------------|
| `securityId`      | string      | Yes      | From instrument master CSV                               |
| `exchangeSegment` | enum string | Yes      | See 08-annexure Section 1                                |
| `instrument`      | enum string | Yes      | See 08-annexure Section 6                                |
| `expiryCode`      | enum int    | No       | 0=Current, 1=Next, 2=Far (see 08-annexure Section 7)    |
| `oi`              | boolean     | No       | Include Open Interest (F&O only)                         |
| `fromDate`        | string      | Yes      | Start date `YYYY-MM-DD`                                  |
| `toDate`          | string      | Yes      | End date `YYYY-MM-DD` (**non-inclusive**)                 |

### Response — Columnar Parallel Arrays

```json
{
    "open": [3978, 3856, 3925, ...],
    "high": [3978, 3925, 3929, ...],
    "low": [3861, 3856, 3836.55, ...],
    "close": [3879.85, 3915.9, 3859.9, ...],
    "volume": [3937092, 1906106, 3203744, ...],
    "timestamp": [1326220200, 1326306600, 1326393000, ...],
    "open_interest": [0, 0, 0, ...]
}
```

> **CRITICAL**: Response is **columnar parallel arrays**, NOT array of candle objects. Index `i` across all arrays = one candle. This is unusual — most APIs return rows.

---

## 3. Intraday Historical Data

OHLCV for 1/5/15/25/60 minute candles. Last **5 years** of data. Max **90 days** per request.

```
POST https://api.dhan.co/v2/charts/intraday
Headers: Content-Type: application/json, access-token: JWT
```

### Request

```json
{
    "securityId": "1333",
    "exchangeSegment": "NSE_EQ",
    "instrument": "EQUITY",
    "interval": "1",
    "oi": false,
    "fromDate": "2024-09-11 09:30:00",
    "toDate": "2024-09-15 13:00:00"
}
```

| Field             | Type        | Required | Description                                |
|-------------------|-------------|----------|--------------------------------------------|
| `securityId`      | string      | Yes      | From instrument master CSV                 |
| `exchangeSegment` | enum string | Yes      | See 08-annexure Section 1                  |
| `instrument`      | enum string | Yes      | See 08-annexure Section 6                  |
| `interval`        | string      | Yes      | `"1"`, `"5"`, `"15"`, `"25"`, or `"60"` (minutes) |
| `oi`              | boolean     | No       | Include Open Interest                      |
| `fromDate`        | string      | Yes      | `YYYY-MM-DD HH:MM:SS`                     |
| `toDate`          | string      | Yes      | `YYYY-MM-DD HH:MM:SS`                     |

### Response

Same columnar parallel array format as daily data.

> **NOTE**: Max 90 days per request. Store data locally for long-term analysis.

---

## 4. Timestamp Format

**Standard UNIX epoch (UTC-based, seconds since 1970-01-01 00:00 UTC).**

Verified examples:
- Daily: `1326220200` = UTC 2012-01-10 18:30:00 = **IST 2012-01-11 00:00:00** (midnight IST)
- Intraday: `1328845500` = UTC 2012-02-10 03:45:00 = **IST 2012-02-10 09:15:00** (market open)

**You MUST add +5:30 to display IST.** See 08-annexure Section 12 for full verification.

```rust
use chrono::{FixedOffset, TimeZone};
let ist = FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap();
let dt = ist.timestamp_opt(ts as i64, 0).unwrap();
```

---

## 5. Rust Struct Definitions

```rust
use serde::{Deserialize, Serialize};

// ─── Request ───

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoricalDailyRequest {
    pub security_id: String,
    pub exchange_segment: String,
    pub instrument: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiry_code: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oi: Option<bool>,
    pub from_date: String,       // YYYY-MM-DD
    pub to_date: String,         // YYYY-MM-DD (non-inclusive)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoricalIntradayRequest {
    pub security_id: String,
    pub exchange_segment: String,
    pub instrument: String,
    pub interval: String,        // "1", "5", "15", "25", "60"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oi: Option<bool>,
    pub from_date: String,       // YYYY-MM-DD HH:MM:SS
    pub to_date: String,         // YYYY-MM-DD HH:MM:SS
}

// ─── Response (Columnar) ───

#[derive(Debug, Deserialize)]
pub struct HistoricalDataResponse {
    pub open: Vec<f64>,
    pub high: Vec<f64>,
    pub low: Vec<f64>,
    pub close: Vec<f64>,
    pub volume: Vec<i64>,
    pub timestamp: Vec<i64>,         // UNIX epoch UTC
    pub open_interest: Vec<i64>,
}

// ─── Converted Candle (Row-based) ───

#[derive(Debug, Clone)]
pub struct Candle {
    pub timestamp: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
    pub open_interest: i64,
}

impl HistoricalDataResponse {
    /// Convert columnar response to row-based candles
    pub fn to_candles(&self) -> Vec<Candle> {
        (0..self.timestamp.len())
            .map(|i| Candle {
                timestamp: self.timestamp[i],
                open: self.open[i],
                high: self.high[i],
                low: self.low[i],
                close: self.close[i],
                volume: self.volume[i],
                open_interest: self.open_interest.get(i).copied().unwrap_or(0),
            })
            .collect()
    }
}
```

---

## 6. Critical Implementation Notes

1. **Columnar parallel arrays** — response arrays are indexed in parallel. `open[0]`, `high[0]`, `close[0]`, `timestamp[0]` all refer to the same candle.

2. **`toDate` is non-inclusive for daily** — requesting `fromDate: "2022-01-08"` to `toDate: "2022-02-08"` gives data from Jan 8 to Feb 7.

3. **Intraday max 90 days per request** — for longer periods, make multiple requests with sliding date windows.

4. **Rate limit: 5 requests/second** — for Data APIs. 100K requests per day.

5. **Timestamps are UNIX epoch UTC** — add +5:30 for IST. Daily candles are stamped at IST midnight (18:30 UTC).

6. **OI array may be all zeros** — for equity instruments, `open_interest` is always `[0,0,0,...]`. Only meaningful for F&O.

7. **Intraday first candle may not be 09:15** — pre-market trades can generate candles before market open.

8. **Store data locally** — Dhan recommends storing historical data at your end for day-to-day analysis. Don't re-fetch the same date ranges repeatedly.

9. **Expired Options Data** — A separate endpoint `POST /v2/charts/rollingoption` exists for fetching historical data of expired option contracts using ATM±strike syntax. See `docs/dhan-ref/05b-expired-options-data.md` if documented separately.

10. **`interval` is a STRING** — despite looking numeric, the API expects `"1"` not `1`. The Python SDK accepts an integer but this may be auto-converted. Always serialize as string in Rust.
