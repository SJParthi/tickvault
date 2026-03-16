# Dhan Statements & Trade History Enforcement

> **Ground truth:** `docs/dhan-ref/14-statements-trade-history.md`
> **Scope:** Any file touching ledger reports, historical trade history, or P&L reconciliation.
> **Cross-reference:** `docs/dhan-ref/07-orders.md` (today's trades via trade book)

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any statement or trade history handler: `Read docs/dhan-ref/14-statements-trade-history.md`.

2. **Two endpoints — exact URLs.**
   - Ledger: `GET /v2/ledger?from-date=YYYY-MM-DD&to-date=YYYY-MM-DD` (query params)
   - Trade history: `GET /v2/trades/{from-date}/{to-date}/{page}` (path params, NOT query)

3. **Ledger `debit` and `credit` are STRINGS, not floats.** Parse them. When one is non-zero, the other is `"0.00"`.

4. **Ledger `voucherdate` format:** `"Jun 22, 2022"` — human-readable, NOT ISO format. Parse accordingly.

5. **Trade history uses 0-indexed pagination.** Start with page `0`. Path params, not query params.

6. **Trade history includes tax breakdown:** `sebiTax`, `stt`, `brokerageCharges`, `serviceTax`, `exchangeTransactionCharges`, `stampDuty`. All floats.

7. **`exchangeTime` is IST string** (`YYYY-MM-DD HH:MM:SS`).

8. **`drvExpiryDate`:** `"NA"` for non-derivatives (string, not null).

9. **Uses `#[serde(rename_all = "camelCase")]`** for structs.

## What This Prevents

- Float parse on string `debit`/`credit` → deserialization failure
- Query params on trade history endpoint → wrong URL → 404
- Page starting at 1 → missing first page of results
- Wrong date format parsing on `voucherdate` → date parse failure

## Trigger

This rule activates when editing files matching:
- `crates/trading/src/reporting/*.rs`
- `crates/api/src/handlers/statements.rs`
- Any file containing `LedgerEntry`, `TradeHistory`, `voucherdate`, `voucherdesc`, `vouchernumber`, `sebiTax`, `brokerageCharges`, `exchangeTransactionCharges`, `stampDuty`, `runbal`
