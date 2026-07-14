# Zerodha Kite Connect v3 — Portfolio (Holdings · Positions · Conversion · Auctions · Holdings Authorisation)

> **Source:** https://kite.trade/docs/connect/v3/portfolio/ (official live docs page — NOT directly fetchable this date, see provenance)
> **Fetched-Verified:** 2026-07-13 via CLIENT-LIB-SOURCE (pykiteconnect@master `f5531bd`, gokiteconnect@master `028ce8b`) + OFFICIAL-MOCK (kiteconnect-mocks@main `c7a8123`) + SEARCH (result snippets of the live portfolio page)
> **Evidence tiers:** see README legend — Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) / SEARCH / Assumed / Unknown
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. TickVault places NO Zerodha orders and holds NO Zerodha portfolio; this file exists for documentation completeness so a future authorized session needs zero re-research.
> **Related:** `01-*` (auth/headers), `03-*` (orders — product enums), `99-UNKNOWNS.md`

---

## Provenance & access boundary (honest)

`kite.trade` was **403-blocked at this sandbox's egress proxy on 2026-07-13** (WebFetch → HTTP 403; curl → proxy `CONNECT tunnel failed, response 403`). `web.archive.org` and `r.jina.ai` were EQUALLY blocked (probe file `zerodha-probe.md`), so **no ARCHIVE-DOC or MIRROR-LIVE tier exists in this file** — the priority-1/2 routes were unattainable, not skipped. Recovery routes actually used:

| Tier used here | Artifact | Commit (pinned 2026-07-13) |
|---|---|---|
| **Verified (CLIENT-LIB-SOURCE pykiteconnect `kiteconnect/connect.py`)** | official Python SDK — `_routes` dict, method signatures, docstrings | `f5531bd2ed3f64e00407eead2a465225f455bef3` |
| **Verified (CLIENT-LIB-SOURCE gokiteconnect `portfolio.go` / `connect.go`)** | official Go SDK — full response structs (field NAMES + wire TYPES), URI constants, holdings-auth flow | `028ce8bf09c3979c6b043d86f7f3069eb058532d` |
| **Verified (OFFICIAL-MOCK `<file>.json`)** | zerodha/kiteconnect-mocks — canonical JSON response shapes | `c7a81238f93b4057c07257823b1bfaf30cba9ae5` |
| **SEARCH** | search-result snippets of the live `kite.trade/docs/connect/v3/portfolio/` page — substantive, URL-cited, NOT byte-verbatim | fetched 2026-07-13 |

Two official SDKs + official mocks agreeing on a field name/type is strong corroboration, but field **descriptions/semantics** from the live docs prose reach at most SEARCH tier here. Re-verify SEARCH rows against the live page from an unblocked network before contracting.

---

## 1. Endpoint inventory

Base URL `https://api.kite.trade`; every request carries `X-Kite-Version: 3` + `Authorization: token api_key:access_token` — **Verified (CLIENT-LIB-SOURCE pykiteconnect `connect.py`** `_default_root_uri = "https://api.kite.trade"`, `kite_header_version = "3"`; auth-header format per the `01` auth file). Response envelope: `{"status": "success", "data": …}` — **Verified (OFFICIAL-MOCK — every portfolio mock carries it)**.

| # | Purpose | Method + path | Tier |
|---|---|---|---|
| 1 | Holdings | `GET /portfolio/holdings` | **Verified (CLIENT-LIB-SOURCE pykiteconnect `_routes["portfolio.holdings"]` + gokiteconnect `URIGetHoldings`)** |
| 2 | Holdings summary | `GET /portfolio/holdings/summary` | **Verified (CLIENT-LIB-SOURCE gokiteconnect `URIGetHoldingsSummary` + OFFICIAL-MOCK `holdings_summary.json`)** — NOT in pykiteconnect (verified absence in `connect.py`) |
| 3 | Holdings compact | `GET /portfolio/holdings/compact` | **Verified (CLIENT-LIB-SOURCE gokiteconnect `URIGetHoldingsCompact` + OFFICIAL-MOCK `holdings_compact.json`)** — NOT in pykiteconnect |
| 4 | Auction instrument list | `GET /portfolio/holdings/auctions` | **Verified (CLIENT-LIB-SOURCE pykiteconnect `_routes["portfolio.holdings.auction"]` / `get_auction_instruments()` + gokiteconnect `URIAuctionInstruments`)** |
| 5 | Holdings authorisation (eDIS) initiate | `POST /portfolio/holdings/authorise` | **Verified (CLIENT-LIB-SOURCE gokiteconnect `URIInitHoldingsAuth` + OFFICIAL-MOCK `holdings_auth.json`)** — NOT in pykiteconnect |
| 6 | Positions (net + day) | `GET /portfolio/positions` | **Verified (CLIENT-LIB-SOURCE both SDKs)** |
| 7 | Convert position product | `PUT /portfolio/positions` | **Verified (CLIENT-LIB-SOURCE pykiteconnect `convert_position()` → `self._put("portfolio.positions.convert")` where the route maps to `/portfolio/positions`; gokiteconnect `ConvertPosition` → `http.MethodPut, URIConvertPosition`)** |

Note the conversion endpoint is the SAME path as the positions GET, distinguished only by HTTP verb `PUT` — **Verified (CLIENT-LIB-SOURCE, both SDKs)**.

---

## 2. Holdings — `GET /portfolio/holdings`

Full row shape — field NAMES + example values **Verified (OFFICIAL-MOCK `holdings.json`)**; wire TYPES **Verified (CLIENT-LIB-SOURCE gokiteconnect `portfolio.go` `type Holding`)**; description column is SEARCH/Assumed where marked:

| Field | Type (Go struct) | Meaning |
|---|---|---|
| `tradingsymbol` | string | exchange trading symbol |
| `exchange` | string | `NSE` / `BSE`; a dual-listed holding is **Assumed** to produce one row per exchange (the holdings mock carries NO dual-listed example — every ISIN in it appears exactly once) |
| `instrument_token` | uint32 | Kite numeric instrument identity, per exchange listing (cross-MOCK evidence, not from holdings.json alone: SBIN NSE = 779521 in `postback.json` vs SBIN BSE = 128028676 in `holdings.json` — same company, two exchanges, two tokens) |
| `isin` | string | ISIN — the cross-broker join key |
| `product` | string | `CNC` in every mock row |
| `price` | float64 | present in wire; 0 in every mock row — semantics **Unknown** (U-ref below) |
| `quantity` | int | net holding quantity |
| `used_quantity` | int | quantity already consumed by transactions — description tier **SEARCH** (live-page snippet, 2026-07-13) |
| `t1_quantity` | int | **T+1**: bought but not yet delivered to demat — description tier **Assumed** (name + Dhan/Groww analogues; the live page's exact wording not byte-captured) |
| `realised_quantity` | int | delivered/demat quantity — **Assumed** (name) |
| `authorised_quantity` | int | quantity authorised for sale via the eDIS holdings-authorisation flow (§6) — **SEARCH**: "authorisations are valid for a single trading session … till 5:30 PM"; cumulative sell quantities must not exceed it |
| `authorised_date` | string `"YYYY-MM-DD HH:MM:SS"` | date of the authorisation — format **Verified (OFFICIAL-MOCK** `"2025-01-17 00:00:00"`**)** |
| `authorisation` | object | empty `{}` in mock; shape **Unknown** |
| `opening_quantity` | int | quantity at day open — **Assumed** (name) |
| `short_quantity` | int | present in mock (0); in gokiteconnect the `Holding` struct does NOT carry it — mock/Go drift, **Verified-absence (CLIENT-LIB-SOURCE gokiteconnect)** of the field in the Go struct |
| `collateral_quantity` | int | quantity pledged as collateral |
| `collateral_type` | string | `""` in mock; enum values **Unknown** |
| `discrepancy` | bool | broker-flagged position discrepancy — **Assumed** (name) |
| `average_price` | float64 | average buy cost |
| `last_price` | float64 | LTP |
| `close_price` | float64 | previous close |
| `pnl` | float64 | unrealised P&L (mock: `(last_price − average_price) × quantity` reconciles: AARON `(352.95−161)×1 = 191.95` — **ARITH**) |
| `day_change` | float64 | `last_price − close_price` (AARON `352.95−352.35 = 0.6` — **ARITH**; raw float noise `0.5999…` on the wire) |
| `day_change_percentage` | float64 | day change % vs close (**ARITH** reconciles) |
| `mtf` | object | Margin Trading Facility sub-object — **Verified (OFFICIAL-MOCK + CLIENT-LIB-SOURCE gokiteconnect `MTFHolding`)**: `quantity` int, `used_quantity` int, `average_price` float, `value` float, `initial_margin` float |

Un-truncated floats on the wire (`0.5999999999999659`, `-629.2999999999993`) — parse as f64, never assume 2-dp strings — **Verified (OFFICIAL-MOCK)**.

### 2.1 Holdings summary — `GET /portfolio/holdings/summary`

**Verified (OFFICIAL-MOCK `holdings_summary.json` + CLIENT-LIB-SOURCE gokiteconnect `HoldingSummary`)** — 6 fields, all float64: `total_pnl`, `total_pnl_percent`, `today_pnl`, `today_pnl_percent`, `invested_amount`, `current_value`. Whether this endpoint appears on the v3 docs page is **Unknown** (Go-SDK + mock only).

### 2.2 Holdings compact — `GET /portfolio/holdings/compact`

**Verified (OFFICIAL-MOCK `holdings_compact.json` + CLIENT-LIB-SOURCE gokiteconnect `HoldingCompact`)** — rows of exactly `exchange`, `tradingsymbol`, `instrument_token`, `t1_quantity`, `quantity` (mock: 21 rows). Docs-page status likewise **Unknown**.

---

## 3. Positions — `GET /portfolio/positions`

Response `data` is an object with TWO arrays — **Verified (OFFICIAL-MOCK `positions.json` + CLIENT-LIB-SOURCE gokiteconnect `type Positions { Net, Day }`)**:

- `net` — overall positions (today's activity folded onto carried-forward overnight quantity),
- `day` — today's activity alone.

Position row: 29 fields — names + examples **Verified (OFFICIAL-MOCK)**, types **Verified (CLIENT-LIB-SOURCE gokiteconnect `type Position`)**:

| Group | Fields |
|---|---|
| identity | `tradingsymbol` string, `exchange` string (mock includes `MCX`, `NSE`), `instrument_token` uint32, `product` string (mock: `NRML`, `CO`) |
| quantities | `quantity` int (SIGNED — mock day row shows `-3` for a net-short day; there is NO positionType enum, sign carries direction), `overnight_quantity` int, `multiplier` float64 (mock: 1000 for LEADMINI — contract multiplier) |
| prices/P&L | `average_price`, `close_price`, `last_price`, `value`, `pnl`, `m2m`, `unrealised`, `realised` — all float64 |
| buy side | `buy_quantity` int, `buy_price`, `buy_value`, `buy_m2m` |
| sell side | `sell_quantity` int, `sell_price`, `sell_value`, `sell_m2m` |
| day slices | `day_buy_quantity` int, `day_buy_price`, `day_buy_value`, `day_sell_quantity` int, `day_sell_price`, `day_sell_value` |

Semantics notes:
- A fully-closed position remains in the arrays with `quantity: 0` and realised/unrealised P&L populated (mock GOLDGUINEA net row: `quantity 0`, `pnl 801`) — **Verified (OFFICIAL-MOCK)**.
- `m2m` vs `pnl` differ on carried positions (mock GOLDGUINEA net row: `pnl` 801 vs `m2m` 276) — m2m is marked-to-market for the day; exact doc wording **SEARCH-unavailable → Assumed**.
- pykiteconnect exposes `POSITION_TYPE_DAY = "day"` / `POSITION_TYPE_OVERNIGHT = "overnight"` constants — **Verified (CLIENT-LIB-SOURCE pykiteconnect `connect.py`)** — used as the `position_type` param of conversion (§4), matching the two array names.

---

## 4. Position conversion — `PUT /portfolio/positions`

**Verified (CLIENT-LIB-SOURCE pykiteconnect `convert_position()` + gokiteconnect `ConvertPositionParams`)** — form-encoded params, ALL required in the Python signature:

| Param | Type (Go) | Values |
|---|---|---|
| `exchange` | string | `NSE`/`BSE`/`NFO`/`CDS`/`BFO`/`MCX`/`BCD` (SDK constants) |
| `tradingsymbol` | string | |
| `transaction_type` | string | `BUY` / `SELL` |
| `position_type` | string | `day` / `overnight` (lowercase — SDK constants) |
| `quantity` | **int** | plain integer on the wire (Go `url:"quantity"` int; Python passes the int through) |
| `old_product` | string | `MIS` / `CNC` / `NRML` / `CO` (SDK product constants) |
| `new_product` | string | |

Response: `{"status": "success", "data": true}` — a bare boolean — **Verified (OFFICIAL-MOCK `convert_position.json`)**.

**Compare: Dhan** — Dhan's `POST /v2/positions/convert` documents `convertQty` as a **STRING** ("convertQty is a STRING, not integer" — repo `docs/dhan-ref/12-portfolio-positions.md` / `.claude/rules/dhan/portfolio-positions.md` rule 9, with the SDK-sends-integer wart), returns `202 Accepted`, and uses `fromProductType`/`toProductType` naming. Kite's conversion is a plain-int form param over PUT with a boolean-`data` 200 envelope — no string-quantity quirk. Dhan also has `DELETE /v2/positions` exit-all; **no exit-all endpoint exists in either Kite SDK** (verified absence in `connect.py` routes + `portfolio.go`).

**Compare: Groww** — Groww's portfolio surface is read-only (holdings + positions GET; no conversion endpoint in the documented v1 API — repo `docs/groww-ref/16-orders-margins-portfolio.md` §1 endpoint inventory).

---

## 5. Holdings auction list — `GET /portfolio/holdings/auctions`

pykiteconnect: `get_auction_instruments()` — docstring "Retrieves list of available instruments for a auction session" [sic] — **Verified (CLIENT-LIB-SOURCE pykiteconnect)**. Row shape — **Verified (OFFICIAL-MOCK `auctions_list.json` + CLIENT-LIB-SOURCE gokiteconnect `AuctionInstrument`)**: the holdings row subset (`tradingsymbol`, `exchange`, `instrument_token`, `isin`, `product`, `price`, `quantity`, `t1_quantity`, `realised_quantity`, `authorised_quantity`, `authorised_date`, `opening_quantity`, `collateral_quantity`, `collateral_type`, `discrepancy`, `average_price`, `last_price`, `close_price`, `pnl`, `day_change`, `day_change_percentage`) **plus** `auction_number` — a **string** (mock: `"20"`, `"34"`) despite being numeric-looking. No `used_quantity`, no `authorisation` object, no `mtf` object in the auction rows (mock/Go struct agree).

Only stocks held in the demat account appear in the auction list — tier **SEARCH** (live-page snippet, 2026-07-13). Auction orders are placed via the orders API with `variety=auction` + the `auction_number` param — **Verified (CLIENT-LIB-SOURCE pykiteconnect `VARIETY_AUCTION = "auction"` + `place_order(..., auction_number=None)` signature)**; details belong to the orders file.

---

## 6. Holdings authorisation (eDIS) — `POST /portfolio/holdings/authorise`

The Kite analogue of Dhan's EDIS/T-PIN surface (`docs/dhan-ref/07d-edis.md`). **Verified (CLIENT-LIB-SOURCE gokiteconnect `InitiateHoldingsAuth`)**:

- POST params (all optional): `type` (`mf` | `equity`), `transfer_type` (`pre` | `post` | `off` | `gift`), `exec_date`, plus repeated `isin` + `quantity` pairs. If no instruments are given, "the entire holdings is presented for authorisation" (Go doc-comment verbatim).
- Response: `{"request_id": "…"}` — **Verified (OFFICIAL-MOCK `holdings_auth.json`** — 32-char alphanumeric request_id**)**; the Go SDK also synthesizes `redirect_url` CLIENT-side as `https://kite.zerodha.com/connect/portfolio/authorise/holdings/<api_key>/<request_id>` (**Verified — `genHolAuthURL` + `kiteBaseURI` in `connect.go`**) — the user must complete the CDSL authorisation in that web view.
- Validity: authorisations last a single trading session ("beginning of the day till 5:30 PM, after which, the authorisations are for the next trading day"); sell quantity need not equal the authorised amount, but cumulative sells must not exceed it — tier **SEARCH** (live-page snippets, 2026-07-13).
- **pykiteconnect does NOT implement this endpoint** (verified absence in `connect.py` `_routes`) — Go SDK + mock only.

### 6.1 Margin-pledge surface — verified absence in the SDKs

**No pledge/unpledge/margin-pledge endpoint exists in either official SDK** — grep of pykiteconnect `connect.py` and gokiteconnect `portfolio.go`/`connect.go` for "pledge" returns zero hits (**Verified-absence, CLIENT-LIB-SOURCE, 2026-07-13**). The read-only pledge visibility is the holdings row's `collateral_quantity` + `collateral_type` fields (§2). Whether the live v3 docs describe any pledge-adjacent surface beyond those fields is **Unknown** (page not fetchable) → OPEN QUESTIONS.

---

## 7. Compare: Dhan / Groww portfolio (quick delta table)

| Aspect | Kite v3 | Dhan v2 (`docs/dhan-ref/12-portfolio-positions.md`) | Groww v1 (`docs/groww-ref/16-orders-margins-portfolio.md`) |
|---|---|---|---|
| Holdings identity | `instrument_token` (uint32, per-exchange) + `isin` | `securityId` (string) + `isin` | `isin` (+ trading_symbol) |
| Field casing | snake_case | camelCase with two snake_case warts (`mtf_qty`, `mtf_tq_qty`) | snake_case |
| T+1 pending | `t1_quantity` | `t1Qty` | t1 split fields |
| Sellable qty | derive from `authorised_quantity`/eDIS state | explicit `availableQty` | pledge/demat/t1 splits |
| Positions shape | `{net: [], day: []}`, SIGNED `quantity` | single array + `positionType` `LONG`/`SHORT`/`CLOSED` + `netQty` | single list, 17-field rows with credit/debit splits |
| Conversion | `PUT /portfolio/positions`, int `quantity`, `data: true` | `POST /v2/positions/convert`, **string `convertQty`**, `202 Accepted` | none documented |
| Exit-all | none (SDK-verified absence) | `DELETE /v2/positions` (may cancel pending orders too) | none documented |
| eDIS | `POST /portfolio/holdings/authorise` + web-view redirect | T-PIN flow (`07d-edis.md`) | n/a |

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Holdings `price` field semantics** — always 0 in mocks, undocumented in SDK structs. Paste: https://kite.trade/docs/connect/v3/portfolio/ (holdings field table). Matters if anyone reads it as a cost/valuation input.
- **Holdings `authorisation` object shape** — `{}` in the mock; populated shape unknown. Paste: same URL. Matters for parsing (must stay `Option`/lenient).
- **`collateral_type` enum values** — `""` in mock; live values unknown. Paste: same URL. Matters for pledge-state classification.
- **`short_quantity` doc status** — present in mock `holdings.json`, ABSENT from the gokiteconnect `Holding` struct; is it documented on the live page? Paste: same URL. Matters as a mock/SDK drift signal.
- **`holdings/summary` + `holdings/compact` doc status** — Go SDK + mocks only; are they on the v3 docs page (and rate-limited differently)? Paste: same URL. Matters for choosing the cheap polling endpoint.
- **`m2m` vs `pnl` exact definitions** — mock arithmetic shows they diverge on carried positions; live-doc wording not captured. Paste: same URL (positions section). Matters for any P&L reconciliation.
- **Pledge surface beyond `collateral_*` fields** — SDK-verified absent; does the live v3 docs page mention margin pledge at all? Paste: same URL. Matters to close the §6.1 Unknown.
- **Conversion failure envelope** — mock only shows success `data: true`; the error shape/status for an invalid conversion is unknown. Paste: same URL + https://kite.trade/docs/connect/v3/exceptions/ . Matters for typed error handling.

## CLAIMS (for README reconciled table)

- Portfolio endpoints: `GET /portfolio/holdings`, `GET /portfolio/positions`, `PUT /portfolio/positions` (conversion — same path, PUT verb), `GET /portfolio/holdings/auctions` — **Verified (CLIENT-LIB-SOURCE pykiteconnect `connect.py` `_routes` @ `f5531bd` + gokiteconnect `connect.go` URI constants @ `028ce8b`)** — https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/connect.py
- Positions response is `{net: [], day: []}` with 29-field rows; direction is the SIGN of `quantity` (no LONG/SHORT enum) — **Verified (OFFICIAL-MOCK positions.json + CLIENT-LIB-SOURCE gokiteconnect portfolio.go)** — https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/positions.json
- Holdings rows carry `t1_quantity` (T+1), `used_quantity`, `authorised_quantity`/`authorised_date` (eDIS), `collateral_quantity`/`collateral_type`, and an `mtf` sub-object (`quantity`, `used_quantity`, `average_price`, `value`, `initial_margin`) — **Verified (OFFICIAL-MOCK holdings.json + CLIENT-LIB-SOURCE gokiteconnect `Holding`/`MTFHolding`)** — https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/holdings.json
- Position conversion params: `exchange, tradingsymbol, transaction_type, position_type (day|overnight), quantity (INT), old_product, new_product`; response `data: true` — **Verified (CLIENT-LIB-SOURCE pykiteconnect `convert_position` + OFFICIAL-MOCK convert_position.json)** — materially different from Dhan's string-`convertQty` + 202 pattern (repo `docs/dhan-ref/12-portfolio-positions.md`)
- Auction list `GET /portfolio/holdings/auctions` returns holdings-like rows plus a STRING `auction_number`; auction orders then use order `variety=auction` — **Verified (OFFICIAL-MOCK auctions_list.json + CLIENT-LIB-SOURCE pykiteconnect `VARIETY_AUCTION`/`get_auction_instruments`)** — https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/auctions_list.json
- Holdings authorisation (eDIS): `POST /portfolio/holdings/authorise` (optional `type` mf|equity, `transfer_type` pre|post|off|gift, `exec_date`, repeated isin+quantity) → `{request_id}`; user completes at `https://kite.zerodha.com/connect/portfolio/authorise/holdings/<api_key>/<request_id>` — **Verified (CLIENT-LIB-SOURCE gokiteconnect `InitiateHoldingsAuth`/`genHolAuthURL` + OFFICIAL-MOCK holdings_auth.json)**; NOT implemented in pykiteconnect (verified absence)
- Authorisations are valid for a single trading session (till 5:30 PM, then next trading day); cumulative sells must not exceed `authorised_quantity` — **SEARCH (kite.trade/docs/connect/v3/portfolio/ result snippets, 2026-07-13)** — https://kite.trade/docs/connect/v3/portfolio/
- Holdings summary (`/portfolio/holdings/summary`, 6 float fields) and compact (`/portfolio/holdings/compact`, 5-field rows) exist as Go-SDK + mock surfaces; live-doc status Unknown — **Verified (CLIENT-LIB-SOURCE gokiteconnect + OFFICIAL-MOCK holdings_summary.json/holdings_compact.json)**
- No pledge/unpledge endpoint and no exit-all-positions endpoint exist in either official SDK — **Verified-absence (CLIENT-LIB-SOURCE pykiteconnect connect.py + gokiteconnect portfolio.go, grep 2026-07-13)**; live-doc status of a pledge surface is Unknown
