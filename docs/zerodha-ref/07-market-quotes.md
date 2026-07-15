# Zerodha Kite Connect v3 — Market Quotes REST (`/quote`, `/quote/ohlc`, `/quote/ltp`)

> **Source:** `https://kite.trade/docs/connect/v3/market-quotes/` (live official page — "Market quotes and instruments")
> **Fetched-Verified:** 2026-07-14 — live-verbatim via GH-runner 2026-07-14 (UTC timestamps in `00-COVERAGE-MANIFEST.md`): `market-quotes/` + the LEGACY `market-data-and-instruments/` page (HTTP 200 but OUT of the live nav) + `exceptions/` + `changelog/` + `websocket/` + live-fetched kite.trade forum threads 8577/13397. Earlier 2026-07-13 basis retained as cross-check: CLIENT-LIB-SOURCE (pykiteconnect@master `kiteconnect/connect.py`; gokiteconnect@master `market.go`) + OFFICIAL-MOCK (zerodha/kiteconnect-mocks@main `quote.json` / `ohlc.json` / `ltp.json`) + SEARCH.
> **Evidence tiers:** see README legend — **Verified (LIVE-DOC runner 2026-07-14)** is the top tier for this pack / Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) / SEARCH / Assumed / Unknown.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. Any TickVault consumer of these endpoints requires its own dated operator grant per `.claude/rules/project/no-rest-except-live-feed-2026-06-27.md`.
> **Related:** `14-option-chain-composition.md` (the chain is COMPOSED from these endpoints; its §5 carries the consolidated India-F&O/BFO coverage note), `99-UNKNOWNS.md`.

---

## 1. The three endpoints

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/market-quotes/ endpoints table)** — cross-check Verified (CLIENT-LIB-SOURCE pykiteconnect@master `kiteconnect/connect.py`, `_routes` dict lines 154–156):

| Method | Endpoint | SDK route key | Returns (live-doc wording) |
|---|---|---|---|
| GET | `/quote` | `market.quote` | "Retrieve full market quotes for one or more instruments" — OHLC, volume, OI, circuit limits, 5-level bid/ask depth |
| GET | `/quote/ohlc` | `market.quote.ohlc` | "Retrieve OHLC quotes for one or more instruments" |
| GET | `/quote/ltp` | `market.quote.ltp` | "Retrieve LTP quotes for one or more instruments" |

- The same live page's endpoints table ALSO lists `GET /instruments` and `GET /instruments/:exchange` (instrument-master CSV — covered in `09-instruments-master.md`); the live nav title is "Market quotes and instruments".
- **Base URL (Verified, LIVE-DOC — every curl example on the page uses `https://api.kite.trade`; cross-check CLIENT-LIB-SOURCE `connect.py` line 36 `_default_root_uri`):** full URL is e.g. `GET https://api.kite.trade/quote?i=NSE:INFY`.
- **HTTP method GET (Verified, LIVE-DOC curl examples + CLIENT-LIB-SOURCE):** all three go through the SDK's `_get(...)` helper (`connect.py` `quote()`/`ohlc()`/`ltp()`); gokiteconnect cross-check: `market.go` `GetQuote`/`GetOHLC`/`GetLTP` all issue `http.MethodGet`.
- **Auth headers (Verified, LIVE-DOC — every curl example carries `-H "X-Kite-Version: 3" -H "Authorization: token api_key:access_token"`; cross-check CLIENT-LIB-SOURCE `connect.py` lines 936–945):** **Compare: Dhan** uses a bare `access-token` header AND additionally requires a `client-id` header specifically on its Market Quote APIs (`docs/dhan-ref/11-market-quote-rest.md`); Kite has no second header.

## 2. The `i=` instrument query parameter

**Verified (LIVE-DOC runner 2026-07-14, market-quotes page, identical sentence under all three endpoints):** *"Instruments are identified by the `exchange:tradingsymbol` combination and are passed as values to the query parameter `i` which is repeated for every instrument."* — i.e. `?i=NSE:INFY&i=BSE:SENSEX&i=NSE:NIFTY+50` (the live OHLC/LTP curl examples show exactly this, including a space-bearing symbol `NSE:NIFTY 50` URL-encoded as `NIFTY+50`). Cross-check **Verified (CLIENT-LIB-SOURCE):** pykiteconnect sends `params={"i": ins}` with a list; gokiteconnect `type quoteParams struct { Instruments []string `url:"i"` }`.

- Exchange values available for the prefix (**Verified, CLIENT-LIB-SOURCE `connect.py` lines 80–86 constants):** `NSE`, `BSE`, `NFO`, `CDS`, `BFO`, `MCX`, `BCD`.
- **Response keys mirror the request keys (Verified, LIVE-DOC examples + OFFICIAL-MOCK):** responses are keyed by the requested `"NSE:INFY"` string (see §4–§6).
- **Missing/no-data key behaviour — RESOLVED by live fetch (Verified, LIVE-DOC, verbatim):** *"If there is no data available for a given key, the key will be absent from the response. The existence of all the instrument keys in the response map should be checked before to accessing them."* Plus the OHLC/LTP Note: *"If there's no data for the particular instrument or if it has expired, the key will be missing from the response."* → a bad/expired/no-data instrument is SILENTLY OMITTED from `data`, not a request-level error. (Whether a syntactically MALFORMED `i=` value instead errors the whole request is still not stated → OPEN QUESTIONS.)
- **Numeric `instrument_token` as an `i=` value — answered at LEGACY-doc level:** the legacy `market-data-and-instruments/` page (Verified LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/market-data-and-instruments/ — still HTTP 200 but NOT in the live site nav) shows the curl `.../quote?i=41729&i=NSE:INFY` with the response keyed by the raw token string `"41729"`. The CURRENT `market-quotes/` page only ever shows `exchange:tradingsymbol`, so numeric-token acceptance TODAY is unconfirmed (the legacy page is provably stale elsewhere — see §3) → OPEN QUESTIONS (probe-only).

## 3. Instruments-per-request caps (LOAD-BEARING) — SETTLED LIVE

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/market-quotes/ — both the per-endpoint prose AND the page-closing "Limits" table, verbatim):**

| Endpoint | Max instruments per request | Live-doc wording | Tier |
|---|---|---|---|
| `GET /quote` | **500** | "the complete market data snapshot of up to 500 instruments in one go" + Limits table `/quote → 500` | Verified (LIVE-DOC) |
| `GET /quote/ohlc` | **1000** | "the OHLC + LTP snapshots of up to 1000 instruments in one go" + Limits table `/quote/ohlc → 1000` | Verified (LIVE-DOC) |
| `GET /quote/ltp` | **1000** | "the LTPs of up to 1000 instruments in one go" + Limits table `/quote/ltp → 1000` | Verified (LIVE-DOC) |

- **Corroboration (Verified, LIVE-DOC https://kite.trade/docs/connect/v3/changelog/):** the Kite Connect 3.0 "API endpoint changes" table repeats the same numbers — `/quote` "up to 500", `/quote/ohlc` "up to 1000", `/quote/ltp` "up to 1000" — and records `/market/instruments/:exchange/:tradingsymbol` as **Removed — "Replaced by the new /quote API"**.
- **The stray "250" — RESOLVED by live fetch (correction of provenance, numbers unchanged):** the conflicting "up to 250 instruments" figure recorded here on 2026-07-13 is now EXPLAINED — it is the LEGACY `market-data-and-instruments/` page (Verified LIVE-DOC runner 2026-07-14), which still serves HTTP 200 but is absent from the live nav and states **250 for ALL THREE endpoints**. That page is provably stale (older example payloads, pre-3.1 field names — see §4 note). **The live current page wins: 500 / 1000 / 1000.** The 2026-07-13 SEARCH-extraction wart was exactly this stale-page pollution class (`docs/groww-ref/README.md` [R#3] analog).
- **No cap is enforced client-side (Verified, CLIENT-LIB-SOURCE):** neither pykiteconnect nor gokiteconnect validates list length — the server is the sole enforcer; over-cap behaviour (413? `InputException`? silent truncation?) is stated NOWHERE in the live docs either → OPEN QUESTIONS.
- **Compare: Dhan** — all three Dhan quote modes accept **up to 1000 instruments** per request, sent as a POST body map of segment→ids (`docs/dhan-ref/11-market-quote-rest.md`). **Compare: Groww** — LTP/OHLC batch cap is **50 instruments** per call and full quote is **single-instrument only** (`docs/groww-ref/README.md` [R#6]). Kite's 500-instrument FULL quote (with depth+OI) in one GET has no Dhan/Groww equivalent shape.

## 4. Full quote (`GET /quote`) — response structure

**Verified (LIVE-DOC runner 2026-07-14, market-quotes page example + "Response attributes" table) + cross-check Verified (OFFICIAL-MOCK kiteconnect-mocks@main `quote.json` — the live page's example IS the same 2021-06-08 INFY snapshot) + Verified (CLIENT-LIB-SOURCE gokiteconnect `market.go` `Quote` struct):**

Envelope: `{"status": "success", "data": { "<exchange:tradingsymbol>": { … } } }`.

Per-instrument fields (JSON name → live-doc type, description now LIVE-DOC verbatim where quoted):

| Field | Type (live doc) | Example | Live-doc description / note |
|---|---|---|---|
| `instrument_token` | uint32 | `408065` | numeric WS-subscribable token |
| `timestamp` | string | `"2021-06-08 15:45:56"` | "The exchange timestamp of the quote packet" — naive `YYYY-MM-DD HH:MM:SS`; **Assumed IST** (no TZ stated on the wire or in the doc) |
| `last_trade_time` | null, string | `"2021-06-08 15:45:52"` | "Last trade timestamp" — nullable per the live type column |
| `last_price` | float64 | `1412.95` | |
| `last_quantity` | int64 | `5` | "Last traded quantity" |
| `average_price` | float64 | `1412.47` | "The volume weighted average price of a stock at a given time during the day" |
| `volume` | int64 | `7360198` | **"Volume traded today"** — day-cumulative CONFIRMED (was Assumed) |
| `buy_quantity` / `sell_quantity` | int64 | `0` / `5191` | "Total quantity of buy/sell orders pending at the exchange" |
| `net_change` | float64 | `0` | **"The absolute change from yesterday's close to last traded price"** — the example still shows `0` alongside last≠close; do NOT derive it yourself |
| `oi` | float64 | `0` | "The Open Interest for a futures or options contract" (F&O only) |
| `oi_day_high` / `oi_day_low` | float64 | `0` / `0` | "highest/lowest Open Interest recorded during the day" |
| `lower_circuit_limit` / `upper_circuit_limit` | float64 | `1250.7` / `1528.6` | "current lower/upper circuit limit" (added in Kite Connect 3.1 per the live changelog) |
| `ohlc` | `{open, high, low, close}` all float64 | `1396 / 1421.75 / 1395.55 / 1389.65` | `close` semantics CONFIRMED — see below |
| `depth` | `{buy: [5], sell: [5]}` | 5 entries per side | each level = `{price: float64, quantity: int64, orders: int64}`; live doc: "Number of open BUY (bid) / SELL (ask) orders at the price", "Net quantity from the pending orders" |

- **Depth is 5 levels per side (Verified, LIVE-DOC example + OFFICIAL-MOCK):** `depth.buy` and `depth.sell` each contain exactly 5 level objects. **Compare: Dhan** full-quote depth is also 5 levels, each level `{quantity, orders, price}` (`docs/dhan-ref/11-market-quote-rest.md`).
- **`ohlc.close` = PREVIOUS trading day's close — RESOLVED by live fetch (Verified, LIVE-DOC, verbatim):** *"ohlc.close — Closing price of the instrument from the last trading day."* (Was Assumed from the in-session mock.) Same statement repeated on the OHLC-endpoint attribute table. **Compare: Dhan** — same semantics, confirmed per Ticket #5525125 (`.claude/rules/dhan/live-market-feed.md`).
- **Legacy field-name divergence (recorded so a stale mirror can't poison a parser):** the LEGACY `market-data-and-instruments/` page's full-quote example/attributes use `open_interest`, `day_high_oi`, `day_low_oi`, `last_time` — the CURRENT page (and the mocks + gokiteconnect struct) use `oi`, `oi_day_high`, `oi_day_low`, `last_trade_time`. Build only against the current names.
- **Post-parse mutation note (Verified, CLIENT-LIB-SOURCE):** pykiteconnect's `quote()` pipes each entry through `_format_response`, converting timestamp strings into Python `datetime` objects — the RAW wire type is the string shown above.

## 5. OHLC quote (`GET /quote/ohlc`) — response structure

**Verified (LIVE-DOC runner 2026-07-14, market-quotes page example + attribute table; cross-check OFFICIAL-MOCK `ohlc.json` + CLIENT-LIB-SOURCE gokiteconnect `QuoteOHLC`):** same envelope; per-instrument payload is exactly three fields:

```json
"NSE:INFY": { "instrument_token": 408065, "last_price": 1075,
              "ohlc": { "open": 1085.8, "high": 1085.9, "low": 1070.9, "close": 1075.8 } }
```

No timestamp, no volume, no depth, no OI (Verified-absence at live-doc attribute-table + mock + struct level). `ohlc.close` here too is "Closing price of the instrument from the last trading day" (LIVE-DOC).

## 6. LTP quote (`GET /quote/ltp`) — response structure

**Verified (LIVE-DOC runner 2026-07-14, market-quotes page example + attribute table; cross-check OFFICIAL-MOCK `ltp.json` + CLIENT-LIB-SOURCE gokiteconnect `QuoteLTP`):** same envelope; per-instrument payload is two fields only:

```json
"NSE:INFY": { "instrument_token": 408065, "last_price": 1074.35 }
```

## 7. Error envelope

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/exceptions/):** a failed call returns `{"status": "error", "message": "…", "error_type": "GeneralException"}` (the page's verbatim example) — exception names include `TokenException` (preceded by a 403; "clear the user's session and re-initiate a login"), `InputException`, `NetworkException`, `DataException`, `GeneralException`, etc. The page's "Common HTTP error codes" table lists **`429 — Too many requests to the API (rate limiting)`** — so the over-rate STATUS CODE is now Verified (LIVE-DOC); whether the 429 body carries the same JSON envelope is still unstated → OPEN QUESTIONS. Cross-check **Verified (CLIENT-LIB-SOURCE pykiteconnect `connect.py` ~line 981 + `exceptions.py`):** the SDK raises the exception class named by `error_type`; HTTP 403 + `TokenException` triggers the session-expiry hook; success responses unwrap `data` only. There is **no client-side 429/backoff handling in pykiteconnect** (no `429` literal anywhere in `connect.py`/`exceptions.py` — Verified-absence at SDK level). **Compare: Groww** SDK likewise ships zero client-side throttling (`docs/groww-ref/README.md` [R#24]).

## 8. Rate limits (quote family)

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/exceptions/ "API rate limit" table, verbatim):**

| API family (live-doc "end-point") | Limit (live-doc "rate-limit") |
|---|---|
| Quote (`/quote`, `/quote/ohlc`, `/quote/ltp`) | **1req/second** |
| Historical candle | 3req/second |
| Order placement | 10req/second (+ the page's Note: 400 orders/min, 5000 orders/day, 25 modifications/order — out of this file's scope; see the orders file) |
| All other endpoints | 10req/second |

- **Enforced at the API-key level (Verified, LIVE-DOC forum — kite.trade forum 8577, live-fetched 2026-07-14, Zerodha staff rakeshr: "combined requests from a unique API key shouldn't exceed 10 req/sec").** **No per-minute or per-day caps for non-order endpoints (Verified, LIVE-DOC forum — kite.trade forum 13397, live-fetched 2026-07-14, Zerodha staff sujith: "There are no per minute or per day limits for any endpoints except for order placement API.").** Forum tier = staff answers on the official forum, not the docs page proper — kept one notch below the docs-table facts.
- **ARITH (from the LIVE-DOC caps above):** max REST snapshot throughput = 1 quote call/s × 500 instruments = **500 full quotes/s**, or 1000 LTP-or-OHLC rows/s — i.e. a ~500-instrument universe refreshes at ~1 Hz full-depth via REST alone. Derivation: cap × rate, both cited in §3/§8.
- **Compare: Dhan** Quote APIs are also **1 request/second** but with up to 1000 instruments/request (`docs/dhan-ref/11-market-quote-rest.md`). **Compare: Groww** pools quote/LTP/OHLC in a Live Data family at 10/s + 300/min, type-level pooled (`docs/groww-ref/README.md` [R#1]).

## 9. Streaming alternative

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/websocket/):** *"You can subscribe for up to 3000 instruments on a single WebSocket connection"* and *"Single API key can have upto 3 websocket connections"* — the REST quote endpoints are point-in-time snapshots ("For realtime streaming market quotes, use the WebSocket API", LIVE-DOC market-quotes page; same doctrine as `.claude/rules/dhan/market-quote.md` rule 11). Binary WS protocol reference: pykiteconnect `ticker.py` `_parse_binary` (separate pack file).

## 10. Trigger range (`trigger_range`) — CO-legacy sibling endpoint

Recorded for pack completeness (added 2026-07-13; liveness settled 2026-07-14):

- **NOT documented anywhere on the live v3 docs — RESOLVED by live fetch (Verified-absence, LIVE-DOC runner 2026-07-14):** a grep for `trigger_range` across ALL 148 live-crawled pages (incl. `market-quotes/`, the legacy `market-data-and-instruments/`, `orders/`, `changelog/`) returns ZERO hits. The endpoint exists only in SDK code + the official mock — it is undocumented/CO-legacy on the live site. Do not build against it.
- **Route DIVERGENCE between the two official SDKs (Verified, CLIENT-LIB-SOURCE both, fetched 2026-07-13) — still UNRESOLVED (docs are silent, so only a live probe can settle it):**
  - pykiteconnect: `market.trigger_range` → `GET /instruments/trigger_range/{transaction_type}` with repeated `i=exchange:tradingsymbol` params and `transaction_type` lower-cased into the path (`connect.py` line 152 + `trigger_range()` lines 712–720; docstring: "Retrieve the buy/sell trigger range for Cover Orders.").
  - gokiteconnect: `URIGetTriggerRange = "/instruments/{exchange}/{tradingsymbol}/trigger_range"` (`connect.go` line 151) — a DIFFERENT per-instrument path shape, and **no Go function consumes the constant** (grep: URI constant only). Which shape the live server honours (or both, or neither) remains **Unknown**.
- **Response (Verified, OFFICIAL-MOCK `trigger_range.json`, kiteconnect-mocks@main, fetched 2026-07-13):** same success envelope, keyed by `exchange:tradingsymbol`, per-instrument payload `{instrument_token (0 in the mock), lower, upper}` — the permissible trigger band (floats, unrounded: `1075.599`, `870.57475`).

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

Closed by the 2026-07-14 live fetch (keep these notes so the 99-UNKNOWNS rebuilder can close the matching U-rows):

- ~~Per-request caps live re-read~~ — **RESOLVED by live fetch:** 500 (`/quote`) / 1000 (`/quote/ohlc`) / 1000 (`/quote/ltp`), Verified (LIVE-DOC market-quotes page "Limits" table + prose + changelog corroboration); the stray "250" traced to the stale legacy `market-data-and-instruments/` page (§3).
- ~~Quote-family rate limit live re-read~~ — **RESOLVED by live fetch:** Quote = 1req/second per the exceptions-page "API rate limit" table (LIVE-DOC); "no per-min/day caps except orders" + API-key-level enforcement confirmed at live-fetched-forum tier (§8).
- ~~Unknown/invalid instrument in `i=` list~~ — **RESOLVED by live fetch:** a no-data/expired key is SILENTLY ABSENT from `data` — callers must check key existence (LIVE-DOC verbatim, §2). Residual (folded into the over-cap question below): whether a syntactically malformed `i=` value errors the whole request instead.
- ~~`ohlc.close` = previous-day close?~~ — **RESOLVED by live fetch:** "Closing price of the instrument from the last trading day" (LIVE-DOC, §4/§5).
- ~~`trigger_range` liveness~~ — **RESOLVED by live fetch (liveness half):** absent from all 148 live-crawled pages → undocumented/CO-legacy confirmed at docs level (§10). The route-shape half stays open below.

Still open:

- **Over-cap request behaviour:** what the server returns for >500/>1000 instruments (error_type? truncation?) — stated nowhere in the live docs; probe-only. Also: behaviour for a syntactically malformed `i=` value (vs the documented silent-absence for no-data keys). Matters: fail-loud batching.
- **`timestamp`/`last_trade_time` timezone:** the live page says only "The exchange timestamp of the quote packet" — IST assumed, still unstated. Matters: candle bucketing off-by-19800s class.
- **`i=` with numeric instrument_token — CURRENT behaviour:** the LEGACY page documents `i=41729` accepted and the response keyed `"41729"` (§2), but that page is provably stale (its caps read 250); the current page shows only `exchange:tradingsymbol`. Probe-only. Matters: join-key design.
- **Over-rate (429) BODY shape:** the 429 status code is now Verified (LIVE-DOC exceptions page); whether its body carries the standard `{"status":"error",…}` envelope is unstated — probe. Matters: backoff ladder design (Dhan DH-904 analog).
- **`trigger_range` route shape:** the two official SDKs ship DIFFERENT paths (§10) and the docs are silent — which shape the server accepts (if any) needs one live probe. Matters: completeness only (CO-legacy, undocumented — do not build against it).

## CLAIMS (for README reconciled table)

- Routes are exactly `GET /quote`, `GET /quote/ohlc`, `GET /quote/ltp` on base `https://api.kite.trade` — **Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/market-quotes/; cross-check CLIENT-LIB-SOURCE pykiteconnect connect.py `_routes`)**
- Instruments are passed as a REPEATED `i=` query param in `exchange:tradingsymbol` format (e.g. `NSE:INFY`) — **Verified (LIVE-DOC runner 2026-07-14, market-quotes page verbatim; cross-check CLIENT-LIB-SOURCE pykiteconnect `quote()` + gokiteconnect `quoteParams` url:"i")**
- Per-request caps: **500** instruments for `/quote`; **1000** for `/quote/ohlc` and `/quote/ltp` — **Verified (LIVE-DOC runner 2026-07-14, market-quotes page "Limits" table + per-endpoint prose + changelog corroboration)**. The 2026-07-13 stray "250" is the stale legacy `market-data-and-instruments/` page (out of live nav, still HTTP 200) — live page wins. UPGRADED from SEARCH.
- A no-data/expired instrument key is SILENTLY ABSENT from the `data` map — callers must check key existence — **Verified (LIVE-DOC runner 2026-07-14, market-quotes page verbatim)** — NEW claim (was Unknown)
- Full quote carries OHLC, volume, OI (+day high/low), lower/upper circuit limits, and 5-level bid/ask depth (`{price, quantity, orders}` per level) — **Verified (LIVE-DOC runner 2026-07-14 attribute table; cross-check OFFICIAL-MOCK quote.json + gokiteconnect Quote struct)**
- Response envelope is `{"status":"success","data":{"<exchange:tradingsymbol>": …}}`, keyed by the requested instrument string — **Verified (LIVE-DOC runner 2026-07-14 examples; cross-check OFFICIAL-MOCK quote.json/ohlc.json/ltp.json)**
- `/quote/ohlc` returns only `instrument_token` + `last_price` + `ohlc{o,h,l,c}`; `/quote/ltp` returns only `instrument_token` + `last_price` — **Verified (LIVE-DOC runner 2026-07-14 examples + attribute tables; cross-check OFFICIAL-MOCK + gokiteconnect QuoteOHLC/QuoteLTP)**
- Auth = `Authorization: token <api_key>:<access_token>` + `X-Kite-Version: 3` headers on every call — **Verified (LIVE-DOC runner 2026-07-14 curl examples; cross-check CLIENT-LIB-SOURCE connect.py lines 936–945)**
- Quote-family rate limit = 1 request/second (Historical 3/s, everything else 10/s); HTTP 429 = "Too many requests to the API (rate limiting)" — **Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/exceptions/ "API rate limit" + HTTP-codes tables)**. UPGRADED from SEARCH.
- Rate limits enforced at API-key level; no per-min/day caps for non-order endpoints — **Verified (LIVE-DOC forum, kite.trade forum 8577 + 13397 staff answers, live-fetched 2026-07-14)** — one notch below docs-table tier
- `volume` = "Volume traded today" (day-cumulative); `net_change` = "absolute change from yesterday's close to last traded price" — **Verified (LIVE-DOC runner 2026-07-14 attribute table)**. UPGRADED from Assumed.
- Wire timestamps are naive `"YYYY-MM-DD HH:MM:SS"` strings, `last_trade_time` nullable (`null, string`); SDK converts to datetime post-parse; timezone unstated — **Verified (LIVE-DOC runner 2026-07-14 + OFFICIAL-MOCK) / Assumed (IST)**
- `ohlc.close` is the PREVIOUS trading day's close ("Closing price of the instrument from the last trading day") — **Verified (LIVE-DOC runner 2026-07-14, market-quotes page attribute tables)**. UPGRADED from Assumed — the Dhan Ticket #5525125 class of ambiguity is closed for Kite.
- WebSocket alternative: up to 3000 instruments per connection, up to 3 WS connections per API key — **Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/websocket/)**. UPGRADED from SEARCH.
- Numeric `instrument_token` as `i=` value: documented ONLY on the stale legacy `market-data-and-instruments/` page (`i=41729`, response keyed `"41729"`); current-page behaviour unconfirmed — **Verified (LIVE-DOC legacy page, stale) / Unknown (current)**
- `trigger_range` (CO-legacy): UNDOCUMENTED on the live v3 docs (zero hits across all 148 live-crawled pages); py route `GET /instruments/trigger_range/{transaction_type}` vs Go path-shape `/instruments/{exchange}/{tradingsymbol}/trigger_range` (constant only, no consumer); mock response `{instrument_token, lower, upper}` per instrument — **Verified-absence (LIVE-DOC runner 2026-07-14) + Verified (CLIENT-LIB-SOURCE both SDKs + OFFICIAL-MOCK trigger_range.json)**; server route shape **Unknown** (probe-only)
