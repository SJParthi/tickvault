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

---

## 2026-07-13 Upstream Update — multi-margin request naming + Multi-leg Margin Calculator — UNVERIFIED-LIVE

Per repo convention this dated section supersedes without rewriting §3 above. Evidence chain:
`verification-2026-07-13.md` §4 flag 4. **No live API call was made — live-probe before any
multi-margin call is trusted.** There is no multi-margin caller in `crates/` today, so this has
zero runtime impact.

1. **The §3 naming ambiguity now tilts to `includeOrder` (singular) + `scripList`.** The earlier
   note (§3, "Our implementation uses `includeOrders` (plural) and `scripts`") is the
   LESS-supported reading as of 2026-07-13:
   - Live-indexed docs summary (search-relayed): the multi-margin request "includes parameters
     like includePosition, **includeOrder**, dhanClientId, and a **scripList**".
   - The OFFICIAL Python SDK — released 2.3.0rc1 sdist, direct PyPI fetch (Verified for the SDK)
     — sends the wire body `{"includePosition": …, "includeOrder": …, "scripList": […]}` to
     `POST /margincalculator/multi` (`src/dhanhq/_funds.py::margin_calculator_multi`).
   The §3 example JSON (`includeOrders` + `scripts`) is retained above as history; any future
   implementation should START from `includeOrder`/`scripList` and confirm by live probe.
2. **Multi-leg Margin Calculator is now documented on the new docs portal:**
   `https://docs.dhanhq.co/api/v2/funds/calculate-multi-margin` (live-indexed). SDK 2.3.0rc1
   (PyPI description verbatim, uploaded 2026-07-07): "Multi-leg Margin Calculator - compute
   combined margin for multiple orders with hedge benefits." Whether that portal page is the
   SAME `/margincalculator/multi` endpoint or a NEW one is **Unknown** — probe target on the
   next live crawl (operator paste list, `verification-2026-07-13.md` §3).
3. **`availabelBalance` typo still live** in the indexed v1 AND v2 funds pages (exact-term
   search hit only those two pages) — the §4/§6 "do NOT fix it" rule stands.

---

## 2026-07-14 Upstream Update (runner-crawled live pages) — multi-margin is now a THREE-artifact split

**Evidence tier: Verified-live.** Raw HTML of `https://dhanhq.co/docs/v2/funds/` (runs 1–3,
sha256 `4244823f` content-identical, latest 2026-07-14T07:58:23Z) + the portal markdown export
`docs.dhanhq.co/markdown/api/v2/funds/calculate-multi-margin.md` (2026-07-14T08:00:01Z) + the
portal OpenAPI yaml (07:58:48Z). Supersedes the 2026-07-13 note's item-2 "whether that portal
page is the SAME endpoint … is Unknown" hedge — run 3 fetched the portal's REAL markdown
content (the SPA-shell limitation applies only to the portal's HTML routes). Full manifest:
`00-COVERAGE-MANIFEST.md`. Still ZERO runtime impact (no multi-margin caller in `crates/`);
live-probe before any caller is written.

1. **The classic funds page is INTERNALLY split on the SAME page:** its curl shows
   `"includeOrder": true` + `scripList` + `dhanClientId` (agreeing with the SDK 2.3.0rc1 wire
   body), while the adjacent Request Structure + param table still show `includeOrders` +
   `scripts` (no dhanClientId). Live-only oddity: the multi curl URL is literally
   `https://api.dhan.co/v2/%20%20/margincalculator/multi` (two encoded spaces in the path —
   a doc bug).
2. **The portal's own two artifacts DISAGREE with each other:** the markdown export says
   `includeOrder` + `scripList[...]` with a camelCase float response and NO `hedge_benefit`;
   the OpenAPI yaml says `includeOrders` + `scripts` with a snake_case all-string response
   INCLUDING `hedge_benefit`. UNRESOLVED between Dhan's own artifacts — **live-probe before
   any caller**. If forced to pick: the rendered markdown + the SDK + the classic curl
   converge on `includeOrder`/`scripList` (the yaml's own x-codeSample body even uses
   `scripts` while its markdown twin uses `scripList` — the yaml smells hand-rolled).
3. **`currency` response-field semantics are now suspect:** the classic live example shows
   `"currency": "INR"` (reads as a currency CODE) while the portal markdown types it
   `currency | float | Currency margin` (a margin amount for the currency segment). Meaning
   Unknown until probed; the §3 table's "Currency segment margin" description can no longer
   be treated as settled.
4. **`NSE_COMM`** appears as a `scripList[0].exchangeSegment` enum value on the portal
   markdown (verbatim: NSE_EQ, NSE_FNO, NSE_COMM, BSE_EQ, BSE_FNO, MCX_COMM — and in the
   yaml's conditional-trigger/multi-order enums) — a documented request-side STRING value
   with NO numeric annexure code on any surface. tickvault never sends it (commodity out of
   scope); recorded for decode awareness.
5. **`availabelBalance` typo re-confirmed live on BOTH surfaces** (classic fundlimit example
   + table; portal get-fund-limits.md; yaml `FundLimit.availabelBalance`); the
   margin-calculator's correctly-spelled `availableBalance` also re-confirmed — the
   two-spellings rule (§6.7) stands, now Verified-live.
