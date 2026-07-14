# Zerodha Kite Connect v3 — Orders (reference)

> **Source:** `https://kite.trade/docs/connect/v3/orders/` (official live doc — UNREACHABLE from this sandbox; see provenance below)
> **Fetched-Verified:** 2026-07-13 via CLIENT-LIB-SOURCE (`zerodha/pykiteconnect@master`, `zerodha/gokiteconnect@master` — raw.githubusercontent.com, fetched 2026-07-13) + OFFICIAL-MOCK (`zerodha/kiteconnect-mocks@main` c7a8123, fetched 2026-07-13) + SEARCH (result snippets of the live orders page + Kite forum, 2026-07-13) + ARITH. **ARCHIVE-DOC and MIRROR-LIVE were BOTH BLOCKED** at the egress proxy on 2026-07-13 (web.archive.org: WebFetch client refusal + proxy CONNECT 403; r.jina.ai: CONNECT 403; kite.trade direct: CONNECT 403 — per the same-day probe log). No claim in this file is byte-verbatim from the live docs page.
> **Evidence tiers:** see README legend — Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) / SEARCH / Assumed / Unknown. A SEARCH-tier fact is substantive but NOT byte-verbatim; re-verify from an unblocked network before contracting.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. TickVault places NO orders on Zerodha; any future order path requires its own dated operator grant per the house rule-file protocol.
> **Related:** `99-UNKNOWNS.md` (Z-ORD-1 … Z-ORD-8 below), `docs/dhan-ref/07-orders.md` (comparison), `docs/groww-ref/16-orders-margins-portfolio.md` (comparison).

---

## 1. Endpoints (the full order surface)

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master `kiteconnect/connect.py` `_routes` dict, fetched 2026-07-13 — https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/connect.py; cross-checked Verified (CLIENT-LIB-SOURCE gokiteconnect@master `connect.go` URI constants + `orders.go` HTTP methods)):**

| Action | Method | Route | SDK fn (py) |
|---|---|---|---|
| Place order | `POST` | `/orders/{variety}` | `place_order(...)` → returns `order_id` |
| Modify order | `PUT` | `/orders/{variety}/{order_id}` | `modify_order(...)` → returns `order_id` |
| Cancel order | `DELETE` | `/orders/{variety}/{order_id}` (optional `parent_order_id` param) | `cancel_order(...)` |
| Exit CO | `DELETE` | same as cancel (py `exit_order` is a literal alias of `cancel_order`; Go `ExitOrder` likewise aliases `CancelOrder`) | `exit_order(...)` |
| Order book (day's orders) | `GET` | `/orders` | `orders()` |
| Single-order history | `GET` | `/orders/{order_id}` | `order_history(order_id)` — returns the LIST of state transitions, not one row |
| Trade book (day's trades) | `GET` | `/trades` | `trades()` |
| Trades of one order | `GET` | `/orders/{order_id}/trades` | `order_trades(order_id)` |
| Order margins | `POST` | `/margins/orders` | `order_margins(...)` (see the margins section file) |
| Basket margins | `POST` | `/margins/basket` | `basket_order_margins(...)` |
| Virtual contract note (charges) | `POST` | `/charges/orders` | `get_virtual_contract_note(...)` |

**Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py):** base URL `https://api.kite.trade` (`_default_root_uri`), header `X-Kite-Version: 3` (`kite_header_version = "3"`), auth header `Authorization: token {api_key}:{access_token}`. Request bodies are form-encoded params (the py client posts `params`; the Go client encodes `url.Values`) — **NOT JSON bodies** (contrast Dhan, which takes JSON bodies on POST/PUT — `docs/dhan-ref/07-orders.md`).

**Verified (OFFICIAL-MOCK order_response.json / order_modify.json / order_cancel.json, kiteconnect-mocks@main, fetched 2026-07-13):** place / modify / cancel all return the same envelope:

```json
{ "status": "success", "data": { "order_id": "151220000000000" } }
```

`order_id` is a **string** on the wire (all mocks; Go `OrderID string`).

## 2. Order varieties

**Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py constants):** the `{variety}` path segment takes exactly these values in the current py SDK:

| Variety | Constant | Meaning |
|---|---|---|
| `regular` | `VARIETY_REGULAR` | Normal in-market order |
| `amo` | `VARIETY_AMO` | After-market order (queued outside market hours) — the py README's own AMO example places `variety=kite.VARIETY_AMO` with `order_type=MARKET`, `product=NRML` (Verified, pykiteconnect README.md fetched 2026-07-13) |
| `co` | `VARIETY_CO` | Cover order |
| `iceberg` | `VARIETY_ICEBERG` | Iceberg order (split into legs; §6) |
| `auction` | `VARIETY_AUCTION` | Auction-session order (§10) |

**Verified (CLIENT-LIB-SOURCE gokiteconnect connect.go):** the Go SDK ADDITIONALLY still ships `VarietyBO = "bo"` and `ProductBO = "BO"` / `ProductCO = "CO"` constants. The py SDK has NO `bo` variety and no BO product constant. **Assumed:** BO (bracket order) is discontinued at the broker and the Go constants are legacy residue — Zerodha publicly discontinued bracket orders in 2020; the constant-set divergence between the two official SDKs is the observable fact. Treat `bo` as dead; do not build against it. → Z-ORD-1.

**AMO timing/eligibility windows: Unknown** — which exchanges/segments accept AMO and the queue-open hours are documented only on the live page/support portal, not in any SDK source. → Z-ORD-2.

## 3. Place order — full parameter set

**Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py `place_order` signature, fetched 2026-07-13; cross-checked against gokiteconnect `orders.go::OrderParams`):**

| Param | Required | Type/values | Notes |
|---|---|---|---|
| `variety` | yes (path) | §2 values | path segment, not a body param |
| `exchange` | yes | `NSE` `BSE` `NFO` `CDS` `BFO` `MCX` `BCD` (py `EXCHANGE_*` constants) | with `tradingsymbol` this IS the instrument identity — **Compare Dhan:** Dhan identifies by numeric-as-string `securityId` + `exchangeSegment` enum instead (`docs/dhan-ref/07-orders.md` rule 4) |
| `tradingsymbol` | yes | string, e.g. `INFY`, `SBIN` | from the instruments dump (see instruments section file) |
| `transaction_type` | yes | `BUY` / `SELL` | |
| `quantity` | yes | int | |
| `product` | yes | §4 values | |
| `order_type` | yes | §5 values | |
| `price` | optional | float | for LIMIT / SL |
| `validity` | optional | `DAY` / `IOC` / `TTL` (§5) | |
| `validity_ttl` | optional | int | order life in **minutes**, used with `validity=TTL` — minutes unit is SEARCH-tier (live-page snippet, 2026-07-13, kite.trade/docs/connect/v3/orders/) corroborated by the mock iceberg order carrying `"validity": "TTL", "validity_ttl": 2` (Verified OFFICIAL-MOCK orders.json) |
| `disclosed_quantity` | optional | int | classic disclosed-qty; min-% rule not in SDK → Unknown (Z-ORD-3). **Compare Dhan:** Dhan documents disclosed > 30% of quantity (`docs/dhan-ref/07-orders.md` rule 12) |
| `trigger_price` | optional | float | for SL / SL-M |
| `iceberg_legs` | optional | int | §6 |
| `iceberg_quantity` | optional | int | §6 |
| `auction_number` | optional | string | §10 (string in Go struct + mocks) |
| `tag` | optional | string | §9 |
| `market_protection` | optional | number | §7 — docstring verbatim: "accepts `-1` for automatic market protection applied by the system as per market protection guidelines, or a value greater than `0` up to `100` representing a percentage" (Verified, CLIENT-LIB-SOURCE pykiteconnect connect.py) |

**Verified (CLIENT-LIB-SOURCE gokiteconnect orders.go, Go-only params):** `OrderParams` additionally carries `squareoff`, `stoploss`, `trailing_stoploss` (the legacy CO/BO leg params — ABSENT from the current py `place_order` signature) and `algo_id`. **Assumed:** `squareoff`/`trailing_stoploss` are BO-era legacy kept for back-compat; `stoploss`/`trigger_price` handling for live CO orders should be probed on the live page. `algo_id` purpose is Unknown (exchange algo-tagging mandate is the plausible reading). → Z-ORD-1, Z-ORD-4.

## 3b. Autoslice (freeze-limit splitting)

**Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py `place_autoslice_order`):** same params as `place_order` plus the client adds `autoslice=true`; docstring: "Place an order with automatic slicing for quantities exceeding freeze limits. The order is automatically split into multiple smaller orders internally when the requested quantity exceeds exchange freeze limits." Returns the FULL response dict: parent `order_id` + a `children` list where each child is either `{order_id}` or an `{error}` payload.

**Verified (OFFICIAL-MOCK autoslice_response.json):** a REAL mixed outcome — parent `order_id`, 3 child `order_id`s AND one child error:

```json
{"error": {"code": 400, "error_type": "MarginException",
  "message": "Insufficient funds. Required margin is 228365.92 but available margin is 228358.50.", "data": null}}
```

i.e. **partial fills of the slice-set are possible: some children place, some reject — the caller must walk `children`.** (Cross-checked: gokiteconnect `orders.go` `OrderResponse{OrderID, Children []OrderChild}` with `OrderChildError{Code, ErrorType, Message, Data}`.)

**Compare Dhan:** Dhan exposes slicing as a SEPARATE endpoint `POST /v2/orders/slicing` with the same body as place (`docs/dhan-ref/07-orders.md` rule 17); Kite does it as a param on the normal place route.

## 4. Products

**Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py constants):** `MIS`, `CNC`, `NRML`, `CO`.
**Verified (CLIENT-LIB-SOURCE gokiteconnect connect.go):** Go adds `ProductMTF = "MTF"` (and legacy `ProductBO`).
**Verified (OFFICIAL-MOCK orders.json):** a real order row with `"product": "MTF"` (`variety: regular`, `order_type: LIMIT`, order_id `250117800776785`) — **MTF is a live wire value even though the py constant set doesn't include it.** A py caller can pass the raw string.

| Product | Meaning (Assumed from standard Indian-broker usage; the live page holds the authoritative table) |
|---|---|
| `CNC` | Cash & carry, equity delivery |
| `NRML` | Normal / carry-forward (F&O, currency, commodity) |
| `MIS` | Margin intraday square-off |
| `MTF` | Margin trading facility (Verified as a wire value per above; semantics Assumed) |
| `CO` | Cover-order product (paired with `variety=co`) |

→ Z-ORD-5 (exact product-by-segment eligibility matrix is live-doc-only).

## 5. Order types + validity

**Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py constants; identical set in gokiteconnect connect.go):**

- Order types: `MARKET`, `LIMIT`, `SL` (stop-loss limit), `SL-M` (stop-loss market). Note the wire literal is `SL-M` with a hyphen (py constant name `ORDER_TYPE_SLM`).
- Validity: `DAY`, `IOC`, `TTL`. `TTL` pairs with `validity_ttl` (minutes — §3). **Compare Dhan:** Dhan has only `DAY`/`IOC` (`docs/dhan-ref/07-orders.md` rule 9). **Compare Groww:** the Groww annexure documents validity `DAY` only (`docs/groww-ref/README.md` claims row 26).

## 6. Iceberg mechanics

- **Verified (CLIENT-LIB-SOURCE):** placed as `variety=iceberg` with `iceberg_legs` (py docstring-free; Go: "Total number of legs") and `iceberg_quantity` (per-leg split quantity).
- **SEARCH (live orders page snippet, 2026-07-13 — https://kite.trade/docs/connect/v3/orders/):** `iceberg_legs` must be between **2 and 10**; `iceberg_quantity` is "split quantity for each iceberg leg order".
- **Verified (OFFICIAL-MOCK orders.json — the real iceberg row, order_id `220524001859672`):** an iceberg order carries `meta.iceberg` on the order book row: `{"leg": 1, "legs": 5, "leg_quantity": 200, "total_quantity": 1000, "remaining_quantity": 800}` — i.e. the book row you see is the CURRENT leg; leg progress is in `meta`. That row also demonstrates iceberg + `validity: TTL` (`validity_ttl: 2`) co-use, and that `quantity` on the row is the LEG quantity (200) while `meta.iceberg.total_quantity` holds the full 1000 (ARITH: 5 legs × 200 = 1000; remaining 800 = 1000 − 200 after leg 1).
- **Unknown:** minimum order value / minimum per-leg quantity constraints, and whether a cancel of the visible leg kills the whole iceberg → Z-ORD-6.

## 7. Market protection (market/SL-M orders)

- **Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py docstrings on BOTH `place_order` and `modify_order`):** `market_protection` accepts `-1` (system-applied automatic protection "as per market protection guidelines") or a value `> 0` up to `100` (a percentage).
- **Verified (OFFICIAL-MOCK orders.json):** every order-book row carries a `market_protection` field (all `0` in the mock data set) — it is a first-class order attribute, echoed back.
- **SEARCH (Kite forum + Zerodha support snippets, 2026-07-13 — kite.trade/forum/discussion/15177, support.zerodha.com "Market price protection"):** snippets indicate market orders must carry protection and that a `market_protection` of `0` is treated as an unprotected market order and rejected in the segments where protection is mandated; the protection band converts the market order into a limit at LTP ± the percentage. **NOT byte-verified against the official doc — treat the mandatory-ness and the exact default band as SEARCH-grade** → Z-ORD-7.
- **Compare Dhan:** Dhan's equivalent is exchange-side MPP (effective 2026-03-21): `orderType: "MARKET"` is auto-converted to LIMIT-with-MPP by the exchange and may rest `PENDING` (`docs/dhan-ref/07-orders.md` rule 18). Kite instead exposes the protection percentage as an explicit API param.

## 8. Modify + cancel

**Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py `modify_order` signature):** modifiable fields = `quantity`, `price`, `order_type`, `trigger_price`, `validity`, `disclosed_quantity`, `market_protection`; plus `parent_order_id` (for second-leg/CO modifications). `PUT /orders/{variety}/{order_id}`.

- **Unknown — quantity semantics on modify** (total vs remaining): not stated in any SDK source. **Compare Dhan:** Dhan explicitly documents modify `quantity` = TOTAL order quantity, a classic foot-gun (`docs/dhan-ref/07-orders.md` rule 6). Do NOT assume Kite matches. → Z-ORD-8.
- **Unknown — modification count cap:** Dhan caps at 25 modifications/order; no Kite equivalent found in SDK/mocks.

**Verified (CLIENT-LIB-SOURCE):** cancel = `DELETE /orders/{variety}/{order_id}` with optional `parent_order_id` (used when cancelling the second leg of a CO — the py `exit_order("co", ...)` alias exists exactly for "Exit a CO order" per its docstring).

## 9. Tags

- **Verified (CLIENT-LIB-SOURCE both SDKs):** `tag` is a free-form string param on place; echoed back on order rows (mock rows show `"tag": "connect test order1"`).
- **Verified (CLIENT-LIB-SOURCE gokiteconnect orders.go + OFFICIAL-MOCK orders.json):** order rows ALSO carry a plural `tags` array field (Go `Tags []string`; present on rows in the orders.json mock) — the read side can return multiple tags.
- **Unknown:** max tag length / charset (Dhan documents 30 chars `[a-zA-Z0-9 _-]` for its `correlationId`; no Kite equivalent found in SDK). **Compare Dhan:** Dhan's `correlationId` also flows through its order-update WebSocket; Kite's Postback/WebSocket order updates echo `tag` (see the postbacks section file).

## 10. Auction variety

- **Verified (CLIENT-LIB-SOURCE pykiteconnect `_routes`):** `GET /portfolio/holdings/auctions` lists holdings eligible for the current auction session (`get_auction_instruments`).
- **Verified (OFFICIAL-MOCK auctions_list.json):** each eligible row carries `auction_number` (string, e.g. `"20"`), ISIN, quantities and P&L context.
- **Verified (OFFICIAL-MOCK orders.json):** a real `variety: "auction"` order row (CNC, LIMIT, DAY) carrying an `auction_number` field.
- **Assumed:** the flow is list auctions → place `variety=auction` with the matching `auction_number` (this is the only coherent reading of the params; live page confirms wording).

## 11. Order response object (order book / history rows)

**Verified (OFFICIAL-MOCK orders.json first-row key set, cross-checked field-for-field against gokiteconnect `orders.go::Order`):**

`placed_by, order_id, exchange_order_id, parent_order_id, status, status_message, status_message_raw, order_timestamp, exchange_update_timestamp, exchange_timestamp, variety, modified (bool), meta (object — iceberg progress lives here), exchange, tradingsymbol, instrument_token (numeric), order_type, transaction_type, validity, (validity_ttl on TTL rows), product, quantity, disclosed_quantity, price, trigger_price, average_price, filled_quantity, pending_quantity, cancelled_quantity, market_protection, tag, tags, guid` (Go also declares `account_id`, `auction_number`, `algo_id`-adjacent fields).

Notes:
- `status_message` (human) vs `status_message_raw` (verbatim OMS/RMS text, e.g. `"RMS:Margin Exceeds,Required:95417.84, Available:74251.80 …"`) — both real in the mock REJECTED row (Verified OFFICIAL-MOCK).
- Timestamps are `"YYYY-MM-DD HH:MM:SS"` strings, 19 chars (the py client date-parses exactly the 19-char forms — Verified CLIENT-LIB-SOURCE `_format_response`). Timezone is not stated on the wire — **Assumed IST** (NSE convention; same assumption class as Dhan/Groww packs).
- **Compare Dhan:** Dhan order-book timestamps are also IST strings (`docs/dhan-ref/07-orders.md` rule 15); Dhan has no `status_message_raw` / `meta` equivalents.

## 12. Order states + lifecycle

**Verified (CLIENT-LIB-SOURCE pykiteconnect constants):** the SDK names only the terminal trio `COMPLETE`, `REJECTED`, `CANCELLED` as constants.

**Verified (OFFICIAL-MOCK order_info.json — a REAL 8-row single-order history, in wire order):**

```
PUT ORDER REQ RECEIVED → VALIDATION PENDING → OPEN PENDING → OPEN
→ MODIFY VALIDATION PENDING → MODIFY PENDING → MODIFIED → OPEN
```

i.e. `GET /orders/{order_id}` returns the full transition list; a modify inserts its own `MODIFY VALIDATION PENDING / MODIFY PENDING / MODIFIED` interim states before the order returns to `OPEN`.

**SEARCH (live orders page snippet, 2026-07-13 — kite.trade/docs/connect/v3/orders/):** the official page says the common statuses are `OPEN, COMPLETE, CANCELLED, REJECTED` and that orders "pass through several interim and temporary statuses". Separately, the `TRIGGER PENDING` literal for resting SL/SL-M orders is **forum-corroborated SEARCH** (its attribution to the docs page vs forum answers could not be disambiguated). `AMO REQ RECEIVED` appears in ecosystem discussions of AMO interim states but was NOT found in any fetched artifact — **Unknown** as an exact wire literal. → Z-ORD-2.

**Practical rule (Assumed, from the observed mock lifecycle):** treat any non-{`COMPLETE`,`CANCELLED`,`REJECTED`} status as non-terminal; poll `order_history`/consume postbacks rather than string-matching interim states, since the official page declines to enumerate them exhaustively. **Compare Dhan:** Dhan documents a closed 9-value enum (`TRANSIT/PENDING/…/TRADED/EXPIRED`, `docs/dhan-ref/08-annexure-enums.md` rule 6) — Kite's interim state space is explicitly open-ended.

## 13. Trades (fills)

**Verified (OFFICIAL-MOCK order_trades.json / trades.json key set, cross-checked vs gokiteconnect `orders.go::Trade`):** trade rows carry `trade_id, order_id, exchange_order_id, tradingsymbol, exchange, instrument_token, transaction_type, product, average_price, quantity, fill_timestamp, order_timestamp, exchange_timestamp`.

**Verified (CLIENT-LIB-SOURCE pykiteconnect `trades` docstring):** "An order can be executed in tranches based on market conditions. These trades are individually recorded under an order." — one order ⇒ N trade rows.

## 14. Order errors (envelope)

**Verified (CLIENT-LIB-SOURCE pykiteconnect exceptions.py):** order-relevant exception CLASSES mapped from the API's `error_type`: `OrderException` (default HTTP 500), `InputException` (400 — missing/invalid params), `TokenException` (403), `NetworkException` (503, client↔OMS network), `DataException` (502, bad OMS response), `GeneralException` (500). Note: `MarginException` is **NOT a pykiteconnect class** — exceptions.py contains exactly 8 classes and no margin variant.

**Verified (OFFICIAL-MOCK autoslice_response.json ONLY):** the wire value `error_type: "MarginException"` (code 400) is real — it appears in the autoslice mock's child error. Because pykiteconnect has no matching class, its dispatcher (`getattr(ex, error_type, GeneralException)`) raises **GeneralException** for it — an error-taxonomy port must keep that unknown-name fallback, not invent a mapped class. Error envelope shape (from the autoslice child): `{code, error_type, message, data}`.

**Compare Dhan:** Dhan's error envelope is 3 string fields `errorType/errorCode/errorMessage` with `DH-9xx` codes (`docs/dhan-ref/01-introduction-and-rate-limits.md` rule 6); Kite uses HTTP status + `error_type` class names instead of a numeric code family.

## 15. Compare: Dhan / Groww (material differences summary)

| Aspect | Kite | Dhan | Cite |
|---|---|---|---|
| Instrument identity in order | `exchange` + `tradingsymbol` strings | `securityId` (numeric-as-STRING) + `exchangeSegment` enum | `docs/dhan-ref/07-orders.md` rule 4 |
| Market order protection | explicit `market_protection` param (−1 auto / % / 0-rejected per SEARCH) | exchange-side MPP auto-converts MARKET→LIMIT (2026-03-21), may rest PENDING | `docs/dhan-ref/07-orders.md` rule 18 |
| Slicing | `autoslice=true` param on the normal place route, `children[]` mixed-outcome response | separate `POST /v2/orders/slicing` | `docs/dhan-ref/07-orders.md` rule 17 |
| Modify `quantity` semantics | Unknown (Z-ORD-8) | documented TOTAL, not remaining | `docs/dhan-ref/07-orders.md` rule 6 |
| Validity | DAY / IOC / TTL(minutes) | DAY / IOC only | `docs/dhan-ref/07-orders.md` rule 9 |
| Order states | open-ended interim set + terminal COMPLETE/CANCELLED/REJECTED | closed 9-value enum | `docs/dhan-ref/08-annexure-enums.md` rule 6 |
| Client order tag | `tag` (+ read-side `tags[]`) | `correlationId` (≤30 chars, charset-limited) | `docs/dhan-ref/07-orders.md` rule 5 |
| Body encoding | form-encoded params | JSON body | `docs/dhan-ref/01-introduction-and-rate-limits.md` rule 5 |

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Z-ORD-1 — CO/BO current status + CO leg params.** Is `variety=co` still accepted, and which of `stoploss`/`trigger_price` drives the CO stop leg? (Go keeps `bo` + `squareoff`/`stoploss`/`trailing_stoploss`; py dropped them.) Paste: `https://kite.trade/docs/connect/v3/orders/` (varieties table + CO section). Matters: an order-path design must not target a discontinued variety.
- **Z-ORD-2 — Exhaustive interim order-state list + AMO windows.** Does the official page enumerate `TRIGGER PENDING`, `AMO REQ RECEIVED`, `CANCEL PENDING`, etc., and the AMO placement windows per exchange? Paste: `https://kite.trade/docs/connect/v3/orders/` (order status + AMO paragraphs). Matters: state-machine mapping (the Dhan-style OMS FSM) needs the authoritative literal set.
- **Z-ORD-3 — `disclosed_quantity` minimum-% rule.** Exchange/broker floor (NSE convention is ≥10%; Dhan documents >30%). Paste: same page, disclosed quantity row. Matters: pre-validation.
- **Z-ORD-4 — `algo_id` semantics.** Present in gokiteconnect `OrderParams`, absent from py. Exchange algo-registration mandate field? Paste: same page + `https://kite.trade/docs/connect/v3/` changelog. Matters: compliance field for automated orders.
- **Z-ORD-5 — product×segment eligibility matrix (incl. MTF).** Which products are legal on which exchanges, and MTF constraints (approved list, delivery-only?). Paste: same page, products table + `https://support.zerodha.com` MTF articles. Matters: order validation.
- **Z-ORD-6 — iceberg floors + cancel semantics.** Minimum order value/leg size for `variety=iceberg`; does cancelling the live leg cancel the remainder? Paste: same page, iceberg section. Matters: split-order planning.
- **Z-ORD-7 — market-protection mandatory-ness + default band.** Is `market_protection` REQUIRED for MARKET/SL-M in F&O, is `0` rejected, and what is the `-1` auto band per segment? Paste: same page + `https://support.zerodha.com/category/trading-and-markets/charts-and-orders/order/articles/market-price-protection-on-the-order-window`. Matters: MARKET orders fail-closed if the param is mishandled (SEARCH-tier today).
- **Z-ORD-8 — modify `quantity` = total or remaining?** Paste: same page, modify section. Matters: the exact Dhan foot-gun class (`docs/dhan-ref/07-orders.md` rule 6); getting it wrong resizes live orders.

## CLAIMS (for README reconciled table)

- Order routes: `POST /orders/{variety}`, `PUT /orders/{variety}/{order_id}`, `DELETE /orders/{variety}/{order_id}`, `GET /orders`, `GET /orders/{order_id}` (full history list), `GET /trades`, `GET /orders/{order_id}/trades`; base `https://api.kite.trade`, header `X-Kite-Version: 3`, `Authorization: token api_key:access_token`, form-encoded bodies — **Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py `_routes` + gokiteconnect connect.go/orders.go)** — https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/connect.py
- Varieties = `regular, amo, co, iceberg, auction` (py); Go additionally ships legacy `bo` — **Verified (CLIENT-LIB-SOURCE both SDKs)**.
- Place-order params: exchange, tradingsymbol, transaction_type, quantity, product, order_type, price, validity, validity_ttl, disclosed_quantity, trigger_price, iceberg_legs, iceberg_quantity, auction_number, tag, market_protection — **Verified (CLIENT-LIB-SOURCE pykiteconnect `place_order` signature)**.
- `market_protection`: `-1` = automatic system protection, else `>0..100` percent; echoed on every order row — **Verified (CLIENT-LIB-SOURCE py docstring + OFFICIAL-MOCK orders.json)**; `0`-rejected behaviour is **SEARCH** only.
- Autoslice: `autoslice=true` on the normal place route; response = parent `order_id` + `children[]` where each child is `{order_id}` OR `{error{code,error_type,message}}` (mixed outcomes real) — **Verified (CLIENT-LIB-SOURCE py `place_autoslice_order` + OFFICIAL-MOCK autoslice_response.json)**.
- Products on the wire include `MTF` (real mock order row) even though the py constant set is only MIS/CNC/NRML/CO — **Verified (OFFICIAL-MOCK orders.json + CLIENT-LIB-SOURCE gokiteconnect `ProductMTF`)**.
- Order types = `MARKET, LIMIT, SL, SL-M`; validity = `DAY, IOC, TTL` with `validity_ttl` in minutes — **Verified (CLIENT-LIB-SOURCE constants)**; minutes-unit **SEARCH** (live-page snippet 2026-07-13) + mock corroboration (`validity_ttl: 2`).
- Iceberg: `iceberg_legs` 2–10 (**SEARCH**, live-page snippet 2026-07-13); book row shows the CURRENT leg with progress in `meta.iceberg{leg,legs,leg_quantity,total_quantity,remaining_quantity}` — **Verified (OFFICIAL-MOCK orders.json)**.
- Single-order history endpoint returns the transition LIST; real interim states observed: `PUT ORDER REQ RECEIVED, VALIDATION PENDING, OPEN PENDING, OPEN, MODIFY VALIDATION PENDING, MODIFY PENDING, MODIFIED` — **Verified (OFFICIAL-MOCK order_info.json)**; `TRIGGER PENDING` for SL orders — **SEARCH**.
- Place/modify/cancel all return `{"status":"success","data":{"order_id":"<string>"}}` — **Verified (OFFICIAL-MOCK order_response/order_modify/order_cancel.json)**.
- Order rows carry both `status_message` and verbatim `status_message_raw` (RMS text), plus `modified` bool, `guid`, `exchange_update_timestamp`, `tags[]` — **Verified (OFFICIAL-MOCK orders.json + CLIENT-LIB-SOURCE gokiteconnect Order struct)**.
- Auction flow: `GET /portfolio/holdings/auctions` lists eligible holdings with `auction_number` (string); auction orders carry `auction_number` — **Verified (CLIENT-LIB-SOURCE `_routes` + OFFICIAL-MOCK auctions_list.json / orders.json)**.
- Error classes: `OrderException(500), InputException(400), TokenException(403), NetworkException(503), DataException(502)` — **Verified (CLIENT-LIB-SOURCE pykiteconnect exceptions.py)**; wire value `MarginException` (400 observed) is attested by the **OFFICIAL-MOCK autoslice_response.json ONLY** (no SDK class; pykiteconnect dispatches it to GeneralException); child-error envelope `{code, error_type, message, data}` — **Verified (OFFICIAL-MOCK)**.
- Modify-order `quantity` semantics (total vs remaining) — **Unknown** (Z-ORD-8; contrast Dhan's documented TOTAL).
