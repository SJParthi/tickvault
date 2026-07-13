# 16 — Orders / Smart Orders / Portfolio / Margins / Annexure Enums / GA Error Codes

> **Source:** https://groww.in/trade-api/docs/curl/orders · https://groww.in/trade-api/docs/curl/smart-orders · https://groww.in/trade-api/docs/curl/portfolio · https://groww.in/trade-api/docs/curl/margin · https://groww.in/trade-api/docs/curl/annexures (+ the python-sdk twins of each page)
> **Fetched/Verified:** 2026-07-13 (compiled this date from the 2026-07-03 lossless capture + 2026-07-13 live cross-checks — see provenance note)
> **Related:** `17-token-lifecycle.md` (auth), `15-rate-limits-and-capacity.md` (families/budgets), `13-annexures-enums.md` (python-sdk annexure twin), `99-UNKNOWNS.md`

> **⚠ NOT USED BY TICKVAULT.** TickVault places NO Groww orders — the Groww feed is capture + candles + cross-verification only (`.claude/rules/project/groww-second-feed-scope-2026-06-19.md` §1/§33: no strategy wiring, no order path, `dry_run = true`). This file exists for DOCUMENTATION COMPLETENESS per the 2026-07-13 full-coverage directive, so a future authorized session needs zero re-research. Any code consuming this surface requires a fresh dated operator quote FIRST.

---

## Provenance & evidence tiers (honest access boundary)

`groww.in` was **403-blocked at this sandbox's egress proxy on 2026-07-13** (WebFetch → HTTP 403; `curl` via proxy → `CONNECT tunnel failed, response 403`, "gateway answered 403 to CONNECT (policy denial)"; web.archive.org equally blocked). Direct byte-verification against the live pages was therefore impossible this date. Tiers used:

| Tier | Meaning | Trust |
|---|---|---|
| **LIVE-DOC (capture 2026-07-03)** | the 26-page scripted lossless capture of the official docs ("heading count, table count, and code-block count … compared against the live HTML for all 26 pages — 100% match, zero loss") — the pack's authority | Highest available |
| **WS (2026-07-13)** | search-backend extraction of the named LIVE page on 2026-07-13 (substantive, URL-cited, not byte-verbatim) | Corroboration + freshness signal |
| **3RD-PARTY** | third-party SDK/README readings — flagged, never authoritative | Low |
| **Unknown** | not established; exact probe question given | — |

Staleness caveat: the live docs have PROVABLY drifted in ≥1 place since 2026-07-03 (a "COMMODITY for commodity contracts" mention on the live-data page absent from the capture), so any capture-only row should be re-verified from an unblocked network before contracting.

---

## 1. Endpoint inventory

Common to every `api.groww.in` call: headers `Authorization: Bearer {ACCESS_TOKEN}` + `Accept: application/json` + `X-API-VERSION: 1.0` (all mandatory; POST adds `Content-Type: application/json`). Base `https://api.groww.in`. Envelope: `{"status": "SUCCESS", "payload": {…}}` / `{"status": "FAILURE", "error": {…}}` (§5).

| # | Purpose | Method + URL | Rate-limit family | One line | Tier |
|---|---|---|---|---|---|
| 1 | Place order | `POST /v1/order/create` | Orders (10/s · 250/min) | New order: trading_symbol, quantity, price, trigger_price, validity, exchange, segment, product, order_type, transaction_type, optional `order_reference_id` (8–20 alphanumeric, ≤2 hyphens) | LIVE-DOC + WS |
| 2 | Modify order | `POST /v1/order/modify` | Orders | "All pending and open orders can be modified" — quantity/price/trigger_price/order_type + segment + groww_order_id | LIVE-DOC + WS |
| 3 | Cancel order | `POST /v1/order/cancel` | Orders | Cancel by `segment` + `groww_order_id` | LIVE-DOC + WS |
| 4 | Order status | `GET /v1/order/status/{groww_order_id}?segment=` | Non Trading (20/s · 500/min) | 5-field status subset (see §2 note) | LIVE-DOC + WS |
| 5 | Order status by reference | `GET /v1/order/status/reference/{order_reference_id}?segment=` | Non Trading | Same 5-field response, keyed by the user reference id | LIVE-DOC (capture resolves the URL the 2026-07-13 research marked Unknown) |
| 6 | Order list | `GET /v1/order/list?segment=&page=&page_size=` | Non Trading | Day's orders ("open, pending, and executed"); max page_size 100; rows = the §2 full schema | LIVE-DOC + WS |
| 7 | Order detail | `GET /v1/order/detail/{groww_order_id}?segment=` | Non Trading | Full single-order detail — the §2 22-field payload | LIVE-DOC + WS |
| 8 | Trades for order | `GET /v1/order/trades/{groww_order_id}?segment=&page=&page_size=` | Non Trading ("Trade list") | Per-order fills (an order can fill 20-50-30); max page_size 50; rows carry groww/exchange trade+order ids, `settlement_number` | LIVE-DOC + WS |
| 9 | Holdings | `GET /v1/holdings/user` | Non Trading | "current stock holdings of the user stored in the user's DEMAT account" — 12-field rows (isin, quantity, average_price, pledge/demat/t1 splits) | LIVE-DOC + WS |
| 10 | Positions (all) | `GET /v1/positions/user?segment=` | Non Trading | All positions — 17-field rows incl. credit/debit + carry-forward splits + `realised_pnl` | LIVE-DOC + WS |
| 11 | Position for symbol | `GET /v1/positions/trading-symbol?trading_symbol=&segment=` | Non Trading | Same row shape, one instrument | LIVE-DOC + WS |
| 12 | User margin (funds) | `GET /v1/margins/detail/user` | Non Trading ("Margin") | Available margin/funds — §3.1 full table | LIVE-DOC + WS |
| 13 | Margin calculator | `POST /v1/margins/detail/orders?segment=` | Non Trading (Assumed — the family table names "Margin" without splitting user vs calculator) | "Calculate the required margin for a single order or basket of orders" — body is a JSON **array**; "Basket orders are supported for `FNO` and `COMMODITY` segments" | LIVE-DOC + WS |
| 14 | Smart order create (GTT/OCO) | `POST /v1/order-advance/create` | **Unknown** (Orders Assumed; the 3-family table predates Smart Orders — probe the live table for a 4th row) | GTT = single trigger; OCO = target+SL, one cancels other. "The `COMMODITY` segment is not supported for Smart Orders. OCO orders for `CASH` segment are currently not supported." | LIVE-DOC + WS |
| 15 | Smart order modify | `PUT /v1/order-advance/modify/{smart_order_id}` | as #14 | Modify an active smart order | LIVE-DOC |
| 16 | Smart order cancel | `POST /v1/order-advance/cancel/{segment}/{smart_order_type}/{smart_order_id}` | as #14 | Cancel an active smart order | LIVE-DOC |
| 17 | Smart order status | `GET /v1/order-advance/status/{segment}/{smart_order_type}/internal/{smart_order_id}` | as #14 | Status of one smart order | LIVE-DOC |
| 18 | Smart order list | `GET /v1/order-advance/list?segment=&smart_order_type=&status=&page=&page_size=&start_date_time=&end_date_time=` | as #14 | Filterable list of smart orders | LIVE-DOC |
| 19 | User profile | `GET /v1/user/detail` | Non Trading (Assumed — not enumerated) | User profile (SDK `get_user_profile`, v1.4.0) | LIVE-DOC |

Order-update WebSocket stream (fields `growwOrderId`, `exchangeOrderId`, `orderStatus`, `filledQty`, `avgFillPrice`, …) exists on the feed surface — documented in `07-feed-websocket-streaming.md`, listed here only for completeness.

---

## 2. Order detail/list response — FULL verbatim field table (22 payload fields)

Verbatim Response Schema of `GET /v1/order/detail/{groww_order_id}` — LIVE-DOC (capture 2026-07-03) of https://groww.in/trade-api/docs/curl/orders; the `GET /v1/order/list` row schema is identical.

| Name | Type | Description |
| --- | --- | --- |
| status | string | SUCCESS if request is processed successfully, FAILURE if the request failed |
| groww_order_id | string | Order id generated by Groww for an order |
| trading_symbol | string | Groww specific trading symbol of the instrument |
| order_status | string | Current status of the placed order (annexure enum, §4.1) |
| remark | string | Remark for the order |
| quantity | integer | Quantity of the instrument to order |
| price | decimal | Price of the instrument in rupees case of Limit order |
| trigger_price | decimal | Trigger price in rupees for the order |
| filled_quantity | integer | Quantity of the order which has been executed. |
| remaining_quantity | integer | Quantity remained to be filled |
| average_fill_price | decimal | Avg price of the order placed |
| deliverable_quantity | integer | Deliverable quantity |
| amo_status | string | Status of the order placed after market (Not applicable during market hours) — enum §4.2 |
| validity | string | Validity of the order — enum §4.8 |
| exchange | string | Stock exchange — enum §4.3 |
| order_type | string | Order type — enum §4.5 |
| transaction_type | string | Transaction type of the trade — enum §4.7 |
| segment | string | Segment of the instrument such as CASH, FNO etc. — enum §4.4 |
| product | string | Product type — enum §4.6 |
| created_at | string | Order created at date and time |
| exchange_time | string | Date and Time at which order was placed at exchange |
| trade_date | string | Date on which trade has taken place |
| order_reference_id | string | User provided reference id to track the status of an order |

(`status` is the envelope field; the 22 payload fields are `groww_order_id` … `order_reference_id`.)

Notes (LIVE-DOC, same page):

- **`GET /v1/order/status/*` returns a 5-field SUBSET, not this table:** `groww_order_id`, `order_status`, `remark`, `filled_quantity`, `order_reference_id`. The 2026-07-13 research file that labeled the 22-field table "order status response" was actually describing order DETAIL — corrected here.
- Timestamp formats are inconsistent in the doc's own example: `created_at`/`exchange_time` = `"2023-10-01T10:15:30"` (naive ISO) while `trade_date` = `"2019-08-24T14:15:22Z"` (Zulu). Wire truth **Unknown** — probe with a live order.
- `order_reference_id` constraint verbatim (place-order request schema): "User provided 8 to 20 length alphanumeric string with atmost two hypens (-)." [sic — doc's own spelling]
- The doc's example values show `order_status: "OPEN"` — NOT an annexure value; see §4.1 flag.

---

## 3. Margin endpoints — FULL verbatim field tables

### 3.1 `GET /v1/margins/detail/user` — response ("All prices in rupees.")

Verbatim Response Schema — LIVE-DOC (capture 2026-07-03) of https://groww.in/trade-api/docs/curl/margin. The response nests `fno_margin_details` / `equity_margin_details` / `commodity_margin_details` objects; the doc's schema table flattens their fields:

| Name | Type | Description |
| --- | --- | --- |
| status | string | SUCCESS if request is processed successfully, FAILURE if the request failed |
| clear_cash | decimal | Clear cash available |
| net_margin_used | decimal | Net margin used |
| brokerage_and_charges | decimal | Brokerage and charges |
| collateral_used | decimal | Collateral used |
| collateral_available | decimal | Collateral available |
| adhoc_margin | decimal | Adhoc margin available |
| net_fno_margin_used | decimal | Net FnO margin used |
| span_margin_used | decimal | Span Margin Used |
| exposure_margin_used | decimal | Exposure Margin Used |
| future_balance_available | decimal | Future Balance Available |
| option_buy_balance_available | decimal | Option Buy Balance Available |
| option_sell_balance_available | decimal | Option Sell Balance Available |
| net_equity_margin_used | decimal | Net equity margin used |
| cnc_margin_used | decimal | CNC margin used |
| mis_margin_used | decimal | MIS margin used |
| cnc_balance_available | decimal | CNC balance available |
| mis_balance_available | decimal | MIS balance available |
| commodity_span_margin | decimal | Commodity Span Margin |
| commodity_exposure_margin | decimal | Commodity Exposure Margin |
| commodity_tender_margin | decimal | Commodity Tender Margin |
| commodity_special_margin | decimal | Commodity Special Margin |
| commodity_additional_margin | decimal | Commodity Additional Margin |
| commodity_unrealised_m2m | decimal | Commodity Unrealised Mark-to-Market |
| commodity_realised_m2m | decimal | Commodity Realised Mark-to-Market |

> Correction vs the 2026-07-13 research pass: `commodity_margin_details`' 7 nested fields were marked "Unknown — probe" there; the capture carries them in full (above). The capture also refutes the search-mediated sample values quoted in research (`clear_cash: 96.21` etc. — the captured example shows `5000` etc.); field NAMES agree across both.

### 3.2 `POST /v1/margins/detail/orders` (margin calculator) — request

Body is a JSON **array** of order objects (basket = multiple elements; "Basket orders are supported for `FNO` and `COMMODITY` segments"); `segment` travels as a QUERY parameter (`?segment=CASH`) per the captured curl example, yet the request-schema table lists it as required — both shown verbatim below. Verbatim Request schema — LIVE-DOC (capture 2026-07-03):

| Name | Type | Description |
| --- | --- | --- |
| trading_symbol `*` | string | Trading Symbol of the instrument as defined by the exchange |
| quantity `*` | integer | Quantity of the instrument to order |
| price | decimal | Price of the instrument in rupees case of Limit order |
| exchange `*` | string | Stock exchange |
| segment `*` | string | Segment of the instrument such as CASH, FNO, COMMODITY etc. |
| product `*` | string | Product type |
| order_type `*` | string | Order type |
| transaction_type `*` | string | Transaction type of the trade |

(`*` = required. The example body comment: `"price": 100, // Optional: Price (include for limit orders; omit or adjust if not applicable).`)

### 3.3 `POST /v1/margins/detail/orders` — response ("All prices in rupees.")

Verbatim Response Schema — LIVE-DOC (capture 2026-07-03):

| Name | Type | Description |
| --- | --- | --- |
| status | string | SUCCESS if request is processed successfully, FAILURE if the request failed |
| exposure_required | decimal | Required exposure for the order |
| span_required | decimal | Required span margin for the order |
| option_buy_premium | decimal | Buy premium required for the order |
| brokerage_and_charges | decimal | Brokerage and Charges applied for the orders |
| total_requirement | decimal | Net margin required |
| cash_cnc_margin_required | decimal | Margin required applicable for CNC orders |
| cash_mis_margin_required | decimal | Margin required applicable for MIS orders |
| physical_delivery_margin_requirement | decimal | Physical delivery margin required if applicable |

> Correction vs the 2026-07-13 research pass: `physical_delivery_margin_requirement` (9th field) was missing from the research table; the capture carries it.

---

## 4. Annexure enums — VERBATIM

All tables LIVE-DOC (capture 2026-07-03) of https://groww.in/trade-api/docs/curl/annexures — byte-identical to the python-sdk annexures page already vendored as `13-annexures-enums.md` (the SDK page additionally prefixes `GrowwAPI.*` constant names). Candle Interval lives with the historical surface — see `11-historical-candles.md`.

### 4.1 Order Status (12 values)

| Value | Description |
| --- | --- |
| NEW | Order is newly created and pending for further processing |
| ACKED | Order has been acknowledged by the system |
| TRIGGER_PENDING | Order is waiting for a trigger event to be executed |
| APPROVED | Order has been approved and is ready for execution |
| REJECTED | Order has been rejected by the system |
| FAILED | Order execution has failed |
| EXECUTED | Order has been successfully executed |
| DELIVERY_AWAITED | Order has been executed and waiting for delivery |
| CANCELLED | Order has been cancelled |
| CANCELLATION_REQUESTED | Request to cancel the order has been initiated |
| MODIFICATION_REQUESTED | Request to modify the order has been initiated |
| COMPLETED | Order has been completed |

> **⚠ Undocumented `OPEN` value:** the orders page's OWN response examples (place, modify, status, list, detail — capture 2026-07-03) all show `"order_status": "OPEN"`, which is NOT among the 12 annexure values. **Unknown** whether `OPEN` is a real wire value the annexure omits or a doc-example artifact. Probe: place a resting LIMIT order live and read `/v1/order/status`; ask Groww which enum the wire emits.

### 4.2 After Market Order Status

| Value | Description |
| --- | --- |
| NA | Status not available |
| PENDING | Order is pending for execution |
| DISPATCHED | Order has been dispatched for execution |
| PARKED | Order is parked for later execution |
| PLACED | Order has been placed in the market |
| FAILED | Order execution has failed |
| MARKET | Order is a market order |

### 4.3 Exchange

| Value | Description |
| --- | --- |
| BSE | Bombay Stock Exchange - Asia's oldest exchange, known for SENSEX index |
| NSE | National Stock Exchange - India's largest exchange by trading volume |
| MCX | Multi Commodity Exchange - India's largest commodity derivatives exchange |

> **MCX three-way contradiction (unresolved):** changelog v1.5.0 "Added: Commodity trading support on MCX exchange" vs REST intro (capture) "Commodities trading (MCX segment) is not available at this time" vs Smart Orders "The `COMMODITY` segment is not supported for Smart Orders" — and the LIVE live-data page gained a "COMMODITY for commodity contracts" mention after 2026-07-03. Treat MCX as unverified; confirm with Groww support.

### 4.4 Segment

| Value | Description |
| --- | --- |
| CASH | Regular equity market for trading stocks with delivery option |
| FNO | Futures and Options segment for trading derivatives contracts |
| COMMODITY | Commodity derivatives segment for trading commodity futures and options on MCX |

### 4.5 Order Type

| Value | Description |
| --- | --- |
| LIMIT | Specify exact price, may not get filled immediately but ensures price control |
| MARKET | Immediate execution at best available price, no price guarantee |
| SL | Stop Loss - Protection order that triggers at specified price to limit losses |
| SL_M | Stop Loss Market - Market order triggered at specified price to limit losses |

### 4.6 Product

| Value | Description |
| --- | --- |
| CNC | Cash and Carry - For delivery-based equity trading with full upfront payment |
| MIS | Margin Intraday Square-off - Higher leverage but must close by day end |
| NRML | Regular margin trading allowing overnight positions with standard leverage |

### 4.7 Transaction Type

| Value | Description |
| --- | --- |
| BUY | Long position - Profit from price increase, loss from price decrease |
| SELL | Short position - Profit from price decrease, loss from price increase |

### 4.8 Validity (DAY only — no IOC documented)

| Value | Description |
| --- | --- |
| DAY | Valid until market close on the same trading day |

### 4.9 Instrument Type

| Value | Description |
| --- | --- |
| EQ | Equity - Represents ownership in a company |
| IDX | Index - Composite value of a group of stocks representing a market |
| FUT | Futures - Derivatives contract to buy/sell an asset at a future date |
| CE | Call Option - Derivatives contract giving the right to buy an asset |
| PE | Put Option - Derivatives contract giving the right to sell an asset |

---

## 5. GA error codes + failure envelope

Failure envelope — verbatim, LIVE-DOC (capture 2026-07-03) of https://groww.in/trade-api/docs/curl §"Failed Request (HTTP 40x or 50x)":

```json
{
    "status": "FAILURE",
    "error": {
        "code": "GA001",
        "message": "Invalid trading symbol.",
        "metadata": null
    }
}
```

Official Error Codes table — verbatim, same page (`GA002` does NOT exist; the set is GA000, GA001, GA003–GA007):

| Code | Message |
| --- | --- |
| GA000 | Internal error occurred |
| GA001 | Bad request |
| GA003 | Unable to serve request currently |
| GA004 | Requested entity does not exist |
| GA005 | User not authorised to perform this operation |
| GA006 | Cannot process this request |
| GA007 | Duplicate order reference id |

**Reconciling the conflicting GA001/GA005 readings (all sources recorded):**

| Reading | Source | Verdict |
|---|---|---|
| GA001 = "Bad request" | Official Error Codes table (LIVE-DOC, capture 2026-07-03, https://groww.in/trade-api/docs/curl) | Authoritative — the per-code GENERIC message |
| GA001 example message "Invalid trading symbol." | The SAME official page's failure-envelope example (LIVE-DOC) | Also authoritative — the wire `message` field is REQUEST-SPECIFIC, not the generic table string; both readings coexist on one page |
| GA001 = "Bad request - check your parameters" | 3RD-PARTY (PHP SDK README, fetched 2026-07-13 — the `04-instruments-orders.md` research reading) | Consistent paraphrase of the official table; not authoritative |
| GA005 = "Authentication error - check your API key" | 3RD-PARTY (PHP SDK README, fetched 2026-07-13) | **Conflicts** with the official "User not authorised to perform this operation" — the official capture wins; the third-party gloss conflates authn/authz |
| GA000–GA007 set without GA002 | 3RD-PARTY (pkg.go.dev `growwapi-go` constants, fetched 2026-07-13) | Matches the official table — corroboration |

HTTP-status → SDK exception mapping (from the official `growwapi` 1.5.0 wheel source, `client.py::_ERROR_MAP`, fetched from PyPI 2026-07-13): `400 → GrowwAPIBadRequestException`, `401 → GrowwAPIAuthenticationException`, `403 → GrowwAPIAuthorisationException`, `404 → GrowwAPINotFoundException`, `429 → GrowwAPIRateLimitException`, `504 → GrowwAPITimeoutException` — the exception's `code` attribute carries the GA-code (taxonomy: `05-exceptions.md` / `12-sdk-exceptions.md`). Per-GA-code → HTTP-status mapping is NOT documented anywhere — **Unknown**; probe by triggering each class live.

---

## 6. Open unknowns (this file's scope)

| # | Unknown | Probe |
|---|---|---|
| 1 | `order_status: "OPEN"` vs the 12-value annexure | Live resting LIMIT order → read `/v1/order/status`; ask Groww |
| 2 | Smart Orders (`/v1/order-advance/*`) rate-limit family | Live rate-limit table for a 4th row; burst probe |
| 3 | Per-GA-code HTTP status mapping | Trigger each failure class live; capture status + body |
| 4 | Wire timestamp formats (`created_at` naive-ISO vs `trade_date` Zulu in one example) | Live order round-trip |
| 5 | Margin calculator `segment`: query param (curl example) vs body field (schema table) | Live call both ways |
| 6 | MCX availability (three-way doc contradiction, drifting live) | Groww support |
| 7 | Any drift of these pages since the 2026-07-03 capture | One unblocked fetch of the /curl doc set |
