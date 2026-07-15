# Zerodha Kite Connect v3 — Portfolio (Holdings · Positions · Conversion · Auctions · Holdings Authorisation)

> **Source:** https://kite.trade/docs/connect/v3/portfolio/ (official live docs page — fetched verbatim 2026-07-14)
> **Fetched-Verified:** 2026-07-14 — **live-verbatim via GH-runner 2026-07-14 (UTC timestamps in 00-COVERAGE-MANIFEST.md)**; cross-checked against the 2026-07-13 CLIENT-LIB-SOURCE (pykiteconnect@master `f5531bd`, gokiteconnect@master `028ce8b`) + OFFICIAL-MOCK (kiteconnect-mocks@main `c7a8123`) evidence, which stays as corroboration
> **Evidence tiers:** see README legend — **Verified (LIVE-DOC runner 2026-07-14)** is the top tier for this pack; Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) remain as cross-check evidence; SEARCH / Assumed / Unknown as before
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. TickVault places NO Zerodha orders and holds NO Zerodha portfolio; this file exists for documentation completeness so a future authorized session needs zero re-research.
> **Related:** `01-*` (auth/headers), `03-*` (orders — product enums), `99-UNKNOWNS.md`

---

## Provenance & access boundary (honest)

`kite.trade` was 403-blocked at this sandbox's egress proxy on 2026-07-13 (the first-pass build used SDK/mock/search recovery routes). On **2026-07-14 the LIVE page was fetched verbatim by the GH-runner crawl** (run 29316744448; `kite.trade/docs/connect/v3/portfolio/` → HTTP 200, 95,783 bytes, sha256 + UTC timestamp in the crawl `fetch-log.tsv`, surfaced through `00-COVERAGE-MANIFEST.md`). This file is now upgraded against that live page; the SDK/mock citations below are RETAINED as independent corroboration.

| Tier used here | Artifact | Pin |
|---|---|---|
| **Verified (LIVE-DOC runner 2026-07-14)** | the live `kite.trade/docs/connect/v3/portfolio/` page, byte-captured | crawl run 29316744448, 2026-07-14T08:05:01Z |
| **Verified (LIVE-DOC runner 2026-07-14, support article)** | `support.zerodha.com/.../articles/difference-between-net-and-day-positions-api` | same crawl, 2026-07-14T08:09:42Z |
| **Verified (CLIENT-LIB-SOURCE pykiteconnect `kiteconnect/connect.py`)** | official Python SDK — `_routes` dict, method signatures, docstrings | `f5531bd2ed3f64e00407eead2a465225f455bef3` (2026-07-13) |
| **Verified (CLIENT-LIB-SOURCE gokiteconnect `portfolio.go` / `connect.go`)** | official Go SDK — full response structs (field NAMES + wire TYPES), URI constants, holdings-auth flow | `028ce8bf09c3979c6b043d86f7f3069eb058532d` (2026-07-13) |
| **Verified (OFFICIAL-MOCK `<file>.json`)** | zerodha/kiteconnect-mocks — canonical JSON response shapes | `c7a81238f93b4057c07257823b1bfaf30cba9ae5` (2026-07-13) |

The live page's example JSON is byte-identical in content to the official mocks (AARON/SBIN holdings rows, LEADMINI/GOLDGUINEA/SBIN positions, ASHOKLEY/BHEL/SBIN auctions) — mocks == live examples, a strong three-way corroboration. The former SEARCH-tier rows in this file are all now upgraded or corrected against the live page; no SEARCH-tier claim remains.

---

## 1. Endpoint inventory

Base URL `https://api.kite.trade`; every request carries `X-Kite-Version: 3` + `Authorization: token api_key:access_token` — **Verified (LIVE-DOC runner 2026-07-14** — every curl example on the portfolio page carries both headers**)** + **CLIENT-LIB-SOURCE pykiteconnect `connect.py`**. Response envelope: `{"status": "success", "data": …}` — **Verified (LIVE-DOC + OFFICIAL-MOCK)**.

The live page's own endpoint table lists exactly FOUR rows — **Verified (LIVE-DOC runner 2026-07-14)**, descriptions live-verbatim:

| # | Purpose (live wording) | Method + path | Tier |
|---|---|---|---|
| 1 | "Retrieve the list of long term equity holdings" | `GET /portfolio/holdings` | **Verified (LIVE-DOC runner 2026-07-14 + CLIENT-LIB-SOURCE both SDKs)** |
| 2 | "Retrieve the list of short term positions" | `GET /portfolio/positions` | **Verified (LIVE-DOC + CLIENT-LIB-SOURCE both SDKs)** |
| 3 | "Convert the margin product of an open position" | `PUT /portfolio/positions` | **Verified (LIVE-DOC + CLIENT-LIB-SOURCE both SDKs)** |
| 4 | "Retrieve the list of auctions that are currently being held" | `GET /portfolio/holdings/auctions` | **Verified (LIVE-DOC + CLIENT-LIB-SOURCE both SDKs)** |
| 5 | Holdings authorisation (eDIS) initiate | `POST /portfolio/holdings/authorise` | **Verified (LIVE-DOC runner 2026-07-14** — documented in the page's "Initiating authorisation" section (curl example), though NOT in the top endpoint table**)** + gokiteconnect `URIInitHoldingsAuth` + OFFICIAL-MOCK — NOT in pykiteconnect (verified absence in `connect.py`) |
| 6 | Holdings summary | `GET /portfolio/holdings/summary` | **Verified (CLIENT-LIB-SOURCE gokiteconnect `URIGetHoldingsSummary` + OFFICIAL-MOCK `holdings_summary.json`)** — NOT in pykiteconnect; **Verified-absence (LIVE-DOC runner 2026-07-14)**: the live portfolio page never mentions "summary" — an UNDOCUMENTED Go-SDK/mock surface |
| 7 | Holdings compact | `GET /portfolio/holdings/compact` | **Verified (CLIENT-LIB-SOURCE gokiteconnect `URIGetHoldingsCompact` + OFFICIAL-MOCK `holdings_compact.json`)** — NOT in pykiteconnect; **Verified-absence (LIVE-DOC runner 2026-07-14)**: "compact" never appears on the live page |

Note the conversion endpoint is the SAME path as the positions GET, distinguished only by HTTP verb `PUT` — **Verified (LIVE-DOC runner 2026-07-14 — the page's own endpoint table + the `curl --request PUT` example; also CLIENT-LIB-SOURCE both SDKs)**.

---

## 2. Holdings — `GET /portfolio/holdings`

Live prose — **Verified (LIVE-DOC runner 2026-07-14)**: "Holdings contain the user's portfolio of long term equity delivery stocks. An instrument in a holdings portfolio remains there indefinitely until its sold or is delisted or changed by the exchanges. Underneath it all, instruments in the holdings reside in the user's DEMAT account, as settled by exchanges and clearing institutions."

Full row shape — field NAMES + example values **Verified (LIVE-DOC example JSON == OFFICIAL-MOCK `holdings.json`)**; wire TYPES from the live attribute table (live doc labels quantities `int64`; the Go struct uses plain `int` — compatible) + **CLIENT-LIB-SOURCE gokiteconnect `type Holding`**. Descriptions below are LIVE-verbatim unless marked otherwise:

| Field | Type (live doc) | Meaning |
|---|---|---|
| `tradingsymbol` | string | "Exchange tradingsymbol of the instrument" — **LIVE-DOC** |
| `exchange` | string | `NSE` / `BSE`; a dual-listed holding is **Assumed** to produce one row per exchange (the live example carries NO dual-listed pair — every ISIN appears once; the live `isin` description "The standard ISIN representing stocks listed on multiple exchanges" gestures at multi-exchange identity but does not state per-exchange rows) |
| `instrument_token` | uint32 | "Unique instrument identifier (used for WebSocket subscriptions)" — **LIVE-DOC**; per-exchange identity (live example: SBIN BSE = 128028676 here vs SBIN NSE = 779521 in the positions example — same company, two exchanges, two tokens) |
| `isin` | string | "The standard ISIN representing stocks listed on multiple exchanges" — **LIVE-DOC**; the cross-broker join key |
| `product` | string | "Margin product applied to the holding" — **LIVE-DOC**; `CNC` in every example row |
| `price` | float64 | listed in the live attribute table with an **EMPTY description cell** — **Verified (LIVE-DOC runner 2026-07-14)**; 0 in every example row — semantics remain **Unknown** (the live page itself does not describe it; U-ref below) |
| `quantity` | int64 | "Realised Quantity(T+2)" [sic] — **LIVE-DOC**; the net holding quantity |
| `used_quantity` | int64 | "Quantity sold from the net holding quantity" — **LIVE-DOC** (upgraded from the 2026-07-13 SEARCH paraphrase "consumed by transactions") |
| `t1_quantity` | int64 | "Quantity on T+1 day after order execution. Stocks are usually delivered into DEMAT accounts on T+2" — **LIVE-DOC** (upgraded from Assumed) |
| `realised_quantity` | int64 | "Quantity delivered to Demat" — **LIVE-DOC** (upgraded from Assumed) |
| `authorised_quantity` | int64 | "Quantity authorised at the depository for sale" — **LIVE-DOC**; eDIS flow §6 governs it |
| `authorised_date` | string `"YYYY-MM-DD HH:MM:SS"` | "Date on which user can sell required holding stock" — **LIVE-DOC**; format from the example `"2025-01-17 00:00:00"` |
| `authorisation` | object | empty `{}` in the live example; NOT documented in the live attribute table — populated shape stays **Unknown** |
| `opening_quantity` | int64 | "Quantity carried forward over night" — **LIVE-DOC** (upgraded from the Assumed "quantity at day open"; same concept, live wording adopted) |
| `short_quantity` | int | present in the LIVE example JSON (0) but NOT in the live attribute table AND NOT in the gokiteconnect `Holding` struct — **Verified (LIVE-DOC runner 2026-07-14 + CLIENT-LIB-SOURCE)**: an example-only/undocumented field; parse leniently |
| `collateral_quantity` | int64 | "Quantity used as collateral" — **LIVE-DOC** |
| `collateral_type` | **null,string** | "Type of collateral" — **LIVE-DOC**; the live doc types it `null,string` — **the field can be NULL on the wire** (CORRECTED 2026-07-14: previously typed plain `string` from the Go struct; parse as nullable). `""` in the example; enum values still **Unknown** |
| `discrepancy` | bool | "Indicates whether holding has any **price** discrepancy" — **LIVE-DOC** (CORRECTED 2026-07-14: the earlier Assumed reading "broker-flagged position discrepancy" was wrong — it flags a PRICE discrepancy) |
| `average_price` | float64 | "Average price at which the net holding quantity was acquired" — **LIVE-DOC** |
| `last_price` | float64 | "Last traded market price of the instrument" — **LIVE-DOC** |
| `close_price` | float64 | "Closing price of the instrument from the last trading day" — **LIVE-DOC** |
| `pnl` | float64 | "Net returns on the stock; Profit and loss" — **LIVE-DOC** (example reconciles `(last_price − average_price) × quantity`: AARON `(352.95−161)×1 = 191.95` — **ARITH**) |
| `day_change` | float64 | "Day's change in absolute value for the stock" — **LIVE-DOC** (`last_price − close_price`, **ARITH** reconciles; raw float noise `0.5999…` on the wire) |
| `day_change_percentage` | float64 | "Day's change in percentage for the stock" — **LIVE-DOC** |
| `mtf` | object | Margin Trading Facility sub-object — present in the LIVE example JSON but NOT in the live attribute table; shape **Verified (LIVE-DOC example + OFFICIAL-MOCK + CLIENT-LIB-SOURCE gokiteconnect `MTFHolding`)**: `quantity` int, `used_quantity` int, `average_price` float, `value` float, `initial_margin` float |

Un-truncated floats on the wire (`0.5999999999999659`, `-629.2999999999993`) — parse as f64, never assume 2-dp strings — **Verified (LIVE-DOC example JSON + OFFICIAL-MOCK)**.

### 2.1 Holdings summary — `GET /portfolio/holdings/summary`

**Verified (OFFICIAL-MOCK `holdings_summary.json` + CLIENT-LIB-SOURCE gokiteconnect `HoldingSummary`)** — 6 fields, all float64: `total_pnl`, `total_pnl_percent`, `today_pnl`, `today_pnl_percent`, `invested_amount`, `current_value`. **RESOLVED by live fetch (2026-07-14):** this endpoint does NOT appear anywhere on the live v3 portfolio page (grep "summary" → zero hits) — it is an undocumented Go-SDK + mock surface. Treat as unofficial/subject to change; rate-limit class unstated anywhere.

### 2.2 Holdings compact — `GET /portfolio/holdings/compact`

**Verified (OFFICIAL-MOCK `holdings_compact.json` + CLIENT-LIB-SOURCE gokiteconnect `HoldingCompact`)** — rows of exactly `exchange`, `tradingsymbol`, `instrument_token`, `t1_quantity`, `quantity` (mock: 21 rows). **RESOLVED by live fetch (2026-07-14):** likewise ABSENT from the live page (grep "compact" → zero hits) — undocumented Go-SDK + mock surface.

---

## 3. Positions — `GET /portfolio/positions`

Live prose — **Verified (LIVE-DOC runner 2026-07-14)**: "Positions contain the user's portfolio of short to medium term derivatives (futures and options contracts) and intraday equity stocks. Instruments in the positions portfolio remain there until they're sold, or until expiry … **Equity positions carried overnight move to the holdings portfolio the next day.**"

Response `data` is an object with TWO arrays — **Verified (LIVE-DOC + OFFICIAL-MOCK `positions.json` + CLIENT-LIB-SOURCE gokiteconnect `type Positions { Net, Day }`)** — live wording:

- `net` — "the actual, current net position portfolio",
- `day` — "a snapshot of the buying and selling activity for that particular day. This is useful for computing intraday profits and losses for trading strategies."

Supplementary — **Verified (LIVE-DOC runner 2026-07-14, support article `difference-between-net-and-day-positions-api`)**: worked example — squaring off a carried NRML lot shows day quantity `-75` (the square-off trade) vs net quantity `0` (the resulting position); a fresh MIS intraday sell shows `-75` in BOTH arrays.

Position row: 29 fields — names + examples **Verified (LIVE-DOC example JSON == OFFICIAL-MOCK)**, types per the live attribute table + **CLIENT-LIB-SOURCE gokiteconnect `type Position`**:

| Group | Fields |
|---|---|
| identity | `tradingsymbol` string, `exchange` string (example includes `MCX`, `NSE`), `instrument_token` uint32 ("The numerical identifier issued by the exchange … Used for subscribing to live market data over WebSocket" — **LIVE-DOC**), `product` string (example: `NRML`, `CO`) |
| quantities | `quantity` int64 "Quantity held" (SIGNED — live day row shows `-3` for a net-short day; there is NO positionType enum, sign carries direction), `overnight_quantity` int64 "Quantity held previously and carried forward over night" — **LIVE-DOC**, `multiplier` "The quantity/lot size multiplier used for calculating P&Ls" — **LIVE-DOC types it int64**, but the gokiteconnect struct is **float64** (doc/SDK type drift, noted; example value 1000 for LEADMINI) |
| prices/P&L | `average_price` ("Average price at which the net position quantity was acquired"), `close_price`, `last_price`, `value` ("Net value of the position"), `pnl` ("Net returns on the position; Profit and loss"), `m2m`, `unrealised` ("Unrealised intraday returns"), `realised` ("Realised intraday returns") — all float64, descriptions **LIVE-DOC** |
| buy side | `buy_quantity` int64, `buy_price`, `buy_value`, `buy_m2m` ("Mark to market returns on the bought quantities") |
| sell side | `sell_quantity` int64, `sell_price`, `sell_value`, `sell_m2m` |
| day slices | `day_buy_quantity` int64, `day_buy_price`, `day_buy_value`, `day_sell_quantity` int64, `day_sell_price`, `day_sell_value` |

Semantics notes:
- A fully-closed position remains in the arrays with `quantity: 0` and realised/unrealised P&L populated (live GOLDGUINEA net row: `quantity 0`, `pnl 801`) — **Verified (LIVE-DOC example + OFFICIAL-MOCK)**.
- `m2m` — "Mark to market returns (computed based on the last close and the last traded price)" — **Verified (LIVE-DOC runner 2026-07-14)** — vs `pnl` "Net returns on the position". They diverge on carried positions (GOLDGUINEA net row: `pnl` 801 vs `m2m` 276) — **RESOLVED by live fetch**: the exact doc wording is now captured (previously Assumed).
- pykiteconnect exposes `POSITION_TYPE_DAY = "day"` / `POSITION_TYPE_OVERNIGHT = "overnight"` constants — **Verified (CLIENT-LIB-SOURCE pykiteconnect `connect.py`)** — used as the `position_type` param of conversion (§4), matching the live conversion example's `position_type=overnight`.

---

## 4. Position conversion — `PUT /portfolio/positions`

Live prose — **Verified (LIVE-DOC runner 2026-07-14)**: "All positions held are of specific margin products such as NRML, MIS etc. A position can have one and only one margin product … a user may want to covert [sic] or change a position's margin product from time to time."

Form-encoded params — **Verified (LIVE-DOC curl example + request-parameters table; corroborated by CLIENT-LIB-SOURCE pykiteconnect `convert_position()` + gokiteconnect `ConvertPositionParams`)** — ALL required in the Python signature:

| Param | Type | Values (live wording) |
|---|---|---|
| `exchange` | string | "Name of the exchange" — `NSE`/`BSE`/`NFO`/`CDS`/`BFO`/`MCX`/`BCD` (SDK constants) |
| `tradingsymbol` | string | "Tradingsymbol of the instrument" |
| `transaction_type` | string | "BUY or SELL" — **LIVE-DOC** |
| `position_type` | string | "overnight or day" (lowercase) — **LIVE-DOC** (live example: `position_type=overnight`) |
| `quantity` | **int** | "Quantity to convert" — plain integer on the wire (live example `quantity=3`; Go `url:"quantity"` int) — **LIVE-DOC** |
| `old_product` | string | "Existing margin product of the position" — `MIS` / `CNC` / `NRML` / `CO` (SDK product constants) |
| `new_product` | string | "Margin product to convert to" |

Response: `{"status": "success", "data": true}` — a bare boolean — **Verified (LIVE-DOC example + OFFICIAL-MOCK `convert_position.json`)**. The live page shows ONLY the success shape; the failure envelope for an invalid conversion remains undocumented (OPEN QUESTIONS).

### 4.1 Exiting holdings and positions — no exit API (LIVE-DOC)

**Verified (LIVE-DOC runner 2026-07-14** — a dedicated live-page section**)**: "There are no special API calls for exiting instruments from holdings and positions portfolios. The way to do it is to place an opposite BUY or SELL order depending on whether the position is a long or a short (MARKET order for an immediate exit). It is important to note that **the exit order should carry the same `product` as the existing position. If the exit order is of a different margin product, it may be treated as a new position** in the portfolio." This upgrades the earlier SDK-verified absence of an exit-all endpoint to an explicit live-doc statement — and adds the same-product exit caveat.

**Compare: Dhan** — Dhan's `POST /v2/positions/convert` documents `convertQty` as a **STRING** ("convertQty is a STRING, not integer" — repo `docs/dhan-ref/12-portfolio-positions.md` / `.claude/rules/dhan/portfolio-positions.md` rule 9, with the SDK-sends-integer wart), returns `202 Accepted`, and uses `fromProductType`/`toProductType` naming. Kite's conversion is a plain-int form param over PUT with a boolean-`data` 200 envelope — no string-quantity quirk. Dhan also has `DELETE /v2/positions` exit-all; Kite explicitly documents that **no exit API exists** (§4.1, LIVE-DOC).

**Compare: Groww** — Groww's portfolio surface is read-only (holdings + positions GET; no conversion endpoint in the documented v1 API — repo `docs/groww-ref/16-orders-margins-portfolio.md` §1 endpoint inventory).

---

## 5. Holdings auction list — `GET /portfolio/holdings/auctions`

Live prose — **Verified (LIVE-DOC runner 2026-07-14)**: "This API returns a list of auctions that are currently being held, along with details about each auction such as the auction number, the security being auctioned, the last price of the security, and the quantity of the security being offered. **Only the stocks that you hold in your demat account will be shown in the auctions list.**" (Upgraded from SEARCH.)

Row shape — **Verified (LIVE-DOC example JSON == OFFICIAL-MOCK `auctions_list.json`; types per CLIENT-LIB-SOURCE gokiteconnect `AuctionInstrument`)**: the holdings row subset (`tradingsymbol`, `exchange`, `instrument_token`, `isin`, `product`, `price`, `quantity`, `t1_quantity`, `realised_quantity`, `authorised_quantity`, `authorised_date`, `opening_quantity`, `collateral_quantity`, `collateral_type`, `discrepancy`, `average_price`, `last_price`, `close_price`, `pnl`, `day_change`, `day_change_percentage`) **plus** `auction_number` — a **string** (live example: `"20"`, `"34"`, `"7529"`) despite being numeric-looking; live attribute table: "A unique identifier for a particular auction" — **LIVE-DOC**. No `used_quantity`, no `authorisation` object, no `mtf` object in the auction rows (live example / mock / Go struct all agree).

Auction orders are placed via the orders API with `variety=auction` + the `auction_number` param — **Verified (CLIENT-LIB-SOURCE pykiteconnect `VARIETY_AUCTION = "auction"` + `place_order(..., auction_number=None)` signature)**; details belong to the orders file.

---

## 6. Holdings authorisation (eDIS) — `POST /portfolio/holdings/authorise`

The Kite analogue of Dhan's EDIS/T-PIN surface (`docs/dhan-ref/07d-edis.md`). Now fully documented on the live page — **Verified (LIVE-DOC runner 2026-07-14)**:

- **Why:** "When executing sell transactions on equity holdings, where shares have to be debited from a user's demat account, a broker either requires a PoA (Power of Attorney) or an electronic authorisation at the depository … Electronic authorisation happens centrally on the depostiory's [sic] portal (CDSL in Zerodha's case) when the user … keys in their demat PIN … The demat PIN is known only to the demat account holder and the depository, and not the broker."
- **Validity:** "The authorisations are valid for a single trading session in a day (beginning of the day till 5:30 PM, after which, the authorisations are for the next trading day)." Sell quantity need not equal the authorised `n`; "at any point, the total sell quantities, **even over multiple days**, should not exceed n" — **LIVE-DOC** (upgraded from SEARCH).
- **The 428 trigger (NEW, LIVE-DOC):** when an unauthorised sell is attempted, "the order API throws an error asking for authorisation" — live worked example: `POST /orders` throws "10 quantity needs authorisation at depository." with **HTTP status 428**. On encountering 428, initiate the authorisation flow. "By default, Kite prompts the user to authorise the maximum quantities for every stock in their holding" so subsequent sells don't re-prompt.
- **Initiate:** `POST /portfolio/holdings/authorise` with repeated `isin` + `quantity` pairs — "The isin and quantity pairs here are optional. If they're provided, authorisation is sought only for those instruments and otherwise, the entire holdings is presented for authorisation" — **LIVE-DOC** (matches the gokiteconnect doc-comment verbatim). The Go SDK's additional `type` (`mf` | `equity`), `transfer_type` (`pre` | `post` | `off` | `gift`), `exec_date` params are NOT shown on the live page — they stay **CLIENT-LIB-SOURCE (gokiteconnect `InitiateHoldingsAuth`)** tier only.
- **Response:** `{"request_id": "…"}` — **Verified (LIVE-DOC example `"na8QgCeQm05UHG6NL9sAGRzdfSF64UdB"` == OFFICIAL-MOCK `holdings_auth.json` — 32-char alphanumeric)**.
- **Redirect:** the user is sent to `https://kite.zerodha.com/connect/portfolio/authorise/holdings/:api_key/:request_id` "in a webivew [sic] or a popup" — **LIVE-DOC** (upgraded: previously known only via the Go SDK's client-side `genHolAuthURL`; the live docs state the URL directly).
- **Completion (NEW, LIVE-DOC):** the webview is redirected to `/connect/portfolio/authorise/holdings/:api_key/:request_id/finish?status=success` — mobile apps watch for this URL; `status` carries `success` or `error`. Web apps can invoke the flow via the **`authHoldings()`** call in the Publisher Javascript plugin (callback event with completion status + metadata).
- **pykiteconnect does NOT implement this endpoint** (verified absence in `connect.py` `_routes`) — live docs + Go SDK + mock.

### 6.1 Margin-pledge surface — verified absence (SDKs + live page)

**No pledge/unpledge/margin-pledge endpoint exists in either official SDK** (grep of pykiteconnect `connect.py` and gokiteconnect `portfolio.go`/`connect.go` — zero hits, 2026-07-13) — **and RESOLVED by live fetch (2026-07-14): the live v3 portfolio page contains ZERO occurrences of "pledge"** (Verified-absence, LIVE-DOC runner 2026-07-14). The only pledge visibility in the portfolio API is the holdings row's `collateral_quantity` + `collateral_type` fields (§2).

---

## 7. Compare: Dhan / Groww portfolio (quick delta table)

| Aspect | Kite v3 | Dhan v2 (`docs/dhan-ref/12-portfolio-positions.md`) | Groww v1 (`docs/groww-ref/16-orders-margins-portfolio.md`) |
|---|---|---|---|
| Holdings identity | `instrument_token` (uint32, per-exchange) + `isin` | `securityId` (string) + `isin` | `isin` (+ trading_symbol) |
| Field casing | snake_case | camelCase with two snake_case warts (`mtf_qty`, `mtf_tq_qty`) | snake_case |
| T+1 pending | `t1_quantity` | `t1Qty` | t1 split fields |
| Sellable qty | derive from `authorised_quantity`/eDIS state; unauthorised sell → **HTTP 428** (LIVE-DOC) | explicit `availableQty` | pledge/demat/t1 splits |
| Positions shape | `{net: [], day: []}`, SIGNED `quantity` | single array + `positionType` `LONG`/`SHORT`/`CLOSED` + `netQty` | single list, 17-field rows with credit/debit splits |
| Conversion | `PUT /portfolio/positions`, int `quantity`, `data: true` | `POST /v2/positions/convert`, **string `convertQty`**, `202 Accepted` | none documented |
| Exit-all | none — live docs explicitly say "no special API calls"; exit = opposite order with the SAME product (LIVE-DOC §4.1) | `DELETE /v2/positions` (may cancel pending orders too) | none documented |
| eDIS | `POST /portfolio/holdings/authorise` + web-view redirect + `/finish?status=` callback | T-PIN flow (`07d-edis.md`) | n/a |

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

Still open after the 2026-07-14 live fetch:

- **Holdings `price` field semantics** — the LIVE attribute table lists `price` float64 with an **EMPTY description cell** (Verified, LIVE-DOC 2026-07-14), and it is 0 in every example row — so the live page CANNOT answer this; semantics remain Unknown. Matters if anyone reads it as a cost/valuation input. (Only a Zerodha support ticket / live account observation can close this.)
- **Holdings `authorisation` object shape** — `{}` in the live example AND absent from the live attribute table; populated shape unknown. Matters for parsing (must stay `Option`/lenient).
- **`collateral_type` values** — narrowed by live fetch: the live doc types it `null,string` ("Type of collateral"), so it is NULLABLE, but the possible string values remain unknown. Matters for pledge-state classification.
- **Conversion failure envelope** — the live page shows only the success `data: true` shape; the error shape/status for an invalid conversion is still undocumented on the portfolio page. Cross-check https://kite.trade/docs/connect/v3/exceptions/ (the general exceptions file). Matters for typed error handling. (Adjacent NEW fact: the eDIS-missing sell error is HTTP **428** on `POST /orders` — LIVE-DOC.)
- **Dual-listed holding row shape** — still Assumed one-row-per-exchange; the live example has no dual-listed pair (the `isin` description references "stocks listed on multiple exchanges" without stating row semantics).

Resolved 2026-07-14 (keep for the 99-UNKNOWNS rebuilder to close the matching U-rows):

- **RESOLVED by live fetch — `short_quantity` doc status:** present in the LIVE example JSON but NOT in the live attribute table (and not in the Go struct) — an example-only, undocumented field. (Was: "is it documented on the live page?")
- **RESOLVED by live fetch — `holdings/summary` + `holdings/compact` doc status:** NEITHER appears anywhere on the live v3 portfolio page (grep zero hits) — undocumented Go-SDK/mock surfaces. Rate-limit class remains unstated but the doc-status question is closed.
- **RESOLVED by live fetch — `m2m` vs `pnl` exact definitions:** live wording captured — `m2m` = "Mark to market returns (computed based on the last close and the last traded price)"; `pnl` = "Net returns on the position; Profit and loss"; `unrealised`/`realised` = "Unrealised/Realised intraday returns".
- **RESOLVED by live fetch — pledge surface:** the live v3 portfolio page mentions NO pledge surface at all (zero "pledge" hits); `collateral_*` holdings fields are the only pledge-adjacent surface (§6.1).

## CLAIMS (for README reconciled table)

- Portfolio endpoints: `GET /portfolio/holdings`, `GET /portfolio/positions`, `PUT /portfolio/positions` (conversion — same path, PUT verb), `GET /portfolio/holdings/auctions`, plus `POST /portfolio/holdings/authorise` (documented in the Initiating-authorisation section) — **Verified (LIVE-DOC runner 2026-07-14)** — https://kite.trade/docs/connect/v3/portfolio/ (in fetch-log); corroborated by CLIENT-LIB-SOURCE pykiteconnect `_routes` @ `f5531bd` + gokiteconnect URI constants @ `028ce8b`
- `holdings/summary` and `holdings/compact` are ABSENT from the live v3 portfolio page — **Verified-absence (LIVE-DOC runner 2026-07-14, grep zero hits)**; they exist only as Go-SDK + mock surfaces — **Verified (CLIENT-LIB-SOURCE gokiteconnect + OFFICIAL-MOCK holdings_summary.json/holdings_compact.json)**
- Positions response is `{net: [], day: []}` with 29-field rows; direction is the SIGN of `quantity` (no LONG/SHORT enum); equity positions carried overnight move to holdings the next day — **Verified (LIVE-DOC runner 2026-07-14 + OFFICIAL-MOCK positions.json + support article difference-between-net-and-day-positions-api)** — https://kite.trade/docs/connect/v3/portfolio/
- `m2m` = "Mark to market returns (computed based on the last close and the last traded price)"; `unrealised`/`realised` = un/realised INTRADAY returns — **Verified (LIVE-DOC runner 2026-07-14)** — https://kite.trade/docs/connect/v3/portfolio/
- Holdings rows carry `t1_quantity` ("Quantity on T+1 day after order execution … usually delivered … on T+2"), `used_quantity` ("Quantity sold from the net holding quantity"), `authorised_quantity`/`authorised_date` (eDIS), `collateral_quantity`/`collateral_type` (live doc type **null,string** — nullable), `discrepancy` ("price discrepancy" flag), and an `mtf` sub-object (`quantity`, `used_quantity`, `average_price`, `value`, `initial_margin` — example-only, not in the live attribute table) — **Verified (LIVE-DOC runner 2026-07-14 + OFFICIAL-MOCK holdings.json + CLIENT-LIB-SOURCE gokiteconnect `Holding`/`MTFHolding`)** — https://kite.trade/docs/connect/v3/portfolio/
- Holdings `price` is documented as float64 with an EMPTY description on the live page (semantics Unknown); `short_quantity` and `authorisation` appear only in the example JSON, not the attribute table — **Verified (LIVE-DOC runner 2026-07-14)**
- Position conversion params: `exchange, tradingsymbol, transaction_type (BUY|SELL), position_type (overnight|day), quantity (INT — live example quantity=3), old_product, new_product`; response `data: true` — **Verified (LIVE-DOC runner 2026-07-14 + CLIENT-LIB-SOURCE pykiteconnect `convert_position` + OFFICIAL-MOCK convert_position.json)** — materially different from Dhan's string-`convertQty` + 202 pattern (repo `docs/dhan-ref/12-portfolio-positions.md`)
- NO exit API: "There are no special API calls for exiting instruments from holdings and positions" — exit by opposite order carrying the SAME `product`, else it may be treated as a NEW position — **Verified (LIVE-DOC runner 2026-07-14)** — https://kite.trade/docs/connect/v3/portfolio/
- Auction list `GET /portfolio/holdings/auctions` returns holdings-like rows plus a STRING `auction_number` ("A unique identifier for a particular auction"); only demat-held stocks appear; auction orders then use order `variety=auction` — **Verified (LIVE-DOC runner 2026-07-14 + OFFICIAL-MOCK auctions_list.json + CLIENT-LIB-SOURCE pykiteconnect `VARIETY_AUCTION`/`get_auction_instruments`)** — https://kite.trade/docs/connect/v3/portfolio/
- Holdings authorisation (eDIS): `POST /portfolio/holdings/authorise` (optional repeated isin+quantity; omitted → entire holdings presented) → `{request_id}`; user completes at `https://kite.zerodha.com/connect/portfolio/authorise/holdings/:api_key/:request_id`; completion redirect `…/finish?status=success|error`; web apps use `authHoldings()` in the Publisher JS plugin — **Verified (LIVE-DOC runner 2026-07-14 + CLIENT-LIB-SOURCE gokiteconnect `InitiateHoldingsAuth`/`genHolAuthURL` + OFFICIAL-MOCK holdings_auth.json)**; NOT implemented in pykiteconnect (verified absence); the Go SDK's extra `type`/`transfer_type`/`exec_date` params are NOT on the live page (CLIENT-LIB-SOURCE only)
- Authorisations are valid for a single trading session (till 5:30 PM, then next trading day); cumulative sells — even over multiple days — must not exceed the authorised quantity; an unauthorised sell makes `POST /orders` fail with **HTTP 428** ("… quantity needs authorisation at depository."); Kite defaults to prompting max-quantity authorisation for every held stock — **Verified (LIVE-DOC runner 2026-07-14)** — https://kite.trade/docs/connect/v3/portfolio/
- No pledge/unpledge endpoint in either official SDK AND zero "pledge" mentions on the live v3 portfolio page — **Verified-absence (CLIENT-LIB-SOURCE grep 2026-07-13 + LIVE-DOC runner 2026-07-14)**
- LIVE-DOC vs SDK type drift noted: positions `multiplier` is typed **int64** on the live page but **float64** in gokiteconnect (example value 1000); holdings quantity fields are `int64` on the live page vs `int` in Go — **Verified (LIVE-DOC runner 2026-07-14 + CLIENT-LIB-SOURCE)**
