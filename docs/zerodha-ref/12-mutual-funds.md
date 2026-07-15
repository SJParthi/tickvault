# Zerodha Kite Connect v3 — Mutual Funds (compact reference)

> **Source:** `https://kite.trade/docs/connect/v3/mutual-funds/` — official live doc, NOW LIVE-VERIFIED.
> **Fetched-Verified:** live-verbatim via GH-runner 2026-07-14 (UTC timestamps in 00-COVERAGE-MANIFEST.md) — HTTP 200, in the fetch-log (per-URL sha256+timestamp in the manifest). Prior evidence retained as cross-check: CLIENT-LIB-SOURCE (`zerodha/pykiteconnect@master` `kiteconnect/connect.py`; `zerodha/gokiteconnect@master` `mutualfunds.go` + `connect.go` — fetched 2026-07-13) + OFFICIAL-MOCK (`zerodha/kiteconnect-mocks@main` c7a8123: `mf_orders.json`, `mf_orders_info.json`, `mf_order_response.json`, `mf_order_cancel.json`, `mf_sips.json`, `mf_sip_info.json`, `mf_sip_place.json`, `mf_sip_modify.json`, `mf_sip_cancel.json`, `mf_holdings.json`, `mf_instruments.csv` — fetched 2026-07-13) + SEARCH (2026-07-13). The 2026-07-13 note "no claim is byte-verbatim from the live page" is OBSOLETE — top-tier claims below are now cited against the live fetch.
> **Evidence tiers:** see README legend — Verified (LIVE-DOC runner 2026-07-14) is the top tier for this pack / Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) / SEARCH / Assumed / Unknown.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. Mutual funds are NOT feed-relevant to tickvault (no ticks, no candles) — this file exists only so the Kite surface map is complete.
> **Related:** `99-UNKNOWNS.md` (Z-MF-1 … Z-MF-4 below — Z-MF-1/Z-MF-2 RESOLVED by live fetch, Z-MF-3/Z-MF-4 narrowed), `03-orders.md` (contrast the equity order family). Neither `docs/dhan-ref/` nor `docs/groww-ref/` has an MF section — Kite is the only broker of the three in this repo exposing an MF API surface (Verified against the repo file listings).

---

## 1. Endpoint surface

**Verified (LIVE-DOC runner 2026-07-14 — https://kite.trade/docs/connect/v3/mutual-funds/):** the live page documents a **read-only** surface of exactly FIVE GET endpoints (its endpoint table, verbatim descriptions):

| Method | Route | Live description |
|---|---|---|
| `GET` | `/mf/orders` | "Retrieve the list of all orders (open and executed) over the last 7 days" |
| `GET` | `/mf/orders/:order_id` | "Retrieve an individual order" |
| `GET` | `/mf/sips/` | "Retrieve the list of all open SIP orders" |
| `GET` | `/mf/holdings` | "Retrieve the list of mutual fund holdings available in the DEMAT" |
| `GET` | `/mf/instruments` | "Retrieve the master list of all mutual funds available on the platform" |

Live intro (verbatim): *"The mutual fund APIs allow managing SIPs of mutual funds listed on Zerodha's Coin platform, where successful purchases are delivered to the buyer's DEMAT account. **Order placement can't be done, as order placement needs payment from the user's bank account.** The APIs are built on top of the BSE STARMF platform."* Plus a live Note: *"Dividend reinvestment schemes are currently not supported."*

**SDK-only routes (Verified CLIENT-LIB-SOURCE pykiteconnect@master `connect.py` `_routes` + gokiteconnect `connect.go`/`mutualfunds.go` — NOT documented on the live page; treat as historical/undocumented):**

| Action | Method | Route | py fn | Go-only? |
|---|---|---|---|---|
| MF orders by date range | `GET` | `/mf/orders?from=&to=` | — | Go `GetMFOrdersByDate(from, to)` |
| Place MF order | `POST` | `/mf/orders` | `place_mf_order(...)` | |
| Cancel MF order | `DELETE` | `/mf/orders/{order_id}` | `cancel_mf_order(order_id)` | |
| One SIP | `GET` | `/mf/sips/{sip_id}` | `mf_sips(sip_id)` | |
| Place SIP | `POST` | `/mf/sips` | `place_mf_sip(...)` | |
| Modify SIP | `PUT` | `/mf/sips/{sip_id}` | `modify_mf_sip(...)` | |
| Cancel SIP | `DELETE` | `/mf/sips/{sip_id}` | `cancel_mf_sip(sip_id)` | |
| One holding's trade breakdown | `GET` | `/mf/holdings/{isin}` | — | Go `GetMFHoldingInfo(isin)` → list of `MFTrade` |
| Allotted ISINs | `GET` | `/mf/allotments` | — | Go `GetMFAllottedISINs()` → `[]string` |

Same base URL / headers as the rest of the API — the live curl samples use `https://api.kite.trade` + `X-Kite-Version: 3` + `Authorization: token api_key:access_token` (Verified LIVE-DOC; see `03-orders.md` §1). Instrument identity everywhere is `tradingsymbol` = the fund **ISIN** (`INF…`) — Verified (LIVE-DOC: "tradingsymbol — ISIN of the fund" in every attribute table) + OFFICIAL-MOCK.

## 2. Placement is NOT available (RESOLVED — live page wins)

**Verified (LIVE-DOC runner 2026-07-14):** the live intro states verbatim — *"Order placement can't be done, as order placement needs payment from the user's bank account."* The live endpoint table carries NO POST/PUT/DELETE route at all (not even SIP place/modify/cancel, despite the intro's "allow managing SIPs" phrasing — no SIP-mutation section exists on the page). This CONFIRMS the 2026-07-13 SEARCH (Kite forum, incl. thread 13433) claim that placement was stopped (~2020, Coin moved to bank-account payment). The SDK placement/SIP-mutation methods and official success mocks still exist at master (Verified 2026-07-13) — the CODE surface is historical; the LIVE surface is read-only. **Z-MF-1 RESOLVED by live fetch.**

## 3. MF orders

**Verified (LIVE-DOC runner 2026-07-14):** `GET /mf/orders` "returns all orders placed in the last 7 days" (7-day window UPGRADED from SEARCH to LIVE-DOC). `GET /mf/orders/:order_id` "will return the order details **irrespective of its age**" (new live fact). The SDK-only `from`/`to` date params on `/mf/orders` stay CLIENT-LIB-SOURCE (Go) — not on the live page.

**Verified (LIVE-DOC — response attribute table + sample JSON):**

- `order_id` string — "Unique order id"; samples are **UUID strings** (e.g. `271989e0-a64e-4cf3-b4e4-afb8f38dd203`) — contrast the numeric-string equity order ids (UUID form UPGRADED to LIVE-DOC).
- `status` (null,string) — "Most common values [are] COMPLETE, REJECTED, CANCELLED, and OPEN. There may be other values as well" (explicitly open-ended).
- `status_message` — human text ("Failed orders come with human readable explanation"; samples: "AMC SIP: Insufficient balance.", "Insufficient fund. 1/5").
- `purchase_type` — "FRESH or ADDITIONAL (**null incase of SELL order**)".
- `transaction_type` — `BUY` or `SELL`.
- `amount` float64 — "Amount placed for purchase of units"; `quantity` float64 — "Number of units allotted or sold"; a **`price` float64 ("Buy or sell price")** row exists in the live attribute table but appears in NO live sample (doc/sample wart).
- `variety` — live table says "(regular, sip)" but live samples ALSO carry `amc_sip` — all three of {`regular`, `sip`, `amc_sip`} observed live (the mock-observed triple is now LIVE-DOC-observed).
- `folio` (null,string) — "Folio number generated by AMC for the completed purchase order".
- `order_timestamp` `"YYYY-MM-DD HH:MM:SS"`; `exchange_timestamp` — "**Date** on which the order was registered by the exchange. Orders that don't reach the exchange have null timestamps" (the DATE-only string is thus documented live, not a mock wart); `exchange_order_id` + `settlement_id` nullable ("Exchange settlement ID").
- `last_price` — "Last available NAV price of the fund"; `average_price` — "Allotted or sold NAV price"; `last_price_date` `YYYY-MM-DD`; `placed_by`; `fund` (full scheme name).
- `tag` — "Tag that was sent with an order to identify it (**alphanumeric, max 8 chars**)" — new live fact; contrast the equity tag limit in `03-orders.md`.

**Verified (OFFICIAL-MOCK mf_order_response.json / mf_order_cancel.json — historical, the write routes are live-disabled per §2):** place/cancel returned `{"status":"success","data":{"order_id":"<uuid>"}}`.

**Historical (CLIENT-LIB-SOURCE, write path live-disabled):** py `place_mf_order` params were `tradingsymbol` (ISIN), `transaction_type`, `quantity`, `amount`, `tag`. The BUY-by-`amount` / SELL-by-`quantity` split is consistent with the live attribute descriptions ("Amount placed for purchase of units" / "Number of units allotted or sold") but no live param matrix exists — placement is disabled, so **Z-MF-2 is RESOLVED-as-moot by live fetch** (historical inference retained, unverifiable-by-docs).

## 4. SIPs

**Verified (LIVE-DOC runner 2026-07-14):** `GET /mf/sips` "returns the list of all active and paused SIPs" (endpoint table wording: "all open SIP orders"). No SIP place/modify/cancel is documented live (§2). Live response attribute table:

- `sip_id` string — "Unique SIP id" (samples: numeric strings — UPGRADED to LIVE-DOC).
- `status` — "**ACTIVE, PAUSED or CANCELLED**" (the read-side enum, now LIVE-DOC; the *modify* literals stay undocumented since no modify endpoint exists live — Z-MF-3 narrowed).
- `frequency` — "Frequency at which order is triggered (**monthly, weekly, or quarterly**)" — the SEARCH triple UPGRADED to LIVE-DOC.
- `instalment_day` int64 — "Calendar day in a month on which SIP order to be triggered (**valid only incase of frequency monthly, else 0**)". **CORRECTION:** the 2026-07-13 SEARCH snippet's monthly day-enum (`1, 5, 10, 15, 20, 25`) is ABSENT from the live page, and live samples carry `instalment_day: 10` and `30` — the live page wins: no day-enum; any calendar day semantics, `0` for non-monthly. The old "conflicting-SEARCH" flag is CLOSED in the live page's favour.
- `instalments` int64 — "Number of instalments (**-1 in case of SIPs active until cancelled**)"; `pending_instalments` likewise "-1 in case of SIPs active until cancelled" — the `-1` = perpetual meaning is UPGRADED from Assumed to LIVE-DOC. The live monthly `amc_sip` sample rows STILL carry `instalments: 9999` / `pending_instalments: 9998` (the second effectively-perpetual encoding, observed live but undocumented — parsers must tolerate BOTH sentinels).
- `instalment_amount` (typed int64 in the live table; samples are floats, e.g. `500.0` — doc-typing wart), `completed_instalments`, `last_instalment` (timestamp), `next_instalment` (date), `created`, `transaction_type` (BUY or SELL), `dividend_type` — live table says "(growth, payout)" but live samples show `idcw` and `growth` (another doc/sample drift), `tag` (max 8 chars).
- Present in live samples but NOT in the live attribute table: `sip_type` (`sip` / `amc_sip`), `trigger_price` (0 observed), `step_up` (map of `"DD-MM" → int percent`, e.g. `{"05-05": 10}` — the wire shape is now LIVE-DOC-observed), `sip_reg_num` (nullable — exchange SIP registration), `fund`, `tradingsymbol`.
- Weekly/quarterly rows observed live with `instalment_day: 0` (matches the "else 0" doc).

**Historical (CLIENT-LIB-SOURCE, write path live-disabled):** py `place_mf_sip` params were `tradingsymbol`, `amount`, `instalments`, `frequency`, `initial_amount`, `instalment_day`, `tag`; Go added `trigger_price`, `step_up` (string), `sip_type`; modify took `amount`, `status`, `instalments`, `frequency`, `instalment_day` (+Go `step_up`). **Verified (OFFICIAL-MOCK mf_sip_place/modify/cancel.json — historical):** all three returned `{"status":"success","data":{"sip_id":"<numeric-string>"}}`; the Go `MFSIPResponse` also declares an optional `order_id` (Assumed: set when `initial_amount` triggered an immediate purchase). `mf_sip_info.json` additionally shows `fund_source: "pool"` (mock-only field, not on the live page).

## 5. Holdings

**Verified (LIVE-DOC runner 2026-07-14):** "Holdings contain the user's portfolio of allotted mutual fund units." Row fields (live attribute table + samples): `folio` (null,string — "null incase of SELL order"), `fund`, `tradingsymbol` (ISIN), `average_price` ("Allotted NAV price for a completed BUY order; Selling NAV price for completed SELL order"), `last_price` (latest NAV), `last_price_date` (**empty string observed in every live sample row** — the wart is live, not mock-only), `pnl` ("Net returns of the holding. Based on the last available NAV price."), `quantity` (fractional units, e.g. `382.488`). `pledged_quantity` appears in the live samples (0 observed) though not in the live attribute table.

**Go-only, NOT on the live page (CLIENT-LIB-SOURCE):** `GET /mf/holdings/{isin}` returns the per-holding trade breakdown (`fund`, `tradingsymbol`, `average_price`, `variety`, `exchange_timestamp`, `amount`, `folio`, `quantity` per trade) and `GET /mf/allotments` returns the ISIN list with ≥1 allotment.

## 6. MF instrument master

**Verified (LIVE-DOC runner 2026-07-14):** "Unlike the rest of the calls that return JSON, the instrument list API returns a **Gzipped CSV dump** of mutual funds supported by Zerodha's Coin platform" — gzip is now LIVE-DOC (closes half of Z-MF-4). Live CSV header (verbatim, identical to the official mock — 16 columns):

```
tradingsymbol,amc,name,purchase_allowed,redemption_allowed,minimum_purchase_amount,purchase_amount_multiplier,minimum_additional_purchase_amount,minimum_redemption_quantity,redemption_quantity_multiplier,dividend_type,scheme_type,plan,settlement_type,last_price,last_price_date
```

**Verified (LIVE-DOC — response-columns table):** `purchase_allowed`/`redemption_allowed` "0 or 1" (typed string in the live table; the py SDK casts to bool — CLIENT-LIB-SOURCE `_parse_mf_instruments`); `amc` = "AMC code as per the exchange"; `minimum_purchase_amount` (first BUY), `purchase_amount_multiplier`, `minimum_additional_purchase_amount`, `minimum_redemption_quantity`, `redemption_quantity_multiplier` — all floats; `dividend_type` "growth or payout"; `scheme_type` "**equity, debt, elss**" (live sample rows show `elss` too); `plan` "**direct or regular**"; `settlement_type` "Settlement type of the fund (**T1, T2 etc.**)" (`T3` in the live sample rows); `last_price` = NAV; `last_price_date` `YYYY-MM-DD`. Refresh cadence and rate-limiting are still NOT stated on the live page (Z-MF-4 residual). `last_price` = NAV — this is the ONLY "market data" in the MF surface; there are no MF quotes on the WebSocket (Verified-absence: no MF mode/token path in any SDK ticker; nothing MF-related on the live websocket page either).

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Z-MF-1 — RESOLVED by live fetch (2026-07-14).** Is MF order/SIP placement live? **NO** — live page verbatim: "Order placement can't be done, as order placement needs payment from the user's bank account"; the live endpoint table is GET-only (no order place/cancel, no SIP place/modify/cancel). §1/§2 rewritten; SDK write routes reclassified historical/undocumented. (Cite: https://kite.trade/docs/connect/v3/mutual-funds/ — LIVE-DOC runner 2026-07-14, in the fetch-log.)
- **Z-MF-2 — RESOLVED-as-moot by live fetch (2026-07-14).** BUY-by-amount / SELL-by-quantity param matrix: no placement param table exists on the live page because placement is disabled (Z-MF-1). The live attribute descriptions ("Amount placed for purchase of units" / "Number of units allotted or sold") are consistent with the historical inference, which stays SDK-historical only.
- **Z-MF-3 — NARROWED by live fetch (2026-07-14).** SIP `status` READ enum is now LIVE-DOC: `ACTIVE`, `PAUSED`, `CANCELLED`. Residual: the *modify* `status` literals (pause/reactivate values on `PUT /mf/sips/{sip_id}`) remain Unknown — the modify endpoint itself is no longer documented live (write surface removed). Only resolvable from historical archives or Zerodha support; low value since the route is live-disabled.
- **Z-MF-4 — NARROWED by live fetch (2026-07-14).** `/mf/instruments` is confirmed a **Gzipped CSV** (LIVE-DOC). Residual: refresh cadence and whether it is rate-limited like the equity master dump — still unstated on the live page. Matters: completeness only (not feed-relevant).

## CLAIMS (for README reconciled table)

- LIVE MF surface = exactly 5 GET endpoints: `/mf/orders` (last 7 days), `/mf/orders/:order_id` (any age), `/mf/sips/`, `/mf/holdings`, `/mf/instruments` — **Verified (LIVE-DOC runner 2026-07-14)** — https://kite.trade/docs/connect/v3/mutual-funds/ (in the fetch-log). SDK-only extras (`POST`/`DELETE /mf/orders`, SIP place/modify/cancel, `/mf/sips/{sip_id}`, Go `/mf/holdings/{isin}`, `/mf/allotments`, `from`/`to` on `/mf/orders`) remain **Verified (CLIENT-LIB-SOURCE)** but are NOT on the live page — historical/undocumented.
- MF order/SIP placement is DISABLED live ("Order placement can't be done, as order placement needs payment from the user's bank account"; APIs built on BSE STARMF; dividend reinvestment schemes not supported) — **Verified (LIVE-DOC runner 2026-07-14)**, UPGRADED from SEARCH (Kite forum); Z-MF-1 closed.
- MF `tradingsymbol` = the fund ISIN; MF `order_id` = UUID string; SIP `sip_id` = numeric string; MF `tag` is alphanumeric max 8 chars — **Verified (LIVE-DOC runner 2026-07-14** — attribute tables + samples**)** + OFFICIAL-MOCK cross-check.
- MF order `status` most common values = COMPLETE / REJECTED / CANCELLED / OPEN (explicitly open-ended); `purchase_type` FRESH/ADDITIONAL (null on SELL); `exchange_timestamp` is a DATE-only string, null when the order never reached the exchange; `variety` ∈ {regular, sip, amc_sip} observed (table lists regular/sip only); a documented `price` field appears in no sample — **Verified (LIVE-DOC runner 2026-07-14)**.
- SIP read enums: `status` ∈ ACTIVE/PAUSED/CANCELLED; `frequency` ∈ monthly/weekly/quarterly; `instalment_day` = calendar day, monthly-only, else 0 — **Verified (LIVE-DOC runner 2026-07-14)**. **CORRECTED:** the SEARCH monthly day-enum (1/5/10/15/20/25) is absent from the live page and contradicted by live samples (days 10 and 30) — the live page wins; the old conflicting-SEARCH flag is closed.
- `instalments: -1` / `pending_instalments: -1` = "SIPs active until cancelled" — **Verified (LIVE-DOC runner 2026-07-14)**, UPGRADED from Assumed. The `instalments: 9999`/`pending: 9998` second effectively-perpetual encoding is OBSERVED in the live monthly `amc_sip` samples but undocumented — parsers must tolerate BOTH sentinels.
- SIP samples carry undocumented fields `sip_type` (sip/amc_sip), `trigger_price`, `step_up` (`"DD-MM"→percent` map), `sip_reg_num` — **Verified (LIVE-DOC samples 2026-07-14** + OFFICIAL-MOCK**)**; `fund_source: "pool"` is mock-only.
- MF holdings fields incl. `pnl` ("net returns … based on last available NAV"), `average_price` BUY/SELL NAV semantics, empty-string `last_price_date` wart, sample-only `pledged_quantity` — **Verified (LIVE-DOC runner 2026-07-14)**.
- MF instruments master is a **Gzipped CSV** with the 16-column header verbatim in §6; `purchase_allowed`/`redemption_allowed` are 0/1; `scheme_type` equity/debt/elss; `plan` direct/regular; `settlement_type` T1/T2/etc. — **Verified (LIVE-DOC runner 2026-07-14)** + OFFICIAL-MOCK/CLIENT-LIB-SOURCE cross-check. Refresh cadence/rate-limit still unstated (Z-MF-4 residual).
- No MF market data exists beyond NAV (`last_price`) in the master/holdings; no MF WebSocket path in any SDK or on the live websocket page — **Verified (CLIENT-LIB-SOURCE grep absence 2026-07-13 + LIVE-DOC absence 2026-07-14)**. Not feed-relevant to tickvault.
