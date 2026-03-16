# Dhan Orders Enforcement

> **Ground truth:** `docs/dhan-ref/07-orders.md`
> **Scope:** Any file touching order placement, modification, cancellation, order book, trade book, or order slicing.
> **Cross-reference:** `docs/dhan-ref/08-annexure-enums.md` (OrderStatus, ProductType, error codes), `docs/dhan-ref/02-authentication.md` (static IP)

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any order handler, OMS client, or trade book parser: `Read docs/dhan-ref/07-orders.md`.

2. **Endpoints — exact URLs and methods.**
   - Place: `POST /v2/orders`
   - Modify: `PUT /v2/orders/{order-id}`
   - Cancel: `DELETE /v2/orders/{order-id}`
   - Slice: `POST /v2/orders/slicing`
   - Order book: `GET /v2/orders`
   - Single order: `GET /v2/orders/{order-id}`
   - By correlation: `GET /v2/orders/external/{correlation-id}`
   - Trade book: `GET /v2/trades`
   - Trades by order: `GET /v2/trades/{order-id}`

3. **Static IP mandatory for Place/Modify/Cancel.** See `dhan-authentication.md` rule 7 for IP endpoints and 7-day cooldown.

4. **`securityId` is STRING in request body.** `"11536"` not `11536`.

5. **`correlationId` — max 30 chars, `[a-zA-Z0-9 _-]`.** Set on every order for tracking through order updates.

6. **`quantity` in modify = TOTAL order quantity, NOT remaining.** Getting this wrong changes the order size.

7. **`orderType` values:** `LIMIT`, `MARKET`, `STOP_LOSS`, `STOP_LOSS_MARKET`.

8. **`productType` values:** `CNC`, `INTRADAY`, `MARGIN`, `MTF`. (See `dhan-annexure-enums.md` rule 5.)

9. **`validity` values:** `DAY`, `IOC` only.

10. **`price` = 0 for MARKET orders.** Non-zero for LIMIT/SL.

11. **`triggerPrice` required for `STOP_LOSS` and `STOP_LOSS_MARKET`.** Empty/zero for LIMIT/MARKET.

12. **`disclosedQuantity` must be >30% of `quantity`** if used.

13. **Response from place/cancel:** `{ "orderId": "...", "orderStatus": "..." }`. Parse `orderId` as String.

14. **Order book status subset:** `TRANSIT`, `PENDING`, `REJECTED`, `CANCELLED`, `PART_TRADED`, `TRADED`, `EXPIRED`. (Full enum: `dhan-annexure-enums.md` rule 6.)

15. **Timestamps in order/trade book are IST strings** (`YYYY-MM-DD HH:MM:SS`), NOT epoch.

16. **Use `#[serde(rename_all = "camelCase")]`** for request/response structs.

17. **Order slicing** — same body as Place Order. Use for F&O when quantity exceeds exchange freeze limit. System auto-splits into multiple legs.

> **Rate limits & error handling:** See `dhan-api-introduction.md` rules 7-8 and `dhan-annexure-enums.md` rule 11.

## What This Prevents

- Wrong quantity semantics on modify → unintended position size
- Missing correlationId → cannot track orders through WebSocket updates
- Wrong orderType string → order rejected
- Integer securityId → order rejected

## Trigger

This rule activates when editing files matching:
- `crates/trading/src/oms/*.rs`
- `crates/trading/src/order_manager.rs`
- `crates/common/src/order_types.rs`
- `crates/api/src/handlers/orders.rs`
- Any file containing `PlaceOrderRequest`, `OrderResponse`, `OrderBookEntry`, `TradeEntry`, `order_slicing`, `correlationId`, `orderType`, `transactionType`, `afterMarketOrder`, `amoTime`, `filledQty`, `remainingQuantity`
