# Zerodha Kite Connect v3 — GTT (Good Till Triggered) Orders

> **Source:** https://kite.trade/docs/connect/v3/gtt/ (official page — **now fetched live-verbatim**), plus the two GTT support articles: https://support.zerodha.com/category/trading-and-markets/kite-features/gtt/articles/what-is-the-validity-of-a-gtt-order (dateModified 2025-09-30) and https://support.zerodha.com/category/trading-and-markets/charts-and-orders/gtt/articles/what-is-the-good-till-triggered-gtt-feature (dateModified 2026-07-03). Cross-check evidence retained: official SDK sources (`zerodha/pykiteconnect`, `zerodha/gokiteconnect`) + official mock responses (`zerodha/kiteconnect-mocks`).
> **Fetched-Verified:** live-verbatim via GH-runner 2026-07-14 (UTC timestamps in `00-COVERAGE-MANIFEST.md`; per-URL sha256 in the crawl fetch-log). Earlier 2026-07-13 evidence retained as cross-check: CLIENT-LIB-SOURCE (raw.githubusercontent.com — pykiteconnect@master `kiteconnect/connect.py`, gokiteconnect@master `gtt.go` + `connect.go`), OFFICIAL-MOCK (kiteconnect-mocks@main `gtt_*.json`), ARITH.
> **Evidence tiers:** see README legend — **Verified (LIVE-DOC runner 2026-07-14)** is the top tier for this pack (claim confirmed against the runner-fetched live page); **Verified (CLIENT-LIB-SOURCE …)** / **Verified (OFFICIAL-MOCK …)** stay as cross-check evidence; **SEARCH** / **Assumed** / **Unknown** as before. Claims the live pages do not cover keep their old tier honestly.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. TickVault places NO orders (dry_run stands); any Kite integration would need its own dated operator grant per the scope-lock protocol.
> **Related:** `07b-forever-order.md` in `docs/dhan-ref/` (the Dhan equivalent), `docs/groww-ref/16-orders-margins-portfolio.md` rows 14–18 (Groww "Smart Orders"), `15-alerts.md` (the ATO — alert-triggered-orders — sibling surface, added 2026-07-13), `99-UNKNOWNS.md`.

---

## 1. Overview + endpoint table

GTT ("Good Till Triggered") triggers sit server-side at Zerodha: a **condition** on an instrument's price is monitored, and when a trigger value is breached, the attached **order(s)** are placed on the exchange. Two trigger types exist: `single` (one trigger value, one order) and `two-leg` (OCO — two trigger values, two orders; per the live docs, when either trigger is reached the corresponding order executes and "the other order is lain dormant"; the support article words it as "the other trigger is cancelled automatically").

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/gtt/ — the page's own endpoint table; cross-check CLIENT-LIB-SOURCE pykiteconnect@master `kiteconnect/connect.py` `_routes` lines 158–163 + gokiteconnect@master `connect.go` lines 140–145):**

| Method | Endpoint | SDK method (py) | Live-doc purpose (verbatim) |
|---|---|---|---|
| POST | `/gtt/triggers` | `place_gtt(...)` | "Place a GTT" |
| GET | `/gtt/triggers` | `get_gtts()` | "Retrieve a list of all GTTs visible in GTT order book" |
| GET | `/gtt/triggers/:id` | `get_gtt(trigger_id)` | "Retrieve an individual trigger" |
| PUT | `/gtt/triggers/:id` | `modify_gtt(trigger_id, ...)` | "Modify an active GTT" |
| DELETE | `/gtt/triggers/:id` | `delete_gtt(trigger_id)` | "Delete an active GTT" |

Base URL `https://api.kite.trade` with headers `X-Kite-Version: 3` and `Authorization: token api_key:access_token` — **Verified (LIVE-DOC runner 2026-07-14** — every curl example on the gtt page carries both**; cross-check CLIENT-LIB-SOURCE pykiteconnect `connect.py` lines 36/41/938)**.

**Doc-typo note (LIVE-DOC):** the live page's "Retrieving triggers" prose says "a GET API call to the `/triggers/gtt` endpoint" — the path in its own curl example (and everywhere else) is `/gtt/triggers`; the prose slug is a typo on Zerodha's page.

## 2. Trigger types

**Verified (LIVE-DOC runner 2026-07-14 — the page's "Type" section names exactly `single` and `two-leg`; cross-check CLIENT-LIB-SOURCE pykiteconnect `connect.py` lines 98–99, gokiteconnect `gtt.go` `GTTTypeSingle`/`GTTTypeOCO`):**

| Wire value (`type`) | SDK constant | Live-doc meaning | `trigger_values` arity |
|---|---|---|---|
| `"single"` | `GTT_TYPE_SINGLE` | "expects a single trigger value, and executes the first order that is in the `orders` array when the trigger value is reached" | exactly 1 — LIVE-DOC note: "expects only one trigger value inside the trigger_values" (SDKs raise `InputException` client-side) |
| `"two-leg"` | `GTT_TYPE_OCO` | "implements the OCO (One Cancels Other) order. It expects two trigger values and executes the corresponding order in the `orders` array when either of the trigger value is reached, the other order is lain dormant" | exactly 2 — LIVE-DOC note: "expects two trigger value inside the trigger_values" |

**Note the naming wart (Verified, LIVE-DOC + both SDKs):** the OCO wire value is the string `"two-leg"`, not `"oco"`.

**OCO leg ordering — Verified (LIVE-DOC runner 2026-07-14):** the page's own two-leg sample condition is `"trigger_values": [702.0, 798.0]` with `"last_price": 742.0` — lower (stop-loss) trigger first, upper (target) second — paired with two SELL orders priced `702.5` and `798.5` respectively. The pairing rule is now live-verbatim: **"The index of the order is used by API to determine which order is executed when a trigger value is reached."** (Cross-check: gokiteconnect `GTTOneCancelsOtherTrigger` serializes `[Lower, Upper]`; the official mock two-leg example has `trigger_values: [102.0, 103.7]` around `last_price: 102.6`.)

## 3. Create — `POST /gtt/triggers`

**Wire format (Verified, LIVE-DOC runner 2026-07-14):** the live curl example sends three `-d` form fields (`application/x-www-form-urlencoded`), NOT a JSON body — with `condition` and `orders` as **JSON-serialized strings** inside the form fields:

```
curl https://api.kite.trade/gtt/triggers \
    -H 'X-Kite-Version: 3' \
    -H 'Authorization: token api_key:access_token' \
    -d 'type=single' \
    -d 'condition={"exchange":"NSE", "tradingsymbol":"INFY", "trigger_values":[702.0], "last_price": 798.0}' \
    -d 'orders=[{"exchange":"NSE", "tradingsymbol": "INFY", "transaction_type": "BUY", "quantity": 1, "order_type": "LIMIT","product": "CNC", "price": 702.5}]'
```

(Cross-check CLIENT-LIB-SOURCE: pykiteconnect's `_post` defaults `is_json=False` → `data=params`; gokiteconnect uses `url.Values`.)

| Form field | Live-doc description |
|---|---|
| `type` | "The type of GTT" — `"single"` or `"two-leg"` |
| `condition` | "The condition parameters (json object)" — a JSON-serialized string |
| `orders` | "The orders to be placed (json array)." — a JSON-serialized string |

i.e. JSON documents are nested as string values inside form fields — an unusual shape worth flagging for any native client.

**The `condition` object (Verified, LIVE-DOC "Condition parameters" table + samples; cross-check CLIENT-LIB-SOURCE pykiteconnect `_get_gtt_payload` lines 732–764, gokiteconnect `GTTCondition`):**

```json
{
  "exchange": "NSE",
  "tradingsymbol": "INFY",
  "trigger_values": [702.0],
  "last_price": 798.0
}
```

| Condition parameter | Live-doc description (verbatim) |
|---|---|
| `exchange` | "Name of the exchange" |
| `tradingsymbol` | "Trading symbol of the instrument" |
| `trigger_values` | "Trigger values (json array)" |
| `last_price` | "Last price of the instrument at the time of placement" |

- Presumably `last_price` is used server-side to validate trigger distance from LTP — **Assumed** (the live gtt page, now fully read, carries NO trigger-distance validation prose; the rule, if any, lives in the un-crawled support article "Why was the sell Good Till Triggered (GTT) order rejected?" and/or zerodha.com/tos/gtt → OPEN QUESTIONS).
- The **response-side** condition additionally carries `instrument_token` (see §6); whether it is accepted/required on create is **Unknown** (the live create sample omits it; neither SDK sends it).

**Each element of the `orders` array (Verified, LIVE-DOC "Orders list" table + sample; cross-check CLIENT-LIB-SOURCE pykiteconnect `_get_gtt_payload` — required keys asserted: `transaction_type`, `quantity`, `order_type`, `product`, `price`):**

```json
{
  "exchange": "NSE",
  "tradingsymbol": "INFY",
  "transaction_type": "BUY",
  "quantity": 1,
  "order_type": "LIMIT",
  "product": "CNC",
  "price": 702.5
}
```

| Order parameter | Live-doc description (verbatim) |
|---|---|
| `exchange` | "Name of the exchange" |
| `tradingsymbol` | "Trading symbol of the instrument" |
| `transaction_type` | "BUY or SELL" |
| `quantity` | "Quantity to transact" |
| `order_type` | "LIMIT" — the ONLY documented value |
| `product` | "Margin product to use for the order" |
| `price` | "The min or max price to execute the order at (for LIMIT orders)" |

- **`order_type` is documented LIMIT-only — Verified (LIVE-DOC runner 2026-07-14):** the live parameter table lists `LIMIT` as the sole `order_type` value, every live sample uses `LIMIT`, and the what-is-gtt support article says "Kite places a limit order on the exchange on your behalf". This matches gokiteconnect hard-coding `order_type = LIMIT` (pykiteconnect passes the caller's value through — but the docs give no other legal value). **Residual wrinkle (SEARCH-free, from the live support article):** the Kite-web GTT UI flow says "Enter the trigger price, quantity, and select limit price **or market price**" — so a market-priced leg may exist in the UI; whether the API accepts `order_type=MARKET` is still **Unknown** → OPEN QUESTIONS (narrowed).
- `exchange`/`tradingsymbol` are copied from the condition by both SDKs — one instrument per trigger; a GTT spanning two instruments is not expressible through either SDK, and every live sample uses the same instrument in condition + orders. **Assumed** the API enforces the same.
- One order per trigger value: `single` → 1-element array; `two-leg` → 2-element array — **Verified (LIVE-DOC samples + prose)**.
- **Allowed products — Verified (LIVE-DOC support article what-is-gtt, dateModified 2026-07-03):** "GTT is available for the Longterm (CNC), NRML (Normal), and MTF (Margin Trading Facility) orders." Additional live restrictions: "You can place buy GTT OCO only in F&O (Futures and Options) contracts" and "For index futures and options GTT OCO orders, only the NRML order type is supported" (the article says "order type" but NRML is a product — quoted verbatim; read as product restriction).

**Response — Verified (LIVE-DOC runner 2026-07-14; cross-check OFFICIAL-MOCK `gtt_place_order.json`):**

```json
{"status":"success","data":{"trigger_id":123}}
```

## 4. Modify — `PUT /gtt/triggers/:id`

**Verified (LIVE-DOC runner 2026-07-14 — "To modify a GTT you need to send a PUT call with updated parameters", curl example with the same three form fields; cross-check CLIENT-LIB-SOURCE pykiteconnect `modify_gtt` lines 791–814, gokiteconnect `ModifyGTT`):** same three form fields as create (`type`, `condition`, `orders`) — the ENTIRE condition + orders set is resent; there is no per-field patch and no per-leg modify. Live-doc note (verbatim): **"It is recommended to fetch the trigger using trigger ID and modify the values and send that to the modify endpoint."** Response mirrors create — **Verified (LIVE-DOC + OFFICIAL-MOCK `gtt_modify_order.json`)**: `{"status":"success","data":{"trigger_id":123}}`.

## 5. Delete — `DELETE /gtt/triggers/:id`

**Verified (LIVE-DOC runner 2026-07-14; cross-check both SDKs + OFFICIAL-MOCK `gtt_delete_order.json`):** no body; response `{"status":"success","data":{"trigger_id":123}}`. Post-delete the trigger shows `status: "deleted"` — **Verified (LIVE-DOC status table: `deleted` "indicates that the trigger was deleted by the user")**; upgraded from Assumed.

## 6. List / fetch — the full GTT object

**Verified (LIVE-DOC runner 2026-07-14 — the live page's GET responses are byte-identical in shape to the official mocks: a `single`/`active` and a `two-leg`/`triggered` example; cross-check OFFICIAL-MOCK `gtt_get_orders.json` + `gtt_get_order.json`, struct gokiteconnect `GTT`):**

Two retrieval scopes, both live-verbatim:

- **List (`GET /gtt/triggers`):** "Active GTTs and GTTs in others states (**previous 7 days**) can be obtained" — i.e. non-active triggers age out of the LIST after ~7 days. **NEW live fact.**
- **Fetch one (`GET /gtt/triggers/:id`):** "will return details of the trigger **irrespective of the age or status** of the GTT." **NEW live fact** — by-id fetch has no 7-day window.

| Field | Type / example | Notes |
|---|---|---|
| `id` | int, `112127` | the trigger_id |
| `user_id` | `"XX0000"` | |
| `parent_trigger` | `null` in every live example + mock | semantics **Unknown** → OPEN QUESTIONS |
| `type` | `"single"` / `"two-leg"` | |
| `created_at` / `updated_at` | `"2019-09-12 13:25:16"` | space-separated timestamps, no timezone marker — **Assumed IST** (same convention as Kite order timestamps; live page adds no timezone prose) |
| `expires_at` | `"2020-09-12 13:25:16"` | see §8 |
| `status` | `"active"` / `"triggered"` | full set + live semantics §7 |
| `condition` | create-shape + **`instrument_token`** (e.g. `408065`) | server enriches with the numeric token — **Verified (LIVE-DOC response examples)** |
| `orders[]` | create-shape + **`result`** | `result` is `null` until the leg triggers — **Verified (LIVE-DOC)** |
| `meta` | `{}` / `null` in live examples | Go SDK models it as `{"rejection_reason": string}` (`gtt.go` `GTTMeta`) — populated shape **Unknown** |

**The triggered-leg `result` object (Verified, LIVE-DOC runner 2026-07-14 — the live page's own by-id example is a real failure case; identical in OFFICIAL-MOCK `gtt_get_order.json`):**

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

Load-bearing honesty: **a `triggered` GTT does NOT mean a filled (or even accepted) order** — the live page's own example is a trigger that fired but whose order was REJECTED at placement (circuit-limit breach). `orders[i].result.order_result.status/order_id/rejection_reason` is the only place that truth lives; `triggered_at` carries the price that fired the leg, and the placed order's validity materializes as `DAY`. Any consumer must poll/inspect the result object (or the postback), never infer execution from `status == "triggered"`. The validity support article states the same rule from the platform side (**LIVE-DOC**, dateModified 2025-09-30): "Once the trigger hits and your order is successfully placed on the exchange, the trigger automatically deactivates. This deactivation occurs **regardless of whether your order executes on the exchange or not**." And the what-is-gtt article: "The trigger is valid only once. If the order triggers but does not execute, you must place a fresh GTT."

## 7. Statuses

**Verified (LIVE-DOC runner 2026-07-14 — the gtt page's "Status" table, semantics verbatim; cross-check CLIENT-LIB-SOURCE pykiteconnect `connect.py` lines 102–108, the seven `GTT_STATUS_*` constants):**

| Status | Live-doc semantics (verbatim) |
|---|---|
| `active` | "indicates that the trigger is active." |
| `triggered` | "indicates that the trigger was triggered by core." |
| `disabled` | "indicates that the trigger is disabled and action is expected from the user." |
| `expired` | "indicates that the trigger has expired based on its expiry date." |
| `cancelled` | "indicates that the trigger has been cancelled by our system." |
| `rejected` | "indicates that the trigger has been rejected by our system." |
| `deleted` | "indicates that the trigger was deleted by the user." |

So the owner split is: **user** deletes (`deleted`), **Zerodha's system** cancels/rejects (`cancelled`/`rejected`), **expiry date** expires (`expired`), and `disabled` awaits user action. Known system-cancel causes — **Verified (LIVE-DOC what-is-gtt support article)**: "GTTs are cancelled for stocks with corporate actions such as bonuses, splits, rights, or amalgamations where the impact is greater than 5% of market value on the ex-date or record date" and "GTTs are cancelled if a stock changes category." The finer disabled-vs-cancelled trigger-cause taxonomy lives in the (un-crawled) support article "Why are GTTs disabled, cancelled, expired, or rejected?" → OPEN QUESTIONS (narrowed).

## 8. Expiry + active-GTT limits

- **Trigger validity = 1 year — Verified (LIVE-DOC runner 2026-07-14, validity support article, dateModified 2025-09-30, verbatim):** "Your GTT (Good Till Triggered) order remains valid for **one year from the date you place it**." The what-is-gtt article (dateModified 2026-07-03) words it "stays active for up to 365 days". Cross-check ARITH (OFFICIAL-MOCK + live example: `created_at 2019-09-12 13:25:16` → `expires_at 2020-09-12 13:25:16` = exactly +1 year to the second). **Upgraded from SEARCH.** The second live/mock example still has `expires_at 2020-01-01 12:00:00`, NOT one year from its `created_at 2019-09-09` — so `expires_at` may be settable/adjustable rather than always created_at+1y; whether the API accepts a client-supplied expiry remains **Unknown** (the live page has no expiry parameter; neither SDK sends one) → OPEN QUESTIONS.
- **Max active GTTs per account = 500 — Verified (LIVE-DOC runner 2026-07-14, what-is-gtt support article, dateModified 2026-07-03, verbatim):** "Your account can have a maximum of **500 active GTTs**." **Upgraded from SEARCH-conflicting (older third-party pages cited 250/100 — the live page settles it at 500 as of 2026-07); the number has moved over time, so re-pin on any refresh.**
- **GTT API rate limit:** **Unknown** — the live gtt page (fully read 2026-07-14) says nothing about which rate bucket `/gtt/triggers` falls into (the order 10/s bucket vs the general bucket) → OPEN QUESTIONS.

## 8b. Platform behaviour (LIVE-DOC support-article facts, what-is-gtt, dateModified 2026-07-03)

Kite-platform GTT semantics that any API consumer inherits (all **Verified (LIVE-DOC runner 2026-07-14)** unless noted):

- **Free:** "Placing a GTT is free of charge."
- **Trigger semantics:** the trigger fires "when the LTP (Last Traded Price) **reaches or crosses**" the trigger price.
- **Gap-open:** "Your GTT triggers even if the stock opens with a gap beyond your trigger price" — e.g. close ₹90, open ₹110, buy trigger ₹100/limit ₹102 → order placed at ₹102; "If it does not execute by the end of the day, it is cancelled like a normal order" (the leg materializes as a `DAY` order — matches §6's `validity: "DAY"`).
- **One-shot triggers:** "The trigger is valid only once."
- **Sell GTT on holdings needs authorization:** "You must authorise sell GTT orders on equity holdings using the CDSL TPIN. This does not apply if you have submitted a POA (Power of Attorney) or DDPI (Demat Debit and Pledge Instruction)." How this authorization interacts with API-placed sell GTTs is **Unknown** (the article is Kite-app/web-phrased).
- **Market-hours placement of triggered orders:** "Zerodha places GTT orders only during market hours. You can place, cancel, or modify GTTs at any time, but triggers activate only during market hours."
- **No dealing-desk support:** "Zerodha's dealing desk does not support GTT orders."
- **Triggered-order history visibility:** "The order history for a triggered GTT appears only on the day it triggers and is not visible from the next day" (consistent with the §6 7-day LIST window for non-active triggers — capture trigger results promptly).
- **Buy OCO scope:** "You can place buy GTT OCO only in F&O (Futures and Options) contracts."
- **Index F&O OCO:** "For index futures and options GTT OCO orders, only the NRML order type is supported" (verbatim; NRML is a product).
- **Trailing Stoploss (TSL):** a Kite-web UI feature — "a stoploss that moves your trigger automatically as the price moves in your favour", addable to both Single and OCO via a "Trailing" checkbox; "currently available on Kite web and will soon be available on the Kite app". Whether TSL is expressible via the Connect API is **Unknown** (the live gtt API page has no TSL parameter; neither SDK models it) → OPEN QUESTIONS.
- **T&Cs pointer:** both articles point to zerodha.com/tos/gtt for full terms.

## 9. Compare: Dhan / Groww

| Aspect | Kite GTT | Dhan Forever Order (`docs/dhan-ref/07b-forever-order.md`) | Groww Smart Order (`docs/groww-ref/16-orders-margins-portfolio.md` row 14) |
|---|---|---|---|
| Endpoint family | `/gtt/triggers` (POST/GET/PUT/DELETE, RESTful id in path) — LIVE-DOC | `/forever/orders` (POST/PUT/DELETE/{order-id}, GET list) | full documented surface: `POST /v1/order-advance/create`, `PUT /v1/order-advance/modify/{smart_order_id}`, `POST /v1/order-advance/cancel/{segment}/{type}/{id}`, status + list (LIVE-DOC — `docs/groww-ref/16-orders-margins-portfolio.md` §1 rows 14–18) |
| Type naming | `type`: `"single"` / `"two-leg"` | `orderFlag`: `SINGLE` / `OCO` | GTT (single trigger) / OCO |
| Payload shape | form fields with **JSON-strings** `condition` + `orders[]` (nested) — LIVE-DOC | FLAT JSON body: `price`/`triggerPrice` + `price1`/`triggerPrice1`/`quantity1` for the second leg | JSON body (per the groww-ref rows) |
| Modify granularity | whole condition+orders resent (LIVE-DOC recommends fetch-then-modify) | per-leg via `legName` (`TARGET_LEG`/`STOP_LOSS_LEG`) | documented modify endpoint (`PUT /v1/order-advance/modify/{smart_order_id}` — groww-ref 16 §1) |
| Order-type restriction | `order_type` documented LIMIT-only (LIVE-DOC) | (see dhan-ref 07b) | (see groww-ref 16) |
| Product restriction | CNC / NRML / MTF (LIVE-DOC support article); buy OCO F&O-only; index F&O OCO = NRML-only | `CNC`, `MTF` only; **static IP required** | OCO not supported for CASH segment; COMMODITY unsupported |
| "Armed" status label | `active` | `CONFIRM` | Unknown |
| Expiry | 1 year (`expires_at`) — LIVE-DOC (validity article + docs example ARITH) | not stated in dhan-ref 07b | Unknown |
| Active-trigger cap | 500 per account — LIVE-DOC (what-is-gtt article, 2026-07) | not stated in dhan-ref 07b | Unknown |

## 10. SDK method signatures (reference facts; no code vendored)

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master `connect.py` lines 724–818):**

- `get_gtts()` / `get_gtt(trigger_id)`
- `place_gtt(trigger_type, tradingsymbol, exchange, trigger_values, last_price, orders)` — asserts `trigger_type ∈ {"single","two-leg"}`; validates `trigger_values` arity (1 vs 2); each order dict must carry `transaction_type, quantity, order_type, product, price` (`quantity` cast to int, `price` to float).
- `modify_gtt(trigger_id, trigger_type, tradingsymbol, exchange, trigger_values, last_price, orders)`
- `delete_gtt(trigger_id)`

Go twin: `PlaceGTT/ModifyGTT/GetGTTs/GetGTT/DeleteGTT` over `GTTParams{Tradingsymbol, Exchange, LastPrice, TransactionType, Product, Trigger}` with `GTTSingleLegTrigger`/`GTTOneCancelsOtherTrigger{Upper, Lower}` — **Verified (CLIENT-LIB-SOURCE gokiteconnect@master `gtt.go`)**. Note `DeleteGTT` reuses `URIGetGTT` with the DELETE verb (same path).

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Trigger-price distance rules:** what min/max % distance from LTP does the server enforce for `trigger_values` (and does `last_price` staleness reject the create)? — the live gtt page (fetched 2026-07-14) carries NO validation prose; remaining sources: the un-crawled support article "Why was the sell Good Till Triggered (GTT) order rejected?" + zerodha.com/tos/gtt. Matters: create-time failures would otherwise be discovered only in production.
- **`order_type=MARKET` in GTT legs (NARROWED by live fetch):** the API docs document `order_type` as LIMIT-only (**RESOLVED for the documented surface** — LIVE-DOC 2026-07-14), but the Kite-web UI flow in the what-is-gtt article says "select limit price **or market price**" — does the API accept `MARKET` legs, or is market-price a UI-only construction? Matters: a native client must not over-promise leg types.
- **`expires_at` semantics:** is expiry always created_at + 1 year (validity article: "one year from the date you place it" — LIVE-DOC), or client-settable/extendable (the live docs' second example is NOT +1y from its created_at)? Can an `expired` trigger be re-armed via modify? — zerodha.com/tos/gtt or live probe. Matters: silent expiry = silently unprotected position.
- ~~**Max active GTTs (current number)**~~ — **RESOLVED by live fetch (2026-07-14):** 500 active GTTs per account — LIVE-DOC, what-is-gtt support article (dateModified 2026-07-03), superseding the conflicting 250/100 SEARCH numbers. (99-UNKNOWNS rebuilder: close the matching U-row.)
- **`parent_trigger` field + populated `meta` shape:** always-null in the live examples + mocks; Go models `meta.rejection_reason`. What populates them? (Plausibly TSL-related for `parent_trigger`, now that TSL exists — Assumed.) — needs a real triggered/rejected/TSL GTT. Matters: rejection forensics.
- **Status semantics (NARROWED by live fetch):** the seven statuses' one-line semantics are now LIVE-DOC (§7 table — who sets each is answered: user vs "our system" vs expiry). Residual: the detailed cause taxonomy (what exactly `disabled`s a trigger; the full corporate-action matrix) — support article "Why are GTTs disabled, cancelled, expired, or rejected?" (exists in the live related-articles list, not crawled). Matters: reconciliation state machine.
- **Rate-limit bucket for `/gtt/triggers`:** which of Kite's per-second buckets applies? — the live gtt page is silent; https://kite.trade/docs/connect/v3/exceptions/ or the FAQ. Matters: burst SL re-placement.
- **Postback/WS behaviour on trigger:** does a GTT firing emit an order postback / order-update WS message tagged with the GTT id (the result `meta` string `{"app_id":…,"gtt":105099}` — LIVE-DOC — suggests tagging)? — postbacks page. Matters: event-driven fill detection vs polling.
- **TSL (Trailing Stoploss) API exposure (NEW from live fetch):** TSL exists on Kite web (what-is-gtt article, 2026-07-03) — is it expressible via `/gtt/triggers` (no TSL parameter on the live API page; neither SDK models it), and does a TSL GTT surface differently in GET responses (e.g. via `parent_trigger`)? Matters: feature-parity claims for any native client.
- **Sell-GTT TPIN/DDPI interaction with API placement:** the CDSL TPIN authorization requirement for sell GTTs on holdings (LIVE-DOC support article) — how does it manifest for API-placed sell GTTs on a non-DDPI account (create-time rejection vs trigger-time failure)? Matters: silent trigger-time failures.

## CLAIMS (for README reconciled table)

- GTT endpoints: `POST|GET /gtt/triggers`, `GET|PUT|DELETE /gtt/triggers/:id` on `https://api.kite.trade` with `X-Kite-Version: 3` + `Authorization: token api_key:access_token` — **Verified (LIVE-DOC runner 2026-07-14)** — https://kite.trade/docs/connect/v3/gtt/ (in fetch-log); cross-check CLIENT-LIB-SOURCE pykiteconnect `connect.py` lines 36/41/158–163, gokiteconnect `connect.go` 140–145.
- Trigger types on the wire: `"single"` and `"two-leg"` (OCO) — **Verified (LIVE-DOC runner 2026-07-14)**; cross-check CLIENT-LIB-SOURCE both SDKs.
- Create/modify body is FORM-ENCODED with `condition` and `orders` as JSON-serialized STRINGS inside form fields (`type` plain) — **Verified (LIVE-DOC runner 2026-07-14** — the page's own curl `-d` examples**)**; cross-check CLIENT-LIB-SOURCE (pykiteconnect `_post(..., is_json=False)`; gokiteconnect `url.Values`).
- `condition` = `{exchange, tradingsymbol, trigger_values[], last_price}`; arity 1 (single) / 2 (two-leg) per the live Notes; response-side condition adds `instrument_token` — **Verified (LIVE-DOC runner 2026-07-14)**; cross-check CLIENT-LIB-SOURCE + OFFICIAL-MOCK.
- Each GTT leg order = `{exchange, tradingsymbol, transaction_type, quantity, order_type, product, price}`, one order per trigger value, `orders[i]` pairs with `trigger_values[i]` ("The index of the order is used by API to determine which order is executed"), single instrument per trigger in every sample — **Verified (LIVE-DOC runner 2026-07-14)**; cross-check both SDKs + OFFICIAL-MOCK. (Index-pairing upgraded from SEARCH; leg ordering [lower, upper] upgraded from Assumed via the live two-leg sample.)
- `order_type` is documented LIMIT-only in the live parameter table — **Verified (LIVE-DOC runner 2026-07-14)**; whether MARKET legs are accepted (the Kite-web UI mentions "market price") remains Unknown.
- Place/modify/delete all respond `{"status":"success","data":{"trigger_id":N}}` — **Verified (LIVE-DOC runner 2026-07-14)**; cross-check OFFICIAL-MOCK `gtt_place_order.json` / `gtt_modify_order.json` / `gtt_delete_order.json`.
- Full GTT object carries `id, user_id, parent_trigger, type, created_at, updated_at, expires_at, status, condition, orders[].result, meta` — **Verified (LIVE-DOC runner 2026-07-14** — the live GET examples, identical to the mocks**)**.
- LIST returns active GTTs + other-state GTTs from the **previous 7 days only**; by-id GET returns the trigger "irrespective of the age or status" — **Verified (LIVE-DOC runner 2026-07-14)** — NEW facts.
- `status == "triggered"` does NOT imply execution: the live page's own example shows a triggered leg whose placed order FAILED (`order_result.status: "failed"`, circuit-limit rejection_reason); execution truth lives in `orders[].result.order_result`; the validity support article confirms the trigger deactivates on placement "regardless of whether your order executes" — **Verified (LIVE-DOC runner 2026-07-14)**; cross-check OFFICIAL-MOCK.
- Seven statuses with live semantics: `active` (trigger active), `triggered` (triggered by core), `disabled` (user action expected), `expired` (per expiry date), `cancelled` (by Zerodha's system), `rejected` (by Zerodha's system), `deleted` (by the user) — **Verified (LIVE-DOC runner 2026-07-14)**; constants cross-check CLIENT-LIB-SOURCE pykiteconnect lines 102–108. (Semantics upgraded from Unknown.)
- GTT validity = 1 year from placement — **Verified (LIVE-DOC runner 2026-07-14** — validity support article verbatim + ARITH on the live example (`created_at` +1y = `expires_at`)**)**; the second live example's expiry is NOT +1y, so client-settable expiry stays an open question. (Upgraded from SEARCH.)
- Max active GTTs = **500** per account — **Verified (LIVE-DOC runner 2026-07-14** — what-is-gtt support article, dateModified 2026-07-03**)**. (Upgraded from SEARCH-conflicting 500/250/100.)
- GTT products: CNC, NRML, MTF; buy OCO only in F&O; index F&O OCO NRML-only; GTTs system-cancelled on >5% corporate actions or category change; triggers fire on reach-or-cross incl. gap-opens; triggered leg becomes a DAY order; placing GTTs is free — **Verified (LIVE-DOC runner 2026-07-14, what-is-gtt support article)**.
- Modify has no per-leg patch — the whole `condition` + `orders` set is resent; the live page recommends fetch-by-id-then-modify — **Verified (LIVE-DOC runner 2026-07-14)**; cross-check both SDKs; contrast Dhan's `legName` per-leg modify (`docs/dhan-ref/07b-forever-order.md`).
- Trailing Stoploss (TSL) exists as a Kite-web GTT feature (2026-07); API exposure Unknown — **Verified-existence (LIVE-DOC runner 2026-07-14, what-is-gtt support article)**.
