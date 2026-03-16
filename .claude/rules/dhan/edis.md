# Dhan EDIS Enforcement

> **Ground truth:** `docs/dhan-ref/07d-edis.md`
> **Scope:** Any file touching EDIS T-PIN generation, CDSL form handling, or eDIS approval inquiry.
> **Cross-reference:** `docs/dhan-ref/12-portfolio-positions.md` (holdings)

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any EDIS handler: `Read docs/dhan-ref/07d-edis.md`.

2. **Three endpoints — exact URLs.**
   - Generate T-PIN: `GET /v2/edis/tpin` → `202 Accepted`
   - eDIS form: `POST /v2/edis/form` → HTML response
   - Inquiry: `GET /v2/edis/inquire/{isin}` — pass `ALL` to check all holdings

3. **EDIS is required to sell holding stocks.** CDSL mandate. Without eDIS approval, sell orders on holdings will be rejected.

4. **eDIS form request body:** `{ "isin": "...", "qty": int, "exchange": "NSE", "segment": "EQ", "bulk": bool }`.
   - `bulk: true` = mark all holdings for sell.

5. **Inquiry response fields:** `totalQty`, `aprvdQty`, `status`.

6. **T-PIN is sent to registered mobile** — cannot be retrieved programmatically. Requires user interaction.

## What This Prevents

- Attempting to sell holdings without eDIS → order rejected by exchange
- Wrong ISIN in form → wrong stock marked for delivery

## Trigger

This rule activates when editing files matching:
- `crates/trading/src/oms/edis.rs`
- `crates/core/src/holdings/*.rs`
- Any file containing `edis/tpin`, `edis/form`, `edis/inquire`, `EdisForm`, `EdisInquiry`, `TpinRequest`, `eDIS`, `bulk`
