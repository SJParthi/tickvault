# Dhan Forever Order (GTT) Enforcement

> **Ground truth:** `docs/dhan-ref/07b-forever-order.md`
> **Scope:** Any file touching forever/GTT order creation, modification, or OCO handling.
> **Cross-reference:** `docs/dhan-ref/07-orders.md` (base order fields), `docs/dhan-ref/08-annexure-enums.md`

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any forever order handler: `Read docs/dhan-ref/07b-forever-order.md`.

2. **Endpoints — exact URLs.**
   - Create: `POST /v2/forever/orders`
   - Modify: `PUT /v2/forever/orders/{order-id}`
   - Delete: `DELETE /v2/forever/orders/{order-id}`
   - List: `GET /v2/forever/all`

3. **Static IP required** (see `dhan-authentication.md` rule 7). Product types: `CNC`, `MTF` only (no INTRADAY, no MARGIN).

4. **Two order types:** `SINGLE` (one trigger) and `OCO` (two triggers — whichever hits first).

5. **`orderFlag` field:** `"SINGLE"` or `"OCO"`. Required.

6. **OCO has second leg fields:**
   - `price1` — second leg price
   - `triggerPrice1` — second leg trigger
   - `quantity1` — second leg quantity
   - These are required for OCO, must NOT be sent for SINGLE.

7. **Modify uses `legName`:** `"TARGET_LEG"` (single or first OCO leg), `"STOP_LOSS_LEG"` (second OCO leg).

8. **Forever-specific status subset:** `TRANSIT`, `PENDING`, `REJECTED`, `CANCELLED`, `TRADED`, `EXPIRED`, `CONFIRM`. (Full enum: `dhan-annexure-enums.md` rule 6.)
   - `CONFIRM` = forever order is active and waiting for trigger. **Unique to forever orders.**

## What This Prevents

- INTRADAY product type on forever order → DH-905 rejection
- Missing OCO second leg fields → DH-905 → order not created
- Sending OCO fields on SINGLE → undefined behavior
- Not handling `CONFIRM` status → missed active order tracking

## Trigger

This rule activates when editing files matching:
- `crates/trading/src/oms/forever_order.rs`
- `crates/trading/src/strategy/gtt*.rs`
- Any file containing `ForeverOrder`, `forever/orders`, `forever/all`, `orderFlag`, `OCO`, `SINGLE`, `price1`, `triggerPrice1`, `quantity1`, `CONFIRM`
