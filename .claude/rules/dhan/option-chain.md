# Dhan Option Chain Enforcement

> **Ground truth:** `docs/dhan-ref/06-option-chain.md`
> **Scope:** Any file touching option chain fetching, expiry list, strike parsing, or greeks.
> **Cross-reference:** `docs/dhan-ref/08-annexure-enums.md` (ExchangeSegment), `docs/dhan-ref/09-instrument-master.md` (SecurityId)

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any option chain handler, strike parser, or greeks struct: `Read docs/dhan-ref/06-option-chain.md`.

2. **Two endpoints — exact URLs.**
   - Full chain: `POST https://api.dhan.co/v2/optionchain`
   - Expiry list: `POST https://api.dhan.co/v2/optionchain/expirylist`

3. **Requires extra `client-id` header** in addition to `access-token`. Missing `client-id` returns auth error.

4. **Rate limit: 1 unique request every 3 seconds.** Multiple different underlyings/expiries can be fetched concurrently within the 3s window.

5. **Request fields — exact casing and types.**
   - `UnderlyingScrip`: integer (SecurityId of underlying, NOT string)
   - `UnderlyingSeg`: string enum (`"IDX_I"`, `"NSE_EQ"`, etc.)
   - `Expiry`: string `"YYYY-MM-DD"` (from expiry list API — never guess)
   - Use `#[serde(rename = "UnderlyingScrip")]` etc. — PascalCase in JSON.

6. **Response: `data.oc` is a HashMap keyed by strike price strings.** Keys are decimal strings like `"25650.000000"`. Parse as f64. Never assume integer strikes.

7. **CE or PE may be `None`.** Deep OTM strikes might have no data on one side. Use `Option<OptionData>`.

8. **`security_id` in response gives SecurityId of each option contract.** Can be used directly for WebSocket subscription without instrument master lookup.

9. **Expiry dates must come from expiry list API.** Never hardcode or compute expiry dates.

10. **Greeks fields:** `delta`, `theta`, `gamma`, `vega` — all f64. Present on every option with data.

## What This Prevents

- Missing `client-id` header → auth error on every request
- Guessed expiry dates → invalid expiry → empty response
- Integer strike key assumption → parse failure on decimal strikes
- Rate limit violation (see `dhan-api-introduction.md` rules 7-8)
- Missing `Option<>` on CE/PE → deserialization panic on deep OTM strikes

## Trigger

This rule activates when editing files matching:
- `crates/core/src/option_chain/*.rs`
- `crates/trading/src/strategy/option_chain*.rs`
- `crates/api/src/handlers/option_chain.rs`
- Any file containing `OptionChainRequest`, `OptionChainResponse`, `OptionChainData`, `StrikeData`, `OptionData`, `Greeks`, `ExpiryListResponse`, `optionchain`, `UnderlyingScrip`, `UnderlyingSeg`, `implied_volatility`
