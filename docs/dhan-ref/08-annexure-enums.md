# Dhan V2 Annexure — Enums & Reference Values

> **Source**: https://dhanhq.co/docs/v2/annexure/
> **Extracted**: 2026-03-13
> **Purpose**: Shared reference — load alongside any other Dhan API doc in Claude Code

---

## 1. Exchange Segment

Used in TWO different ways:
- **JSON requests** (subscribe, order placement): Use the **string** attribute (e.g., `"NSE_EQ"`)
- **Binary response header** (byte 4 in WebSocket): Use the **numeric enum** value

| Attribute       | Exchange | Segment              | Numeric Enum |
|-----------------|----------|----------------------|--------------|
| `IDX_I`         | Index    | Index Value          | `0`          |
| `NSE_EQ`        | NSE      | Equity Cash          | `1`          |
| `NSE_FNO`       | NSE      | Futures & Options    | `2`          |
| `NSE_CURRENCY`  | NSE      | Currency             | `3`          |
| `BSE_EQ`        | BSE      | Equity Cash          | `4`          |
| `MCX_COMM`      | MCX      | Commodity            | `5`          |
| `BSE_CURRENCY`  | BSE      | Currency             | `7`          |
| `BSE_FNO`       | BSE      | Futures & Options    | `8`          |

> **CRITICAL**: There is NO enum `6`. Gap between MCX_COMM(5) and BSE_CURRENCY(7).

### Rust Enum

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExchangeSegment {
    IdxI        = 0,
    NseEq       = 1,
    NseFno      = 2,
    NseCurrency = 3,
    BseEq       = 4,
    McxComm     = 5,
    BseCurrency = 7,
    BseFno      = 8,
}

impl ExchangeSegment {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::IdxI),
            1 => Some(Self::NseEq),
            2 => Some(Self::NseFno),
            3 => Some(Self::NseCurrency),
            4 => Some(Self::BseEq),
            5 => Some(Self::McxComm),
            7 => Some(Self::BseCurrency),
            8 => Some(Self::BseFno),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::IdxI        => "IDX_I",
            Self::NseEq       => "NSE_EQ",
            Self::NseFno      => "NSE_FNO",
            Self::NseCurrency => "NSE_CURRENCY",
            Self::BseEq       => "BSE_EQ",
            Self::McxComm     => "MCX_COMM",
            Self::BseCurrency => "BSE_CURRENCY",
            Self::BseFno      => "BSE_FNO",
        }
    }
}
```

---

## 2. Feed Request Codes (JSON subscribe/unsubscribe)

| Code | Action                         |
|------|--------------------------------|
| `11` | Connect Feed                   |
| `12` | Disconnect Feed                |
| `15` | Subscribe — Ticker Packet      |
| `16` | Unsubscribe — Ticker Packet    |
| `17` | Subscribe — Quote Packet       |
| `18` | Unsubscribe — Quote Packet     |
| `21` | Subscribe — Full Packet        |
| `22` | Unsubscribe — Full Packet      |
| `23` | Subscribe — Full Market Depth  |
| `25` | Unsubscribe — Full Market Depth|

> **SDK Note**: Python SDK `marketfeed.py` defines `Depth = 19` as a v1-only depth subscribe code (standalone market depth packet). This is deprecated in v2 and NOT listed above. The SDK rejects it for v2 subscriptions. Also note: the SDK's generic unsubscribe logic (`subscribe_code + 1`) produces code `24` for depth unsubscribe, but the correct code per this annexure is `25`. Our code uses `25`.

---

## 3. Feed Response Codes (Binary Header Byte 1)

| Code | Meaning                |
|------|------------------------|
| `1`  | Index Packet           |
| `2`  | Ticker Packet          |
| `3`  | Market Depth Packet (v1 legacy — deprecated in v2, replaced by Full packet code 8. Python SDK still handles it for backward compat.) |
| `4`  | Quote Packet           |
| `5`  | OI Packet              |
| `6`  | Prev Close Packet      |
| `7`  | Market Status Packet (8 bytes, header only — no additional payload) |
| `8`  | Full Packet            |
| `50` | Feed Disconnect        |

---

## 4. Product Type

| Attribute  | Detail                                  |
|------------|-----------------------------------------|
| `CNC`      | Cash & Carry for equity deliveries      |
| `INTRADAY` | Intraday for Equity, Futures & Options  |
| `MARGIN`   | Carry Forward in Futures & Options      |
| `MTF`      | Margin Trading Facility (appears in Order/Forever Order APIs, not in annexure table) |
| `CO`       | Cover Order — with stop-loss (appears in Order Update WS `Product` field as `"V"`) |
| `BO`       | Bracket Order — with target + stop-loss (appears in Order Update WS `Product` field as `"B"`) |

---

## 5. Order Status

| Attribute      | Detail                                                    |
|----------------|-----------------------------------------------------------|
| `TRANSIT`      | Did not reach the exchange server                         |
| `PENDING`      | Awaiting execution                                        |
| `CLOSED`       | Super Order: both entry and exit orders placed            |
| `TRIGGERED`    | Super Order: Target or Stop Loss leg triggered            |
| `REJECTED`     | Rejected by broker/exchange                               |
| `CANCELLED`    | Cancelled by user                                         |
| `PART_TRADED`  | Partial quantity traded                                   |
| `TRADED`       | Executed successfully                                     |
| `EXPIRED`      | Order expired (Order Book, Forever Orders, Order Update WS — not in original annexure table) |
| `CONFIRM`      | Forever Order specific: active and waiting for trigger condition to be met |

---

## 6. Instrument Types

| Attribute  | Detail                      |
|------------|-----------------------------|
| `INDEX`    | Index                       |
| `FUTIDX`   | Futures of Index            |
| `OPTIDX`   | Options of Index            |
| `EQUITY`   | Equity                      |
| `FUTSTK`   | Futures of Stock            |
| `OPTSTK`   | Options of Stock            |
| `FUTCOM`   | Futures of Commodity        |
| `OPTFUT`   | Options of Commodity Futures|
| `FUTCUR`   | Futures of Currency         |
| `OPTCUR`   | Options of Currency         |

---

## 7. Expiry Code

| Code | Detail                     |
|------|----------------------------|
| `0`  | Current Expiry/Near Expiry |
| `1`  | Next Expiry                |
| `2`  | Far Expiry                 |

> **SDK Note**: Python SDK `_historical_data.py` validates `expiry_code` against `[0, 1, 2, 3]`, accepting a fourth value `3`. The Dhan API documentation only lists 0/1/2. The meaning of `3` is undocumented — avoid using it unless verified.

---

## 8. After Market Order Time

| Attribute  | Detail                                |
|------------|---------------------------------------|
| `PRE_OPEN` | AMO pumped at pre-market session      |
| `OPEN`     | AMO pumped at market open             |
| `OPEN_30`  | AMO pumped 30 min after market open   |
| `OPEN_60`  | AMO pumped 60 min after market open   |

---

## 9. Rate Limits

| Rate Limit  | Order APIs | Data APIs | Quote APIs | Non Trading APIs |
|-------------|-----------|-----------|------------|------------------|
| per second  | 10        | 5         | 1          | 20               |
| per minute  | 250       | —         | Unlimited  | Unlimited        |
| per hour    | 500       | —         | Unlimited  | Unlimited        |
| per day     | 5000      | 100000    | Unlimited  | Unlimited        |

> Order modifications capped at **25 modifications per order**.

---

## 10. Trading API Errors

| Type                   | Code     | Message                                                |
|------------------------|----------|--------------------------------------------------------|
| Invalid Authentication | `DH-901` | Client ID or access token invalid/expired              |
| Invalid Access         | `DH-902` | Not subscribed to Data APIs or no Trading API access   |
| User Account           | `DH-903` | Account issues — check segment activation              |
| Rate Limit             | `DH-904` | Too many requests — throttle API calls                 |
| Input Exception        | `DH-905` | Missing fields, bad parameter values                   |
| Order Error            | `DH-906` | Incorrect order request                                |
| Data Error             | `DH-907` | Incorrect params or no data present                    |
| Internal Server Error  | `DH-908` | Server processing failure (rare)                       |
| Network Error          | `DH-909` | API unable to communicate with backend                 |
| Others                 | `DH-910` | Other errors                                           |

---

## 11. Data API Errors (WebSocket & Data endpoints)

| Code  | Description                                                              |
|-------|--------------------------------------------------------------------------|
| `800` | Internal Server Error                                                    |
| `804` | Requested number of instruments exceeds limit                            |
| `805` | Too many requests/connections — may result in user being blocked         |
| `806` | Data APIs not subscribed                                                 |
| `807` | Access token expired                                                     |
| `808` | Authentication Failed — Client ID or Access Token invalid                |
| `809` | Access token invalid                                                     |
| `810` | Client ID invalid                                                        |
| `811` | Invalid Expiry Date                                                      |
| `812` | Invalid Date Format                                                      |
| `813` | Invalid SecurityId                                                       |
| `814` | Invalid Request                                                          |

---

## 12. Timestamp Format (V2 — all APIs)

All Dhan V2 timestamps are **standard UNIX epoch (seconds since Jan 1, 1970 00:00 UTC)**.

Cross-verified from 4 sources:
- Historical Data daily: `1326220200` = IST 2012-01-11 00:00:00 (midnight IST)
- Historical Data intraday: `1328845500` = IST 09:15:00 (NSE market open)
- Python SDK `marketfeed.py`: `utc_time()` uses `datetime.utcfromtimestamp()`
- Python SDK `dhanhq.py`: `convert_to_date_time()` explicitly adds `+5:30` IST offset

**To get IST: add +5:30 (19800 seconds) or use timezone-aware parsing.**

```rust
use chrono::{FixedOffset, TimeZone};
let ist = FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap();
let dt = ist.timestamp_opt(epoch as i64, 0).unwrap();
```

> Dhan v1 Historical Data used custom epoch from Jan 1, 1980 IST — v2 uses standard UNIX epoch everywhere.

---

## 13. Conditional Trigger Enums

### Comparison Types

| Type                        | What it compares                           |
|-----------------------------|-------------------------------------------|
| `TECHNICAL_WITH_VALUE`      | Technical indicator vs fixed number        |
| `TECHNICAL_WITH_INDICATOR`  | Technical indicator vs another indicator   |
| `TECHNICAL_WITH_CLOSE`      | Technical indicator vs closing price       |
| `PRICE_WITH_VALUE`          | Market price vs fixed value                |

### Indicator Names

SMA: `SMA_5`, `SMA_10`, `SMA_20`, `SMA_50`, `SMA_100`, `SMA_200`
EMA: `EMA_5`, `EMA_10`, `EMA_20`, `EMA_50`, `EMA_100`, `EMA_200`
Bollinger: `BB_UPPER`, `BB_LOWER`
Others: `RSI_14`, `ATR_14`, `STOCHASTIC`, `STOCHRSI_14`, `MACD_26`, `MACD_12`, `MACD_HIST`

### Operators

| Operator             | Description        |
|----------------------|--------------------|
| `CROSSING_UP`        | Crosses above      |
| `CROSSING_DOWN`      | Crosses below      |
| `CROSSING_ANY_SIDE`  | Crosses either side|
| `GREATER_THAN`       | Greater than       |
| `LESS_THAN`          | Less than          |
| `GREATER_THAN_EQUAL` | Greater or equal   |
| `LESS_THAN_EQUAL`    | Less or equal      |
| `EQUAL`              | Equal              |
| `NOT_EQUAL`          | Not equal          |

### Alert Status

| Status      | Description           |
|-------------|-----------------------|
| `ACTIVE`    | Alert currently active|
| `TRIGGERED` | Condition met         |
| `EXPIRED`   | Alert expired         |
| `CANCELLED` | Alert cancelled       |

> **Note**: Conditional trigger order sub-objects use `discQuantity` (abbreviated), while regular orders use `disclosedQuantity`. Use the exact field name for each endpoint.
