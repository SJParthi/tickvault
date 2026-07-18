---
paths:
  - "crates/trading/src/oms/api_client.rs"
  - "crates/trading/src/oms/types.rs"
  - "crates/trading/src/oms/conditional.rs"
  - "crates/trading/tests/conditional_gate_guard.rs"
---

# Dhan Conditional Trigger & Multi Order Enforcement

> **Ground truth:** `docs/dhan-ref/07c-conditional-trigger.md`
> **Scope:** Any file touching conditional triggers (`/alerts/orders*`), the
> Place Multi Order endpoint (`/alerts/multi/orders`), their wire types, or
> the alerts gate.
> **Cross-reference:** `docs/dhan-ref/08-annexure-enums.md` (ExchangeSegment,
> ProductType, OrderType, operators/indicators annexure),
> `.claude/rules/dhan/orders.md` (price/trigger coupling, correlationId,
> static IP), `.claude/rules/dhan/forever-order.md` (the CONFIRM status
> belongs THERE, never here).

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing
   any conditional-trigger or multi-order handler:
   `Read docs/dhan-ref/07c-conditional-trigger.md` (including the two
   2026-07-14 dated updates).

2. **Six endpoints — exact URLs.**
   - Place: `POST /v2/alerts/orders`
   - Modify: `PUT /v2/alerts/orders/{alertId}`
   - Delete: `DELETE /v2/alerts/orders/{alertId}`
   - Get one: `GET /v2/alerts/orders/{alertId}`
   - Get all: `GET /v2/alerts/orders`
   - Place Multi Order: `POST /v2/alerts/multi/orders` — PORTAL-only page,
     ABSENT from the classic surface (cross-surface divergence). Path param
     name is exactly `alertId`.

3. **Headers: `access-token` only — NO `client-id` required** for this family
   (that header is Option Chain + Market Quote only). Our shared
   `auth_headers` sends `client-id` on every call anyway — harmless extra
   header, deliberately kept (no per-family header forks).

4. **Segment lock (fail-closed).** `condition.exchangeSegment` ∈
   `{NSE_EQ, BSE_EQ, IDX_I}` ONLY (the verbatim live enum). Order LEGS are
   fail-closed to `{NSE_EQ, BSE_EQ}` in our constructors
   (`oms::conditional::ConditionalLegSegment`): the family support note
   ("Conditional Triggers are currently supported only for Equities and
   Indices") GOVERNS the generic order enum's wider textual listing (the
   funds-margin enum-reuse precedent), and an index is not an orderable
   instrument — "Indices" can only mean the condition side. Widening = dated
   operator quote + THIS file edit FIRST, then the enum variant + the
   `conditional_gate_guard.rs` ratchet edit, same PR.

5. **Two DISTINCT leg schemas — never merge.** Conditional legs
   (`TriggerOrder`) carry STRING prices + STRING `discQuantity`; multi-order
   legs (`MultiOrderLeg`) carry FLOAT `price`/`triggerPrice` + INT
   `disclosedQuantity`, plus `sequence`/`correlationId`/`afterMarketOrder`/
   `amoTime` which the conditional leg lacks.

6. **Up to 15 orders** per conditional trigger AND per multi request
   (2026-07-14 live callout). Multi `sequence` = STRING "1".."N",
   builder-stamped in slice order ("Start with Order 1 and add additional
   orders sequentially"). NO atomicity guarantee is documented ANYWHERE —
   never assume all-or-nothing; inspect per-leg `orderId`/`orderStatus`.

7. **`comparingValue` type wobble:** bare number in the request, STRING
   (`"250.00"`) in the GET response. Tolerate both (`DhanNumeric`). Same for
   `lastPrice` (string in the doc example, number in the yaml). The ENTIRE
   GET echo is parsed tolerantly — condition via `TriggerConditionDetail`
   AND the order legs via `TriggerOrderDetail` (all fields defaulted,
   prices `DhanNumeric`; the OpenAPI yaml marks NO order sub-field
   required) — NEVER with the strict request-side structs: one
   UI-created sparse leg must not brick a GET or the whole GET-all list
   (2026-07-14 review fix).

8. **`createdTime`/`triggeredTime` are ISO-8601 UTC `Z`** — the ONLY UTC
   timestamps in the trading-API family (everything else is IST strings).
   Convert; NEVER assume IST. `triggeredTime` is `null` until triggered.

9. **GET-all returns a BARE ARRAY** of the Get-by-ID structure — NOT wrapped
   in `data`.

10. **`frequency`: ship `ONCE`;** `ALWAYS` is OpenAPI-yaml-only
    (UNVERIFIED-LIVE) — tolerate on parse (the response field is a String),
    never send.

11. **Alert statuses:** `ACTIVE` | `TRIGGERED` | `EXPIRED` | `CANCELLED` —
    there is NO `CONFIRM` status in this family (`CONFIRM` belongs to
    Forever Orders; do not conflate).

12. **Multi response is YAML-only** (the portal page documents no body):
    `orderStatus` is a 10-value enum incl. `MODIFIED`/`INACTIVE` beyond the
    repo's 9-variant `OrderStatus` — UNVERIFIED-LIVE; parse as a plain
    String, never panic on unknown values (annexure rule 15). The client
    tolerates the PORTAL-documented bodyless 200 (empty/whitespace body →
    empty per-leg results, `place_multi_order` 2026-07-14 round-3 fix) — a
    200 means the legs are ALREADY placed, and a parse brick would invite a
    double-placing retry; a NON-empty unparsable 200 body stays a typed
    JSON error (which legs went live is then genuinely unknown).

13. **Strict lowerCamelCase serde everywhere** in this family
    (`dhanClientId`, `comparisonType`, `timeFrame`, `alertId`,
    `alertStatus`, `createdTime`, `disclosedQuantity`). No PascalCase, no
    snake_case on the wire.

14. **Cross-surface divergences:** the classic page's `DATE` timeframe
    literal is a doc typo — use `DAY`; the classic table writes `timeframe`
    (lowercase f) while every JSON example, the portal, and the yaml use
    `timeFrame` — serde name is `timeFrame`.

15. **Modify = full re-spec** (same `dhanClientId` + `condition` + `orders[]`
    body as Place) + the documented OPTIONAL body `alertId` echo
    (2026-07-14 portal). Quantity-on-modify semantics are UNDOCUMENTED for
    this family — do NOT assume the plain-orders TOTAL-quantity rule.

16. **Rate limit: none stated per-page** — ASSUMED the Order-API bucket
    (10/sec, 250/min, 1000/hr, 7000/day; label Assumed, not documented).
    The static-IP NOTICE applies (order-class API — orders from
    unregistered IPs are rejected).

17. **`expDate` is REQUIRED, `YYYY-MM-DD`, default 1 year.** Triggers are
    GTT-like (persist to `expDate`, then `EXPIRED`) — NOT session-scoped.
    The attached legs carry their own `validity: DAY|IOC` once fired.

18. **The tickvault surface is DORMANT** behind the hardcoded
    `alerts_gate_armed` gate inside `OrderApiClient` (`#[cfg(test)]`-only
    arming; every one of the six senders — GETs included — refuses with
    `OmsError::AlertsSurfaceDisarmed` before any URL/socket work). See
    `crates/trading/tests/conditional_gate_guard.rs`. Arming in production
    requires a dated operator quote + a live-activation PR that edits THIS
    file and the ratchet together.

## What This Prevents

- F&O/commodity conditional or multi legs sneaking past the
  Equities-and-Indices-only support note (unrepresentable segment enums)
- Merged leg structs → string/float price type corruption on the wire
  (`discQuantity` vs `disclosedQuantity` trap)
- Assuming multi-order atomicity → phantom "all filled" bookkeeping
- Parsing `comparingValue`/`lastPrice` with a single numeric type →
  deserialization failure on the string forms
- Treating `createdTime`/`triggeredTime` as IST → 5.5-hour audit skew
- Panicking on `MODIFIED`/`INACTIVE`/unknown multi `orderStatus`
- Sending `frequency: ALWAYS` (yaml-only, unverified on the wire)
- Any live `/alerts/*` HTTP while the surface is dormant (gate + ratchet)

## Trigger

This rule activates when editing files matching:
- `crates/trading/src/oms/api_client.rs`
- `crates/trading/src/oms/types.rs`
- `crates/trading/src/oms/conditional.rs`
- `crates/trading/tests/conditional_gate_guard.rs`
- Any file containing `alerts/orders`, `alerts/multi`,
  `DhanConditionalTriggerRequest`, `DhanMultiOrderRequest`,
  `TriggerCondition`, `MultiOrderLeg`, `alerts_gate_armed`,
  `AlertsSurfaceDisarmed`, `DHAN_ALERTS_MULTI_ORDERS_PATH`
