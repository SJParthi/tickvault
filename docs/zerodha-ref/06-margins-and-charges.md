# Zerodha Kite Connect v3 — Funds, Margins & Charges (evidence-tiered reference)

> **Source:** https://kite.trade/docs/connect/v3/user/#funds-and-margins · https://kite.trade/docs/connect/v3/margins/ · https://kite.trade/docs/connect/v3/exceptions/ (rate-limit table) · https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/what-are-the-charges-for-kite-apis
> **Fetched-Verified:** live-verbatim via GH-runner 2026-07-14 (UTC timestamps in 00-COVERAGE-MANIFEST.md) — the live `kite.trade/docs/connect/v3/margins/` + `…/user/` + `…/exceptions/` pages and the charges support article were fetched by the LIVE-DOC runner (per-URL sha256 in the crawl fetch-log). This supersedes the 2026-07-13 state where the live docs were egress-blocked and the file was CLIENT-LIB-SOURCE/OFFICIAL-MOCK-only. Cross-check evidence retained: CLIENT-LIB-SOURCE (pykiteconnect@master + gokiteconnect@master, fetched 2026-07-13) + OFFICIAL-MOCK (zerodha/kiteconnect-mocks@main `c7a8123`). Notable: the live docs' embedded response examples are byte-identical (modulo float repr) to the official mocks, so mock-derived shapes were confirmed wholesale.
> **Evidence tiers:** see README legend — **Verified (LIVE-DOC runner 2026-07-14)** (top tier: confirmed against the runner-fetched live page) / Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) / Assumed / Unknown.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. TickVault places no Kite orders; order/margin surfaces are documented for completeness (the groww-ref `16-…` precedent).

---

## 1. Endpoint inventory (this file's surface)

**Verified (LIVE-DOC runner 2026-07-14 — https://kite.trade/docs/connect/v3/margins/ endpoint table + https://kite.trade/docs/connect/v3/user/ endpoint table; cross-checked CLIENT-LIB-SOURCE pykiteconnect@master `_routes` + gokiteconnect@master URI constants):**

| # | Purpose (live-doc wording) | Method + path | py route key / method | Go URI const / method |
|---|---|---|---|---|
| 1 | "Retrieve detailed funds and margin information" — both segments | `GET /user/margins` | `user.margins` / `margins()` | `URIUserMargins` / `GetUserMargins()` |
| 2 | same, one segment (`/user/margins/:segment`) | `GET /user/margins/{segment}` | `user.margins.segment` / `margins(segment)` | `URIUserMarginsSegment` / `GetUserSegmentMargins(segment)` |
| 3 | "Calculates margins for each order considering the existing positions and open orders" | `POST /margins/orders` | `order.margins` / `order_margins(params)` | `URIOrderMargins` / `GetOrderMargins()` |
| 4 | "Calculates margins for spread orders" | `POST /margins/basket` | `order.margins.basket` / `basket_order_margins(params, consider_positions=True, mode=None)` | `URIBasketMargins` / `GetBasketMargins()` |
| 5 | "Calculates order-wise charges for orderbook" (virtual contract note) | `POST /charges/orders` | `order.contract_note` / `get_virtual_contract_note(params)` | `URIOrderCharges` / `GetOrderCharges()` |
| 6 | ⚠ legacy/dead route | `GET /margins/{segment}` | `market.margins` — defined in `_routes` but **NO pykiteconnect method calls it** (grep over connect.py, 2026-07-13); absent from gokiteconnect entirely | — |

Row 6 purpose is **Unknown** — and the live v3 margins/user pages do NOT document any `GET /margins/{segment}` route (**Verified-absence, LIVE-DOC runner 2026-07-14** over the fetched margins page; the only documented routes are rows 1–5) — see OPEN QUESTIONS.

**Content type — Verified (LIVE-DOC runner 2026-07-14, margins page Note):** "Requests to the above endpoints are JSON POST and it needs application/json header" — i.e. rows 3–5 take a raw JSON body with `Content-Type: application/json`, unlike the form-encoded order APIs.

**Common wire conventions — Verified (LIVE-DOC runner 2026-07-14 margins-page curl examples; cross-check CLIENT-LIB-SOURCE pykiteconnect connect.py):** base URL `https://api.kite.trade`; headers `X-Kite-Version: 3` + `Authorization: token api_key:access_token`; JSON envelope `{"status": "success", "data": …}` — the SDK returns `data["data"]`; error envelope has `status: "error"` + `error_type` mapped to a typed exception (a 403 `TokenException` additionally fires the session-expiry hook).

**Plan availability — Verified (LIVE-DOC runner 2026-07-14 — https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/what-are-the-charges-for-kite-apis):** "Margin computation, portfolio management, etc." is included in the **free Personal plan** ("Personal (Free): Full-fledged order, GTT, alerts management · Margin computation, portfolio management, etc. · No historical or real-time data included"); Connect at ₹500/month per app adds WebSocket + historical data. So the §3–§5 calculators need no paid data subscription. Full pricing model: `13-pricing-and-access-model.md`.

**Compare: Dhan/Groww.** Dhan's equivalents are `GET /fundlimit` (one flat object, no per-segment split) + `POST /margincalculator` taking a SINGLE order object (+ a `/margincalculator/multi` variant), with `brokerage` folded into the margin response and no dedicated charges endpoint (`docs/dhan-ref/13-funds-margin.md` §1–§2). Groww's are `GET /v1/margins/detail/user` + one combined `POST /v1/margins/detail/orders?segment=` (JSON-array body; "Basket orders are supported for FNO and COMMODITY segments") with no charges endpoint (`docs/groww-ref/16-orders-margins-portfolio.md` §1 rows 12–13). Kite is the only one of the three with (a) a segment-split funds response, (b) a separate basket endpoint with position-offset semantics, and (c) a standalone order-wise charges calculator.

---

## 2. Funds & margins — `GET /user/margins` and `GET /user/margins/{segment}`

### 2.1 Segments

**Verified (LIVE-DOC runner 2026-07-14 — https://kite.trade/docs/connect/v3/user/#funds-and-margins):** "A GET request to /user/margins returns funds, cash, and margin information for the user for equity and commodity segments. A GET request to /user/margins/:segment … segment in the URI can be either **equity** or **commodity**." Cross-check: pykiteconnect constants `MARGIN_EQUITY = "equity"`, `MARGIN_COMMODITY = "commodity"`; gokiteconnect `AllMargins{Equity, Commodity}`.

### 2.2 Response shape (full-segment call) — verbatim-derived

**Verified (LIVE-DOC runner 2026-07-14 — the user page embeds the full response sample; byte-identical field set to OFFICIAL-MOCK `margins.json`/`margins_equity.json`/`margin_commodity.json` kiteconnect-mocks@main c7a8123 and to gokiteconnect `Margins`/`AvailableMargins`/`UsedMargins` structs):**

```json
{
  "status": "success",
  "data": {
    "equity": {
      "enabled": true,
      "net": 99725.05000000002,
      "available": {
        "adhoc_margin": 0, "cash": 245431.6, "opening_balance": 245431.6,
        "live_balance": 99725.05000000002, "collateral": 0, "intraday_payin": 0
      },
      "utilised": {
        "debits": 145706.55, "exposure": 38981.25, "m2m_realised": 761.7,
        "m2m_unrealised": 0, "option_premium": 0, "payout": 0, "span": 101989,
        "holding_sales": 0, "turnover": 0, "liquid_collateral": 0,
        "stock_collateral": 0, "delivery": 0
      }
    },
    "commodity": { "…same shape…": null }
  }
}
```

(The live sample renders `net`/`live_balance` with the long-tail float repr `99725.05000000002`; the mock has `99725.05` — same value class, another store-as-f64 reminder.)

The segmented call (`/user/margins/equity`) returns the SAME per-segment object directly under `data` (no `equity` wrapper) — **Verified (OFFICIAL-MOCK `margins_equity.json`, `margin_commodity.json`)**; the live page shows only the unsegmented sample.

### 2.3 Field table

Field NAMES + types AND per-field meanings are now **Verified (LIVE-DOC runner 2026-07-14 — the user page "Response attributes" table, quoted verbatim below)**. Types per the live doc annotations (`bool`, `float64`).

| Field | Type | Live-doc definition |
|---|---|---|
| `enabled` | bool | "Indicates whether the segment is enabled for the user" |
| `net` | float64 | "Net cash balance available for trading (intraday_payin + adhoc_margin + collateral)" — ⚠ the parenthetical cannot be the complete formula: in the doc's own sample all three named components are 0 while `net` is 99725.05 (**ARITH over the live sample**). The mock/live arithmetic that DOES hold: `net = available.cash − utilised.debits` (245431.6 − 145706.55 = 99725.05 ✓) and `net == available.live_balance` in every sample. Treat the parenthetical as listing additional contributors, not the whole sum |
| `available.adhoc_margin` | float64 | "Additional margin provided by the broker" |
| `available.cash` | float64 | "Raw cash balance in the account available for trading (also includes intraday_payin)" |
| `available.opening_balance` | float64 | "Opening balance at the day start" |
| `available.live_balance` | float64 | "Current available balance" (equals `net` in every sample) |
| `available.collateral` | float64 | "Margin derived from pledged stocks" |
| `available.intraday_payin` | float64 | "Amount that was deposited during the day" |
| `utilised.debits` | float64 | "Sum of all utilised margins (unrealised M2M + realised M2M + SPAN + Exposure + Premium + Holding sales)" — ⚠ **ARITH:** the doc's own sample violates its own formula: 0 + 761.7 + 101989 + 38981.25 + 0 + 0 = 141731.95 ≠ `debits` 145706.55. The listed components still do NOT reconstruct `debits`; do not rebuild it client-side |
| `utilised.exposure` | float64 | "Exposure margin blocked for all open F&O positions" |
| `utilised.span` | float64 | "SPAN margin blocked for all open F&O positions" |
| `utilised.m2m_realised` | float64 | "Booked intraday profits and losses" |
| `utilised.m2m_unrealised` | float64 | "Un-booked (open) intraday profits and losses" |
| `utilised.option_premium` | float64 | "Value of options premium received by shorting" |
| `utilised.payout` | float64 | "Funds paid out or withdrawn to bank account during the day". **Wire wart — Verified (OFFICIAL-MOCK margin_commodity.json):** the value `-0` (negative zero) appears on the wire; parsers must tolerate it |
| `utilised.holding_sales` | float64 | "Value of holdings sold during the day" |
| `utilised.turnover` | float64 | "Utilised portion of the maximum turnover limit (only applicable to certain clients)" |
| `utilised.delivery` | float64 | "Margin blocked when you sell securities (20% of the value of stocks sold) from your demat or T1 holdings" |
| `utilised.liquid_collateral` | float64 | "Margin utilised against pledged liquidbees ETFs and liquid mutual funds" |
| `utilised.stock_collateral` | float64 | "Margin utilised against pledged stocks/ETFs" |

**Compare: Dhan.** Dhan's `/fundlimit` is a single flat object and famously carries the `availabelBalance` spelling typo which integrators must keep (`.claude/rules/dhan/funds-margin.md`); Kite's funds response has no known typo fields and is segment-nested.

---

## 3. Order margin calculator — `POST /margins/orders`

### 3.1 Request

**Verified (LIVE-DOC runner 2026-07-14 — margins page curl example + "Order structure" param table; cross-check CLIENT-LIB-SOURCE gokiteconnect `OrderMarginParam`):** the body is a **JSON array** of order objects. Live-doc param table (verbatim definitions):

| Field | JSON key | Live-doc definition | Go type |
|---|---|---|---|
| Exchange | `exchange` | "Name of the exchange" | string (no omitempty) |
| Trading symbol | `tradingsymbol` | ⚠ present in the live curl example (`"INFY"`) but MISSING from the live param table — a live-doc omission; it is required in practice (every example + both SDK structs carry it) | string |
| Transaction type | `transaction_type` | "BUY/SELL" | string |
| Variety | `variety` | "Order variety (regular, amo, co etc.)" (`regular`/`co`/`amo`/`iceberg`/`auction` per pykiteconnect constants) | string |
| Product | `product` | "Margin product to use for the order (margins are blocked based on this)" | string |
| Order type | `order_type` | "Order type (MARKET, LIMIT etc.)" | string |
| Quantity | `quantity` | "Quantity of the order" — untyped on the live margins page (the sibling `/charges/orders` table types it `int64`) | **float64** in Go |
| Price | `price` | "Price at which the order is going to be placed (LIMIT orders)" | float64, `omitempty` |
| Trigger price | `trigger_price` | "Trigger price (for SL, SL-M, CO orders)" | float64, `omitempty` |

The live table carries NO required/optional markers — required-vs-optional remains inferred from Go `omitempty` tags (**Assumed** for server-side truth; the live example sends `price: 0` and `trigger_price: 0` explicitly for a MARKET order). `quantity` being float64 in the official Go SDK is a faithful report, not a claim that fractional quantities are accepted (**Unknown**; the live charges-endpoint table typing `quantity` as `int64` suggests integers).

**Compact mode — Verified (LIVE-DOC runner 2026-07-14):** the live margins page documents a query parameter on `/margins/orders`: `mode` — "compact - Compact mode will only give the total margins". This CONFIRMS the gokiteconnect `?mode=compact` behaviour; **pykiteconnect still does NOT expose any `mode` parameter on `order_margins()`** (only on the basket call) — an SDK gap, not a docs gap. *(RESOLVED by live fetch — was OPEN QUESTIONS #3.)*

### 3.2 Response

**Verified (LIVE-DOC runner 2026-07-14 — the margins page embeds the response sample, byte-identical to OFFICIAL-MOCK `order_margins.json`; struct twin: gokiteconnect `OrderMargins`):** `data` is an **array**, one element per input order:

```json
{
  "type": "equity", "tradingsymbol": "INFY", "exchange": "NSE",
  "span": 0, "exposure": 0, "option_premium": 0, "additional": 0,
  "bo": 0, "cash": 0, "var": 1498,
  "pnl": { "realised": 0, "unrealised": 0 },
  "leverage": 1,
  "charges": { "…see §6…": null },
  "total": 1498
}
```

Live-doc "Margin structure" definitions (**Verified (LIVE-DOC runner 2026-07-14)**):

| Field | Live-doc definition |
|---|---|
| `type` | "equity/commodity" — the margin segment classification. *(RESOLVED by live fetch — value domain was Unknown; note NFO option legs classify as `"equity"` in the basket sample)* |
| `span` / `exposure` | "SPAN margins" / "Exposure margins" |
| `option_premium` | "Option premium" |
| `additional` | "Additional margins" |
| `bo` | "BO margins" (bracket order) |
| `cash` | "Cash credit" |
| `var` | "VAR" |
| `pnl` | "Realised and unrealised profit and loss" |
| `leverage` | "Margin leverage allowed for the trade" |
| `charges` | "The breakdown of the various charges that will be applied to an order" — the full §6 object embedded per order |
| `total` | "Total margin block" (**ARITH:** equity sample `total` 1498 = `var` 1498) |

**Compare: Dhan.** Dhan's calculator takes ONE order per call (`POST /margincalculator`, single JSON object with `dhanClientId` in the body) and returns a flat response with `insufficientBalance` and `leverage` as a STRING (`docs/dhan-ref/13-funds-margin.md` §2); Kite takes an array, returns per-order component + charges breakdowns, and has no shortfall field.

---

## 4. Basket margin calculator — `POST /margins/basket`

### 4.1 Request

**Verified (LIVE-DOC runner 2026-07-14 — margins page basket curl example + query-param table):** body = the SAME JSON array of §3.1 order objects. Documented query params: `consider_positions` — "Boolean to consider users positions" — and `mode` — "compact - Compact mode will only give the total margins". The live example sends lowercase `consider_positions=true`.

**Query-param serialization warts — Verified (both SDK sources, divergent):**
- pykiteconnect passes the Python bool straight into `requests` query params → the wire sees `consider_positions=True` (capital T) and omits `mode` when `None`.
- gokiteconnect sets `consider_positions=true` (lowercase, matching the live example) ONLY when true — **the Go SDK cannot send `consider_positions=false` at all** (the `url.Values` set is inside `if baskparam.ConsiderPositions`).
- The live doc documents the param as "Boolean" but states no server-side default and no accepted-casing rules — those remain **Unknown** (OPEN QUESTIONS #2).

### 4.2 Response

**Verified (LIVE-DOC runner 2026-07-14 — the margins page embeds the full basket sample, byte-identical to OFFICIAL-MOCK `basket_margins.json`):** `data` is an object with FOUR keys, now documented:

| Key | Shape | Live-doc definition |
|---|---|---|
| `initial` | one §3.2 `OrderMargins` object (identity fields empty) | "Total margins required to execute the orders" — i.e. WITHOUT basket offsets (sample: initial.total 96504.975) |
| `final` | same shape | "Total margins with the spread benefit" (sample: final.total 34786.725 — a short-option + long-option NFO basket where SPAN drops 66832.5 → 7788 and `option_premium` turns negative −2152.5 — negative values occur on the wire) |
| `orders` | array of §3.2 objects | "Individual margins per order" (+ per-leg charges) |
| `charges` | one §6 charges object | "Final charges block" — **documented with an official ignore-note (verbatim): "The final charges block can be ignored as it may not include transaction_tax charges because baskets can contain both mcx and equity instruments, with different tax types (STT or CTT). Users can refer to the individual order charges response in the orders block."** The doc's own sample shows the artifact class: aggregate `brokerage: 40` (= 20+20 of the legs) yet `total: 0` — real wire behaviour, officially not to be trusted for totals. *(RESOLVED by live fetch — was OPEN QUESTIONS #4.)* |

**SDK divergence (unchanged, now against a DOCUMENTED key) — Verified (CLIENT-LIB-SOURCE):** gokiteconnect's `BasketMargins` struct decodes only `{Initial, Final, Orders}` — the official Go SDK silently drops the documented `charges` key. pykiteconnect returns the raw dict, so Python callers DO see it.

**Compare: Groww.** Groww has no separate basket endpoint — its single `POST /v1/margins/detail/orders?segment=` accepts the array and basket-treats FNO/COMMODITY; it returns no charges breakdown (`docs/groww-ref/16-orders-margins-portfolio.md` §1 row 13, §3).

---

## 5. Virtual contract note — `POST /charges/orders`

**Verified (LIVE-DOC runner 2026-07-14 — margins page "Virtual contract note" section):** "A virtual contract provides detailed charges order-wise for brokerage, STT, stamp duty, exchange transaction charges, SEBI turnover charge, and GST."

### 5.1 Request

JSON array of order descriptors. **Hypothetical orders ARE accepted — Verified (LIVE-DOC runner 2026-07-14, verbatim):** `order_id` is a "Unique order ID (It can be any random string to calculate charges for an imaginary order)" — so the endpoint serves BOTH post-trade order-book pricing and pre-trade cost estimation. *(RESOLVED by live fetch — was OPEN QUESTIONS #7; CORRECTED: the previous Assumed claim "prices the day's order book, not hypotheticals" is contradicted by the live page and is withdrawn.)*

Live-doc param table (types per the live doc annotations):

| Field | JSON key | Type | Live-doc definition |
|---|---|---|---|
| Order id | `order_id` | string | "Unique order ID (It can be any random string to calculate charges for an imaginary order)" |
| Exchange | `exchange` | string | "Name of the exchange" |
| Trading symbol | `tradingsymbol` | string | "Exchange tradingsymbol of the instrument" |
| Transaction / variety / product / order type | `transaction_type`, `variety`, `product`, `order_type` | string | as §3.1 |
| Quantity | `quantity` | **int64** | "Quantity of the order" (float64 in the Go SDK — SDK looseness, the doc types it int64) |
| Average fill price | `average_price` | float64 | "Average price at which the order was executed (Note: Should be non-zero)." — for imaginary orders, the assumed execution price |

### 5.2 Response

**Verified (LIVE-DOC runner 2026-07-14 — embedded sample byte-identical to OFFICIAL-MOCK `virtual_contract_note.json`; live "Response attributes" table):** `data` = array, one element per order, echoing the order identity (`transaction_type`, `tradingsymbol`, `exchange`, `variety`, `product`, `order_type`, `quantity`, `price`) + a §6 `charges` object ("chargesmap — The breakdown of the various charges that will be applied to an order"). The response does NOT echo `order_id` — absent from the live response-attributes table, the sample, and the Go `OrderCharges` struct — so callers must correlate by array position (**Assumed** as the correlation practice; the non-echo itself is **Verified (LIVE-DOC)**). The input's `average_price` comes back as `price` ("Price at which the order is completed").

Sample rows worth keeping (**Verified (LIVE-DOC runner 2026-07-14)** for the shapes and values — the live doc's own example; the underlying pricing schedule matches Zerodha's published ₹0-delivery / ₹20-or-0.03% pricing but that schedule is sourced in `13-pricing-and-access-model.md`, not here):

| Order | `transaction_tax_type` | `brokerage` | Observation |
|---|---|---|---|
| NSE CNC BUY SBIN 1 @ 560 | `stt` | 0 | zero-brokerage equity delivery; STT 0.56 = 0.1% of 560 (**ARITH**) |
| MCX NRML SELL GOLDPETAL fut @ 5862 | **`ctt`** | 1.7586 | commodity transaction tax type differs; 1.7586 = 0.03% × 5862 (**ARITH**) |
| NFO NRML BUY option 100 @ 1.5 | `stt` | 20 | flat ₹20 F&O; buy-side option STT 0 |

---

## 6. The shared `charges` object (margins + basket + contract note)

**Verified (LIVE-DOC runner 2026-07-14 — margins page "Charges structure" table + embedded samples; cross-check gokiteconnect `Charges`/`GST`; byte-consistent across all three mock families):**

```json
"charges": {
  "transaction_tax": 1.498,
  "transaction_tax_type": "stt",
  "exchange_turnover_charge": 0.051681,
  "sebi_turnover_charge": 0.001498,
  "brokerage": 0.01,
  "stamp_duty": 0.22,
  "gst": { "igst": 0.01137222, "cgst": 0, "sgst": 0, "total": 0.01137222 },
  "total": 1.79255122
}
```

Live-doc field definitions (verbatim):

| Field | Live-doc definition |
|---|---|
| `total` | "Total charges" |
| `transaction_tax` | "Tax levied for each transaction on the exchanges" |
| `transaction_tax_type` | "Type of transaction tax" — observed values: `stt`, `ctt`, and `""` (empty, on basket aggregate/initial/final rows). Other values (e.g. for currency segment) **Unknown** |
| `exchange_turnover_charge` | "Charge levied by the exchange on the total turnover of the day" |
| `sebi_turnover_charge` | "Charge levied by SEBI on the total turnover of the day" |
| `brokerage` | "The brokerage charge for a particular trade" |
| `stamp_duty` | "Duty levied on the transaction value by Government of India" |
| `gst.igst` / `gst.cgst` / `gst.sgst` | "Integrated / Central / State Goods and Services Tax levied by the government" |
| `gst.total` | "Total GST" |

- Every live/mock sample shows IGST-only GST splits. No `cess` field exists anywhere on the live page, in the mocks, or in the SDK structs (**Verified-absence over the fetched artifacts, incl. the LIVE-DOC page**).
- Values are unrounded floats (e.g. `1.79255122`, `0.011372219999999999` — the long-tail repr appears verbatim on the live page) — store as f64, round only at display (**Verified (LIVE-DOC + OFFICIAL-MOCK)**).

**Compare: Dhan/Groww.** Neither Dhan nor Groww documents a per-order statutory-charges breakdown endpoint at all — Dhan exposes only a single `brokerage` number inside `/margincalculator` (`docs/dhan-ref/13-funds-margin.md` §2), Groww nothing. Kite's `charges` object is unique among the three packs.

---

## 7. Rate limits for these endpoints

**Verified (LIVE-DOC runner 2026-07-14 — https://kite.trade/docs/connect/v3/exceptions/ "API rate limit" table, verbatim):**

| end-point | rate-limit |
|---|---|
| Quote | 1req/second |
| Historical candle | 3req/second |
| Order placement | 10req/second |
| **All other endpoints** | **10req/second** |

`POST /margins/orders`, `POST /margins/basket`, `POST /charges/orders`, and `GET /user/margins*` are not named in the table, so they fall under the documented catch-all **"All other endpoints — 10req/second"** (classification is ours; the table row is LIVE-DOC). The page's order-specific notes (400 orders/min, 5,000 orders/day, 25 modifications/order) apply to order PLACEMENT, not these calculators. *(RESOLVED by live fetch — was OPEN QUESTIONS #5; supersedes the 2026-07-13 SEARCH-weak forum numbers.)* Neither SDK implements any client-side throttling for these calls (**Verified-absence, both SDK sources** — same finding class as groww-ref [R#24]).

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- ~~Funds field semantics~~ — **RESOLVED by live fetch (2026-07-14):** the `GET /user/margins` per-field description table is on the live user page and is quoted verbatim in §2.3 (incl. `live_balance`, `adhoc_margin`, `delivery`, `holding_sales`, `turnover`). Residual (NEW, narrow): the live doc's `utilised.debits` composition formula does not match its own sample arithmetic (141731.95 vs 145706.55), and the `net` parenthetical "(intraday_payin + adhoc_margin + collateral)" cannot be the complete formula — the exact server-side formulas for `debits` and `net` remain **Unknown**; do not reconstruct either client-side. Matters: any capital/risk display should read the fields, never derive them.
- ~~`POST /margins/orders` documented param table~~ — **RESOLVED by live fetch (2026-07-14):** table quoted in §3.1. Residuals: the live table omits `tradingsymbol` (doc omission — present in every example), carries no required/optional markers, and leaves `quantity` untyped on this endpoint (the `/charges/orders` twin types it `int64`) — server-side required-set and fractional-quantity acceptance remain **Assumed/Unknown**.
- ~~`mode=compact` on `/margins/orders`~~ — **RESOLVED by live fetch (2026-07-14):** documented on the live margins page for BOTH `/margins/orders` and `/margins/basket` ("compact - Compact mode will only give the total margins"). pykiteconnect not exposing it on `order_margins()` is an SDK gap. Residual: the exact compact-mode response SHAPE is not shown on the live page — **Unknown**.
- ~~Basket top-level `charges` block~~ — **RESOLVED by live fetch (2026-07-14):** documented as the "Final charges block" with an official ignore-note (mixed STT/CTT baskets make it unreliable; use per-leg `orders[].charges`). The `brokerage: 40, total: 0` aggregate is the doc's own sample — real wire behaviour. The Go SDK still drops the key (§4.2).
- ~~Rate-limit family for `/margins/*`, `/charges/orders`~~ — **RESOLVED by live fetch (2026-07-14):** the official exceptions-page table has no margins-specific row → the documented "All other endpoints 10req/second" catch-all applies (§7).
- ~~`/charges/orders` input domain~~ — **RESOLVED by live fetch (2026-07-14):** imaginary orders are explicitly accepted (`order_id` "can be any random string to calculate charges for an imaginary order"); `average_price` "Should be non-zero". It works as BOTH pre-trade estimator and post-trade contract note.
- **`consider_positions` wire contract (narrowed):** the live doc documents it as "Boolean to consider users positions" and the example uses lowercase `true`, but the server-side DEFAULT when omitted and accepted casings (`True` vs `true` vs `1` — pykiteconnect sends `True`) remain **Unknown**. Paste: `https://kite.trade/docs/connect/v3/margins/#basket-margins`. Matters: hedged-margin numbers silently change with this flag.
- **Dead route `GET /margins/{segment}`:** what does the pykiteconnect `market.margins` route serve today (legacy SPAN files?) — the LIVE v3 docs do not document it anywhere (live-absence confirmed 2026-07-14 over the fetched margins/user pages), so it is undocumented-legacy. Matters: pack completeness only (blocking: none).

## CLAIMS (for README reconciled table)

- Funds endpoints are `GET /user/margins` (both segments) + `GET /user/margins/:segment`, segment ∈ `equity`|`commodity` — **Verified (LIVE-DOC runner 2026-07-14)** — https://kite.trade/docs/connect/v3/user/#funds-and-margins (in fetch-log); cross-check CLIENT-LIB-SOURCE pykiteconnect `_routes` + `MARGIN_*` constants
- Per-segment funds response = `{enabled, net, available{adhoc_margin, cash, opening_balance, live_balance, collateral, intraday_payin}, utilised{debits, exposure, m2m_realised, m2m_unrealised, option_premium, payout, span, holding_sales, turnover, liquid_collateral, stock_collateral, delivery}}` — **Verified (LIVE-DOC runner 2026-07-14 embedded sample)** — https://kite.trade/docs/connect/v3/user/#funds-and-margins; cross-check OFFICIAL-MOCK margins.json + gokiteconnect user.go
- Per-field funds semantics (all 20 fields) documented on the live user page and quoted in §2.3 — **Verified (LIVE-DOC runner 2026-07-14)**
- `utilised.debits` does NOT equal its own documented composition in the doc's own sample (141731.95 vs 145706.55); exact composition undetermined — **ARITH over Verified (LIVE-DOC) values**
- Margin/charges calculators: `POST /margins/orders` ("considering the existing positions and open orders"), `POST /margins/basket` ("spread orders"), `POST /charges/orders` ("order-wise charges for orderbook"), all JSON POST with `application/json`, all taking a JSON **array** of order objects — **Verified (LIVE-DOC runner 2026-07-14)** — https://kite.trade/docs/connect/v3/margins/ (in fetch-log)
- Order-margin request object = `{exchange, tradingsymbol, transaction_type, variety, product, order_type, quantity, price?, trigger_price?}` with live-doc definitions; the live param table omits `tradingsymbol` (doc omission) and carries no required markers — **Verified (LIVE-DOC runner 2026-07-14 + CLIENT-LIB-SOURCE gokiteconnect `OrderMarginParam`)**
- `mode=compact` is documented on BOTH `/margins/orders` and `/margins/basket` ("Compact mode will only give the total margins"); pykiteconnect exposes it only on basket (SDK gap) — **Verified (LIVE-DOC runner 2026-07-14)**
- Order-margin response element = `{type: equity|commodity, span, exposure, option_premium, additional, bo, cash, var, pnl{realised,unrealised}, leverage, total}` + embedded per-order `charges`; `total` = "Total margin block", `leverage` = "Margin leverage allowed for the trade" — **Verified (LIVE-DOC runner 2026-07-14; sample byte-identical to OFFICIAL-MOCK order_margins.json)**
- Basket response = `{initial, final, orders[], charges}` — all four keys documented; `charges` = "Final charges block" with the official note that it "can be ignored" (mixed STT/CTT baskets) in favour of per-leg `orders[].charges`; the aggregate `brokerage: 40, total: 0` is the doc's own sample — **Verified (LIVE-DOC runner 2026-07-14)** — https://kite.trade/docs/connect/v3/margins/#basket-margins
- gokiteconnect's `BasketMargins` struct drops the documented top-level `charges` key ({Initial, Final, Orders} only) — **Verified (CLIENT-LIB-SOURCE vs LIVE-DOC)**
- Basket query params: `consider_positions` ("Boolean to consider users positions"; live example lowercase `true`; server default undocumented; Go can never send false; py sends `True`) + `mode=compact` — **Verified (LIVE-DOC runner 2026-07-14 + CLIENT-LIB-SOURCE, divergence stated)**
- Virtual contract note accepts IMAGINARY orders: `order_id` "can be any random string to calculate charges for an imaginary order"; `average_price` float64 "Should be non-zero"; `quantity` typed int64 — **Verified (LIVE-DOC runner 2026-07-14)** — corrects the prior Assumed "executed orders only" reading
- Virtual contract note response echoes order identity WITHOUT `order_id` (input `average_price` returns as `price`) + the charges object — **Verified (LIVE-DOC runner 2026-07-14 + OFFICIAL-MOCK virtual_contract_note.json + gokiteconnect `OrderCharges`)**
- Charges object = `{transaction_tax, transaction_tax_type: "stt"|"ctt"|"", exchange_turnover_charge, sebi_turnover_charge, brokerage, stamp_duty, gst{igst,cgst,sgst,total}, total}` with per-field live-doc definitions; no cess field exists in any fetched artifact incl. the live page — **Verified (LIVE-DOC runner 2026-07-14 + OFFICIAL-MOCK ×3 families + gokiteconnect `Charges`/`GST`)**
- Wire warts: JSON `-0` appears (`margin_commodity.json` `utilised.payout` — mock evidence); unrounded long-tail floats appear verbatim on the LIVE page (`0.011372219999999999`, `99725.05000000002`) — **Verified (LIVE-DOC + OFFICIAL-MOCK)**
- pykiteconnect defines a route `market.margins → GET /margins/{segment}` that NO method calls and the LIVE v3 docs do not document — **Verified (CLIENT-LIB-SOURCE, grep 2026-07-13; live-absence LIVE-DOC runner 2026-07-14)**
- Official rate-limit table (exceptions page): Quote 1/s, Historical 3/s, Order placement 10/s, **All other endpoints 10/s** — margins/charges/funds endpoints fall under the catch-all — **Verified (LIVE-DOC runner 2026-07-14)** — https://kite.trade/docs/connect/v3/exceptions/ (in fetch-log); supersedes the 2026-07-13 SEARCH-weak forum numbers
- Margin computation is included in the FREE Personal plan (no data subscription needed for the §3–§5 calculators); Connect ₹500/month/app adds WebSocket + historical data — **Verified (LIVE-DOC runner 2026-07-14)** — https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/what-are-the-charges-for-kite-apis (in fetch-log)
