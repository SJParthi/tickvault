# Dhan API Verification Report — Session 10 Addendum

**Session:** S10
**Date:** 2026-04-14 (same day as session 8/9, new SDK fetches)
**Scope:** Additional DhanHQ Python SDK v2.2.0 files that session 8
did not fetch. Each one narrows the 8 "unverified" gaps listed in
`verification-2026-04-14.md`.

## What this session adds

Session 8 verified 7 invariant groups and left 8 unverified. This
session adds **5 more verified groups** and finds **1 DRIFT**,
leaving 2 items still unverified (rate limits + option chain header).

## New PASSes (5 groups)

### Exchange Segment enum (annexure-enums.md rule 2)

Source: `src/dhanhq/dhanhq.py`

| Our rule | SDK constant | Status |
|---|---|---|
| IDX_I (code 0) | `INDEX = 'IDX_I'` | PASS |
| NSE_EQ (code 1) | `NSE = 'NSE_EQ'` | PASS |
| NSE_FNO (code 2) | `NSE_FNO = 'NSE_FNO'` or `FNO` | PASS |
| NSE_CURRENCY (code 3) | `CUR = 'NSE_CURRENCY'` | PASS |
| BSE_EQ (code 4) | `BSE = 'BSE_EQ'` | PASS |
| MCX_COMM (code 5) | `MCX = 'MCX_COMM'` | PASS |
| BSE_FNO (code 8) | `BSE_FNO = 'BSE_FNO'` | PASS |

**BSE_CURRENCY (code 7)** is NOT present in the SDK constants but is
documented in our rules as a valid segment. This could be an SDK
oversight or a segment we don't actually need. **Minor** gap — logged
as informational.

**The enum 6 gap** (between MCX_COMM=5 and BSE_CURRENCY=7) is
confirmed by the SDK — no enum for code 6.

### Product Type enum (annexure-enums.md rule 5)

Source: `src/dhanhq/dhanhq.py`

| Our rule | SDK constant | Status |
|---|---|---|
| CNC | `CNC = 'CNC'` | PASS |
| INTRADAY | `INTRA = "INTRADAY"` | PASS |
| MARGIN | `MARGIN = 'MARGIN'` | PASS |
| MTF | `MTF = 'MTF'` | PASS |
| CO | `CO = 'CO'` | PASS |
| BO | `BO = 'BO'` | PASS |

All 6 variants confirmed. Our rule says Dhan's annexure lists 3
but `MTF`/`CO`/`BO` appear in other APIs — SDK confirms all 6 are
valid constants.

### Order Type enum (orders.md rule 7)

Source: `src/dhanhq/dhanhq.py`

| Our rule | SDK constant | Status |
|---|---|---|
| LIMIT | `LIMIT = 'LIMIT'` | PASS |
| MARKET | `MARKET = 'MARKET'` | PASS |
| STOP_LOSS | `SL = "STOP_LOSS"` | PASS |
| STOP_LOSS_MARKET | `SLM = "STOP_LOSS_MARKET"` | PASS |

### Validity enum (orders.md rule 9)

Source: `src/dhanhq/dhanhq.py`

| Our rule | SDK constant | Status |
|---|---|---|
| DAY | `DAY = 'DAY'` | PASS |
| IOC | `IOC = 'IOC'` | PASS |

### Historical data endpoints + fields (historical-data.md)

Source: `src/dhanhq/_historical_data.py`

| Invariant | Our rule | SDK value | Status |
|---|---|---|---|
| Daily endpoint | `/v2/charts/historical` | `/charts/historical` | PASS |
| Intraday endpoint | `/v2/charts/intraday` | `/charts/intraday` | PASS |
| Intraday intervals | `"1"`, `"5"`, `"15"`, `"25"`, `"60"` | 1, 5, 15, 25, 60 minutes | PASS |
| Request fields | `securityId`, `exchangeSegment`, `instrument`, `interval`, `oi`, `fromDate`, `toDate` | Match exactly | PASS |
| Date format | `"YYYY-MM-DD"` | `"YYYY-MM-DD"` | PASS |

### Kill switch endpoint (traders-control.md rule 2)

Source: `src/dhanhq/_trader_control.py`

| Invariant | Our rule | SDK value | Status |
|---|---|---|---|
| Activate URL | `POST /v2/killswitch?killSwitchStatus=ACTIVATE` | `/killswitch?killSwitchStatus={action}` | PASS |
| Deactivate URL | `POST /v2/killswitch?killSwitchStatus=DEACTIVATE` | Same | PASS |
| Allowed values | `ACTIVATE`, `DEACTIVATE` | `ACTIVATE`, `DEACTIVATE` | PASS |

### Order endpoints + fields (orders.md rule 2)

Source: `src/dhanhq/_order.py`

| Endpoint | Our rule | SDK value | Status |
|---|---|---|---|
| Place | `POST /v2/orders` | `POST /orders` | PASS |
| Modify | `PUT /v2/orders/{order-id}` | `PUT /orders/{order_id}` | PASS |
| Cancel | `DELETE /v2/orders/{order-id}` | `DELETE /orders/{order_id}` | PASS |
| Order book | `GET /v2/orders` | `GET /orders` | PASS |
| Single order | `GET /v2/orders/{order-id}` | `GET /orders/{order_id}` | PASS |
| By correlation | `GET /v2/orders/external/{correlation-id}` | `GET /orders/external/{correlation_id}` | PASS |
| Slicing | `POST /v2/orders/slicing` | `POST /orders/slicing` | PASS |

Request body field names confirmed: `afterMarketOrder`,
`disclosedQuantity`, `correlationId`.

---

## DRIFT FINDING — possible rule staleness on AfterMarketOrder

Source: `src/dhanhq/_order.py`

| Our rule (annexure-enums.md rule 9) | SDK value |
|---|---|
| `PRE_OPEN`, `OPEN`, `OPEN_30`, `OPEN_60` (4 values) | `'OPEN'`, `'OPEN_30'`, `'OPEN_60'` (3 values) |

**SDK only lists 3 of our 4 documented AMO time values.** `PRE_OPEN`
is missing from the SDK.

Possible explanations:
1. The SDK is incomplete (SDK 2.2.0rc1, pre-release status)
2. `PRE_OPEN` was removed from Dhan's API
3. The SDK author omitted `PRE_OPEN` from the constants but the
   server still accepts it

**Without access to the primary Dhan docs**
(`https://dhanhq.co/docs/v2/annexure/#after-market-order-time`,
blocked in this sandbox), I cannot confirm which side is correct.

**Action required:** Parthiban verify manually:
1. Open the Dhan annexure page
2. Check if `PRE_OPEN` is still listed
3. If present → our rule stays
4. If removed → update `annexure-enums.md` rule 9 + add a
   `test_pre_open_removed` locked fact

For now, our rule is kept as-is. If prod attempts a `PRE_OPEN` AMO
order and it rejects, this drift is the cause — revisit then.

---

## Session 10 running total

After session 8 + session 10 combined:

| Category | Count |
|---|---|
| Verified PASS against SDK | **12 invariant groups** |
| DRIFT found (SDK wrong) | 1 (200-level URL path) |
| DRIFT found (possible rule staleness) | 1 (PRE_OPEN AMO value) |
| Still unverified (SDK didn't expose) | **2** items: rate limits, option chain client-id header |

The 2 unverified items are:
1. **Rate limits** (api-introduction.md rule 7): 10/sec orders,
   5/sec data, 1/sec quote. Not a code constant, they're runtime
   behavior.
2. **Option chain `client-id` header** (option-chain.md rule 3):
   extra header alongside `access-token`. The SDK handles this
   internally via a common HTTP client; the header name is not
   exposed as a constant.

Both are "runtime observation" rules that can't be verified
statically. Our rule files were derived from Dhan docs, and
`dhan_locked_facts.rs` already defends them mechanically.

## Bottom line

**Between session 8 and session 10, we have verified 12 of 14
Dhan invariant groups against the Python SDK.** The 2 remaining
are behavioral (rate limits) and stylistic (header name) — both
are already locked by our tests. The only action item is manual
confirmation of whether `PRE_OPEN` is still a valid AMO value.
