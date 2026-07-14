# Dhan Portfolio & Positions Enforcement

> **Ground truth:** `docs/dhan-ref/12-portfolio-positions.md`
> **Scope:** Any file touching holdings, positions, position conversion, or exit-all.
> **Cross-reference:** `docs/dhan-ref/08-annexure-enums.md` (ProductType, ExchangeSegment), `docs/dhan-ref/07d-edis.md` (EDIS for selling holdings)

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any portfolio handler: `Read docs/dhan-ref/12-portfolio-positions.md`.

2. **Four endpoints — exact URLs.**
   - Holdings: `GET /v2/holdings`
   - Positions: `GET /v2/positions`
   - Convert: `POST /v2/positions/convert`
   - Exit all: `DELETE /v2/positions`

3. **Holdings response is an array** of holding objects (not wrapped in `data`).

4. **Positions response is an array** of position objects.

5. **`positionType` values:** `LONG`, `SHORT`, `CLOSED`.

6. **`netQty`** = `buyQty` - `sellQty`. Can be negative (short position).

7. **`realizedProfit`** = booked P&L. **`unrealizedProfit`** = open/mark-to-market P&L.

8. **`carryForward*` fields** — F&O positions carried from previous sessions. `day*` fields = today's intraday activity.

9. **Convert position request:**
   - `fromProductType` → `toProductType` (e.g., `INTRADAY` → `CNC`)
   - `convertQty` is a STRING, not integer
   - **SDK Note**: Python SDK sends `convertQty` as integer. Dhan API may accept both string and integer.
   - Response: `202 Accepted`
   - **2026-07-14 note (runner-crawled, Verified-live):** the documented type is now INTEGER
     on all three schema/table statements — the classic table, the portal export table, and
     the OpenAPI yaml (`convertQty: integer`, required); only the classic page's example JSON
     still shows the string `"40"`. The "STRING, not integer" line above is superseded — send
     int, accept both on parse. See `docs/dhan-ref/12-portfolio-positions.md` "2026-07-14
     Upstream Update".

10. **Exit all (`DELETE /v2/positions`)** — exits all open positions. Response: `{ "status": "SUCCESS", "message": "All orders and positions exited successfully" }`.
    - **Note**: Dhan's API description says "only squares off open positions and does not cancel pending orders", but the v2.5 release notes say "close all open positions and open orders", and the response message mentions "All orders and positions". Treat conservatively: assume it MAY cancel pending orders too. Use alongside kill switch for full emergency stop.
    - **2026-07-14 resolution (runner-crawled, Verified-live):** BOTH live surfaces now state
      verbatim "Exit all active positions and cancel all open orders for the current trading
      day." — the old "does not cancel pending orders" wording is GONE from the live page. The
      conservative treat-as-may-cancel guidance above is now documented fact: exit-all DOES
      cancel open orders. Kill-switch pairing advice unchanged. See
      `docs/dhan-ref/12-portfolio-positions.md` "2026-07-14 Upstream Update".

11. **Holdings `availableQty`** — what's actually available to sell/pledge. May differ from `totalQty` due to T+1 settlement or collateral.

12. **`drvExpiryDate`, `drvOptionType`, `drvStrikePrice`** — present on positions for derivatives. Null/zero for equity.

13. **Use `#[serde(rename_all = "camelCase")]`** for all structs.

14. **`crossCurrency`** — boolean field on positions. `true` for non-INR currency pairs (currency F&O). `false` for all others. Use `Option<bool>` with `#[serde(default)]`.

## What This Prevents

- Using `totalQty` instead of `availableQty` for sell orders → order rejected (insufficient qty)
- Integer `convertQty` → deserialization mismatch
- Calling exit-all without understanding it cancels ALL orders → unintended order cancellations
- Missing carryForward fields → wrong P&L calculation for F&O positions

## Trigger

This rule activates when editing files matching:
- `crates/trading/src/portfolio/*.rs`
- `crates/trading/src/oms/position_tracker.rs`
- `crates/api/src/handlers/portfolio.rs`
- Any file containing `Holding`, `Position`, `ConvertPosition`, `positionType`, `netQty`, `realizedProfit`, `unrealizedProfit`, `availableQty`, `carryForwardBuyQty`, `positions/convert`, `exitAll`
