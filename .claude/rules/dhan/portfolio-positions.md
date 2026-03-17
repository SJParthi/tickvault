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

10. **Exit all (`DELETE /v2/positions`)** — exits ALL open positions AND cancels ALL pending orders. Use as emergency stop alongside kill switch.

11. **Holdings `availableQty`** — what's actually available to sell/pledge. May differ from `totalQty` due to T+1 settlement or collateral.

12. **`drvExpiryDate`, `drvOptionType`, `drvStrikePrice`** — present on positions for derivatives. Null/zero for equity.

13. **Use `#[serde(rename_all = "camelCase")]`** for all structs.

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
