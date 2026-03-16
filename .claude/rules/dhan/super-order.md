# Dhan Super Order Enforcement

> **Ground truth:** `docs/dhan-ref/07a-super-order.md`
> **Scope:** Any file touching super order placement, modification, cancellation, or leg management.
> **Cross-reference:** `docs/dhan-ref/07-orders.md` (base order fields), `docs/dhan-ref/08-annexure-enums.md`

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any super order handler: `Read docs/dhan-ref/07a-super-order.md`.

2. **Endpoints — exact URLs.**
   - Place: `POST /v2/super/orders`
   - Modify: `PUT /v2/super/orders/{order-id}`
   - Cancel: `DELETE /v2/super/orders/{order-id}/{order-leg}`
   - List: `GET /v2/super/orders`

3. **Static IP required.** See `dhan-authentication.md` rule 7.

4. **Three extra fields beyond base order:**
   - `targetPrice` (f64) — target exit price
   - `stopLossPrice` (f64) — stop loss price
   - `trailingJump` (f64) — price jump for trailing SL (0 = no trail)

5. **Three leg types:** `ENTRY_LEG`, `TARGET_LEG`, `STOP_LOSS_LEG`.

6. **Modify restrictions by leg:**
   - `ENTRY_LEG`: all fields modifiable (only when `PENDING` or `PART_TRADED`)
   - `TARGET_LEG`: only `targetPrice`
   - `STOP_LOSS_LEG`: only `stopLossPrice` and `trailingJump`

7. **Cancel semantics:**
   - Cancelling `ENTRY_LEG` cancels ALL legs
   - Cancelling `TARGET_LEG` or `STOP_LOSS_LEG` individually — that leg cannot be re-added

8. **`trailingJump` set to 0 cancels trailing.** Not `None`, not omitted — explicitly `0`.

9. **Response includes `legDetails[]` array** with nested target and stop loss legs.

## What This Prevents

- Modifying wrong fields on wrong leg → DH-905 or silent ignore
- Cancelling target/SL leg without knowing it's permanent → lost protection
- Wrong trailing jump semantics → trailing never activates or never cancels

## Trigger

This rule activates when editing files matching:
- `crates/trading/src/oms/super_order.rs`
- `crates/trading/src/strategy/bracket*.rs`
- Any file containing `PlaceSuperOrderRequest`, `SuperOrderLeg`, `super/orders`, `targetPrice`, `stopLossPrice`, `trailingJump`, `ENTRY_LEG`, `TARGET_LEG`, `STOP_LOSS_LEG`
