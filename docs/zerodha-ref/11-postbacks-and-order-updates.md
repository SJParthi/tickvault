# Zerodha Kite Connect v3 — Postbacks (WebHooks) & Order Updates (reference)

> **Source:** `https://kite.trade/docs/connect/v3/postbacks/` (primary) · `https://kite.trade/docs/connect/v3/websocket/` (the text-frame order-update half) — official live docs, UNREACHABLE from this sandbox; see provenance below.
> **Fetched-Verified:** 2026-07-13 via CLIENT-LIB-SOURCE (`zerodha/pykiteconnect@master` `kiteconnect/ticker.py` via raw.githubusercontent.com; `zerodha/kiteconnectjs@e366e45` (2026-04-23) `lib/connect.ts` + `lib/ticker.ts` + `types/*.d.ts` via `git clone --depth 1`; `zerodha/gokiteconnect@master` `ticker/ticker.go` via raw.githubusercontent.com — all fetched 2026-07-13) + OFFICIAL-MOCK (`zerodha/kiteconnect-mocks@main` c7a8123 `postback.json`, fetched 2026-07-13) + SEARCH (result snippets of the live postbacks + websocket pages and the Kite Connect forum, 2026-07-13). **ARCHIVE-DOC and MIRROR-LIVE were BOTH BLOCKED** at the egress proxy on 2026-07-13 (web.archive.org: WebFetch client refusal + proxy CONNECT 403; r.jina.ai: CONNECT 403; kite.trade direct: CONNECT 403 — per the same-day probe log). No claim in this file is byte-verbatim from the live docs pages.
> **Evidence tiers:** see README legend — Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) / SEARCH / ARITH / Assumed / Unknown. SEARCH-tier facts are substantive but NOT byte-verbatim; re-verify from an unblocked network before contracting.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. TickVault places NO orders on Zerodha and registers NO webhook; any future order/webhook path requires its own dated operator grant per the house rule-file protocol.
> **Related:** `03-orders.md` (the order-row field family this payload mirrors), `10-websocket-streaming.md`-class sibling (binary market frames on the same socket), `99-UNKNOWNS.md` (Z-PB-1 … Z-PB-7 below), `docs/dhan-ref/07e-postback.md` + `docs/dhan-ref/10-live-order-update-websocket.md` (comparison), `docs/groww-ref/07-feed-websocket-streaming.md` (comparison).

---

## 1. The two delivery channels (one event family, two transports)

**SEARCH (live postbacks/websocket page snippets + forum thread 7067 "Postbacks to both the postback url and websocket", 2026-07-13):** Kite Connect delivers order lifecycle updates over TWO channels simultaneously:

| Channel | Transport | Coverage | Intended audience |
|---|---|---|---|
| **HTTP Postback** | `POST` with a JSON payload to the app's registered `postback_url` on every order status change (`COMPLETE`, `CANCEL`, `REJECTED`, `UPDATE`) | **ONLY orders placed via that app's `api_key`** | "platforms and public apps where a single api_key will place orders for multiple users" |
| **WebSocket order update** | JSON **text frames** on the SAME market-data WebSocket (`wss://ws.kite.trade`) that carries binary ticks | **ALL orders of the connected user** — including orders placed on Kite web/mobile/desktop, outside the API | "for individual developers, Postbacks over WebSocket is recommended" |

The coverage asymmetry (postback = api_key-scoped; WebSocket = user-scoped, all origins) is the load-bearing operational fact of this page. It is SEARCH-tier here (consistent across the docs snippet and multiple forum answers) — re-verify wording from the live page (Z-PB-1).

**Compare: Dhan** — Dhan's webhook is tied to the ACCESS TOKEN (URL entered at token generation on web.dhan.co), not to an app config, and Dhan's order-update stream is a SEPARATE WebSocket endpoint (`wss://api-order-update.dhan.co`, JSON MsgCode-42 login) rather than text frames on the market-data socket (`docs/dhan-ref/07e-postback.md`, `docs/dhan-ref/10-live-order-update-websocket.md`). **Compare: Groww** — Groww delivers order updates as a SUBSCRIPTION on its feed (`subscribe_fno_order_updates`, `feed_type == "order_updates"`), no HTTP webhook documented (`docs/groww-ref/07-feed-websocket-streaming.md`).

## 2. Postback registration (app config)

- **SEARCH (live-page snippet + forum, 2026-07-13):** the `postback_url` is configured in the Kite Connect **developer console** app settings ("if you have entered a reachable HTTPS URL as postback URL in developer's settings"). It must be a **reachable HTTPS URL**.
- **Assumed:** the URL is set/edited on the app record at `developers.kite.trade` (the same console that issues `api_key`/`api_secret` and holds the `redirect_url`). The console UI itself is not fetchable from this sandbox — see Z-PB-2 for the exact field list.
- **Unknown:** whether `localhost`/private-IP URLs are explicitly rejected (Dhan documents that ban — `docs/dhan-ref/07e-postback.md`; no Kite statement sourced), whether the URL can be changed without re-creating the app, and whether a non-2xx response triggers retries (Z-PB-2, Z-PB-3).

## 3. Postback POST payload (full observed structure)

**Verified (OFFICIAL-MOCK kiteconnect-mocks@main c7a8123 `postback.json`, fetched 2026-07-13 — https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/postback.json), verbatim:**

```json
{
    "user_id": "AB1234",
    "unfilled_quantity": 0,
    "app_id": 1234,
    "checksum": "2011845d9348bd6795151bf4258102a03431e3bb12a79c0df73fcb4b7fde4b5d",
    "placed_by": "AB1234",
    "order_id": "220303000308932",
    "exchange_order_id": "1000000001482421",
    "parent_order_id": null,
    "status": "COMPLETE",
    "status_message": null,
    "status_message_raw": null,
    "order_timestamp": "2022-03-03 09:24:25",
    "exchange_update_timestamp": "2022-03-03 09:24:25",
    "exchange_timestamp": "2022-03-03 09:24:25",
    "variety": "regular",
    "exchange": "NSE",
    "tradingsymbol": "SBIN",
    "instrument_token": 779521,
    "order_type": "MARKET",
    "transaction_type": "BUY",
    "validity": "DAY",
    "product": "CNC",
    "quantity": 1,
    "disclosed_quantity": 0,
    "price": 0,
    "trigger_price": 0,
    "average_price": 470,
    "filled_quantity": 1,
    "pending_quantity": 0,
    "cancelled_quantity": 0,
    "market_protection": 0,
    "meta": {},
    "tag": null,
    "guid": "XXXXXX"
}
```

Field notes:

- **Verified (OFFICIAL-MOCK):** the payload is the standard order-row field family (see `03-orders.md` §on the order book) **plus three postback-specific additions**: `user_id`, `app_id` (integer), and `checksum`. Versus the order-book row (`orders.json`), the postback carries `unfilled_quantity` and does NOT carry the `tags[]` array or `modified` bool.
- **Verified (OFFICIAL-MOCK):** timestamps are IST strings `"YYYY-MM-DD HH:MM:SS"` (no timezone suffix, no epoch) — the same convention as the REST order book. **Compare: Dhan** — same string convention on Dhan's postback (`createTime`/`updateTime`/`exchangeTime` — `docs/dhan-ref/07e-postback.md`), but Dhan's payload has **no checksum field at all**, i.e. Kite is the only one of the three brokers in this repo with an authenticated webhook.
- **Verified (OFFICIAL-MOCK):** `order_id` is a string; `instrument_token` is the numeric WS token; `status` here is the terminal `COMPLETE`.
- **SEARCH:** postbacks fire on status changes — `COMPLETE`, `CANCEL`, `REJECTED`, and `UPDATE` (partial-fill / modification class updates). The exhaustive trigger list and whether every intermediate state fires is NOT sourced (Z-PB-4).
- **Unknown:** the exact `Content-Type` header of the POST (JSON body is stated by the docs snippet; whether the header is `application/json` and whether the body is ever form-wrapped is unverified — a forum thread title "Orders postback returning only b\"..." suggests raw-body reads; Z-PB-3).

## 4. Checksum validation — the exact formula

**Verified (CLIENT-LIB-SOURCE kiteconnectjs@e366e45 `lib/connect.ts` `validatePostback()`, fetched 2026-07-13 via git clone — https://github.com/zerodha/kiteconnectjs `lib/connect.ts` ~line 922), verbatim core:**

```ts
const inputString = postback_data.order_id + postback_data.order_timestamp + api_secret;
checksum = sha256(inputString).toString();
return postback_data.checksum === checksum;
```

- The checksum is **`SHA-256(order_id + order_timestamp + api_secret)`** — plain string concatenation, in exactly that order, hex-digest compared as lowercase hex string. The typed declaration (`types/connect.d.ts` ~2257) pins the required input keys: `order_id`, `order_timestamp`, `checksum` ("Postback data received. Must be an json object with required keys order_id, checksum and order_timestamp").
- **ARITH:** the mock's `checksum` value is 64 hex chars = 256 bits — consistent with a SHA-256 hex digest. (The digest itself cannot be reproduced without the issuing app's `api_secret`.)
- **SEARCH (live-page snippet, 2026-07-13):** the documented purpose — "For every Postback you receive, you should compute the checksum at your end and match it with the checksum in the payload to ensure that the update is being POSTed by Kite Connect and not by an unauthorised entity, as only Kite Connect can generate a checksum that contains your api_secret."
- **Verified (CLIENT-LIB-SOURCE, cross-check):** this is a DIFFERENT concatenation from the login checksum — session generation uses `sha256(api_key + request_token + api_secret)` and token renewal `sha256(api_key + refresh_token + api_secret)` (`kiteconnectjs lib/connect.ts` ~354/405; same in pykiteconnect `connect.py`). Do not confuse the two.
- Note: pykiteconnect and gokiteconnect ship **no** postback-validation helper (grep-verified over both trees, 2026-07-13) — the JS client is the only official SDK with `validatePostback`. Implementers on other stacks re-implement the one-liner above.

## 5. Order updates over the WebSocket (text frames)

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master `kiteconnect/ticker.py`, fetched 2026-07-13 — https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/ticker.py):**

- Root: `ROOT_URI = "wss://ws.kite.trade"` (same in gokiteconnect `ticker.go` `url.URL{Scheme: "wss", Host: "ws.kite.trade"}` and kiteconnectjs `ticker.ts` default `'wss://ws.kite.trade/'`). *Doc wart:* the kiteconnectjs JSDoc param comment says `'wss://websocket.kite.trade/'` while the code default is `'wss://ws.kite.trade/'` — the CODE value is authoritative.
- Frame discipline (`_on_message`): **binary frames = market ticks** (parsed only when `len(payload) > 4`); **text frames = JSON control/updates**, routed to `_parse_text_message`. This matches the docs snippet: "market data is always binary and postbacks and other updates are always text" (SEARCH, websocket page).
- Text-frame shape for an order update — `_parse_text_message` fires the `on_order_update(ws, data["data"])` callback when:

```json
{ "type": "order", "data": { ...order fields... } }
```

- A second text-frame type exists: `{"type": "error", "data": "<message string>"}` → routed to `on_error` with synthetic code `0` ("Custom error with websocket error code 0", verbatim code comment).

**Verified (CLIENT-LIB-SOURCE gokiteconnect@master `ticker/ticker.go`, cross-check):** `messageOrder = "order"` / `messageError = "error"`; the `data` object of an `order` frame unmarshals directly into the SAME `kiteconnect.Order` struct used for the REST order book — i.e. the WS order-update payload is the standard order-row field family (the §3 payload minus the postback-only `checksum`/`app_id` additions; exact field delta on the wire is Unknown — Z-PB-5). **Verified (CLIENT-LIB-SOURCE kiteconnectjs `lib/ticker.ts` `parseTextMessage`):** same `data.type === 'order'` dispatch; the `order_update` event doc-comment reads "When order update (postback) is received for the connected user".

- **Verified (CLIENT-LIB-SOURCE ticker.py docstring):** `on_order_update(ws, data)` — "Triggered when there is an order update for the connected user."
- **No checksum on the WS channel** (Assumed from struct shape: the WS payload is authenticated by the socket's own `api_key + access_token` connect params, so no per-message checksum exists in any SDK's WS parse path; grep-verified absence in all three SDKs).
- **SEARCH (forum threads 11290/7067):** WS order updates cover all of the user's orders from any origin; delivery is not formally guaranteed/acked — a missed text frame during a reconnect window is unrecoverable except by polling `GET /orders`. Mark the guarantee level Unknown (Z-PB-6).

## 6. Delivery caveats (the honest envelope)

| Caveat | Status |
|---|---|
| Postback fires only for orders placed with the registering app's `api_key`; WS covers all user orders | SEARCH (docs snippet + forum, consistent) — Z-PB-1 to pin verbatim |
| `postback_url` must be reachable HTTPS, set in developer console | SEARCH |
| Retry policy on non-2xx / timeout | **Unknown** (Z-PB-3) |
| Ordering / at-least-once vs at-most-once semantics | **Unknown** (Z-PB-6) |
| `UPDATE` postbacks on partial fills — one POST per fill? | SEARCH mentions `UPDATE` class; per-fill granularity **Unknown** (Z-PB-4) |
| WS text frames can interleave with binary ticks on one socket — consumers MUST branch on frame opcode first | Verified (CLIENT-LIB-SOURCE, all 3 SDKs' message loops) |
| Checksum MUST be validated before trusting a postback (only Kite can produce it) | Verified formula (CLIENT-LIB-SOURCE) + SEARCH purpose text |

**Compare: Dhan/Groww (for the tickvault OMS mapping):** Kite's WS order-update JSON uses full snake_case field names identical to its REST order rows — unlike Dhan's order-update WS which uses abbreviated PascalCase codes that differ from its own REST API (`Product: "I"`, `TxnType: "B"` — `docs/dhan-ref/10-live-order-update-websocket.md` rule 6). A Kite consumer therefore needs ONE order deserializer for REST + WS + postback; a Dhan consumer needs two.

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Z-PB-1 — verbatim coverage wording.** Pin the exact live-page sentences on postback (api_key-scoped) vs WebSocket (user-scoped, all platforms) coverage. Paste: `https://kite.trade/docs/connect/v3/postbacks/` (intro paragraphs). Matters: determines whether an api_key-holder can observe manually-placed orders via webhook at all (it cannot, per SEARCH — but this is a design-load-bearing negative).
- **Z-PB-2 — registration mechanics.** Where exactly is `postback_url` set (developer console field list), can it be edited live without app re-creation, and is `localhost`/private-IP rejected? Paste: `https://developers.kite.trade` app-edit screen text + `https://kite.trade/docs/connect/v3/postbacks/`. Matters: ops runbook for webhook rotation.
- **Z-PB-3 — delivery mechanics.** POST `Content-Type`, retry count/backoff on non-2xx, timeout. Paste: `https://kite.trade/docs/connect/v3/postbacks/` (delivery paragraphs, if any). Matters: whether a webhook consumer must be idempotent + whether missed events are re-POSTed (if not, `GET /orders` polling is the only reconciliation).
- **Z-PB-4 — trigger matrix.** Exhaustive list of statuses/events that fire a postback (does every partial fill fire an `UPDATE`? do `MODIFY`/`TRIGGER PENDING` transitions fire?). Paste: same page. Matters: fill-tracking granularity.
- **Z-PB-5 — exact WS order-update payload field list.** Does the WS `data` object carry `checksum`/`app_id`/`user_id` like the HTTP postback, or the plain order-book row? Paste: `https://kite.trade/docs/connect/v3/websocket/` ("Postbacks and non-binary updates" section). Matters: one-deserializer claim in §6.
- **Z-PB-6 — delivery guarantee on the WS channel.** Any official statement on missed order updates across reconnects (forum answers say poll `/orders` to reconcile). Paste: same page + forum thread 11290. Matters: OMS reconciliation design (the Dhan-style reconcile loop).
- **Z-PB-7 — checksum edge cases.** Is `order_timestamp` in the checksum the exact payload string (IST `"YYYY-MM-DD HH:MM:SS"`), and what happens for events where `order_timestamp` is null? Paste: `https://kite.trade/docs/connect/v3/postbacks/` (checksum section). Matters: validation must byte-match; a null-timestamp event would break the concatenation.

## CLAIMS (for README reconciled table)

- Postback checksum = `SHA-256(order_id + order_timestamp + api_secret)`, plain string concat in that order, lowercase-hex compared — **Verified (CLIENT-LIB-SOURCE kiteconnectjs@e366e45 lib/connect.ts `validatePostback`)** — https://github.com/zerodha/kiteconnectjs
- Postback payload = the standard order-row family + `user_id`, `app_id` (int), `checksum` (64-hex); IST string timestamps; example status `COMPLETE` — **Verified (OFFICIAL-MOCK postback.json)** — https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/postback.json
- Postback is an HTTPS `POST` with JSON payload to the app-registered `postback_url` on order status changes (`COMPLETE`/`CANCEL`/`REJECTED`/`UPDATE`) — **SEARCH (kite.trade/docs/connect/v3/postbacks/ snippet, 2026-07-13)**.
- HTTP postbacks fire ONLY for orders placed via the registering `api_key`; WebSocket order updates cover ALL of the user's orders from any platform — **SEARCH (docs snippet + forum 7067/5230, consistent)**.
- WS order updates arrive as JSON TEXT frames on the SAME `wss://ws.kite.trade` socket as binary ticks, shape `{"type":"order","data":{...}}`; `{"type":"error","data":"<str>"}` also exists — **Verified (CLIENT-LIB-SOURCE pykiteconnect ticker.py `_parse_text_message` + gokiteconnect ticker.go `messageOrder` + kiteconnectjs ticker.ts)**.
- The WS `data` object deserializes into the SAME struct as REST order rows (Go unmarshals into `kiteconnect.Order`) — **Verified (CLIENT-LIB-SOURCE gokiteconnect ticker.go)**; exact wire field delta vs HTTP postback **Unknown** (Z-PB-5).
- Only the JS SDK ships a postback-validation helper; py/Go have none (re-implement the formula) — **Verified (CLIENT-LIB-SOURCE, grep absence over all three trees, 2026-07-13)**.
- Postback checksum ≠ login checksum: session mint uses `sha256(api_key + request_token + api_secret)` — **Verified (CLIENT-LIB-SOURCE kiteconnectjs connect.ts + pykiteconnect connect.py)**.
- Delivery retry/ordering guarantees on both channels — **Unknown** (Z-PB-3/Z-PB-6).
- Compare: Kite is the only broker of the three in this repo with a checksum-authenticated webhook (Dhan's postback has no checksum; Groww has no HTTP webhook) — **Verified against repo packs** (`docs/dhan-ref/07e-postback.md`, `docs/groww-ref/07-feed-websocket-streaming.md`).
