# Zerodha Kite Connect v3 — Alerts API (price alerts + ATO alert-triggered orders)

> **Source:** `https://kite.trade/docs/connect/v3/alerts/` — the live Kite Connect Alerts docs page, **fetched HTTP 200 by the LIVE-DOC runner 2026-07-14** (in fetch-log; the pre-existing "Assumed to exist" URL guess is now CONFIRMED — the page sits in the live nav between "GTT orders" and "Portfolio").
> **Fetched-Verified:** live-verbatim via GH-runner 2026-07-14 (UTC timestamps in `00-COVERAGE-MANIFEST.md`). Cross-check evidence retained: CLIENT-LIB-SOURCE (gokiteconnect@master `028ce8b` `alerts.go`, 279 lines, read 2026-07-13) + OFFICIAL-MOCK (zerodha/kiteconnect-mocks@main — all 7 `alerts_*.json` files fetched 2026-07-13). The live page's example JSON matches the mocks byte-for-byte where they overlap (the `alerts_get` GOLDBEES ATO entry and the `alerts_history` NIFTY NEXT 50 entry appear verbatim on the live page).
> **Evidence tiers:** see README legend — **Verified (LIVE-DOC runner 2026-07-14)** is the top tier for this pack; Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) stay as cross-checks; SEARCH / Assumed / Unknown as before.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. This file was added 2026-07-13 by the completeness review (H1); upgraded 2026-07-14 against the live page — its first docs corroboration.
> **Related:** `04-gtt.md` (the GTT sibling — ATO overlaps its territory), `03-orders.md` (the order-params family the ATO basket reuses), `99-UNKNOWNS.md`.

---

## 1. The headline facts

1. **The Alerts API is REAL, LIVE, documented official surface:** the live docs page ("Alerts") exists in the v3 nav with a full endpoint table, curl examples, and parameter tables — **Verified (LIVE-DOC runner 2026-07-14, `https://kite.trade/docs/connect/v3/alerts/`)**. Cross-checks: gokiteconnect ships a complete `alerts.go`; kiteconnect-mocks carries 7 `alerts_*.json` mocks — **Verified (CLIENT-LIB-SOURCE + OFFICIAL-MOCK, 2026-07-13)**.
2. **Cap: "The maximum number of active alerts allowed per user is 500."** — live-verbatim — **Verified (LIVE-DOC runner 2026-07-14)**. (Previously Unknown.)
3. **Cross-SDK divergence:** pykiteconnect@master has **NO alerts route at all** (grep over `connect.py`: zero `alert` hits — **Verified-absence, CLIENT-LIB-SOURCE 2026-07-13**). Despite the live docs page, the Go SDK remains the only official client exposing this family — same Go-only pattern as `/user/profile/full`, holdings summary/compact, holdings-authorise (`01`/`05`).
4. Two alert types exist: **`simple`** ("Standard price alert") and **`ato`** ("Alert Triggers Order - when triggered, automatically places an order" — live-verbatim; the alert carries a BASKET of order params) — **Verified (LIVE-DOC alert-types table; cross-check CLIENT-LIB-SOURCE `AlertType` consts + OFFICIAL-MOCK)**.
5. Auth/headers are the standard REST family: `X-Kite-Version: 3` + `Authorization: token api_key:access_token` against base `https://api.kite.trade` — **Verified (LIVE-DOC, every curl example)**.

**Compare: Dhan** — the closest Dhan analog is the Conditional Trigger API (equities/indices only, indicator conditions — `.claude/rules/dhan/conditional-trigger.md`); **Groww** has no alerts API in `docs/groww-ref/`. Kite's ATO is GTT-adjacent (server-side condition → order placement) but with a DIFFERENT wire family (`/alerts` vs `/gtt/triggers`) and LHS/RHS condition algebra instead of bare trigger_values.

## 2. Endpoints

**Verified (LIVE-DOC runner 2026-07-14 — the endpoint table + per-section curl examples; cross-check CLIENT-LIB-SOURCE gokiteconnect `alerts.go` URI constants + 6 method bodies, in full agreement):**

| Method | Endpoint | Live-page description | Go method |
|---|---|---|---|
| POST | `/alerts` | "Create a new alert" (form-encoded fields; ATO basket as a JSON-string form field — the GTT nesting pattern) | `CreateAlert(params)` |
| GET | `/alerts` | "Retrieve all alerts for a user" — documented query params `status` / `page` / `page_size` (§2.1) | `GetAlerts(filters)` |
| GET | `/alerts/{uuid}` | "Retrieve a specific alert" | `GetAlert(uuid)` |
| PUT | `/alerts/{uuid}` | "Modify an existing alert" — full params resent, no per-field patch; live note: *"It is recommended to fetch the alert using alert UUID and modify the values and send that to the modify endpoint."* | `ModifyAlert(uuid, params)` |
| DELETE | `/alerts?uuid={uuid}` | "Delete a specific alert"; **multi-delete** = repeated `uuid` query params (`?uuid=A&uuid=B` — live has a dedicated "Delete multiple alerts" example). Response `{"status":"success","data":null}` for BOTH single and multi (note `data: null`, unlike most `data:true` deletes) | `DeleteAlerts(uuids...)` |
| GET | `/alerts/{uuid}/history` | "Retrieve the history of a specific alert" — "history of when an alert was triggered, including market data at the time of trigger" | `GetAlertHistory(uuid)` |

Alert identity is a **UUID string** (`uuid` field, e.g. `550e8400-e29b-41d4-a716-446655440000`) — NOT a numeric id like GTT's `trigger_id` — **Verified (LIVE-DOC examples + OFFICIAL-MOCK)**.

### 2.1 GET /alerts query parameters + status lifecycle (NEW — live-only facts)

**Verified (LIVE-DOC runner 2026-07-14 "Query parameters" + "Status" tables):**

| Query param | Live description |
|---|---|
| `status` | "Filter alerts by status (enabled, disabled, deleted)" |
| `page` | "Page number for pagination" |
| `page_size` | "Number of alerts per page" |

(The pre-live guess "filter keys Unknown — likely `status` etc." is now RESOLVED — and the list is paginated, previously unrecorded. Defaults/max for `page_size`: **Unknown**.)

| `status` value | Live definition |
|---|---|
| `enabled` | "the alert is active and being evaluated" |
| `disabled` | "the alert is disabled and not being evaluated" |
| `deleted` | "the alert has been **soft-deleted** by the user" |

Soft-delete semantics (a DELETEd alert persists with `status=deleted` and remains listable via `?status=deleted`) explain the `data: null` delete response — **Verified (LIVE-DOC)**. The live list-endpoint prose confirms: "Active alerts and alerts in other states can be obtained by a GET API call to the /alerts endpoint."

## 3. The condition algebra (LHS operator RHS)

**Verified (LIVE-DOC "Alert parameters" + "Operator types" tables; cross-check CLIENT-LIB-SOURCE `AlertParams`/`Alert` structs + OFFICIAL-MOCK):** an alert is a comparison `LHS <operator> RHS`:

| Piece | Fields | Live-documented values |
|---|---|---|
| LHS | `lhs_exchange`, `lhs_tradingsymbol`, `lhs_attribute` | **`lhs_exchange` legal set now documented: `NSE, BSE, NFO, CDS, BCD, MCX, INDICES`** (live-verbatim — previously Unknown). `lhs_attribute` = "Attribute to monitor (e.g., LastTradedPrice)" — the live page too gives only the ONE example; the full attribute enum remains **Unknown** (Z-AL-5) |
| Operator | `operator` | `<=`, `>=`, `<`, `>`, `==` — live operator table with descriptions ("Less than or equal to" … "Equal to") — **Verified (LIVE-DOC; matches Go `AlertOperator` consts exactly)** |
| RHS | `rhs_type` = `"constant"` (with `rhs_constant` float) OR `"instrument"` (with `rhs_exchange`/`rhs_tradingsymbol`/`rhs_attribute` — "when rhs_type is instrument") | Both branches now DOCUMENTED on the live parameter table (previously the instrument branch was SDK-only); all live examples still use `constant` (27000, 27500, 71.8) — no instrument-vs-instrument example anywhere |

- **`lhs_exchange: "INDICES"`** — the Alerts API uses an `INDICES` pseudo-exchange for index underlyings (with the display-name tradingsymbol `NIFTY 50`), unlike the quote API's `NSE:NIFTY 50` convention — **Verified (LIVE-DOC create examples + history `exchange: "INDICES"`; cross-check OFFICIAL-MOCK)**. `INDICES` is a first-class member of the documented exchange list above.
- Alert object fields (live create/get responses): `type, user_id, uuid, name, status, disabled_reason, lhs_attribute, lhs_exchange, lhs_tradingsymbol, operator, rhs_type, rhs_attribute, rhs_exchange, rhs_tradingsymbol, rhs_constant, alert_count` (int — times fired; the GOLDBEES ATO shows `alert_count: 1` after firing), `created_at`/`updated_at` (`"YYYY-MM-DD HH:MM:SS"` naive strings, Assumed IST) — **Verified (LIVE-DOC; cross-check CLIENT-LIB-SOURCE + OFFICIAL-MOCK)**. Unused RHS-branch fields are returned as empty strings, not omitted.

## 4. ATO (alert-triggered orders) — the `basket`

**Verified (LIVE-DOC "ATO (Alert Triggers Order) alert" curl + "Basket structure for ATO alerts" section; cross-check CLIENT-LIB-SOURCE `Basket`/`BasketItem`/`AlertOrderParams` + OFFICIAL-MOCK alerts_get.json):**

- On create/modify with `type=ato`, the `basket` form field is "a JSON string containing basket configuration (required for ATO alerts)" — live-verbatim (same JSON-string-in-form nesting as GTT's `condition`/`orders`).
- `Basket` = `{name, type, tags[], items[]}` — the live create example uses `"name":"alerts-basket"`, `"type":"alert"`. Each request `BasketItem` = `{type, tradingsymbol, exchange, weight, params}` with **`"type":"insert"`** in the live create example (the item-type enum beyond `insert` is **Unknown**); on READS the item comes back server-enriched with `id` + `instrument_token` (live GET example: `id: 275218517`, `instrument_token: 3693569`) — **Verified (LIVE-DOC both directions)**.
- `params` (`AlertOrderParams`) is the FULL regular-order param family: `transaction_type, product, order_type, validity, validity_ttl, quantity, price, trigger_price, disclosed_quantity, variety, tags[], squareoff, stoploss, trailing_stoploss, iceberg_legs, market_protection, last_price` + an optional `gtt: {target, stoploss}` sub-object — i.e. an ATO can arm a GTT on the resulting position (the live GET example's GOLDBEES item carries `"gtt":{"target":0,"stoploss":0}`) — **Verified (LIVE-DOC; matches CLIENT-LIB-SOURCE exactly)**. Live create example notably sends `"market_protection": -1`.
- `weight` semantics: **Unknown** — but the live create example REFINES the picture: it sends `weight: 10000` with `quantity: 1` (the mock/live GET example had weight == quantity == 10000), so weight is NOT simply a quantity mirror.
- Whether an ATO's triggered order placement respects the same RMS caps / margin checks as a manual order: **Assumed yes**, undocumented on the live page too.

## 5. Alert history

**Verified (LIVE-DOC "Retrieve alert history" example — byte-identical to OFFICIAL-MOCK alerts_history.json; cross-check CLIENT-LIB-SOURCE `AlertHistory`/`AlertHistoryMeta`):** `GET /alerts/{uuid}/history` returns an array; each entry = `{uuid, type, meta[], condition, created_at, order_meta}`:

- `condition` is a STRING rendering of the fired condition — live-verbatim: `"LastTradedPrice(\"INDICES:NIFTY NEXT 50\") <= 58290.35"` — i.e. the server exposes a function-call-style condition grammar (`Attribute("EXCHANGE:SYMBOL") op value`).
- `meta[]` = full market snapshot(s) at trigger time ("including market data at the time of trigger" — live prose): `instrument_token, tradingsymbol, timestamp, last_price, ohlc{}, net_change, exchange, last_trade_time, last_quantity, buy_quantity, sell_quantity, volume, volume_tick, average_price, oi, oi_day_high, oi_day_low, lower_circuit_limit, upper_circuit_limit` (index example: volumes/OI all 0, `last_trade_time: "0001-01-01 00:00:00"` — the zero-date sentinel wart is on the LIVE page too; parsers must tolerate it).
- `order_meta` is `null` for a simple alert (**Verified LIVE-DOC**); its populated ATO shape (presumably the placed order(s) outcome — the GTT `result` analog) remains **Unknown** — the live page has no ATO history example either (Z-AL-2).

## 6. Limits, rate family, delivery channel

- **Max active alerts per user: 500** — **Verified (LIVE-DOC runner 2026-07-14, intro paragraph)**. Previously Unknown; RESOLVED.
- List pagination exists (`page`/`page_size`, §2.1) — **Verified (LIVE-DOC)**; defaults/caps Unknown.
- Alert-evaluation cadence, notification channels for `simple` alerts (push/email?), and which rate bucket `/alerts` calls fall into: **still Unknown** — the live page is silent on all three.
- ATO fire → order lifecycle visibility: **Assumed** the placed order then flows through the normal postback/WS order-update channels (`11-…`), tagged via the basket item's `tags[]`; unverified (live page silent).

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Z-AL-1 — RESOLVED by live fetch (2026-07-14).** The Alerts docs page (`https://kite.trade/docs/connect/v3/alerts/`) was fetched live-verbatim by the LIVE-DOC runner: URL confirmed, 500-alert cap, `lhs_exchange` legal set (NSE/BSE/NFO/CDS/BCD/MCX/INDICES), GET query params (`status`/`page`/`page_size`), status lifecycle incl. soft-delete — all now cited inline above. Residual (attribute enum) split out as Z-AL-5.
- **Z-AL-2 — ATO entitlement + `order_meta` shape.** STILL OPEN — the live page has no ATO history example and no entitlement/gating language. Is ATO available to all Connect apps, and what does a fired ATO's history `order_meta` carry? Needs firing one test ATO. Matters: execution-truth semantics (the GTT `triggered`≠filled lesson, `04-gtt.md` §6).
- **Z-AL-3 — pykiteconnect absence.** STILL OPEN — the live docs page exists yet pykiteconnect@master still has no alerts route. Is it py-SDK-pending or deliberately Go-first? Watch the pykiteconnect changelog. Matters: none (recording only).
- **Z-AL-4 — RESOLVED by live fetch (2026-07-14).** GetAlerts filter keys are now documented on the live page: `status` (enabled/disabled/deleted), `page`, `page_size` (§2.1). Residual: `page_size` default/max undocumented (folded into §6, minor).
- **Z-AL-5 — the `lhs_attribute` enum (NEW, split from Z-AL-1's residual).** The live parameter table gives only "e.g., LastTradedPrice" — no other attribute is named anywhere (docs, SDK, or mocks). Does the alerts engine support other attributes (OHLC, volume, %change — the Kite-web alerts UI suggests more)? Matters: condition-design space; only `LastTradedPrice` is safe to rely on today.

## CLAIMS (for README reconciled table)

- The Alerts docs page is LIVE at `https://kite.trade/docs/connect/v3/alerts/`; max **500 active alerts per user** — **Verified (LIVE-DOC runner 2026-07-14)**.
- Endpoints: `POST|GET /alerts`, `GET|PUT /alerts/{uuid}`, `DELETE /alerts?uuid={uuid}` (multi-delete via repeated `uuid` params; response `data: null`), `GET /alerts/{uuid}/history`; alert identity is a UUID string — **Verified (LIVE-DOC runner 2026-07-14; cross-check CLIENT-LIB-SOURCE gokiteconnect alerts.go @028ce8b + OFFICIAL-MOCK, 7 alerts_*.json, 2026-07-13)**.
- GET /alerts is filterable + paginated: `status` / `page` / `page_size`; statuses `enabled` / `disabled` / `deleted` (deleted = SOFT-deleted) — **Verified (LIVE-DOC runner 2026-07-14)**.
- Two alert types: `simple` and `ato` (alert-triggered orders — a `basket` JSON-string form field of full order params placed on fire; request items `type:"insert"`, read-back items server-enriched with `id`+`instrument_token`; items may carry a nested `gtt{target,stoploss}`) — **Verified (LIVE-DOC; cross-check CLIENT-LIB-SOURCE + OFFICIAL-MOCK)**.
- Condition algebra: `lhs_attribute(lhs_exchange:lhs_tradingsymbol) <operator> rhs` with operators `<= >= < > ==`; rhs = constant OR another instrument's attribute; `lhs_exchange` ∈ {NSE, BSE, NFO, CDS, BCD, MCX, INDICES} (indices via the `INDICES` pseudo-exchange, e.g. `INDICES:NIFTY 50`) — **Verified (LIVE-DOC runner 2026-07-14; attribute enum beyond LastTradedPrice still Unknown, Z-AL-5)**.
- Alert history returns a rendered condition string + a full market snapshot per trigger (`meta[]`, incl. the `"0001-01-01 00:00:00"` zero-date sentinel wart — present on the live page verbatim); simple-alert `order_meta` is null; ATO `order_meta` shape still Unknown — **Verified (LIVE-DOC runner 2026-07-14 + OFFICIAL-MOCK)**.
- pykiteconnect@master has NO alerts surface — Go-SDK-only family today despite the live docs page — **Verified-absence (CLIENT-LIB-SOURCE grep, 2026-07-13)**.
- Evaluation cadence, notification channels, rate-limit family, `weight` semantics (live create example: `weight:10000` with `quantity:1` — decoupled), ATO entitlement — **Unknown** (Z-AL-2/Z-AL-5).
