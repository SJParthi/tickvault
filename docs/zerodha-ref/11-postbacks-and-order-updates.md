# Zerodha Kite Connect v3 — Postbacks (WebHooks) & Order Updates (reference)

> **Source:** `https://kite.trade/docs/connect/v3/postbacks/` (primary) · `https://kite.trade/docs/connect/v3/websocket/` (the text-frame order-update half) — official live docs, now fetched LIVE via the GH-runner crawl.
> **Fetched-Verified:** live-verbatim via GH-runner 2026-07-14 (UTC timestamps in 00-COVERAGE-MANIFEST.md) — both pages are in the crawl fetch-log (postbacks/ 200, 26,130 B, 08:05:16Z; websocket/ 200, 32,567 B, 08:05:14Z; per-URL sha256 in the manifest). Cross-check evidence retained from 2026-07-13: CLIENT-LIB-SOURCE (`zerodha/pykiteconnect@master` `kiteconnect/ticker.py`; `zerodha/kiteconnectjs@e366e45` (2026-04-23) `lib/connect.ts` + `lib/ticker.ts` + `types/*.d.ts`; `zerodha/gokiteconnect@master` `ticker/ticker.go`) + OFFICIAL-MOCK (`zerodha/kiteconnect-mocks@main` c7a8123 `postback.json`) + SEARCH (2026-07-13 snippets). The 2026-07-13 ARCHIVE/MIRROR egress blocks are superseded by the live crawl — every claim tagged **Verified (LIVE-DOC runner 2026-07-14)** below IS byte-verbatim from the live page.
> **Evidence tiers:** see README legend — **Verified (LIVE-DOC runner 2026-07-14)** is the top tier for this pack; Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) stay as cross-checks; SEARCH / ARITH / Assumed / Unknown as before. Remaining SEARCH-tier facts are the ones the live pages do NOT cover.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. TickVault places NO orders on Zerodha and registers NO webhook; any future order/webhook path requires its own dated operator grant per the house rule-file protocol.
> **Related:** `03-orders.md` (the order-row field family this payload mirrors), `10-websocket-streaming.md`-class sibling (binary market frames on the same socket), `99-UNKNOWNS.md` (Z-PB rows below — several now RESOLVED by live fetch), `docs/dhan-ref/07e-postback.md` + `docs/dhan-ref/10-live-order-update-websocket.md` (comparison), `docs/groww-ref/07-feed-websocket-streaming.md` (comparison).

---

## 1. The two delivery channels (one event family, two transports)

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/postbacks/), verbatim intro:** "The Postback API sends a POST request with a JSON payload to the registered postback_url of your app when an order's status changes. This enables you to get arbitrary updates to your orders reliably, irrespective of when they happen (COMPLETE, CANCEL, REJECTED, UPDATE). An UPDATE postback is triggered when an open order is modified or when there's a partial fill. This can be used to track trades."

**Verified (LIVE-DOC runner 2026-07-14, same page), verbatim Note block:** "This Postback API is meant for platforms and public apps where a single api_key will place orders for multiple users. **Only orders placed using the app's api_key are notified.** For individual developers, Postbacks over WebSocket is recommended, where, orders placed for a particular user anywhere, for instance, web, mobile, or desktop platforms, are sent."

| Channel | Transport | Coverage | Intended audience |
|---|---|---|---|
| **HTTP Postback** | `POST` with a JSON payload to the app's registered `postback_url` on every order status change (`COMPLETE`, `CANCEL`, `REJECTED`, `UPDATE`) | **ONLY orders placed via that app's `api_key`** | "platforms and public apps where a single api_key will place orders for multiple users" |
| **WebSocket order update** | JSON **text frames** on the SAME market-data WebSocket (`wss://ws.kite.trade`) that carries binary ticks | **ALL orders of the connected user** — "orders placed for a particular user anywhere, for instance, web, mobile, or desktop platforms, are sent" | "For individual developers, Postbacks over WebSocket is recommended" |

The coverage asymmetry (postback = api_key-scoped; WebSocket = user-scoped, all origins) is the load-bearing operational fact of this page — now **Verified (LIVE-DOC runner 2026-07-14)** verbatim (was SEARCH; Z-PB-1 RESOLVED). The websocket page intro cross-confirms: "In addition, the text messages, alerts, and order updates (the same as the ones available as Postbacks) are also streamed." (LIVE-DOC, https://kite.trade/docs/connect/v3/websocket/).

**Wording wart (LIVE-DOC, same page — intra-page inconsistency):** the intro's trigger list says `CANCEL`; the payload-attributes table's `status` row says the possible values are "COMPLETE, REJECTED, **CANCELLED**, and UPDATE". Consumers should match the order-row status vocabulary (`CANCELLED`) on the payload and treat the intro's `CANCEL` as the event-class label.

**Compare: Dhan** — Dhan's webhook is tied to the ACCESS TOKEN (URL entered at token generation on web.dhan.co), not to an app config, and Dhan's order-update stream is a SEPARATE WebSocket endpoint (`wss://api-order-update.dhan.co`, JSON MsgCode-42 login) rather than text frames on the market-data socket (`docs/dhan-ref/07e-postback.md`, `docs/dhan-ref/10-live-order-update-websocket.md`). **Compare: Groww** — Groww delivers order updates as a SUBSCRIPTION on its feed (`subscribe_fno_order_updates`, `feed_type == "order_updates"`), no HTTP webhook documented (`docs/groww-ref/07-feed-websocket-streaming.md`).

## 2. Postback registration (app config)

- **Verified (LIVE-DOC runner 2026-07-14):** the postback is sent "to the registered postback_url of your app" — i.e. registration is at APP level. That is all the live postbacks page says about registration.
- **CORRECTION (2026-07-14):** the previous revision attributed "reachable HTTPS URL as postback URL in developer's settings" to a live-page snippet. The live 2026-07-14 page contains NO such sentence (grep-verified over the fetched HTML) — that wording came from forum answers / an older page revision and is DOWNGRADED to SEARCH (2026-07-13, forum). The HTTPS-only requirement is therefore NOT live-doc-backed; treat as SEARCH until the developer console itself is inspected.
- **Assumed:** the URL is set/edited on the app record at `developers.kite.trade` (the same console that issues `api_key`/`api_secret` and holds the `redirect_url`). The console is LOGIN-GATED — the 2026-07-14 crawl fetched only its login shell (`developers.kite.trade` 200, 3,915 B) — so the exact field list stays open (Z-PB-2).
- **Unknown:** whether `localhost`/private-IP URLs are explicitly rejected (Dhan documents that ban — `docs/dhan-ref/07e-postback.md`; no Kite statement sourced), whether the URL can be changed without re-creating the app, and whether a non-2xx response triggers retries (Z-PB-2, Z-PB-3).

## 3. Postback POST payload (full observed structure)

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/postbacks/ "Sample payload" — byte-identical to the OFFICIAL-MOCK `postback.json`, cross-checked 2026-07-13), verbatim:**

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

**Verified (LIVE-DOC runner 2026-07-14), delivery mechanics verbatim:** "The JSON payload is posted as a raw HTTP POST body. You will have to read the raw body and then decode it." — the raw-body read is now doc-confirmed (was a forum inference; Z-PB-3 partially resolved). The `Content-Type` header is still NOT stated on the page (residual Unknown).

Field notes (the live page carries a full "Payload attributes" table — types + descriptions now LIVE-DOC):

- **Verified (LIVE-DOC + OFFICIAL-MOCK):** the payload is the standard order-row field family (see `03-orders.md` §on the order book) **plus three postback-specific additions**: `user_id` ("ID of the user for whom the order was placed."), `app_id` (`int64`, "Your kiteconnect app ID"), and `checksum`. Versus the order-book row (`orders.json`), the postback carries `unfilled_quantity` ("Quantity that has not filled") and does NOT carry the `tags[]` array or `modified` bool.
- **Verified (LIVE-DOC):** declared attribute types — `order_id` string; `instrument_token` `uint32`; `app_id` `int64`; quantities `int64`; prices `float64`; `status`/`status_message`/`status_message_raw`/`exchange_order_id`/`parent_order_id`/`tag` are `null, string`; `meta` is `{}, string`.
- **Verified (LIVE-DOC):** `status` possible values on the payload: "COMPLETE, REJECTED, CANCELLED, and UPDATE" (note `UPDATE` appears AS a status value here; see the §1 CANCEL/CANCELLED wording wart).
- **Verified (LIVE-DOC):** `placed_by` — "ID of the user that placed the order. This may different [sic] from the user's id for orders placed outside of Kite, for instance, by dealers at the brokerage using dealer terminals."
- **Verified (LIVE-DOC + OFFICIAL-MOCK):** timestamps are IST strings `"YYYY-MM-DD HH:MM:SS"` (no timezone suffix, no epoch) — the same convention as the REST order book; `exchange_timestamp` — "Orders that don't reach the exchange have null timestamps". **Compare: Dhan** — same string convention on Dhan's postback (`createTime`/`updateTime`/`exchangeTime` — `docs/dhan-ref/07e-postback.md`), but Dhan's payload has **no checksum field at all**, i.e. Kite is the only one of the three brokers in this repo with an authenticated webhook.
- **Verified (LIVE-DOC), the fire-when-logged-out note verbatim:** "Postback API works even when the user is not logged in. Just make sure you validate the checksum value to ensure that the update is indeed coming from Kite Connect."
- **Verified (LIVE-DOC):** UPDATE trigger semantics — "An UPDATE postback is triggered when an open order is modified or when there's a partial fill." (Z-PB-4 resolved at the class level; whether EVERY partial fill produces exactly one POST is still not stated — residual in Z-PB-4.)

## 4. Checksum validation — the exact formula

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/postbacks/ "Checksum" section), verbatim:**

> "The JSON payload comes with a checksum, which is the SHA-256 hash of (order_id + order_timestamp + api_secret). For every Postback you receive, you should compute this checksum at your end and match it with the checksum in the payload. This is to ensure that the update is being POSTed by Kite Connect and not by an unauthorised entity, as only Kite Connect can generate a checksum that contains your api_secret."

The payload-attributes table's `checksum` row abbreviates the same construction: "SHA-256 hash of (order_id + timestamp + api_secret)" — the Checksum section's `order_timestamp` is the operative input name (Z-PB-7 partially resolved: the timestamp operand is `order_timestamp`; the null-timestamp edge case remains Unknown).

**Verified (CLIENT-LIB-SOURCE kiteconnectjs@e366e45 `lib/connect.ts` `validatePostback()`, fetched 2026-07-13 via git clone — https://github.com/zerodha/kiteconnectjs `lib/connect.ts` ~line 922 — cross-check, matches the live formula exactly), verbatim core:**

```ts
const inputString = postback_data.order_id + postback_data.order_timestamp + api_secret;
checksum = sha256(inputString).toString();
return postback_data.checksum === checksum;
```

- The checksum is **`SHA-256(order_id + order_timestamp + api_secret)`** — plain string concatenation, in exactly that order, hex-digest compared as lowercase hex string. Now doubly verified: LIVE-DOC prose + CLIENT-LIB-SOURCE implementation. The typed declaration (`types/connect.d.ts` ~2257) pins the required input keys: `order_id`, `order_timestamp`, `checksum` ("Postback data received. Must be an json object with required keys order_id, checksum and order_timestamp").
- **ARITH:** the mock/live sample `checksum` value is 64 hex chars = 256 bits — consistent with a SHA-256 hex digest. (The digest itself cannot be reproduced without the issuing app's `api_secret`.)
- **Verified (CLIENT-LIB-SOURCE, cross-check):** this is a DIFFERENT concatenation from the login checksum — session generation uses `sha256(api_key + request_token + api_secret)` and token renewal `sha256(api_key + refresh_token + api_secret)` (`kiteconnectjs lib/connect.ts` ~354/405; same in pykiteconnect `connect.py`). Do not confuse the two.
- Note: pykiteconnect and gokiteconnect ship **no** postback-validation helper (grep-verified over both trees, 2026-07-13) — the JS client is the only official SDK with `validatePostback`. Implementers on other stacks re-implement the one-liner above.

## 5. Order updates over the WebSocket (text frames)

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/websocket/ "Postbacks and non-binary updates"), verbatim:** "Apart from binary market data, the WebSocket stream delivers postbacks and other updates in the text mode. These messages are JSON encoded and should be parsed on receipt. For order Postbacks, the payload is contained in the `data` key and **has the same structure described in the Postbacks section.**"

Message structure (LIVE-DOC, verbatim):

```json
{
  "type": "order",
  "data": {}
}
```

**Verified (LIVE-DOC) — the "Message types" table (THREE types, not two):**

| `type` | LIVE-DOC description (verbatim) |
|---|---|
| `order` | "Order Postback. The data field will contain the full order Postback payload" |
| `error` | "Error responses. The data field contain the error string" |
| `message` | "Messages and alerts from the broker. The data field will contain the message string" |

- **CORRECTION (2026-07-14) — WS payload structure:** the previous revision ASSUMED "no checksum on the WS channel" from SDK struct shape and marked the field delta Unknown (Z-PB-5). The live page contradicts the assumption: the WS `data` field carries "the full order Postback payload" with "the same structure described in the Postbacks section" — i.e. the §3 payload INCLUDING the postback-specific `user_id`/`app_id`/`checksum` additions. The SDK `Order` structs simply don't map those extra keys (they deserialize the order-row subset and ignore the rest). LIVE-DOC wins; Z-PB-5 RESOLVED. (Honest residual: the socket is already authenticated by its `api_key + access_token` connect params, so no SDK validates the WS-side checksum.)
- **CORRECTION/ADDITION (2026-07-14):** the `message` text-frame type (broker messages/alerts) exists on the live page — the previous revision listed only `order` + `error` (the two types the SDK parse paths were grep-verified to dispatch).
- **Verified (LIVE-DOC, websocket page Modes note, verbatim):** "Always check the type of an incoming WebSocket messages [sic]. Market data is always binary and Postbacks and other updates are always text." (upgraded from SEARCH.)

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master `kiteconnect/ticker.py`, fetched 2026-07-13 — cross-checks retained):**

- Root: `ROOT_URI = "wss://ws.kite.trade"` (same in gokiteconnect `ticker.go` `url.URL{Scheme: "wss", Host: "ws.kite.trade"}` and kiteconnectjs `ticker.ts` default `'wss://ws.kite.trade/'`). *Doc wart:* the kiteconnectjs JSDoc param comment says `'wss://websocket.kite.trade/'` while the code default is `'wss://ws.kite.trade/'` — the CODE value is authoritative.
- Frame discipline (`_on_message`): **binary frames = market ticks** (parsed only when `len(payload) > 4`); **text frames = JSON control/updates**, routed to `_parse_text_message` — matches the LIVE-DOC frame-discipline note above.
- `_parse_text_message` fires `on_order_update(ws, data["data"])` on `type == "order"`; `{"type": "error", "data": "<message string>"}` routes to `on_error` with synthetic code `0` ("Custom error with websocket error code 0", verbatim code comment).
- **Verified (CLIENT-LIB-SOURCE gokiteconnect@master `ticker/ticker.go`):** `messageOrder = "order"` / `messageError = "error"`; the `data` object of an `order` frame unmarshals directly into the SAME `kiteconnect.Order` struct used for the REST order book. **Verified (CLIENT-LIB-SOURCE kiteconnectjs `lib/ticker.ts` `parseTextMessage`):** same `data.type === 'order'` dispatch; the `order_update` event doc-comment reads "When order update (postback) is received for the connected user".
- **Verified (CLIENT-LIB-SOURCE ticker.py docstring):** `on_order_update(ws, data)` — "Triggered when there is an order update for the connected user."
- **SEARCH (forum threads 11290/7067, 2026-07-13):** delivery is not formally guaranteed/acked — a missed text frame during a reconnect window is unrecoverable except by polling `GET /orders`. The live pages carry NO delivery-guarantee statement either way; guarantee level stays Unknown (Z-PB-6).

## 6. Delivery caveats (the honest envelope)

| Caveat | Status |
|---|---|
| Postback fires only for orders placed with the registering app's `api_key`; WS covers all user orders from any platform | **Verified (LIVE-DOC runner 2026-07-14)** — verbatim in §1 (Z-PB-1 RESOLVED) |
| Payload is a raw HTTP POST body — "read the raw body and then decode it" | **Verified (LIVE-DOC)**; `Content-Type` header still unstated (Z-PB-3 residual) |
| Postbacks fire even when the user is not logged in — validate the checksum | **Verified (LIVE-DOC)** |
| `postback_url` must be reachable HTTPS, set in developer console | SEARCH (forum/legacy wording — NOT on the live 2026-07-14 page; see §2 CORRECTION) |
| Retry policy on non-2xx / timeout | **Unknown** (Z-PB-3 — live page says only "reliably, irrespective of when they happen") |
| Ordering / at-least-once vs at-most-once semantics | **Unknown** (Z-PB-6) |
| `UPDATE` postbacks — "triggered when an open order is modified or when there's a partial fill" | **Verified (LIVE-DOC)**; exactly-one-POST-per-fill granularity still unstated (Z-PB-4 residual) |
| WS text frames can interleave with binary ticks on one socket — consumers MUST branch on frame opcode first | **Verified (LIVE-DOC frame-discipline note + CLIENT-LIB-SOURCE, all 3 SDKs' message loops)** |
| Checksum MUST be validated before trusting a postback (only Kite can produce it) | **Verified (LIVE-DOC verbatim formula + purpose text; CLIENT-LIB-SOURCE cross-check)** |
| WS `order` frame `data` = the FULL postback payload structure (incl. `checksum`/`app_id`/`user_id`) | **Verified (LIVE-DOC)** — corrected 2026-07-14 (Z-PB-5 RESOLVED) |

**Compare: Dhan/Groww (for the tickvault OMS mapping):** Kite's WS order-update JSON uses full snake_case field names identical to its REST order rows — unlike Dhan's order-update WS which uses abbreviated PascalCase codes that differ from its own REST API (`Product: "I"`, `TxnType: "B"` — `docs/dhan-ref/10-live-order-update-websocket.md` rule 6). A Kite consumer therefore needs ONE order deserializer for REST + WS + postback; a Dhan consumer needs two. (The LIVE-DOC "same structure" statement strengthens this: WS and HTTP postback payloads are the SAME document.)

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Z-PB-1 — verbatim coverage wording. RESOLVED by live fetch (2026-07-14).** The live postbacks page states verbatim: "Only orders placed using the app's api_key are notified." and "For individual developers, Postbacks over WebSocket is recommended, where, orders placed for a particular user anywhere, for instance, web, mobile, or desktop platforms, are sent." (https://kite.trade/docs/connect/v3/postbacks/, LIVE-DOC runner 2026-07-14). The design-load-bearing negative is confirmed: an api_key-holder canNOT observe manually-placed orders via the HTTP webhook.
- **Z-PB-2 — registration mechanics. STILL OPEN.** The live postbacks page says only "the registered postback_url of your app"; `developers.kite.trade` is login-gated (the 2026-07-14 crawl fetched its login shell only). Field list, live-editability, HTTPS/localhost rules remain unsourced. NOTE (live correction): the "reachable HTTPS URL in developer's settings" wording is NOT on the live page — forum/legacy SEARCH only.
- **Z-PB-3 — delivery mechanics. PARTIALLY RESOLVED by live fetch (2026-07-14):** the body is "posted as a raw HTTP POST body. You will have to read the raw body and then decode it" (LIVE-DOC). REMAINING: POST `Content-Type` header, retry count/backoff on non-2xx, timeout — the live page states none of these ("reliably, irrespective of when they happen" is the only delivery language). Matters: idempotency + whether missed events are re-POSTed (if not, `GET /orders` polling is the only reconciliation).
- **Z-PB-4 — trigger matrix. LARGELY RESOLVED by live fetch (2026-07-14):** triggers are `COMPLETE, CANCEL, REJECTED, UPDATE`; "An UPDATE postback is triggered when an open order is modified or when there's a partial fill"; payload `status` values are "COMPLETE, REJECTED, CANCELLED, and UPDATE" (LIVE-DOC — note the intro-vs-attribute CANCEL/CANCELLED wording wart). REMAINING: whether every partial fill fires exactly one POST, and whether `TRIGGER PENDING`-class transitions fire anything.
- **Z-PB-5 — exact WS order-update payload field list. RESOLVED by live fetch (2026-07-14):** the websocket page states the `data` key "has the same structure described in the Postbacks section" and "will contain the full order Postback payload" (LIVE-DOC) — i.e. including `checksum`/`app_id`/`user_id`. The previous no-checksum assumption is corrected; the §6 one-deserializer claim holds.
- **Z-PB-6 — delivery guarantee on the WS channel. STILL OPEN.** Neither live page states missed-update/reconnect semantics; forum answers (11290/7067) say poll `/orders` to reconcile. Matters: OMS reconciliation design (the Dhan-style reconcile loop).
- **Z-PB-7 — checksum edge cases. PARTIALLY RESOLVED by live fetch (2026-07-14):** the operand is `order_timestamp` per the live Checksum section ("SHA-256 hash of (order_id + order_timestamp + api_secret)"); the attributes-table row abbreviates it as "timestamp". REMAINING: whether the concatenated value is the exact payload string (IST `"YYYY-MM-DD HH:MM:SS"` — near-certain but not stated as byte rule) and behaviour when `order_timestamp` is null. Matters: validation must byte-match.

## CLAIMS (for README reconciled table)

- Postback checksum = "the SHA-256 hash of (order_id + order_timestamp + api_secret)", plain string concat in that order, hex digest compared — **Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/postbacks/ Checksum section, verbatim)** + **Verified (CLIENT-LIB-SOURCE kiteconnectjs@e366e45 lib/connect.ts `validatePostback`)** cross-check.
- Checksum purpose (verbatim): "to ensure that the update is being POSTed by Kite Connect and not by an unauthorised entity, as only Kite Connect can generate a checksum that contains your api_secret" — **Verified (LIVE-DOC runner 2026-07-14)** (upgraded from SEARCH).
- Postback payload = the standard order-row family + `user_id`, `app_id` (int64), `checksum` (64-hex); IST string timestamps; sample status `COMPLETE`; full attribute table with declared types — **Verified (LIVE-DOC runner 2026-07-14 sample payload + Payload attributes table; byte-identical OFFICIAL-MOCK postback.json cross-check)**.
- Postback is a `POST` with a JSON payload — "posted as a raw HTTP POST body. You will have to read the raw body and then decode it" — to the app-registered `postback_url` on order status changes (`COMPLETE`/`CANCEL`/`REJECTED`/`UPDATE`); "An UPDATE postback is triggered when an open order is modified or when there's a partial fill" — **Verified (LIVE-DOC runner 2026-07-14)** (upgraded from SEARCH; raw-body was previously a forum inference).
- Payload `status` values: "COMPLETE, REJECTED, CANCELLED, and UPDATE" — **Verified (LIVE-DOC runner 2026-07-14)** (new; note intro says `CANCEL`, attributes say `CANCELLED`).
- HTTP postbacks fire ONLY for orders placed via the registering `api_key`; WebSocket order updates cover ALL of the user's orders from any platform (web/mobile/desktop) — **Verified (LIVE-DOC runner 2026-07-14, verbatim Note)** (upgraded from SEARCH).
- "Postback API works even when the user is not logged in" (validate the checksum) — **Verified (LIVE-DOC runner 2026-07-14)** (new).
- WS order updates arrive as JSON TEXT frames on the SAME `wss://ws.kite.trade` socket as binary ticks — "Market data is always binary and Postbacks and other updates are always text"; shape `{"type":"order","data":{}}`; THREE text types documented: `order` / `error` / `message` (broker messages/alerts) — **Verified (LIVE-DOC runner 2026-07-14, websocket page)** + **Verified (CLIENT-LIB-SOURCE pykiteconnect ticker.py + gokiteconnect ticker.go + kiteconnectjs ticker.ts)** for the `order`/`error` dispatch (the `message` type is LIVE-DOC-only).
- The WS `order` frame's `data` "has the same structure described in the Postbacks section" / "will contain the full order Postback payload" — **Verified (LIVE-DOC runner 2026-07-14)** — CORRECTED 2026-07-14 (previously Assumed no-checksum + Unknown field delta, Z-PB-5); SDKs deserialize it into the REST `Order` struct, ignoring the postback-only keys — **Verified (CLIENT-LIB-SOURCE gokiteconnect ticker.go)**.
- `postback_url` registration is app-level ("the registered postback_url of your app") — **Verified (LIVE-DOC runner 2026-07-14)**; the "reachable HTTPS URL in developer's settings" wording is NOT on the live page — **SEARCH (forum/legacy, 2026-07-13)** — CORRECTED attribution 2026-07-14.
- Only the JS SDK ships a postback-validation helper; py/Go have none (re-implement the formula) — **Verified (CLIENT-LIB-SOURCE, grep absence over all three trees, 2026-07-13)**.
- Postback checksum ≠ login checksum: session mint uses `sha256(api_key + request_token + api_secret)` — **Verified (CLIENT-LIB-SOURCE kiteconnectjs connect.ts + pykiteconnect connect.py)**.
- Delivery retry/ordering guarantees on both channels — **Unknown** (Z-PB-3/Z-PB-6 — the live pages state none).
- Compare: Kite is the only broker of the three in this repo with a checksum-authenticated webhook (Dhan's postback has no checksum; Groww has no HTTP webhook) — **Verified against repo packs** (`docs/dhan-ref/07e-postback.md`, `docs/groww-ref/07-feed-websocket-streaming.md`).
