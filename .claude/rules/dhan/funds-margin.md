# Dhan Funds & Margin Enforcement

> **Ground truth:** `docs/dhan-ref/13-funds-margin.md`
> **Scope:** Any file touching margin calculation, fund limits, or pre-trade balance checks.
> **Cross-reference:** `docs/dhan-ref/08-annexure-enums.md` (ExchangeSegment, ProductType)

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any margin or funds handler: `Read docs/dhan-ref/13-funds-margin.md`.

2. **Three endpoints — exact URLs.**
   - Single margin: `POST /v2/margincalculator`
   - Multi margin: `POST /v2/margincalculator/multi`
   - Fund limit: `GET /v2/fundlimit`

3. **`availabelBalance` has a typo in Dhan's API** — missing 'l'. Use this exact field name: `availabel_balance` (with `camelCase` rename). Do NOT "fix" it.

4. **Margin calculator request** uses same fields as order placement: `exchangeSegment`, `transactionType`, `quantity`, `productType`, `securityId` (STRING), `price`, `triggerPrice`.

5. **Single margin response fields (camelCase, floats except leverage):**
   - `totalMargin` — total margin required
   - `spanMargin` — SPAN margin component
   - `exposureMargin` — exposure margin component
   - `availableBalance` — available in account (note: different spelling from fundlimit!)
   - `variableMargin` — VAR or variable margin required
   - `insufficientBalance` — shortfall (0 if sufficient)
   - `brokerage` — brokerage charges for the order
   - `leverage` — STRING, not float (`"4.00"`)

6. **Multi margin calculator** — includes `includePosition` and `includeOrders` booleans, `scripts` array of orders.
   - **Response uses snake_case and ALL STRING values** (different from single margin!):
   - `total_margin`, `span_margin`, `exposure_margin` — strings like `"150000.00"`
   - `equity_margin`, `fo_margin`, `commodity_margin` — per-segment breakdown
   - `currency` — currency margin
   - `hedge_benefit` — hedge benefit amount (string)

7. **Fund limit response fields:** `sodLimit`, `collateralAmount`, `receiveableAmount`, `utilizedAmount`, `blockedPayoutAmount`, `withdrawableBalance`.

8. **Pre-trade margin check:** Call margin calculator before placing orders. If `insufficientBalance > 0`, do NOT place the order.

## What This Prevents

- "Fixing" `availabelBalance` typo → field not found → fund check fails
- Not checking margin before order → order rejected at exchange → wasted rate limit
- `leverage` parsed as float → deserialization error (it's a string)
- Wrong `securityId` type in margin request → DH-905

## Trigger

This rule activates when editing files matching:
- `crates/trading/src/oms/margin*.rs`
- `crates/trading/src/oms/fund*.rs`
- `crates/trading/src/risk/*.rs`
- Any file containing `MarginCalculator`, `FundLimit`, `margincalculator`, `fundlimit`, `availabelBalance`, `spanMargin`, `exposureMargin`, `insufficientBalance`, `totalMargin`, `hedge_benefit`, `sodLimit`
