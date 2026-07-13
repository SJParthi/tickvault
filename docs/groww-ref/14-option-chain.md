# Groww Trade API — Option Chain (REST)

> **Source:** https://groww.in/trade-api/docs/curl/live-data#get-option-chain · https://groww.in/trade-api/docs/python-sdk/live-data
> **Fetched/Verified:** 2026-07-13 (via the 2026-07-03 lossless capture + 2026-07-13 live cross-checks; groww.in direct fetch proxy-blocked from the sandbox — provenance stated honestly)
> **Evidence tiers:** Verified / Assumed / Unknown per README legend.
> **Related:** `11-historical-candles.md` (expiry discovery), `15-rate-limits-and-capacity.md` (family attribution), `99-UNKNOWNS.md` (U-4, U-11, U-12, U-13)

---

## 1. Endpoint

**Verified (capture 2026-07-03 + SDK 1.5.0 + live search extraction 2026-07-13):**

```
GET https://api.groww.in/v1/option-chain/exchange/{exchange}/underlying/{underlying}?expiry_date={expiry_date}
```

Full request example, verbatim (capture 2026-07-03):

```
# You can also use wget
curl -X GET https://api.groww.in/v1/option-chain/exchange/{exchange}/underlying/{underlying}?expiry_date={expiry_date} \
  -H 'Accept: application/json' \
  -H 'Authorization: Bearer {ACCESS_TOKEN}' \
  -H 'X-API-VERSION: 1.0'
```

Purpose text, verbatim: "This API provides the complete option chain data for FNO (Futures and Options) contracts including Greeks. Option chains are lists of available contracts for a specific underlying symbol and expiry date. This API is specifically designed for derivatives trading."

- **Added in SDK v1.3.0 — 24th November, 2025** ("Added: Option Chain retrieval API" — Verified, changelog capture).
- SDK signature (reference fact, Verified SDK 1.5.0): `get_option_chain(exchange: str, underlying: str, expiry_date: str, timeout: Optional[int] = None) -> dict`.
- **Path note (load-bearing):** the URL prefix is `/v1/option-chain/…` — NOT under `/v1/live-data/` — relevant to the §4 rate-limit-family question. The single-contract `get_greeks` companion IS under `/v1/live-data/greeks/…`.

Request schema, verbatim (capture 2026-07-03 — all three required):

| Name | Type | Description |
| --- | --- | --- |
| exchange `*` | string | Stock exchange - NSE or BSE |
| underlying `*` | string | Underlying symbol for the contract such as NIFTY, BANKNIFTY, RELIANCE etc. |
| expiry\_date `*` | string | Expiry date of the contract in YYYY-MM-DD format. |

## 2. Response shape (full, verbatim)

Verbatim (capture 2026-07-03; truncated by the doc itself with `....`; "All prices in rupees." per the page family convention):

```json
{
    "status": "SUCCESS",
    "payload": {
        "underlying_ltp": 25641.7,
        "strikes": {
            "23400": {
                "CE": {
                    "greeks": {
                        "delta": 0.9936,
                        "gamma": 0,
                        "theta": -1.0787,
                        "vega": 0.6943,
                        "rho": 5.1802,
                        "iv": 25.3409
                    },
                    "trading_symbol": "NIFTY25N1823400CE",
                    "ltp": 2200,
                    "open_interest": 7,
                    "volume": 5
                },
                "PE": {
                    "greeks": {
                        "delta": -0.0064,
                        "gamma": 0,
                        "theta": -1.0787,
                        "vega": 0.6943,
                        "rho": -0.0373,
                        "iv": 25.3409
                    },
                    "trading_symbol": "NIFTY25N1823400PE",
                    "ltp": 2.05,
                    "open_interest": 7453,
                    "volume": 9339
                }
            },
            "23450": { "CE": { "...": "..." }, "PE": { "...": "..." } }
        }
    }
}
```

Response schema, verbatim (capture 2026-07-03):

| Name | Type | Description |
| --- | --- | --- |
| status | string | SUCCESS if request is processed successfully, FAILURE if the request failed |
| underlying\_ltp | decimal | Last Traded Price of the underlying |
| strikes | object | Strike-wise option chain data containing CE and PE contracts |
| trading\_symbol | string | Trading symbol of the option contract |
| ltp | float | Last traded price of the option contract |
| open\_interest | int | Open interest for the contract |
| volume | int | Total volume traded for the contract |
| delta | float | Delta measures the rate of change of option price based on every 1 rupee change in the price of underlying |
| gamma | float | Gamma measures the rate of change of delta with respect to underlying asset price |
| theta | float | Theta measures the rate of time decay of option price |
| vega | float | Vega measures the rate of change of option price based on every 1% change in implied volatility |
| rho | float | Rho measures the sensitivity of option price to changes in interest rates |
| iv | float | Implied Volatility represents the market's expectation of future volatility, expressed as a percentage |

## 3. Coverage + documented-absence facts

- **"All available strikes" (Verified, python-sdk page verbatim):** "This method provides comprehensive data including Greeks, LTP, open interest, and volume for **all available strikes** for both Call (CE) and Put (PE) options." There is **NO strike-window parameter** — one call always returns the whole chain for the (underlying, expiry). No pagination parameter exists either (Verified-absence).
- **NO response timestamp (Verified-absence):** the documented schema carries no timestamp field of any kind. The snapshot moment must be stamped client-side at receipt.
- **NO per-leg bid/ask/depth (Verified-absence within the documented schema):** only ltp / open_interest / volume / greeks per leg. Contrast the full-quote endpoint (`GET /v1/live-data/quote`), which carries bid/offer + depth for a single instrument.
- Strike keys are integer-string rupee values in the example (`"23400"`). Decimal-strike key format (stock options): **Unknown** — probe in `99-UNKNOWNS.md` U-11.
- Documented strike COUNT per underlying: **none exists anywhere in the docs (Verified-absence)** — see §5.

## 4. Expiry discovery + rate-limit family

- The endpoint takes a mandatory `expiry_date`; there is NO expiry-list sub-endpoint under `/option-chain/` (Verified-absence, SDK + capture). Documented expiry sources:
  1. `GET https://api.groww.in/v1/historical/expiries` (see `11-historical-candles.md` §8), or
  2. the instruments master CSV (`https://growwapi-assets.groww.in/instruments/instrument.csv`) — per-contract `expiry_date` column; this is what tickvault already consumes for the §36 futures selection.
- **Rate-limit family: UNDOCUMENTED (Verified-absence).** The official table's Live Data row names only "Market Quote, LTP, OHLC"; option chain (added Nov 2025) appears in NO family as of the 2026-07-03 capture, and no 2026-07-13 live signal showed an added row. The path sits OUTSIDE `/live-data/`. See `15-rate-limits-and-capacity.md` §3; probe in `99-UNKNOWNS.md` U-4.
- **The Dhan contrast (honest framing):** Dhan bills its option chain to an explicit endpoint-specific limit (1 unique request per 3 seconds). Groww documents NO chain-specific limit whatsoever — that is **Unknown, not "unlimited"**: a smaller undocumented bucket cannot be ruled out from the docs, only by live probe.

## 5. Payload-size estimate (expiry-day NIFTY weekly) — Assumed

One request returns the ENTIRE chain for one (underlying, expiry); a NIFTY + BANKNIFTY (NSE) + SENSEX (BSE) current-expiry sweep = **3 requests per snapshot cycle** (Verified from the endpoint contract). Sizing, labelled **Assumed** (market-structure order-of-magnitude, NOT a Groww-documented number):

- An NSE NIFTY weekly chain typically carries on the order of **~90–110 strikes ≈ ~200 CE+PE legs**; expiry-day chains are the largest (maximum accumulated strikes; NSE adds strikes intraday on big moves).
- Per-leg JSON weight from the documented example ≈ 260–300 bytes → payload ≈ strikes × 2 × ~280 B ≈ **~100–300 KB JSON per underlying**.
- The doc example itself shows deep-ITM strikes (23400 CE, delta 0.9936, at underlying 25641.7), confirming the chain spans far beyond ATM.
- Exact counts and byte sizes are a day-0 live probe (`len(payload["strikes"])` + raw bytes per underlying) — `99-UNKNOWNS.md` U-12.

## 6. Companion single-contract greeks endpoint

**Verified (capture verbatim + SDK 1.5.0):**

```
GET https://api.groww.in/v1/live-data/greeks/exchange/{exchange}/underlying/{underlying}/trading_symbol/{trading_symbol}/expiry/{expiry}
```

Greeks for ONE FNO contract; response `{"greeks": {"delta", "gamma", "theta", "vega", "rho", "iv"}}`. This one IS under `/live-data/`. SDK quirk (Verified): `get_greeks` is the only live-data method with NO `timeout` parameter.

## 7. Errors

Same global envelope as every REST endpoint (see `11-historical-candles.md` §10): `{"status":"FAILURE","error":{"code","message","metadata"}}`, GA000–GA007 codes, HTTP 400/401/403/404/**429**/504 → typed SDK exceptions. No chain-specific error section exists (Verified-absence). Failure shape for an un-entitled key (Dhan DH-902/806 analogue): **Unknown** — `99-UNKNOWNS.md` U-13.
