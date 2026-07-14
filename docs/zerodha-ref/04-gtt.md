# Zerodha Kite Connect v3 — GTT (Good Till Triggered) Orders

> **Source:** https://kite.trade/docs/connect/v3/gtt/ (official page — NOT directly fetchable from this sandbox; kite.trade is proxy-blocked at CONNECT, and web.archive.org + r.jina.ai are equally blocked — see the probe record). Recovered from: official SDK sources (`zerodha/pykiteconnect`, `zerodha/gokiteconnect`) + official mock responses (`zerodha/kiteconnect-mocks`) + SEARCH snippets of the live page.
> **Fetched-Verified:** 2026-07-13 via CLIENT-LIB-SOURCE (raw.githubusercontent.com — pykiteconnect@master `kiteconnect/connect.py`, gokiteconnect@master `gtt.go` + `connect.go`), OFFICIAL-MOCK (kiteconnect-mocks@main `gtt_*.json`, 5 files actually fetched), SEARCH (web search snippets of kite.trade + support.zerodha.com), ARITH.
> **Evidence tiers:** see README legend — **Verified (CLIENT-LIB-SOURCE …)** / **Verified (OFFICIAL-MOCK …)** / **SEARCH** / **Assumed** / **Unknown**. A claim sourced ONLY from a search snippet is labeled SEARCH and must be re-read from the live page before contracting.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. TickVault places NO orders (dry_run stands); any Kite integration would need its own dated operator grant per the scope-lock protocol.
> **Related:** `07b-forever-order.md` in `docs/dhan-ref/` (the Dhan equivalent), `docs/groww-ref/16-orders-margins-portfolio.md` rows 14–18 (Groww "Smart Orders"), `15-alerts.md` (the ATO — alert-triggered-orders — sibling surface, added 2026-07-13), `99-UNKNOWNS.md`.

---

## 1. Overview + endpoint table

GTT ("Good Till Triggered") triggers sit server-side at Zerodha: a **condition** on an instrument's price is monitored, and when a trigger value is breached, the attached **order(s)** are placed on the exchange. Two trigger types exist: `single` (one trigger value, one order) and `two-leg` (OCO — two trigger values, two orders, first hit cancels the other).

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master `kiteconnect/connect.py` `_routes` dict, lines 158–163, fetched 2026-07-13; cross-checked gokiteconnect@master `connect.go` `URIPlaceGTT…URIDeleteGTT`, lines 140–145):**

| Method | Endpoint | SDK method (py) | Purpose |
|---|---|---|---|
| GET | `/gtt/triggers` | `get_gtts()` | List all GTTs on the account |
| POST | `/gtt/triggers` | `place_gtt(...)` | Create a trigger |
| GET | `/gtt/triggers/{trigger_id}` | `get_gtt(trigger_id)` | Fetch one trigger |
| PUT | `/gtt/triggers/{trigger_id}` | `modify_gtt(trigger_id, ...)` | Modify a trigger (full condition + orders resent) |
| DELETE | `/gtt/triggers/{trigger_id}` | `delete_gtt(trigger_id)` | Delete a trigger |

Base URL `https://api.kite.trade` with header `X-Kite-Version: 3` — **Verified (CLIENT-LIB-SOURCE pykiteconnect `connect.py` lines 36/41/938)**.

The live doc page exists at the URL above — **Verified-existence (SEARCH 2026-07-13:** result title "GTT orders - Kite Connect 3 / API documentation", https://kite.trade/docs/connect/v3/gtt/**)**; its full prose was NOT capturable.

## 2. Trigger types

**Verified (CLIENT-LIB-SOURCE pykiteconnect `connect.py` lines 98–99; identical in gokiteconnect `gtt.go` `GTTTypeSingle`/`GTTTypeOCO`):**

| Wire value (`type`) | SDK constant | Meaning | `trigger_values` arity |
|---|---|---|---|
| `"single"` | `GTT_TYPE_SINGLE` | one trigger value → one order | exactly 1 (SDK raises `InputException` otherwise) |
| `"two-leg"` | `GTT_TYPE_OCO` | OCO — two trigger values, two orders; executing one cancels the other | exactly 2 (SDK-enforced) |

**Note the naming wart (Verified, both SDKs):** the OCO wire value is the string `"two-leg"`, not `"oco"`.

**OCO leg ordering — Assumed (from gokiteconnect `gtt.go` `GTTOneCancelsOtherTrigger`):** the Go SDK serializes `trigger_values` as `[Lower, Upper]` (stop-loss trigger first, target second), and the official mock two-leg example has `trigger_values: [102.0, 103.7]` around `last_price: 102.6` — lower/stop first, upper/target second. The `orders[i]` index pairs with `trigger_values[i]` — **SEARCH (2026-07-13 snippet of the gtt doc page:** "The index of the order is used by the API to determine which order is executed when a trigger value is reached"**)**. Re-verify the exact pairing prose on the live page.

## 3. Create — `POST /gtt/triggers`

**Wire format (Verified, CLIENT-LIB-SOURCE both SDKs):** the request body is **form-encoded** (`application/x-www-form-urlencoded`), NOT a JSON body — pykiteconnect's `_post` defaults `is_json=False` → `data=params` (`connect.py` `_request`), and gokiteconnect uses `url.Values`. Three form fields:

| Form field | Content |
|---|---|
| `type` | `"single"` or `"two-leg"` |
| `condition` | a **JSON-serialized string** of the condition object |
| `orders` | a **JSON-serialized string** of the orders array |

i.e. JSON documents are nested as string values inside form fields — an unusual shape worth flagging for any native client.

**The `condition` object (Verified, CLIENT-LIB-SOURCE pykiteconnect `_get_gtt_payload`, lines 732–764; gokiteconnect `GTTCondition`):**

```json
{
  "exchange": "NSE",
  "tradingsymbol": "INFY",
  "trigger_values": [702],
  "last_price": 798
}
```

- `last_price` = the instrument's last price at placement time (the caller supplies it; per the SDK docstring "Last price of the instrument at the time of order placement"). Presumably used server-side to validate trigger distance from LTP — **Assumed** (the validation rule itself, e.g. a minimum % distance, is on the blocked live page → OPEN QUESTIONS).
- The **response-side** condition additionally carries `instrument_token` (see §6); whether it is accepted/required on create is **Unknown** (neither SDK sends it).

**Each element of the `orders` array (Verified, CLIENT-LIB-SOURCE pykiteconnect `_get_gtt_payload` — required keys asserted: `transaction_type`, `quantity`, `order_type`, `product`, `price`):**

```json
{
  "exchange": "NSE",
  "tradingsymbol": "INFY",
  "transaction_type": "BUY",
  "quantity": 1,
  "order_type": "LIMIT",
  "price": 702.5,
  "product": "CNC"
}
```

- `exchange`/`tradingsymbol` are copied from the condition by both SDKs — one instrument per trigger; a GTT spanning two instruments is not expressible through either SDK. **Assumed** the API enforces the same.
- **SDK divergence (Verified):** pykiteconnect passes the caller's `order_type` through; **gokiteconnect hard-codes `order_type = LIMIT`** and defaults `product` to `CNC` when empty (`gtt.go` `newGTT`). Whether the server accepts `MARKET`/`SL` GTT legs is **Unknown** → OPEN QUESTIONS.
- One order per trigger value: `single` → 1-element array; `two-leg` → 2-element array (Go SDK constructs exactly one order per trigger value).

**Response — Verified (OFFICIAL-MOCK kiteconnect-mocks@main `gtt_place_order.json`, fetched 2026-07-13):**

```json
{"status":"success","data":{"trigger_id":123}}
```

## 4. Modify — `PUT /gtt/triggers/{trigger_id}`

**Verified (CLIENT-LIB-SOURCE pykiteconnect `modify_gtt`, lines 791–814; gokiteconnect `ModifyGTT`):** same three form fields as create (`type`, `condition`, `orders`) — the ENTIRE condition + orders set is resent; there is no per-field patch and no per-leg modify. Response mirror of create — **Verified (OFFICIAL-MOCK `gtt_modify_order.json`)**: `{"status":"success","data":{"trigger_id":123}}`.

## 5. Delete — `DELETE /gtt/triggers/{trigger_id}`

**Verified (CLIENT-LIB-SOURCE both SDKs; OFFICIAL-MOCK `gtt_delete_order.json`):** no body; response `{"status":"success","data":{"trigger_id":123}}`. Post-delete the trigger presumably shows `status: "deleted"` in listings (the status constant exists — §7) — **Assumed**.

## 6. List / fetch — the full GTT object

**Verified (OFFICIAL-MOCK `gtt_get_orders.json` — a `single`/`active` and a `two-leg`/`triggered` example — and `gtt_get_order.json`, both fetched 2026-07-13; struct cross-checked against gokiteconnect `GTT`):**

| Field | Type / example | Notes |
|---|---|---|
| `id` | int, `112127` | the trigger_id |
| `user_id` | `"XX0000"` | |
| `parent_trigger` | `null` in every mock | semantics **Unknown** → OPEN QUESTIONS |
| `type` | `"single"` / `"two-leg"` | |
| `created_at` / `updated_at` | `"2019-09-12 13:25:16"` | space-separated timestamps, no timezone marker — **Assumed IST** (same convention as Kite order timestamps) |
| `expires_at` | `"2020-09-12 13:25:16"` | see §8 |
| `status` | `"active"` / `"triggered"` | full set §7 |
| `condition` | create-shape + **`instrument_token`** (e.g. `408065`) | server enriches with the numeric token |
| `orders[]` | create-shape + **`result`** | `result` is `null` until the leg triggers |
| `meta` | `{}` / `null` in mocks | Go SDK models it as `{"rejection_reason": string}` (`gtt.go` `GTTMeta`) — populated shape **Unknown** |

**The triggered-leg `result` object (Verified, OFFICIAL-MOCK `gtt_get_order.json` — a real failure example):**

```json
"result": {
  "account_id": "XX0000",
  "exchange": "NSE", "tradingsymbol": "RAIN",
  "validity": "DAY", "product": "CNC", "order_type": "LIMIT",
  "transaction_type": "SELL", "quantity": 1, "price": 1,
  "meta": "{\"app_id\":12617,\"gtt\":105099}",
  "timestamp": "2019-09-09 15:15:08",
  "triggered_at": 103.7,
  "order_result": {
    "status": "failed",
    "order_id": "",
    "rejection_reason": "Your order price is lower than the current lower circuit limit of 70.65. Place an order within the daily range."
  }
}
```

Load-bearing honesty: **a `triggered` GTT does NOT mean a filled (or even accepted) order** — the mock's own example is a trigger that fired but whose order was REJECTED at placement (circuit-limit breach). `orders[i].result.order_result.status/order_id/rejection_reason` is the only place that truth lives; `triggered_at` carries the price that fired the leg, and the placed order's validity materializes as `DAY`. Any consumer must poll/inspect the result object (or the postback), never infer execution from `status == "triggered"`.

## 7. Statuses

**Verified (CLIENT-LIB-SOURCE pykiteconnect `connect.py` lines 102–108 — the seven `GTT_STATUS_*` constants):**

`active` · `triggered` · `disabled` · `expired` · `cancelled` · `rejected` · `deleted`

`active` and `triggered` are additionally **Verified (OFFICIAL-MOCK)** on the wire. The precise semantics of `disabled` (broker-side disable, e.g. after corporate actions or risk checks) vs `cancelled` vs `deleted` are on the blocked live/support pages — **Unknown**, → OPEN QUESTIONS.

## 8. Expiry + active-GTT limits

- **Trigger validity ≈ 1 year — SEARCH (support.zerodha.com "What is the validity of a GTT order?", https://support.zerodha.com/category/trading-and-markets/kite-features/gtt/articles/what-is-the-validity-of-a-gtt-order, snippet 2026-07-13: GTT "remains valid for one year from the date you place it" / "active for up to 365 days") + ARITH (OFFICIAL-MOCK `gtt_get_orders.json`: `created_at 2019-09-12 13:25:16` → `expires_at 2020-09-12 13:25:16` = exactly +1 year to the second).** The second mock entry has `expires_at 2020-01-01 12:00:00` NOT one year from its `created_at 2019-09-09` — so `expires_at` may be settable/adjustable rather than always created_at+1y; whether the API accepts a client-supplied expiry is **Unknown** (neither SDK sends one) → OPEN QUESTIONS.
- **Max active GTTs per account — SEARCH (conflicting):** the 2026-07-13 search summary reports a current cap of **500 active GTTs** (support.zerodha.com "What is Good Till Triggered (GTT)…"), with older third-party pages citing 250 or 100 — the number has demonstrably moved over time (same lesson as the Groww rate-limit drift, groww-ref [R#3/#4]). Treat as **SEARCH, unpinned** → OPEN QUESTIONS for the live re-read.
- **GTT API rate limit:** **Unknown** — which rate bucket `/gtt/triggers` falls into (the order 10/s bucket vs the general bucket) is on the blocked live docs → OPEN QUESTIONS.

## 9. Compare: Dhan / Groww

| Aspect | Kite GTT | Dhan Forever Order (`docs/dhan-ref/07b-forever-order.md`) | Groww Smart Order (`docs/groww-ref/16-orders-margins-portfolio.md` row 14) |
|---|---|---|---|
| Endpoint family | `/gtt/triggers` (POST/GET/PUT/DELETE, RESTful id in path) | `/forever/orders` (POST/PUT/DELETE/{order-id}, GET list) | full documented surface: `POST /v1/order-advance/create`, `PUT /v1/order-advance/modify/{smart_order_id}`, `POST /v1/order-advance/cancel/{segment}/{type}/{id}`, status + list (LIVE-DOC — `docs/groww-ref/16-orders-margins-portfolio.md` §1 rows 14–18) |
| Type naming | `type`: `"single"` / `"two-leg"` | `orderFlag`: `SINGLE` / `OCO` | GTT (single trigger) / OCO |
| Payload shape | form fields with **JSON-strings** `condition` + `orders[]` (nested) | FLAT JSON body: `price`/`triggerPrice` + `price1`/`triggerPrice1`/`quantity1` for the second leg | JSON body (per the groww-ref rows) |
| Modify granularity | whole condition+orders resent | per-leg via `legName` (`TARGET_LEG`/`STOP_LOSS_LEG`) | documented modify endpoint (`PUT /v1/order-advance/modify/{smart_order_id}` — groww-ref 16 §1) |
| Product restriction | none SDK-enforced (Go defaults CNC) | `CNC`, `MTF` only; **static IP required** | OCO not supported for CASH segment; COMMODITY unsupported |
| "Armed" status label | `active` | `CONFIRM` | Unknown |
| Expiry | ~1 year (`expires_at`, SEARCH+ARITH) | not stated in dhan-ref 07b | Unknown |

## 10. SDK method signatures (reference facts; no code vendored)

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master `connect.py` lines 724–818):**

- `get_gtts()` / `get_gtt(trigger_id)`
- `place_gtt(trigger_type, tradingsymbol, exchange, trigger_values, last_price, orders)` — asserts `trigger_type ∈ {"single","two-leg"}`; validates `trigger_values` arity (1 vs 2); each order dict must carry `transaction_type, quantity, order_type, product, price` (`quantity` cast to int, `price` to float).
- `modify_gtt(trigger_id, trigger_type, tradingsymbol, exchange, trigger_values, last_price, orders)`
- `delete_gtt(trigger_id)`

Go twin: `PlaceGTT/ModifyGTT/GetGTTs/GetGTT/DeleteGTT` over `GTTParams{Tradingsymbol, Exchange, LastPrice, TransactionType, Product, Trigger}` with `GTTSingleLegTrigger`/`GTTOneCancelsOtherTrigger{Upper, Lower}` — **Verified (CLIENT-LIB-SOURCE gokiteconnect@master `gtt.go`)**. Note `DeleteGTT` reuses `URIGetGTT` with the DELETE verb (same path).

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Trigger-price distance rules:** what min/max % distance from LTP does the server enforce for `trigger_values` (and does `last_price` staleness reject the create)? — open https://kite.trade/docs/connect/v3/gtt/ and paste the validation prose. Matters: create-time failures would otherwise be discovered only in production.
- **Allowed `order_type` values in GTT legs:** does the server accept `MARKET`/`SL`/`SL-M` legs, or LIMIT-only (as gokiteconnect hard-codes)? — same page. Matters: a native client must not over-promise leg types.
- **`expires_at` semantics:** is expiry always created_at + 1 year, or client-settable/extendable (one mock is exactly +1y, the other is not)? Can an `expired` trigger be re-armed via modify? — https://kite.trade/docs/connect/v3/gtt/ + https://support.zerodha.com/category/trading-and-markets/kite-features/gtt/articles/what-is-the-validity-of-a-gtt-order. Matters: silent expiry = silently unprotected position.
- **Max active GTTs (current number):** 500 vs older 250/100 — SEARCH sources conflict. Paste https://support.zerodha.com/category/trading-and-markets/charts-and-orders/gtt/articles/what-is-the-good-till-triggered-gtt-feature. Matters: capacity planning for any SL-management design.
- **`parent_trigger` field + populated `meta` shape:** always-null in mocks; Go models `meta.rejection_reason`. What populates them? — live page + a real triggered/rejected GTT. Matters: rejection forensics.
- **Status semantics:** exact meaning/transitions of `disabled` vs `cancelled` vs `deleted` vs `rejected` (who sets each; is `disabled` broker-initiated on corporate actions?) — live page. Matters: reconciliation state machine.
- **Rate-limit bucket for `/gtt/triggers`:** which of Kite's per-second buckets applies? — https://kite.trade/docs/connect/v3/exceptions/ or the FAQ. Matters: burst SL re-placement.
- **Postback/WS behaviour on trigger:** does a GTT firing emit an order postback / order-update WS message tagged with the GTT id (the result `meta` string `{"app_id":…,"gtt":105099}` suggests tagging)? — live page (postbacks section). Matters: event-driven fill detection vs polling.

## CLAIMS (for README reconciled table)

- GTT endpoints: `GET|POST /gtt/triggers`, `GET|PUT|DELETE /gtt/triggers/{trigger_id}` on `https://api.kite.trade` with `X-Kite-Version: 3` — Verified (CLIENT-LIB-SOURCE pykiteconnect@master `connect.py` lines 36/41/158–163; cross-checked gokiteconnect `connect.go` 140–145) — https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/connect.py
- Trigger types on the wire: `"single"` and `"two-leg"` (OCO) — Verified (CLIENT-LIB-SOURCE both SDKs) — same URLs.
- Create/modify body is FORM-ENCODED with `condition` and `orders` as JSON-serialized STRINGS inside form fields (`type` plain) — Verified (CLIENT-LIB-SOURCE pykiteconnect `_post(..., is_json=False)` → `data=params`; gokiteconnect `url.Values`).
- `condition` = `{exchange, tradingsymbol, trigger_values[], last_price}`; arity SDK-enforced 1 (single) / 2 (two-leg); response-side condition adds `instrument_token` — Verified (CLIENT-LIB-SOURCE `_get_gtt_payload` + OFFICIAL-MOCK `gtt_get_orders.json`).
- Each GTT leg order = `{exchange, tradingsymbol, transaction_type, quantity, order_type, product, price}`, one order per trigger value, single instrument per trigger — Verified (CLIENT-LIB-SOURCE both SDKs; OFFICIAL-MOCK).
- Place/modify/delete all respond `{"status":"success","data":{"trigger_id":N}}` — Verified (OFFICIAL-MOCK `gtt_place_order.json` / `gtt_modify_order.json` / `gtt_delete_order.json`, kiteconnect-mocks@main, fetched 2026-07-13).
- Full GTT object carries `id, user_id, parent_trigger, type, created_at, updated_at, expires_at, status, condition, orders[].result, meta` — Verified (OFFICIAL-MOCK `gtt_get_orders.json` / `gtt_get_order.json`; struct-matched gokiteconnect `GTT`).
- `status == "triggered"` does NOT imply execution: the official mock shows a triggered leg whose placed order FAILED (`order_result.status: "failed"`, circuit-limit rejection_reason); execution truth lives in `orders[].result.order_result` — Verified (OFFICIAL-MOCK `gtt_get_order.json`).
- Seven statuses: `active, triggered, disabled, expired, cancelled, rejected, deleted` — Verified (CLIENT-LIB-SOURCE pykiteconnect `connect.py` lines 102–108).
- GTT validity ≈ 1 year — SEARCH (support.zerodha.com validity article, snippet 2026-07-13) + ARITH (mock `created_at 2019-09-12 13:25:16` → `expires_at 2020-09-12 13:25:16`, exactly +1y); second mock's expiry is NOT +1y, so client-settable expiry is an open question.
- Max active GTTs: reported 500 (older sources 250/100) — SEARCH ONLY, conflicting, unpinned — https://support.zerodha.com/category/trading-and-markets/charts-and-orders/gtt/articles/what-is-the-good-till-triggered-gtt-feature (re-read live before contracting).
- Modify has no per-leg patch — the whole `condition` + `orders` set is resent (contrast Dhan's `legName` per-leg modify) — Verified (CLIENT-LIB-SOURCE both SDKs) + repo compare `docs/dhan-ref/07b-forever-order.md`.
