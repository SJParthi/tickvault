# Dhan Live Order Update WebSocket Enforcement

> **Ground truth:** `docs/dhan-ref/10-live-order-update-websocket.md`
> **Scope:** Any file touching order update WebSocket connection, auth, message parsing, or order status tracking.
> **Cross-reference:** `docs/dhan-ref/08-annexure-enums.md` (OrderStatus, ProductType), `docs/dhan-ref/07e-postback.md` (alternative)

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any order update WebSocket handler: `Read docs/dhan-ref/10-live-order-update-websocket.md`.

2. **Endpoint:** `wss://api-order-update.dhan.co`. No query params — auth is via JSON message after connect.

3. **Auth message — exact structure.**
   ```json
   { "LoginReq": { "MsgCode": 42, "ClientId": "...", "Token": "JWT" }, "UserType": "SELF" }
   ```
   - `MsgCode` is always `42`
   - `UserType`: `"SELF"` for individual, `"PARTNER"` for partner (with `Secret` field)
   - PascalCase field names — use `#[serde(rename)]`

4. **JSON messages, NOT binary.** Unlike market feed WebSockets. Parse with `serde_json` directly.

5. **Message wrapper:** `{ "Data": { ... }, "Type": "order_alert" }`. PascalCase top-level keys.

6. **Field name abbreviations in Data — NOT the same as REST API.**
   - `Product`: `C`=CNC, `I`=INTRADAY, `M`=MARGIN, `F`=MTF, `V`=CO, `B`=BO
   - `TxnType`: `B`=Buy, `S`=Sell
   - `OrderType`: `LMT`, `MKT`, `SL`, `SLM`
   - `Segment`: `E`=Equity, `D`=Derivatives, `C`=Currency, `M`=Commodity
   - `Source`: `P`=API, `N`=Normal (Dhan web/app)

7. **Receives ALL account orders** — not just API orders. Filter by `Source: "P"` for your own orders.

8. **Timestamps are IST strings** (`YYYY-MM-DD HH:MM:SS`), NOT epoch. No timezone conversion needed.

9. **`CorrelationId`** — matches what you set when placing the order. Use for order tracking.

10. **`Remarks: "Super Order"`** — indicates order belongs to a Super Order group.

11. **`LegNo`:** `1`=Entry, `2`=Stop Loss, `3`=Target (for BO/CO/Super orders).

12. **WS status subset:** `TRANSIT`, `PENDING`, `REJECTED`, `CANCELLED`, `TRADED`, `EXPIRED`. (Full enum: `dhan-annexure-enums.md` rule 6.)

13. **`OffMktFlag: "1"`** = AMO order, `"0"` = normal.

14. **`OptType: "XX"`** = non-option instrument. `"CE"` or `"PE"` for options.

## What This Prevents

- Binary parser on JSON WebSocket → complete parse failure
- REST field names on WS data → missing fields, wrong mapping
- Single-character product/txn codes not mapped → unknown order types
- Missing Source filter → processing orders from Dhan web app as own orders
- Wrong auth message format → connection rejected

## Trigger

This rule activates when editing files matching:
- `crates/core/src/websocket/order_update*.rs`
- `crates/core/src/order_monitor/*.rs`
- `crates/trading/src/oms/order_tracker.rs`
- Any file containing `OrderUpdateMessage`, `OrderUpdateData`, `OrderUpdateAuth`, `LoginReq`, `MsgCode`, `order_alert`, `api-order-update.dhan.co`, `order_update_ws`, `TxnType`, `ExchOrderNo`, `ReasonDescription`
