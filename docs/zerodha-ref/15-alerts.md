# Zerodha Kite Connect v3 — Alerts API (price alerts + ATO alert-triggered orders)

> **Source:** the Kite Connect Alerts docs section (Assumed to exist at `https://kite.trade/docs/connect/v3/alerts/` — the live docs TOC was UNREACHABLE from this sandbox; the section name is training-tier + consistent with the artifacts below, NOT fetched).
> **Fetched-Verified:** 2026-07-13 via CLIENT-LIB-SOURCE (gokiteconnect@master `028ce8b` `alerts.go` — read from the local shallow clone, 279 lines) + OFFICIAL-MOCK (zerodha/kiteconnect-mocks@main — all 7 `alerts_*.json` files fetched via raw.githubusercontent.com 2026-07-13: `alerts_create`, `alerts_create_ato`, `alerts_get`, `alerts_get_one`, `alerts_modify`, `alerts_delete`, `alerts_history`). **ARCHIVE-DOC / MIRROR-LIVE blocked** as for every file in this pack (kite.trade + web.archive.org + r.jina.ai all proxy CONNECT-403 per `zerodha-probe`). No claim here is byte-verbatim from the live docs page.
> **Evidence tiers:** see README legend — Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) / SEARCH / Assumed / Unknown.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. This file was added 2026-07-13 by the completeness review (H1): a whole official API family — with 7 official mocks — was invisible to pykiteconnect-only route enumeration.
> **Related:** `04-gtt.md` (the GTT sibling — ATO overlaps its territory), `03-orders.md` (the order-params family the ATO basket reuses), `99-UNKNOWNS.md`.

---

## 1. The headline facts

1. **The Alerts API is REAL official surface:** gokiteconnect@master ships a complete `alerts.go` (URI constants, enums, structs, 6 client methods) and the official kiteconnect-mocks@main repo carries 7 `alerts_*.json` response mocks including an ATO example — **Verified (CLIENT-LIB-SOURCE + OFFICIAL-MOCK, fetched 2026-07-13)**.
2. **Cross-SDK divergence:** pykiteconnect@master has **NO alerts route at all** (grep over `connect.py`: zero `alert` hits — **Verified-absence, CLIENT-LIB-SOURCE**). The Go SDK is currently the only official client exposing this family — the same Go-only pattern as `/user/profile/full`, holdings summary/compact, and holdings-authorise (`01`/`05`).
3. Two alert types exist: **`simple`** (notification-only price alert) and **`ato`** (Alert-Triggered Order — the alert carries a BASKET of order params placed when it fires) — **Verified (CLIENT-LIB-SOURCE `AlertType` consts + OFFICIAL-MOCK alerts_create/alerts_create_ato)**.

**Compare: Dhan** — the closest Dhan analog is the Conditional Trigger API (equities/indices only, indicator conditions — `.claude/rules/dhan/conditional-trigger.md`); **Groww** has no alerts API in `docs/groww-ref/`. Kite's ATO is GTT-adjacent (server-side condition → order placement) but with a DIFFERENT wire family (`/alerts` vs `/gtt/triggers`) and LHS/RHS condition algebra instead of bare trigger_values.

## 2. Endpoints

**Verified (CLIENT-LIB-SOURCE gokiteconnect `alerts.go` lines 13–17 URI constants + the 6 method bodies):**

| Method | Endpoint | Go method | Purpose |
|---|---|---|---|
| POST | `/alerts` | `CreateAlert(params)` | create (form-encoded fields; ATO basket as a JSON-string form field — the GTT nesting pattern) |
| GET | `/alerts` | `GetAlerts(filters)` | list all; arbitrary `key=value` query filters pass through (filter keys **Unknown** — likely `status` etc.) |
| GET | `/alerts/{uuid}` | `GetAlert(uuid)` | fetch one |
| PUT | `/alerts/{uuid}` | `ModifyAlert(uuid, params)` | modify (full params resent — no per-field patch, same as GTT) |
| DELETE | `/alerts` (repeated `uuid=` params) | `DeleteAlerts(uuids...)` | delete one or MORE alerts in one call — multi-delete via repeated `uuid` query params; response `{"status":"success","data":null}` (**Verified OFFICIAL-MOCK alerts_delete.json** — note `data: null`, unlike most `data:true` deletes) |
| GET | `/alerts/{uuid}/history` | `GetAlertHistory(uuid)` | trigger history of one alert |

Alert identity is a **UUID string** (`uuid` field, e.g. `550e8400-e29b-41d4-a716-446655440000`) — NOT a numeric id like GTT's `trigger_id` — **Verified (OFFICIAL-MOCK, every alerts mock)**.

## 3. The condition algebra (LHS operator RHS)

**Verified (CLIENT-LIB-SOURCE `AlertParams` + `Alert` struct; OFFICIAL-MOCK examples):** an alert is a comparison `LHS <operator> RHS`:

| Piece | Fields | Observed values |
|---|---|---|
| LHS | `lhs_exchange`, `lhs_tradingsymbol`, `lhs_attribute` | mock: `lhs_exchange: "INDICES"` + `lhs_tradingsymbol: "NIFTY 50"`, and `NSE`/`GOLDBEES`; `lhs_attribute: "LastTradedPrice"` in every mock. Full attribute enum **Unknown** (docs-page item) |
| Operator | `operator` | Go consts: `<=`, `>=`, `<`, `>`, `==` (**Verified**, `AlertOperator`) |
| RHS | `rhs_type` = `"constant"` (with `rhs_constant` float) OR `"instrument"` (with `rhs_exchange`/`rhs_tradingsymbol`/`rhs_attribute`) | mocks show only `constant` (27000, 71.8); instrument-vs-instrument comparison is SDK-supported, no mock example |

- Note **`lhs_exchange: "INDICES"`** — the Alerts API uses an `INDICES` pseudo-exchange for index underlyings (with the display-name tradingsymbol `NIFTY 50`), unlike the quote API's `NSE:NIFTY 50` convention — **Verified (OFFICIAL-MOCK alerts_create + alerts_history)**. The full legal exchange set for alerts is **Unknown**.
- Alert object extras: `name`, `status` (`enabled`/`disabled`/`deleted` — Go consts), `disabled_reason` (string), `alert_count` (int — times fired), `created_at`/`updated_at` (`"YYYY-MM-DD HH:MM:SS"` naive strings, Assumed IST) — **Verified (CLIENT-LIB-SOURCE + OFFICIAL-MOCK)**.

## 4. ATO (alert-triggered orders) — the `basket`

**Verified (CLIENT-LIB-SOURCE `alerts.go` `Basket`/`BasketItem`/`AlertOrderParams`; OFFICIAL-MOCK alerts_get.json second entry — a real ATO alert):**

- On create/modify with `type=ato`, the client marshals a `basket` JSON object into a form field (same JSON-string-in-form nesting as GTT's `condition`/`orders`).
- `Basket` = `{name, type, tags[], items[]}`; each `BasketItem` = `{type, tradingsymbol, exchange, weight, params, id?, instrument_token?}` (id/token appear server-enriched on reads — the mock's item carries `id: 275218517` + `instrument_token: 3693569`).
- `AlertOrderParams` is the FULL regular-order param family: `transaction_type, product, order_type, validity, validity_ttl, quantity, price, trigger_price, disclosed_quantity, variety, tags[], squareoff, stoploss, trailing_stoploss, iceberg_legs, market_protection, last_price` + an optional `gtt: {target, stoploss}` sub-object (`OrderGTTParams`) — i.e. an ATO can arm a GTT on the resulting position. **Verified (CLIENT-LIB-SOURCE)**; mock example: a disabled ATO "buy gold" alert with one GOLDBEES CNC LIMIT BUY item (`weight: 10000`, `quantity: 10000`, `price: 72.22`).
- `weight` semantics: **Unknown** (mock has weight == quantity; docs-page item).
- Whether an ATO's triggered order placement respects the same RMS caps / margin checks as a manual order: **Assumed yes**, undocumented in the artifacts.

## 5. Alert history

**Verified (OFFICIAL-MOCK alerts_history.json + CLIENT-LIB-SOURCE `AlertHistory`/`AlertHistoryMeta`):** `GET /alerts/{uuid}/history` returns an array; each entry = `{uuid, type, meta[], condition, created_at, order_meta}`:

- `condition` is a STRING rendering of the fired condition — verbatim mock: `"LastTradedPrice(\"INDICES:NIFTY NEXT 50\") <= 58290.35"` — i.e. the server exposes a function-call-style condition grammar (`Attribute("EXCHANGE:SYMBOL") op value`).
- `meta[]` = full market snapshot(s) at trigger time: `instrument_token, tradingsymbol, timestamp, last_price, ohlc{}, net_change, exchange, last_trade_time, last_quantity, buy_quantity, sell_quantity, volume, volume_tick, average_price, oi, oi_day_high, oi_day_low, lower_circuit_limit, upper_circuit_limit` (index example: volumes/OI all 0, `last_trade_time: "0001-01-01 00:00:00"` — a zero-date sentinel wart, parsers must tolerate it).
- `order_meta` is `null` for a simple alert (**Verified mock**); its populated ATO shape (presumably the placed order(s) outcome — the GTT `result` analog) is **Unknown** — no ATO history mock exists.

## 6. Limits, rate family, delivery channel — Unknown

- Max alerts per account, the alert-evaluation cadence, notification channels for `simple` alerts (push/email?), and which rate bucket `/alerts` calls fall into: **ALL Unknown** — no SDK constant, no mock, no fetched snippet. Docs-page paste items.
- ATO fire → order lifecycle visibility: **Assumed** the placed order then flows through the normal postback/WS order-update channels (`11-…`), tagged via the basket item's `tags[]`; unverified.

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Z-AL-1 — the Alerts docs page verbatim.** Paste the whole Alerts section of the live docs (URL from the TOC — Assumed `https://kite.trade/docs/connect/v3/alerts/`). Matters: this entire file is SDK+mock-derived; the attribute enum (`lhs_attribute` beyond `LastTradedPrice`), alert caps, and evaluation cadence live only there.
- **Z-AL-2 — ATO entitlement + `order_meta` shape.** Is ATO available to all Connect apps (or gated), and what does a fired ATO's history `order_meta` carry? Paste the ATO paragraphs + fire one test ATO. Matters: execution-truth semantics (the GTT `triggered`≠filled lesson, `04-gtt.md` §6).
- **Z-AL-3 — pykiteconnect absence.** Is the Alerts API py-SDK-pending, or deliberately Go-first? (Cross-SDK divergence recorded; a py release adding alerts routes would date this file.) Watch the pykiteconnect changelog. Matters: none (recording only).
- **Z-AL-4 — GetAlerts filter keys.** The Go method passes arbitrary `key=value` filters; the legal set (`status=enabled`?) is undocumented in the SDK. Paste from the docs page. Matters: list-pagination/filtering design.

## CLAIMS (for README reconciled table)

- An official Alerts API exists: `POST|GET /alerts`, `GET|PUT /alerts/{uuid}`, `DELETE /alerts` (multi-uuid), `GET /alerts/{uuid}/history`; alert identity is a UUID string — **Verified (CLIENT-LIB-SOURCE gokiteconnect alerts.go @028ce8b + OFFICIAL-MOCK, 7 alerts_*.json files, kiteconnect-mocks@main, fetched 2026-07-13)**.
- Two alert types: `simple` and `ato` (alert-triggered orders — a basket of full order params placed on fire; items may carry a nested `gtt{target,stoploss}`) — **Verified (CLIENT-LIB-SOURCE AlertType/Basket/AlertOrderParams + OFFICIAL-MOCK alerts_create_ato.json / alerts_get.json)**.
- Condition algebra: `lhs_attribute(lhs_exchange:lhs_tradingsymbol) <operator> rhs` with operators `<= >= < > ==`; rhs = constant OR another instrument's attribute; indices use the `INDICES` pseudo-exchange (`INDICES:NIFTY 50`) — **Verified (CLIENT-LIB-SOURCE + OFFICIAL-MOCK; attribute enum beyond LastTradedPrice Unknown)**.
- Alert history returns a rendered condition string + a full market snapshot per trigger (`meta[]`, incl. a `"0001-01-01 00:00:00"` zero-date sentinel wart); simple-alert `order_meta` is null — **Verified (OFFICIAL-MOCK alerts_history.json)**.
- pykiteconnect@master has NO alerts surface — Go-SDK-only family today — **Verified-absence (CLIENT-LIB-SOURCE grep, 2026-07-13)**.
- Alert caps, evaluation cadence, notification channels, rate-limit family — **Unknown** (Z-AL-1).
