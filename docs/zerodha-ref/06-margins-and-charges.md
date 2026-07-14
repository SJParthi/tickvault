# Zerodha Kite Connect v3 — Funds, Margins & Charges (evidence-tiered reference)

> **Source:** https://kite.trade/docs/connect/v3/user/#funds-and-margins · https://kite.trade/docs/connect/v3/margins/
> **Fetched-Verified:** 2026-07-13 via CLIENT-LIB-SOURCE (pykiteconnect@master + gokiteconnect@master, raw.githubusercontent.com / git clone) + OFFICIAL-MOCK (zerodha/kiteconnect-mocks@main `c7a8123`) + SEARCH (one weak rate-limit probe). The LIVE kite.trade docs, web.archive.org, and r.jina.ai were ALL blocked at the sandbox egress proxy (CONNECT 403, per the 2026-07-13 route probe) — **zero ARCHIVE-DOC / MIRROR-LIVE content exists in this file**; every wire shape below comes from official Zerodha SDK sources and official mock responses, and every prose *field meaning* that lives only on the blocked docs pages is labeled Assumed or Unknown.
> **Evidence tiers:** see README legend — Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) / Assumed / Unknown.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. TickVault places no Kite orders; order/margin surfaces are documented for completeness (the groww-ref `16-…` precedent).

---

## 1. Endpoint inventory (this file's surface)

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master `kiteconnect/connect.py` `_routes` dict, fetched 2026-07-13 — https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/connect.py; cross-checked against gokiteconnect@master `connect.go` URI constants):**

| # | Purpose | Method + path | py route key / method | Go URI const / method |
|---|---|---|---|---|
| 1 | Funds + margins, both segments | `GET /user/margins` | `user.margins` / `margins()` | `URIUserMargins` / `GetUserMargins()` |
| 2 | Funds + margins, one segment | `GET /user/margins/{segment}` | `user.margins.segment` / `margins(segment)` | `URIUserMarginsSegment` / `GetUserSegmentMargins(segment)` |
| 3 | Order margin calculator | `POST /margins/orders` | `order.margins` / `order_margins(params)` | `URIOrderMargins` / `GetOrderMargins()` |
| 4 | Basket margin calculator | `POST /margins/basket` | `order.margins.basket` / `basket_order_margins(params, consider_positions=True, mode=None)` | `URIBasketMargins` / `GetBasketMargins()` |
| 5 | Virtual contract note (order-wise charges) | `POST /charges/orders` | `order.contract_note` / `get_virtual_contract_note(params)` | `URIOrderCharges` / `GetOrderCharges()` |
| 6 | ⚠ legacy/dead route | `GET /margins/{segment}` | `market.margins` — defined in `_routes` but **NO pykiteconnect method calls it** (grep over connect.py, 2026-07-13); absent from gokiteconnect entirely | — |

Row 6 purpose is **Unknown** (historically the daily SPAN/margin-file surface; nothing in either SDK exercises it — see OPEN QUESTIONS).

**Common wire conventions — Verified (CLIENT-LIB-SOURCE pykiteconnect@master connect.py):** base URL `https://api.kite.trade` (`_default_root_uri`); headers `X-Kite-Version: 3` (`kite_header_version = "3"`) + `Authorization: token {api_key}:{access_token}`; JSON envelope `{"status": "success", "data": …}` — the SDK returns `data["data"]`; error envelope has `status: "error"` + `error_type` mapped to a typed exception (a 403 `TokenException` additionally fires the session-expiry hook).

**Compare: Dhan/Groww.** Dhan's equivalents are `GET /fundlimit` (one flat object, no per-segment split) + `POST /margincalculator` taking a SINGLE order object (+ a `/margincalculator/multi` variant), with `brokerage` folded into the margin response and no dedicated charges endpoint (`docs/dhan-ref/13-funds-margin.md` §1–§2). Groww's are `GET /v1/margins/detail/user` + one combined `POST /v1/margins/detail/orders?segment=` (JSON-array body; "Basket orders are supported for FNO and COMMODITY segments") with no charges endpoint (`docs/groww-ref/16-orders-margins-portfolio.md` §1 rows 12–13). Kite is the only one of the three with (a) a segment-split funds response, (b) a separate basket endpoint with position-offset semantics, and (c) a standalone order-wise charges calculator.

---

## 2. Funds & margins — `GET /user/margins` and `GET /user/margins/{segment}`

### 2.1 Segments

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master connect.py):** `segment` ∈ `equity` | `commodity` (constants `MARGIN_EQUITY = "equity"`, `MARGIN_COMMODITY = "commodity"`). The unsegmented call returns BOTH under keys `equity` and `commodity` (gokiteconnect `AllMargins{Equity, Commodity}`; mock `margins.json` matches).

### 2.2 Response shape (full-segment call) — verbatim-derived

**Verified (OFFICIAL-MOCK `margins.json` + `margins_equity.json` + `margin_commodity.json`, kiteconnect-mocks@main c7a8123, fetched 2026-07-13 — https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/margins.json; field set independently confirmed by gokiteconnect@master `user.go` structs `Margins`/`AvailableMargins`/`UsedMargins`):**

```json
{
  "status": "success",
  "data": {
    "equity": {
      "enabled": true,
      "net": 99725.05,
      "available": {
        "adhoc_margin": 0, "cash": 245431.6, "opening_balance": 245431.6,
        "live_balance": 99725.05, "collateral": 0, "intraday_payin": 0
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

The segmented call (`/user/margins/equity`) returns the SAME per-segment object directly under `data` (no `equity` wrapper) — **Verified (OFFICIAL-MOCK `margins_equity.json`, `margin_commodity.json`)**.

### 2.3 Field table

Field NAMES + JSON types are **Verified** (mock + Go struct json tags). Field MEANINGS below are **Assumed from the field/struct names** unless noted — the official per-field description table lives on the blocked `…/user/#funds-and-margins` page (OPEN QUESTIONS #1).

| Field | Type | Meaning |
|---|---|---|
| `enabled` | bool | segment is active for the account — Assumed |
| `net` | float | net available margin for the segment — Assumed (mock arithmetic: equity `net` 99725.05 ≠ `cash` − `debits` = 99725.05 ✓ — **ARITH:** 245431.6 − 145706.55 = 99725.05, so `net = available.cash − utilised.debits` holds in the mock; also `net == available.live_balance` in all three mocks) |
| `available.adhoc_margin` | float | manually granted ad-hoc margin — Assumed (Go `AdHocMargin`) |
| `available.cash` | float | raw cash balance — Assumed |
| `available.opening_balance` | float | day-open balance — Assumed |
| `available.live_balance` | float | real-time available balance — Assumed (equals `net` in every mock) |
| `available.collateral` | float | collateral margin (pledged) — Assumed |
| `available.intraday_payin` | float | funds added during the day — Assumed |
| `utilised.debits` | float | total utilised — Assumed (**ARITH note:** in the equity mock, `debits` 145706.55 ≠ span+exposure+m2m_realised = 101989 + 38981.25 + 761.7 = 141731.95 — the components do NOT sum to `debits`; the exact composition is Unknown, do not reconstruct it) |
| `utilised.exposure` / `utilised.span` | float | exposure / SPAN margins blocked (F&O) — Assumed |
| `utilised.m2m_realised` / `m2m_unrealised` | float | realised / unrealised mark-to-market — Assumed |
| `utilised.option_premium` | float | premium blocked for options — Assumed |
| `utilised.payout` | float | requested payout — Assumed. **Wire wart — Verified (OFFICIAL-MOCK margin_commodity.json):** the value `-0` (negative zero) appears on the wire; parsers must tolerate it |
| `utilised.holding_sales` / `turnover` / `delivery` | float | names only Verified; semantics Unknown |
| `utilised.liquid_collateral` / `stock_collateral` | float | collateral utilisation split (liquid funds vs stocks) — Assumed |

**Compare: Dhan.** Dhan's `/fundlimit` is a single flat object and famously carries the `availabelBalance` spelling typo which integrators must keep (`.claude/rules/dhan/funds-margin.md`); Kite's funds response has no known typo fields and is segment-nested.

---

## 3. Order margin calculator — `POST /margins/orders`

### 3.1 Request

**Verified (CLIENT-LIB-SOURCE gokiteconnect@master `margins.go` `OrderMarginParam`; pykiteconnect `order_margins(params)` posts the caller's list verbatim with `is_json=True`):** the body is a **JSON array** of order objects:

| Field | JSON key | Go type | Required? |
|---|---|---|---|
| Exchange | `exchange` | string | yes (no omitempty) |
| Trading symbol | `tradingsymbol` | string | yes |
| Transaction type | `transaction_type` | string (`BUY`/`SELL`) | yes |
| Variety | `variety` | string (`regular`/`co`/`amo`/`iceberg`/`auction` per pykiteconnect constants) | yes |
| Product | `product` | string | yes |
| Order type | `order_type` | string | yes |
| Quantity | `quantity` | **float64** in Go | yes |
| Price | `price` | float64, `omitempty` | optional |
| Trigger price | `trigger_price` | float64, `omitempty` | optional |

Required-vs-optional above is inferred from Go `omitempty` tags — **Assumed** for server-side truth (the documented param table is on the blocked `…/v3/margins/` page; OPEN QUESTIONS #2). `quantity` being float64 in the official Go SDK is a faithful report, not a claim that fractional quantities are accepted (**Unknown**).

**Compact mode divergence — Verified (both SDK sources):** gokiteconnect appends `?mode=compact` to `/margins/orders` when `Compact` is set; **pykiteconnect does NOT expose any `mode` parameter on `order_margins()`** (only on the basket call). So the official Go SDK implies `/margins/orders` supports compact mode; the live docs confirmation is OPEN QUESTIONS #3.

### 3.2 Response

**Verified (OFFICIAL-MOCK `order_margins.json` — https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/order_margins.json; struct twin: gokiteconnect `OrderMargins`):** `data` is an **array**, one element per input order:

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

- `type` is the margin segment classification (`"equity"` in both equity and NFO mock rows — see the basket mock where NFO option legs also carry `type: "equity"`). Meaning of possible values is **Unknown**.
- `span`/`exposure`/`option_premium`/`additional`/`bo`/`cash`/`var` are the margin components; `total` is the blocked margin for the order (**ARITH:** equity mock `total` 1498 = `var` 1498). `bo` = bracket-order margin — Assumed from the name.
- `charges` embeds the full §6 charges breakdown per order — i.e. the margin API also returns estimated charges. **Verified (OFFICIAL-MOCK)**.

**Compare: Dhan.** Dhan's calculator takes ONE order per call (`POST /margincalculator`, single JSON object with `dhanClientId` in the body) and returns a flat response with `insufficientBalance` and `leverage` as a STRING (`docs/dhan-ref/13-funds-margin.md` §2); Kite takes an array, returns per-order component + charges breakdowns, and has no shortfall field.

---

## 4. Basket margin calculator — `POST /margins/basket`

### 4.1 Request

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master connect.py `basket_order_margins`):** body = the SAME JSON array of §3.1 order objects; query params `consider_positions` (py default `True`) and `mode` (py docstring: "compact - Compact mode will only give the total margins").

**Query-param serialization warts — Verified (both SDK sources, divergent):**
- pykiteconnect passes the Python bool straight into `requests` query params → the wire sees `consider_positions=True` (capital T) and omits `mode` when `None`.
- gokiteconnect sets `consider_positions=true` (lowercase) ONLY when true — **the Go SDK cannot send `consider_positions=false` at all** (the `url.Values` set is inside `if baskparam.ConsiderPositions`).
- Accepted casings/values and the server-side default are **Unknown** (OPEN QUESTIONS #6).

### 4.2 Response

**Verified (OFFICIAL-MOCK `basket_margins.json` — https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/basket_margins.json):** `data` is an object with FOUR keys:

| Key | Shape | Meaning |
|---|---|---|
| `initial` | one §3.2 `OrderMargins` object (identity fields empty) | margin required WITHOUT basket offsets — Assumed (mock: initial.total 96504.975) |
| `final` | same shape | margin required WITH basket/hedge benefit — Assumed (mock: final.total 34786.725, i.e. a short-option + long-option NFO basket where SPAN drops 66832.5 → 7788 and `option_premium` turns negative −2152.5 — negative values occur on the wire) |
| `orders` | array of §3.2 objects | per-leg standalone margins + per-leg charges |
| `charges` | one §6 charges object | **top-level aggregate charges — present in the OFFICIAL MOCK but ABSENT from gokiteconnect's `BasketMargins` struct** (`{Initial, Final, Orders}` only) — the official Go SDK silently drops this key. Verified divergence; documented meaning is OPEN QUESTIONS #4. Mock oddity: the aggregate has `brokerage: 40` (= 20+20 of the legs) yet `total: 0` — do not trust the aggregate `total` without live confirmation |

pykiteconnect returns the raw dict, so Python callers DO see `charges`.

**Compare: Groww.** Groww has no separate basket endpoint — its single `POST /v1/margins/detail/orders?segment=` accepts the array and basket-treats FNO/COMMODITY; it returns no charges breakdown (`docs/groww-ref/16-orders-margins-portfolio.md` §1 row 13, §3).

---

## 5. Virtual contract note — `POST /charges/orders`

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master connect.py `get_virtual_contract_note` — "Calculates detailed charges order-wise for the order book"; gokiteconnect `OrderChargesParam`/`GetOrderCharges`):**

### 5.1 Request

JSON array of EXECUTED-order descriptors (this endpoint prices the day's order book, not hypotheticals — Assumed from the docstring "for the order book" + the `order_id`/`average_price` fields; whether hypothetical orders are also accepted is OPEN QUESTIONS #7):

| Field | JSON key | Note |
|---|---|---|
| Order id | `order_id` | string; no omitempty in Go |
| Exchange / symbol / txn / variety / product / order type | `exchange`, `tradingsymbol`, `transaction_type`, `variety`, `product`, `order_type` | as §3.1 |
| Quantity | `quantity` | float64 in Go |
| Average fill price | `average_price` | float64 — the EXECUTED price, not a limit price |

### 5.2 Response

**Verified (OFFICIAL-MOCK `virtual_contract_note.json` — https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/virtual_contract_note.json):** `data` = array, one element per order, echoing the order identity (`transaction_type`, `tradingsymbol`, `exchange`, `variety`, `product`, `order_type`, `quantity`, `price`) + a §6 `charges` object. Notably the response does NOT echo `order_id` (absent from both the mock and the Go `OrderCharges` struct) — callers must correlate by array position (**Assumed**) — and the input's `average_price` comes back as `price`.

Mock rows worth keeping (real charge behaviour, **Verified (OFFICIAL-MOCK)** for the shapes; the underlying pricing schedule itself is Assumed — it matches Zerodha's published ₹0-delivery / ₹20-or-0.03% pricing but that schedule is not sourced here):

| Order | `transaction_tax_type` | `brokerage` | Observation |
|---|---|---|---|
| NSE CNC BUY SBIN 1 @ 560 | `stt` | 0 | zero-brokerage equity delivery; STT 0.56 = 0.1% of 560 (**ARITH**) |
| MCX NRML SELL GOLDPETAL fut @ 5862 | **`ctt`** | 1.7586 | commodity transaction tax type differs; 1.7586 = 0.03% × 5862 (**ARITH**) |
| NFO NRML BUY option 100 @ 1.5 | `stt` | 20 | flat ₹20 F&O; buy-side option STT 0 |

---

## 6. The shared `charges` object (margins + basket + contract note)

**Verified (CLIENT-LIB-SOURCE gokiteconnect@master margins.go `Charges`/`GST`; byte-consistent across all three mock families):**

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

- `transaction_tax_type` observed values: `stt`, `ctt`, and `""` (empty, on basket aggregate/initial/final rows) — **Verified (OFFICIAL-MOCK)**. Other values (e.g. for currency segment) **Unknown**.
- GST is split `igst`/`cgst`/`sgst` + `total`; every mock shows IGST-only. No `cess` field exists anywhere in the mocks or SDK structs (**Verified-absence over the fetched artifacts**; whether the live schema has since grown one is Unknown).
- Values are unrounded floats (e.g. `1.79255122`, `0.011372219999999999`) — store as f64, round only at display (**Verified (OFFICIAL-MOCK)**).

**Compare: Dhan/Groww.** Neither Dhan nor Groww documents a per-order statutory-charges breakdown endpoint at all — Dhan exposes only a single `brokerage` number inside `/margincalculator` (`docs/dhan-ref/13-funds-margin.md` §2), Groww nothing. Kite's `charges` object is unique among the three packs.

---

## 7. Rate limits for these endpoints

**SEARCH (2026-07-13, weak):** a search-backend probe surfaced only forum-grade numbers — "all GET calls < 10 req/s combined", "10 orders/s, [200–400]/min order caps" — from [kite.trade forum threads](https://kite.trade/forum/discussion/2354/what-are-the-limits-of-kite-api) ([rate limits](https://kite.trade/forum/discussion/13397/rate-limits), [API rate limits](https://kite.trade/forum/discussion/8577/api-rate-limits)), NOT from the official limits table, and NOTHING names a family for `POST /margins/*` / `POST /charges/orders`. Treat every number in this paragraph as unconfirmed; the exact question is OPEN QUESTIONS #5. Neither SDK implements any client-side throttling for these calls (**Verified-absence, both SDK sources** — same finding class as groww-ref [R#24]).

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Funds field semantics:** the official per-field description table for `GET /user/margins` (exact meaning of `live_balance` vs `net`, `adhoc_margin`, `delivery`, `holding_sales`, `turnover`, and the composition of `utilised.debits` — the mock proves components don't sum to it). Paste: `https://kite.trade/docs/connect/v3/user/#funds-and-margins`. Matters: any capital/risk display built on these fields is guess-labeled until then.
- **`POST /margins/orders` documented param table:** required vs optional fields, `quantity` integer-vs-float, `price` handling for MARKET orders, accepted `variety` values. Paste: `https://kite.trade/docs/connect/v3/margins/`. Matters: request contract for any margin pre-check.
- **`mode=compact` on `/margins/orders`:** documented? (gokiteconnect sends it; pykiteconnect doesn't expose it). Paste: `https://kite.trade/docs/connect/v3/margins/`. Matters: response-shape switch a parser must handle.
- **Basket top-level `charges` block:** documented meaning (aggregate of legs?) and why the official Go struct omits it; is the observed `brokerage: 40, total: 0` aggregate a mock artifact or real wire behaviour? Paste: `https://kite.trade/docs/connect/v3/margins/#basket-margins`. Matters: anyone summing basket costs from `charges.total` would read 0.
- **Rate-limit family for `/margins/orders`, `/margins/basket`, `/charges/orders`:** the official limits table (the docs FAQ / exceptions page). Paste: `https://kite.trade/docs/connect/v3/exceptions/` (+ the limits section of the docs). Matters: budget math for any pre-trade margin-check loop.
- **`consider_positions` wire contract:** server default, accepted values/casing (`True` vs `true` vs `1`), and whether `false` is honored (the Go SDK cannot even send it). Paste: `https://kite.trade/docs/connect/v3/margins/#basket-margins`. Matters: hedged-margin numbers silently change with this flag.
- **`/charges/orders` input domain:** executed orders only (order_id + average_price mandatory?) or hypothetical orders too? Paste: `https://kite.trade/docs/connect/v3/margins/#virtual-contract-note`. Matters: whether it can serve as a pre-trade cost estimator or only post-trade.
- **Dead route `GET /margins/{segment}`:** what does the pykiteconnect `market.margins` route serve today (legacy SPAN files?) — is it documented anywhere in v3? Paste: `https://kite.trade/docs/connect/v3/` (search "margins"). Matters: pack completeness only (blocking: none).

## CLAIMS (for README reconciled table)

- Funds endpoints are `GET /user/margins` (both segments) + `GET /user/margins/{segment}`, segment ∈ `equity`|`commodity` — **Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py `_routes` + `MARGIN_*` constants; gokiteconnect connect.go)** — https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/connect.py
- Per-segment funds response = `{enabled, net, available{adhoc_margin, cash, opening_balance, live_balance, collateral, intraday_payin}, utilised{debits, exposure, m2m_realised, m2m_unrealised, option_premium, payout, span, holding_sales, turnover, liquid_collateral, stock_collateral, delivery}}` — **Verified (OFFICIAL-MOCK margins.json/margins_equity.json/margin_commodity.json + gokiteconnect user.go structs)** — https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/margins.json
- `utilised.debits` does NOT equal span+exposure+m2m in the official mock (145706.55 vs 141731.95); its composition is undetermined — **ARITH over Verified mock values**
- Margin/charges calculators: `POST /margins/orders`, `POST /margins/basket`, `POST /charges/orders`, all taking a JSON **array** of order objects — **Verified (CLIENT-LIB-SOURCE pykiteconnect `_routes` "Margin computation endpoints" + gokiteconnect margins.go)**
- Order-margin request object = `{exchange, tradingsymbol, transaction_type, variety, product, order_type, quantity, price?, trigger_price?}` — **Verified (CLIENT-LIB-SOURCE gokiteconnect margins.go `OrderMarginParam`)** — https://raw.githubusercontent.com/zerodha/gokiteconnect/master/margins.go
- Order-margin response element = margin components `{type, span, exposure, option_premium, additional, bo, cash, var, pnl{realised,unrealised}, leverage, total}` PLUS an embedded per-order `charges` breakdown — **Verified (OFFICIAL-MOCK order_margins.json + gokiteconnect `OrderMargins`)** — https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/order_margins.json
- Basket response = `{initial, final, orders[]}` + a top-level `charges` aggregate that the official Go SDK struct DROPS (mock has 4 keys, `BasketMargins` decodes 3) — **Verified (OFFICIAL-MOCK basket_margins.json vs CLIENT-LIB-SOURCE gokiteconnect margins.go)** — https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/basket_margins.json
- Basket query params: `consider_positions` (py default True; Go can never send false) + `mode=compact`; pykiteconnect exposes `mode` only on basket while gokiteconnect also sends `?mode=compact` on `/margins/orders` — **Verified (CLIENT-LIB-SOURCE, both SDKs; divergence stated)**
- Virtual contract note request = executed-order descriptors incl. `order_id` + `average_price`; response echoes order identity (WITHOUT order_id) + the charges object — **Verified (CLIENT-LIB-SOURCE gokiteconnect `OrderChargesParam`/`OrderCharges` + OFFICIAL-MOCK virtual_contract_note.json)**
- Charges object = `{transaction_tax, transaction_tax_type: "stt"|"ctt"|"", exchange_turnover_charge, sebi_turnover_charge, brokerage, stamp_duty, gst{igst,cgst,sgst,total}, total}`; no cess field exists in any fetched artifact — **Verified (OFFICIAL-MOCK ×3 families + gokiteconnect `Charges`/`GST`)**
- Wire warts: JSON `-0` appears (`margin_commodity.json` `utilised.payout`); unrounded long-tail floats throughout — **Verified (OFFICIAL-MOCK)**
- pykiteconnect defines a route `market.margins → GET /margins/{segment}` that NO method calls (dead/legacy) — **Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py, grep 2026-07-13)**
- Rate limits for the margin/charges endpoints: NOT determinable from SDK/mocks; forum-grade search numbers only (10 req/s combined GET; 10/s + per-min order caps) — **SEARCH (weak) / Unknown**
