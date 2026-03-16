# Dhan Conditional Trigger Enforcement

> **Ground truth:** `docs/dhan-ref/07c-conditional-trigger.md`
> **Scope:** Any file touching conditional/alert-based order triggers, technical indicator conditions, or alert management.
> **Cross-reference:** `docs/dhan-ref/08-annexure-enums.md` (ComparisonType, IndicatorName, Operator)

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any conditional trigger handler: `Read docs/dhan-ref/07c-conditional-trigger.md`.

2. **Endpoints — exact URLs.**
   - Create: `POST /v2/alerts/orders`
   - Modify: `PUT /v2/alerts/orders/{alertId}`
   - Delete: `DELETE /v2/alerts/orders/{alertId}`
   - Get one: `GET /v2/alerts/orders/{alertId}`
   - Get all: `GET /v2/alerts/orders`

3. **Equities and Indices ONLY.** No F&O, no commodity support. Reject at build time.

4. **Four comparison types:**
   - `TECHNICAL_WITH_VALUE` — indicator vs fixed number
   - `TECHNICAL_WITH_INDICATOR` — indicator vs another indicator
   - `TECHNICAL_WITH_CLOSE` — indicator vs closing price
   - `PRICE_WITH_VALUE` — market price vs fixed value

5. **Available indicators:** SMA (5/10/20/50/100/200), EMA (5/10/20/50/100/200), BB_UPPER, BB_LOWER, RSI_14, ATR_14, STOCHASTIC, STOCHRSI_14, MACD_26, MACD_12, MACD_HIST.

6. **Operators:** `CROSSING_UP`, `CROSSING_DOWN`, `CROSSING_ANY_SIDE`, `GREATER_THAN`, `LESS_THAN`, `GREATER_THAN_EQUAL`, `LESS_THAN_EQUAL`, `EQUAL`, `NOT_EQUAL`.

7. **Timeframes:** `DAY`, `ONE_MIN`, `FIVE_MIN`, `FIFTEEN_MIN`.

8. **`frequency: "ONCE"`** — triggers once then deactivates.

9. **`expDate` default = 1 year** from creation if not specified.

10. **Multiple orders can attach to a single condition.**

11. **Alert statuses:** `ACTIVE`, `TRIGGERED`, `EXPIRED`, `CANCELLED`.

12. **Response:** `{ "alertId": "...", "alertStatus": "..." }`.

## What This Prevents

- F&O conditional trigger → silent failure, condition never fires
- Wrong indicator name string → DH-905 → trigger not created
- Wrong operator string → DH-905
- Missing required fields per comparison type → DH-905

## Trigger

This rule activates when editing files matching:
- `crates/trading/src/oms/conditional_trigger.rs`
- `crates/trading/src/strategy/alert*.rs`
- Any file containing `ConditionalTrigger`, `alerts/orders`, `comparisonType`, `indicatorName`, `CROSSING_UP`, `CROSSING_DOWN`, `TECHNICAL_WITH_VALUE`, `PRICE_WITH_VALUE`, `alertId`, `alertStatus`
