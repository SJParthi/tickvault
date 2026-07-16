# Dhan V2 Conditional Trigger — Complete Reference

> **Source**: https://dhanhq.co/docs/v2/conditional-trigger/
> **Extracted**: 2026-03-13
> **Related**: `08-annexure-enums.md` (Sections on Comparison Type, Indicator Name, Operator, Status)

---

## 1. Overview

Place orders triggered by price or technical indicator conditions. Only Equities and Indices supported.

| Method | Endpoint                       | Description            |
|--------|--------------------------------|------------------------|
| POST   | `/alerts/orders`               | Create trigger         |
| PUT    | `/alerts/orders/{alertId}`     | Modify trigger         |
| DELETE | `/alerts/orders/{alertId}`     | Delete trigger         |
| GET    | `/alerts/orders/{alertId}`     | Get trigger by ID      |
| GET    | `/alerts/orders`               | Get all triggers       |

---

## 2. Place Conditional Trigger

```json
{
    "dhanClientId": "123456789",
    "condition": {
        "comparisonType": "TECHNICAL_WITH_VALUE",
        "exchangeSegment": "NSE_EQ",
        "securityId": "12345",
        "indicatorName": "SMA_5",
        "timeFrame": "DAY",
        "operator": "CROSSING_UP",
        "comparingValue": 250,
        "expDate": "2026-08-24",
        "frequency": "ONCE",
        "userNote": "Price crossing SMA"
    },
    "orders": [{
        "transactionType": "BUY",
        "exchangeSegment": "NSE_EQ",
        "productType": "CNC",
        "orderType": "LIMIT",
        "securityId": "12345",
        "quantity": 10,
        "validity": "DAY",
        "price": "250.00",
        "discQuantity": "0",
        "triggerPrice": "0"
    }]
}
```

### Comparison Types (from 08-annexure)

| Type                        | What it compares                           | Required fields                                    |
|-----------------------------|--------------------------------------------|----------------------------------------------------|
| `TECHNICAL_WITH_VALUE`      | Indicator vs fixed number                  | `indicatorName`, `operator`, `timeFrame`, `comparingValue` |
| `TECHNICAL_WITH_INDICATOR`  | Indicator vs another indicator             | `indicatorName`, `operator`, `timeFrame`, `comparingIndicatorName` |
| `TECHNICAL_WITH_CLOSE`      | Indicator vs closing price                 | `indicatorName`, `operator`, `timeFrame`           |
| `PRICE_WITH_VALUE`          | Market price vs fixed value                | `operator`, `comparingValue`                       |

### Available Indicators

SMA (5/10/20/50/100/200), EMA (5/10/20/50/100/200), BB_UPPER, BB_LOWER, RSI_14, ATR_14, STOCHASTIC, STOCHRSI_14, MACD_26, MACD_12, MACD_HIST

### Operators

`CROSSING_UP`, `CROSSING_DOWN`, `CROSSING_ANY_SIDE`, `GREATER_THAN`, `LESS_THAN`, `GREATER_THAN_EQUAL`, `LESS_THAN_EQUAL`, `EQUAL`, `NOT_EQUAL`

### Timeframes

`DAY`, `ONE_MIN`, `FIVE_MIN`, `FIFTEEN_MIN`

---

## 3. Response

```json
{ "alertId": "12345", "alertStatus": "ACTIVE" }
```

Status: `ACTIVE`, `TRIGGERED`, `EXPIRED`, `CANCELLED`

---

## 4. Notes

1. **Equities and Indices only** — no F&O or commodity support.
2. **`expDate` default = 1 year** from creation.
3. **`frequency: "ONCE"`** — triggers once then deactivates.
4. **Multiple orders** can be attached to a single condition.
5. **Postback** updates sent to webhook URL when triggered.

---

## 2026-07-14 Upstream Update (runner-crawled live page)

**Evidence tier: Verified-live.** Raw HTML of `https://dhanhq.co/docs/v2/conditional-trigger/`
(runs 1–3, sha256 `f3ddbe28` content-identical, latest 2026-07-14T07:58:00Z). Full manifest:
`00-COVERAGE-MANIFEST.md`.

1. **Timeframe enum-literal vs example split:** the live param tables list "**DATE** ONE_MIN
   FIVE_MIN FIFTEEN_MIN" while the Sample Value column and every JSON example use
   `"timeFrame": "DAY"`. "DATE" is the literal enum text but almost certainly a doc typo —
   use `DAY` (matches the repo list + the wire examples).
2. **Field-name case split:** the param tables say `condition.timeframe` (lowercase f); the
   JSON examples use `timeFrame`. The repo JSON (`timeFrame`) matches the wire examples.
3. **UTC-Z timestamps (live-only):** get-by-ID/get-all responses carry
   `createdTime`/`triggeredTime` in ISO-8601 UTC `Z` format ("2019-08-24T14:15:22Z") — the
   ONLY UTC timestamps in the trading-API family (everything else is IST strings) — plus a
   `lastPrice` field. Any future consumer must convert; do not assume IST here.

---

## 2026-07-14 Upstream Update (2) — Place Multi Order (/alerts/multi/orders) + up-to-15-orders callout

**Evidence tiers per claim.** Sources: the 2026-07-14 sha-manifested runner crawl of
`docs.dhanhq.co` markdown routes (place/modify/place-multi-order pages, run 3, 191/191
HTTP 200 — see `00-COVERAGE-MANIFEST.md`), the official OpenAPI export
`docs.dhanhq.co/openapi/dhan-api-v2.yaml` (2026-07-14 crawl), and the 2026-06-30 portal
docs-export (`dhanhq-v2-upstream-2026-07-03/05-conditional-triggers.md`).

1. **NEW endpoint (Verified-live DOCS, PORTAL-only): `POST /v2/alerts/multi/orders` —
   Place Multi Order.** Verbatim description: *"Place a multi order directly without any
   conditions."* Despite living under the Conditional Triggers family (yaml tag
   "Conditional and Multi Order", operationId `placeMultiOrder`), this is a
   condition-LESS batch order placement. The endpoint is ABSENT from the classic
   `dhanhq.co/docs/v2/conditional-trigger/` surface — a cross-surface divergence row.
   Same static-IP NOTICE as the other order APIs; headers `access-token` + JSON
   (no `client-id`).

2. **Up-to-15-orders callout (Verified-live, NEW vs the 2026-06-30 export).** The multi
   page says verbatim: *"**Multi-order:** Place up to 15 orders in one request. Start
   with Order 1 and add additional orders sequentially."* The SAME ≤15 callout now also
   appears on the conditional Place AND Modify pages (*"Create a conditional trigger
   with up to 15 orders."*) — the 2026-06-30 export showed only `orders[0]`. That
   sequencing sentence is the ONLY sequencing/atomicity language anywhere — **no
   atomicity / all-or-nothing guarantee is stated**; never assume it.

3. **Modify body `alertId` (Verified-live).** The modify page's parameter table gains an
   OPTIONAL body field `alertId` (string, No — "Alert specific identification generated
   by Dhan") alongside the required path param of the same name. Modify remains a full
   re-spec of the Place body, not a patch.

4. **Multi per-leg request schema (Verified-live DOCS) — divergence traps vs the
   conditional leg:** `sequence` (string, REQUIRED), `correlationId` (string, ≤30 chars),
   `transactionType`/`exchangeSegment` (REQUIRED enums; segment default `NSE_EQ`),
   `productType`/`orderType`/`validity` (optional enums), `securityId` (string),
   `quantity` (int), `afterMarketOrder` (boolean) + `amoTime`
   (`PRE_OPEN|OPEN|OPEN_30|OPEN_60`), **`price`/`triggerPrice` as FLOATS** (vs STRINGS on
   conditional legs) and **`disclosedQuantity` as INT** (vs STRING `discQuantity`).
   Two distinct serde structs are mandatory — never merge them.
   yaml `MultiOrderRequestOrder.required: [sequence, transactionType, exchangeSegment]`.

5. **Multi response schema (YAML-ONLY — UNVERIFIED-LIVE).** The portal page documents no
   response body (just `200 Successful operation`); the OpenAPI yaml defines
   `PlaceMultiOrderResponse.orders[]` of `{orderId: string, sequence: string,
   orderStatus: enum}` where `orderStatus` carries **10 values** — `TRANSIT PENDING
   REJECTED CANCELLED PART_TRADED TRADED EXPIRED MODIFIED TRIGGERED INACTIVE` —
   `MODIFIED`/`INACTIVE` beyond the repo's 9-variant `OrderStatus`. Parse tolerantly
   (plain String); wire casing of this response is unverified.

6. **Rate limit (Assumed).** No family-specific limit is stated on any page; the
   static-IP NOTICE classes these as order APIs → the Order-API bucket (10/sec, 250/min,
   1000/hr, 7000/day) is the ASSUMED bucket, labeled Assumed, not documented.

**Repo consumer note:** the tickvault surface for this family
(`crates/trading/src/oms/{types,conditional,api_client}.rs`) ships DORMANT behind the
hardcoded `alerts_gate_armed` gate — see `.claude/rules/dhan/conditional-trigger.md`
rule 18.
