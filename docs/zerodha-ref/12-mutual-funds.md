# Zerodha Kite Connect v3 — Mutual Funds (compact reference)

> **Source:** `https://kite.trade/docs/connect/v3/mutual-funds/` — official live doc, UNREACHABLE from this sandbox; see provenance below.
> **Fetched-Verified:** 2026-07-13 via CLIENT-LIB-SOURCE (`zerodha/pykiteconnect@master` `kiteconnect/connect.py` via raw.githubusercontent.com; `zerodha/gokiteconnect@master` `mutualfunds.go` + `connect.go` via git clone — fetched 2026-07-13) + OFFICIAL-MOCK (`zerodha/kiteconnect-mocks@main` c7a8123: `mf_orders.json`, `mf_orders_info.json`, `mf_order_response.json`, `mf_order_cancel.json`, `mf_sips.json`, `mf_sip_info.json`, `mf_sip_place.json`, `mf_sip_modify.json`, `mf_sip_cancel.json`, `mf_holdings.json`, `mf_instruments.csv` — all fetched 2026-07-13) + SEARCH (live MF page snippets + Kite forum, 2026-07-13). **ARCHIVE-DOC and MIRROR-LIVE were BOTH BLOCKED** at the egress proxy on 2026-07-13 (web.archive.org + r.jina.ai + kite.trade direct: CONNECT 403 per the same-day probe log). No claim in this file is byte-verbatim from the live docs page.
> **Evidence tiers:** see README legend — Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) / SEARCH / Assumed / Unknown.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. Mutual funds are NOT feed-relevant to tickvault (no ticks, no candles) — this file exists only so the Kite surface map is complete.
> **Related:** `99-UNKNOWNS.md` (Z-MF-1 … Z-MF-4 below), `03-orders.md` (contrast the equity order family). Neither `docs/dhan-ref/` nor `docs/groww-ref/` has an MF section — Kite is the only broker of the three in this repo exposing an MF API surface (Verified against the repo file listings).

---

## 1. Endpoint surface

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master `connect.py` `_routes` — https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/connect.py; cross-checked gokiteconnect@master `connect.go` URI constants + `mutualfunds.go` HTTP methods):**

| Action | Method | Route | py fn | Go-only? |
|---|---|---|---|---|
| List MF orders | `GET` | `/mf/orders` | `mf_orders()` | |
| MF orders by date range | `GET` | `/mf/orders?from=&to=` | — | Go `GetMFOrdersByDate(from, to)` (params `from`, `to`) |
| One MF order | `GET` | `/mf/orders/{order_id}` | `mf_orders(order_id)` | |
| Place MF order | `POST` | `/mf/orders` | `place_mf_order(...)` | |
| Cancel MF order | `DELETE` | `/mf/orders/{order_id}` | `cancel_mf_order(order_id)` | |
| List SIPs | `GET` | `/mf/sips` | `mf_sips()` | |
| One SIP | `GET` | `/mf/sips/{sip_id}` | `mf_sips(sip_id)` | |
| Place SIP | `POST` | `/mf/sips` | `place_mf_sip(...)` | |
| Modify SIP | `PUT` | `/mf/sips/{sip_id}` | `modify_mf_sip(...)` | |
| Cancel SIP | `DELETE` | `/mf/sips/{sip_id}` | `cancel_mf_sip(sip_id)` | |
| MF holdings | `GET` | `/mf/holdings` | `mf_holdings()` | |
| One holding's trade breakdown | `GET` | `/mf/holdings/{isin}` | — | Go `GetMFHoldingInfo(isin)` → list of `MFTrade` |
| Allotted ISINs | `GET` | `/mf/allotments` | — | Go `GetMFAllottedISINs()` → `[]string` |
| MF instrument master (CSV) | `GET` | `/mf/instruments` | `mf_instruments()` | |

Same base URL / headers / form-encoded bodies as the rest of the API (`03-orders.md` §1). Instrument identity everywhere is `tradingsymbol` = the fund **ISIN** (`INF…`) — Verified (OFFICIAL-MOCK: every mock row) + SEARCH ("tradingsymbol (ISIN) of the fund").

## 2. ⚠ Live availability of PLACEMENT is doubtful (read first)

**SEARCH (Kite forum, incl. thread 13433 "Mutual Fund API", 2026-07-13):** Zerodha reportedly STOPPED MF order placement via Kite Connect around 2020 when Coin purchases moved to bank-account payment — "one cannot buy or sell mutual funds using Kite Connect APIs, and can only fetch orders and portfolio", with the team saying docs/endpoints would be updated. The SDK placement/SIP-mutation methods above still exist at master (Verified, 2026-07-13) and the official mocks still ship success responses for them — so the CODE surface and the LIVE entitlement disagree. Current live state = **Unknown** (Z-MF-1). Treat `/mf/*` writes as historical-reference until the live page is pasted.

## 3. MF orders

**Verified (CLIENT-LIB-SOURCE py `place_mf_order` signature + Go `MFOrderParams`):** place params = `tradingsymbol` (ISIN), `transaction_type` (`BUY`/`SELL`), `quantity`, `amount`, `tag` — quantity and amount both optional-typed. **Assumed:** BUY is by `amount` (₹) and SELL by `quantity` (units) — inferred from the param split + every BUY mock row carrying `amount` with `quantity: 0.0`; no SELL mock exists to confirm (Z-MF-2).

**Verified (OFFICIAL-MOCK mf_order_response.json / mf_order_cancel.json):** place/cancel return `{"status":"success","data":{"order_id":"<uuid>"}}` — MF `order_id` is a **UUID string** (contrast the numeric-string equity order ids).

**Verified (OFFICIAL-MOCK mf_orders.json / mf_orders_info.json — full observed field set):** `status` (`OPEN`/`REJECTED`/… — open-ended), `purchase_type` (`FRESH`/`ADDITIONAL`), `folio` (nullable), `order_timestamp` (`"YYYY-MM-DD HH:MM:SS"` IST), `exchange_timestamp` (nullable, DATE-only string when present — a wart), `exchange_order_id` (nullable), `settlement_id` (nullable), `average_price`, `last_price`, `last_price_date` (`YYYY-MM-DD`), `tradingsymbol` (ISIN), `transaction_type`, `order_id` (UUID), `amount`, `quantity` (fractional float), `tag`, `placed_by`, `variety` (`regular` / `sip` / `amc_sip` observed), `status_message` (human text, e.g. "SIP: Insufficient balance."), `fund` (full scheme name). **SEARCH:** the plain `GET /mf/orders` returns orders of the **last 7 days** (the Go `from`/`to` params widen it).

## 4. SIPs

**Verified (CLIENT-LIB-SOURCE py `place_mf_sip` + Go `MFSIPParams`/`MFSIPModifyParams`):**

- Place: `tradingsymbol`, `amount` (per instalment), `instalments`, `frequency`, `initial_amount` (optional — purchase before the SIP starts), `instalment_day` (optional), `tag`. Go additionally exposes `trigger_price`, `step_up` (string), `sip_type`.
- Modify: `amount`, `status` (pause/activate — literal values Unknown, Z-MF-3), `instalments`, `frequency`, `instalment_day`; Go adds `step_up`.
- Cancel: `DELETE /mf/sips/{sip_id}`.

**SEARCH (live-page snippet, 2026-07-13) — CONFLICTED by the official mock:** the snippet says `frequency` ∈ `weekly` / `monthly` / `quarterly` and `instalment_day` for monthly SIPs is one of `1, 5, 10, 15, 20, 25`. **BUT Verified (OFFICIAL-MOCK mf_sips.json):** the mock's own MONTHLY rows carry `instalment_day: 30` (sip 749073272501476) and `instalment_day: 10` — **`30` is NOT in the snippet's enum**, so the day-enum is either stale, partial, or wrong; treat it as conflicting-SEARCH only (both readings recorded per the pack convention). Weekly rows observed with `instalment_day: 0`.

**Verified (OFFICIAL-MOCK mf_sips.json / mf_sip_info.json — full observed field set):** `sip_id` (numeric-string), `status` (`ACTIVE`), `sip_type` (`sip`), `created`, `frequency`, `instalment_amount`, `instalments`, `pending_instalments`, `completed_instalments`, `last_instalment` (timestamp), `next_instalment` (date), `instalment_day`, `dividend_type` (`idcw` / `growth` observed), `transaction_type`, `trigger_price` (0 observed), `step_up` (map of `"DD-MM" → int percent`, e.g. `{"05-05": 10}`), `sip_reg_num` (nullable — exchange SIP registration), `fund`, `tradingsymbol` (ISIN), `tag`; `mf_sip_info.json` additionally shows `fund_source: "pool"`. `instalments: -1` with `pending_instalments: -1` observed on the weekly/quarterly rows — **Assumed** to mean perpetual/until-cancelled; the mock's MONTHLY rows instead carry `instalments: 9999` / `pending_instalments: 9998` (a second "effectively-perpetual" encoding — parsers must tolerate BOTH sentinels). **Verified (OFFICIAL-MOCK mf_sip_place/modify/cancel.json):** all three return `{"status":"success","data":{"sip_id":"<numeric-string>"}}`; the Go `MFSIPResponse` also declares an optional `order_id` (Assumed: set when `initial_amount` triggers an immediate purchase).

## 5. Holdings

**Verified (OFFICIAL-MOCK mf_holdings.json + CLIENT-LIB-SOURCE Go `MFHolding`):** row fields = `folio`, `fund`, `tradingsymbol` (ISIN), `average_price`, `last_price` (latest NAV), `last_price_date` (empty string observed — wart), `pledged_quantity`, `pnl`, `quantity` (fractional units, e.g. `382.488`). Go-only extras: `GET /mf/holdings/{isin}` returns the per-holding trade breakdown (`fund`, `tradingsymbol`, `average_price`, `variety`, `exchange_timestamp`, `amount`, `folio`, `quantity` per trade) and `GET /mf/allotments` returns the ISIN list with ≥1 allotment.

## 6. MF instrument master

**Verified (OFFICIAL-MOCK mf_instruments.csv header, verbatim):**

```
tradingsymbol,amc,name,purchase_allowed,redemption_allowed,minimum_purchase_amount,purchase_amount_multiplier,minimum_additional_purchase_amount,minimum_redemption_quantity,redemption_quantity_multiplier,dividend_type,scheme_type,plan,settlement_type,last_price,last_price_date
```

**Verified (CLIENT-LIB-SOURCE py `_parse_mf_instruments`):** `purchase_allowed`/`redemption_allowed` are `0/1` ints (SDK casts to bool); the min/multiplier columns and `last_price` are floats; `last_price_date` is `YYYY-MM-DD`. Observed values (mock rows): `dividend_type` `payout`/`growth`, `scheme_type` `equity`, `plan` `regular`, `settlement_type` `T3`. Like the equity master, this is a **CSV dump, not JSON** — gzip/caching behaviour and refresh cadence are Unknown (Z-MF-4). `last_price` = NAV — this is the ONLY "market data" in the MF surface; there are no MF quotes on the WebSocket (Verified-absence: no MF mode/token path in any SDK ticker).

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Z-MF-1 — is MF order/SIP placement live at all?** Forum says placement was disabled (~2020, bank-account payment); SDKs + mocks still ship it. Paste: `https://kite.trade/docs/connect/v3/mutual-funds/` (intro + any deprecation banner). Matters: determines whether §3/§4 write routes are historical-only.
- **Z-MF-2 — BUY-by-amount / SELL-by-quantity rule.** Exact required-param matrix per transaction_type. Paste: same page, place-order param table. Matters: the only param-validation rule of the surface.
- **Z-MF-3 — SIP `status` modify literals.** Legal values for pausing/reactivating a SIP (e.g. `paused`/`active`?). Paste: same page, SIP modify table. Matters: exact enum for any future consumer.
- **Z-MF-4 — `/mf/instruments` serving details.** Refresh cadence, gzip, and whether it is rate-limited like the equity master dump. Paste: same page, instruments section. Matters: completeness only (not feed-relevant).

## CLAIMS (for README reconciled table)

- MF surface = `/mf/orders`, `/mf/orders/{order_id}`, `/mf/sips`, `/mf/sips/{sip_id}`, `/mf/holdings`, `/mf/instruments` (+ Go-only `/mf/holdings/{isin}`, `/mf/allotments`, and `from`/`to` date filter on `/mf/orders`) — **Verified (CLIENT-LIB-SOURCE pykiteconnect `_routes` + gokiteconnect connect.go/mutualfunds.go)** — https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/connect.py
- MF `tradingsymbol` = the fund ISIN; MF `order_id` = UUID string; SIP `sip_id` = numeric string — **Verified (OFFICIAL-MOCK, all MF mocks)** — https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/mf_orders.json
- SIP place params: tradingsymbol, amount, instalments, frequency (weekly/monthly/quarterly per SEARCH), initial_amount, instalment_day, tag; Go adds trigger_price/step_up/sip_type; step_up on the wire is a `"DD-MM"→percent` map — **Verified (CLIENT-LIB-SOURCE both SDKs + OFFICIAL-MOCK mf_sips.json)**. The SEARCH day-enum (monthly: 1/5/10/15/20/25) is **CONTRADICTED by the official mock** (monthly rows with instalment_day 30 and 10) — conflicting-SEARCH, unresolved.
- `instalments: -1` (perpetual, Assumed) AND `instalments: 9999`/`pending: 9998` (second effectively-perpetual encoding, monthly rows) both observed; `variety` on MF orders ∈ {regular, sip, amc_sip} observed — **Verified (OFFICIAL-MOCK mf_sips.json / mf_orders.json)**.
- MF placement via the live API is reportedly DISABLED (~2020) despite SDK/mocks shipping it — **SEARCH (Kite forum)**, current state **Unknown** (Z-MF-1).
- MF instruments master is CSV with 16 columns (header verbatim in §6); `purchase_allowed`/`redemption_allowed` are 0/1 ints — **Verified (OFFICIAL-MOCK mf_instruments.csv + CLIENT-LIB-SOURCE py `_parse_mf_instruments`)**.
- No MF market data exists beyond NAV (`last_price`) in the master/holdings; no MF WebSocket path in any SDK — **Verified (CLIENT-LIB-SOURCE, grep absence, 2026-07-13)**. Not feed-relevant to tickvault.
