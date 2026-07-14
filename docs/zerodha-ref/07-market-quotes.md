# Zerodha Kite Connect v3 — Market Quotes REST (`/quote`, `/quote/ohlc`, `/quote/ltp`)

> **Source:** `https://kite.trade/docs/connect/v3/market-quotes/` (live official page — NOT directly fetchable from this sandbox)
> **Fetched-Verified:** 2026-07-13 via CLIENT-LIB-SOURCE (pykiteconnect@master `kiteconnect/connect.py`; gokiteconnect@master `market.go`) + OFFICIAL-MOCK (zerodha/kiteconnect-mocks@main `quote.json` / `ohlc.json` / `ltp.json`) + SEARCH (search-backend extraction of the live docs page + kite.trade forum). Direct kite.trade fetch, web.archive.org, and r.jina.ai were ALL proxy-blocked (CONNECT 403) — no ARCHIVE-DOC or MIRROR-LIVE tier was attainable for this file (see `zerodha-probe`).
> **Evidence tiers:** see README legend — Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) / SEARCH / Assumed / Unknown.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. Any TickVault consumer of these endpoints requires its own dated operator grant per `.claude/rules/project/no-rest-except-live-feed-2026-06-27.md`.
> **Related:** `14-option-chain-composition.md` (the chain is COMPOSED from these endpoints; its §5 carries the consolidated India-F&O/BFO coverage note), `99-UNKNOWNS.md`.

---

## 1. The three endpoints

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master `kiteconnect/connect.py`, `_routes` dict lines 154–156; fetched 2026-07-13, https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/connect.py):**

| Method | Endpoint | SDK route key | Returns |
|---|---|---|---|
| GET | `/quote` | `market.quote` | Full quote snapshot: OHLC, volume, OI, circuit limits, 5-level bid/ask depth |
| GET | `/quote/ohlc` | `market.quote.ohlc` | OHLC + last price only |
| GET | `/quote/ltp` | `market.quote.ltp` | Last traded price only |

- **Base URL (Verified, CLIENT-LIB-SOURCE `connect.py` line 36):** `_default_root_uri = "https://api.kite.trade"` — so the full URL is e.g. `GET https://api.kite.trade/quote?i=NSE:INFY`.
- **HTTP method GET (Verified, CLIENT-LIB-SOURCE):** all three go through the SDK's `_get(...)` helper (`connect.py` `quote()`/`ohlc()`/`ltp()` methods, lines 621–662); gokiteconnect cross-check: `market.go` `GetQuote`/`GetOHLC`/`GetLTP` all issue `http.MethodGet`.
- **Auth headers (Verified, CLIENT-LIB-SOURCE `connect.py` lines 936–945):** every request carries `X-Kite-Version: 3` and `Authorization: token <api_key>:<access_token>`. **Compare: Dhan** uses a bare `access-token` header AND additionally requires a `client-id` header specifically on its Market Quote APIs (`docs/dhan-ref/11-market-quote-rest.md` — "All three endpoints require both `access-token` AND `client-id` headers"); Kite has no second header.

## 2. The `i=` instrument query parameter

**Verified (CLIENT-LIB-SOURCE pykiteconnect `connect.py` `quote()` docstring, line 625):** *"`instruments` is a list of instruments, Instrument are in the format of `exchange:tradingsymbol`. For example NSE:INFY"* — sent as `params={"i": ins}` where `ins` is a list, i.e. the query parameter **`i` is REPEATED once per instrument** (`?i=NSE:INFY&i=NSE:TCS&…`). Cross-check **Verified (CLIENT-LIB-SOURCE gokiteconnect `market.go`):** `type quoteParams struct { Instruments []string `url:"i"` }` — the same repeated-`i` encoding.

- Exchange values available for the prefix (**Verified, CLIENT-LIB-SOURCE `connect.py` lines 80–86 constants):** `NSE`, `BSE`, `NFO`, `CDS`, `BFO`, `MCX`, `BCD`.
- **Response keys mirror the request keys (Verified, OFFICIAL-MOCK):** all three mock responses are keyed by the requested `"NSE:INFY"` string (see §4–§6).
- **Unknown:** whether a numeric `instrument_token` is also accepted as an `i=` value (and how the response is then keyed) — the SDK only ever sends `exchange:tradingsymbol`; behaviour for tokens is undocumented in any fetched artifact → OPEN QUESTIONS.
- **Unknown:** behaviour for an invalid/unknown instrument in the list (silently omitted from `data` vs whole-request error) → OPEN QUESTIONS.

## 3. Instruments-per-request caps (LOAD-BEARING)

**SEARCH (search-backend extraction of `https://kite.trade/docs/connect/v3/market-quotes/` + kite.trade forum discussion 15692 `getquote-instruments-upper-limit-on-number-of-instruments`, extracted 2026-07-13):**

| Endpoint | Max instruments per request | Tier |
|---|---|---|
| `GET /quote` | **500** — "complete market data snapshot of up to 500 instruments in one go" | SEARCH |
| `GET /quote/ohlc` | **1000** | SEARCH |
| `GET /quote/ltp` | **1000** | SEARCH |

- **Extraction wart, recorded honestly:** one search extraction ALSO surfaced a conflicting "up to 250 instruments" figure for the OHLC+LTP snapshot family (source generation unknown — the same stale-mirror pollution class the groww-ref pack documented for its rate-limit table, `docs/groww-ref/README.md` [R#3]). **A targeted disconfirmation pass (adversarial review, 2026-07-13) failed to reproduce the 250 figure and explicitly re-returned 1000/1000/500 as the current-docs set** — the outlier is now doubly weakened. The 500/1000/1000 set is the consistently-repeated current set, but **these numbers are still NOT Verified-tier** — they need a day-0 live re-read of the docs page from an unblocked network → OPEN QUESTIONS.
- **No cap is enforced client-side (Verified, CLIENT-LIB-SOURCE):** neither pykiteconnect nor gokiteconnect validates list length — the server is the sole enforcer; over-cap behaviour (413? `InputException`? silent truncation?) is undocumented in fetched artifacts → OPEN QUESTIONS.
- **Compare: Dhan** — all three Dhan quote modes accept **up to 1000 instruments** per request, sent as a POST body map of segment→ids (`docs/dhan-ref/11-market-quote-rest.md`). **Compare: Groww** — LTP/OHLC batch cap is **50 instruments** per call and full quote is **single-instrument only** (`docs/groww-ref/README.md` [R#6]). Kite's 500-instrument FULL quote (with depth+OI) in one GET has no Dhan/Groww equivalent shape.

## 4. Full quote (`GET /quote`) — response structure

**Verified (OFFICIAL-MOCK kiteconnect-mocks@main `quote.json`, fetched 2026-07-13, https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/quote.json) + Verified (CLIENT-LIB-SOURCE gokiteconnect `market.go` `Quote` struct):**

Envelope: `{"status": "success", "data": { "<exchange:tradingsymbol>": { … } } }`.

Per-instrument fields (JSON name → Go type from `market.go`, values from the mock):

| Field | Type (gokiteconnect) | Mock example | Note |
|---|---|---|---|
| `instrument_token` | int | `408065` | numeric WS-subscribable token |
| `timestamp` | Time (string on wire) | `"2021-06-08 15:45:56"` | naive `YYYY-MM-DD HH:MM:SS`; **Assumed IST** (exchange-local; no TZ on the wire) |
| `last_trade_time` | Time (string) | `"2021-06-08 15:45:52"` | same format |
| `last_price` | float64 | `1412.95` | |
| `last_quantity` | int | `5` | |
| `average_price` | float64 | `1412.47` | |
| `volume` | int | `7360198` | day cumulative (**Assumed** — semantics not stated in fetched artifacts) |
| `buy_quantity` / `sell_quantity` | int | `0` / `5191` | total pending qty |
| `net_change` | float64 | `0` | mock shows `0` alongside last≠close — do NOT derive it yourself |
| `oi` | float64 | `0` | open interest (derivatives) |
| `oi_day_high` / `oi_day_low` | float64 | `0` / `0` | |
| `lower_circuit_limit` / `upper_circuit_limit` | float64 | `1250.7` / `1528.6` | circuit band |
| `ohlc` | `{open, high, low, close}` all float64 | `1396 / 1421.75 / 1395.55 / 1389.65` | see the `close` semantics note below |
| `depth` | `{buy: [5], sell: [5]}` | 5 entries per side in the mock | each level = `{price: float64, quantity: int, orders: int}` |

- **Depth is 5 levels per side (Verified, OFFICIAL-MOCK):** the mock's `depth.buy` and `depth.sell` arrays each contain exactly 5 level objects. **Compare: Dhan** full-quote depth is also 5 levels, each level `{quantity, orders, price}` (`docs/dhan-ref/11-market-quote-rest.md`).
- **`ohlc.close` semantics — Assumed previous-day close:** the mock was captured in-session (`timestamp` 15:45) with `last_price 1412.95` ≠ `ohlc.close 1389.65`, consistent with `close` being the PREVIOUS session's close, but no fetched artifact states it. Load-bearing for any prev-close consumer (**Compare: Dhan** — the `close` field in Dhan Quote/Full packets is confirmed PREVIOUS-day close per Ticket #5525125, `.claude/rules/dhan/live-market-feed.md`) → OPEN QUESTIONS.
- **Post-parse mutation note (Verified, CLIENT-LIB-SOURCE):** pykiteconnect's `quote()` pipes each entry through `_format_response`, converting timestamp strings into Python `datetime` objects — the RAW wire type is the string shown above.

## 5. OHLC quote (`GET /quote/ohlc`) — response structure

**Verified (OFFICIAL-MOCK `ohlc.json` + CLIENT-LIB-SOURCE gokiteconnect `QuoteOHLC`):** same envelope; per-instrument payload is exactly three fields:

```json
"NSE:INFY": { "instrument_token": 408065, "last_price": 1075,
              "ohlc": { "open": 1085.8, "high": 1085.9, "low": 1070.9, "close": 1075.8 } }
```

No timestamp, no volume, no depth, no OI (Verified-absence at mock+struct level: `QuoteOHLC` has only these 3 members).

## 6. LTP quote (`GET /quote/ltp`) — response structure

**Verified (OFFICIAL-MOCK `ltp.json` + CLIENT-LIB-SOURCE gokiteconnect `QuoteLTP`):** same envelope; per-instrument payload is two fields only:

```json
"NSE:INFY": { "instrument_token": 408065, "last_price": 1074.35 }
```

## 7. Error envelope

**Verified (CLIENT-LIB-SOURCE pykiteconnect `connect.py` response handler, ~line 981 + `exceptions.py`):** a failed call returns JSON with `"status": "error"` + `error_type` + `message`; the SDK raises the exception class named by `error_type` (`TokenException`, `InputException`, `NetworkException` code 503, `GeneralException` fallback, etc.). HTTP 403 + `TokenException` triggers the session-expiry hook. Success responses unwrap `data` only. There is **no client-side 429/backoff handling in pykiteconnect** (no `429` literal anywhere in `connect.py`/`exceptions.py` — Verified-absence at SDK level); over-rate behaviour on the wire (status code + body) is undocumented in fetched artifacts → OPEN QUESTIONS. **Compare: Groww** SDK likewise ships zero client-side throttling (`docs/groww-ref/README.md` [R#24]).

## 8. Rate limits (quote family)

**SEARCH (kite.trade forum threads `8577/api-rate-limits`, `13397/rate-limits`, `15701/clarity-on-api-rate-limit` + `kite.trade/docs/connect/v3/exceptions/`, extracted 2026-07-13):**

| API family | Limit |
|---|---|
| Quote (`/quote`, `/quote/ohlc`, `/quote/ltp`) | **1 request/second** |
| Historical | 3 requests/second |
| Order placement | 10 requests/second (+ per-minute/day order caps — out of this file's scope) |
| All other endpoints | 10 requests/second |

- Per the same extraction: limits are enforced **at the API-key level** (aggregated across users/tokens on that key), and there are **no per-minute or per-day caps for non-order endpoints**. SEARCH tier — needs day-0 live confirmation → OPEN QUESTIONS.
- **ARITH (from the SEARCH caps above):** max REST snapshot throughput = 1 quote call/s × 500 instruments = **500 full quotes/s**, or 1000 LTP-or-OHLC rows/s — i.e. a ~500-instrument universe refreshes at ~1 Hz full-depth via REST alone. Derivation: cap × rate, both cited in §3/§8.
- **Compare: Dhan** Quote APIs are also **1 request/second** but with up to 1000 instruments/request (`docs/dhan-ref/11-market-quote-rest.md`). **Compare: Groww** pools quote/LTP/OHLC in a Live Data family at 10/s + 300/min, type-level pooled (`docs/groww-ref/README.md` [R#1]).

## 9. Streaming alternative

**SEARCH (kite.trade docs `websocket/` page extraction, 2026-07-13):** for continuous data, the Kite WebSocket allows subscribing **up to 3000 instruments on a single connection** — the REST quote endpoints are point-in-time snapshots (same doctrine as `.claude/rules/dhan/market-quote.md` rule 11). Binary WS protocol reference: pykiteconnect `ticker.py` `_parse_binary` (separate pack file).

## 10. Trigger range (`trigger_range`) — CO-legacy sibling endpoint

Recorded for pack completeness (added 2026-07-13 per the completeness review — previously only a one-line footnote in `09-instruments-master.md`):

- **Route DIVERGENCE between the two official SDKs (Verified, CLIENT-LIB-SOURCE both, fetched 2026-07-13):**
  - pykiteconnect: `market.trigger_range` → `GET /instruments/trigger_range/{transaction_type}` with repeated `i=exchange:tradingsymbol` params and `transaction_type` lower-cased into the path (`connect.py` line 152 + `trigger_range()` lines 712–720; docstring: "Retrieve the buy/sell trigger range for Cover Orders.").
  - gokiteconnect: `URIGetTriggerRange = "/instruments/{exchange}/{tradingsymbol}/trigger_range"` (`connect.go` line 151) — a DIFFERENT per-instrument path shape, and **no Go function consumes the constant** (grep: URI constant only). Which shape the live server honours (or both) is **Unknown**.
- **Response (Verified, OFFICIAL-MOCK `trigger_range.json`, kiteconnect-mocks@main, fetched 2026-07-13):** same success envelope, keyed by `exchange:tradingsymbol`, per-instrument payload `{instrument_token (0 in the mock), lower, upper}` — the permissible trigger band (floats, unrounded: `1075.599`, `870.57475`).
- **Status: likely CO-legacy / deprecation-suspect** — the py docstring ties it to Cover Orders; whether it is still live/documented on the v3 docs is an OPEN QUESTION. Do not build against it without the live re-read.

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Per-request caps live re-read (LOAD-BEARING):** confirm 500 (`/quote`) and 1000 (`/quote/ohlc`, `/quote/ltp`) verbatim, and rule out the stray "250" extraction — open `https://kite.trade/docs/connect/v3/market-quotes/` and paste the caps sentences. Matters: batching math for any universe-wide snapshot consumer.
- **Quote-family rate limit live re-read:** confirm quote = 1 req/s and "no per-min/day caps except orders" — open `https://kite.trade/docs/connect/v3/exceptions/` (rate-limit section) and paste the table. Matters: refresh-cadence design + 429 avoidance.
- **Over-cap request behaviour:** what does the server return for >500/>1000 instruments (error_type? truncation?) — not documented; live probe or paste from `https://kite.trade/docs/connect/v3/market-quotes/`. Matters: fail-loud batching.
- **Unknown/invalid instrument in `i=` list:** silently omitted from `data` vs request-level error — paste from the same page or probe. Matters: coverage accounting (audit Rule 11 — a missing key must be counted, not assumed).
- **`ohlc.close` = previous-day close?** — paste the field table from `https://kite.trade/docs/connect/v3/market-quotes/`. Matters: any prev-close consumer (the Dhan Ticket #5525125 class of bug).
- **`timestamp`/`last_trade_time` timezone:** IST assumed, unstated on the wire — paste the field descriptions. Matters: candle bucketing off-by-19800s class.
- **`i=` with numeric instrument_token:** accepted? response keying? — paste from the same page. Matters: join-key design.
- **Over-rate (429?) wire shape:** status code + body when the 1/s quote limit is exceeded — probe or paste `https://kite.trade/docs/connect/v3/exceptions/`. Matters: backoff ladder design (Dhan DH-904 analog).
- **`trigger_range` liveness + which route shape is real:** the two official SDKs ship DIFFERENT paths (§10) and the mock exists — is the endpoint still documented/live, and which shape does the server accept? Paste: `https://kite.trade/docs/connect/v3/market-quotes/` (any trigger-range section) or probe both shapes once. Matters: completeness only (CO-legacy suspect).

## CLAIMS (for README reconciled table)

- Routes are exactly `GET /quote`, `GET /quote/ohlc`, `GET /quote/ltp` on base `https://api.kite.trade` — **Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py `_routes` lines 154–156 + line 36)** — https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/connect.py
- Instruments are passed as a REPEATED `i=` query param in `exchange:tradingsymbol` format (e.g. `NSE:INFY`) — **Verified (CLIENT-LIB-SOURCE pykiteconnect `quote()` docstring + gokiteconnect `quoteParams` url:"i")** — connect.py line 625 / market.go
- Per-request caps: 500 instruments for `/quote`; 1000 for `/quote/ohlc` and `/quote/ltp` — **SEARCH (extraction of kite.trade/docs/connect/v3/market-quotes/ + forum 15692, 2026-07-13; one conflicting "250" extraction recorded; needs live re-read)**
- Full quote carries OHLC, volume, OI (+day high/low), lower/upper circuit limits, and 5-level bid/ask depth (`{price, quantity, orders}` per level) — **Verified (OFFICIAL-MOCK quote.json + CLIENT-LIB-SOURCE gokiteconnect Quote struct)** — https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/quote.json
- Response envelope is `{"status":"success","data":{"<exchange:tradingsymbol>": …}}`, keyed by the requested instrument string — **Verified (OFFICIAL-MOCK quote.json/ohlc.json/ltp.json)**
- `/quote/ohlc` returns only `instrument_token` + `last_price` + `ohlc{o,h,l,c}`; `/quote/ltp` returns only `instrument_token` + `last_price` — **Verified (OFFICIAL-MOCK ohlc.json/ltp.json + gokiteconnect QuoteOHLC/QuoteLTP)**
- Auth = `Authorization: token <api_key>:<access_token>` + `X-Kite-Version: 3` headers on every call — **Verified (CLIENT-LIB-SOURCE connect.py lines 936–945)**
- Quote-family rate limit = 1 request/second, enforced at API-key level; no per-min/day caps for non-order endpoints — **SEARCH (kite.trade forum 8577/13397/15701 + docs exceptions page, 2026-07-13)**
- Wire timestamps are naive `"YYYY-MM-DD HH:MM:SS"` strings (SDK converts to datetime post-parse); timezone unstated — **Verified (OFFICIAL-MOCK quote.json) / Assumed (IST)**
- `ohlc.close` is the PREVIOUS session's close (mock: in-session capture with last_price ≠ close) — **Assumed** (mock-consistent, no doc statement fetched)
- `trigger_range` (CO-legacy): py route `GET /instruments/trigger_range/{transaction_type}` vs Go path-shape `/instruments/{exchange}/{tradingsymbol}/trigger_range` (constant only, no consumer); mock response `{instrument_token, lower, upper}` per instrument — **Verified (CLIENT-LIB-SOURCE both SDKs + OFFICIAL-MOCK trigger_range.json)**; liveness/deprecation **Unknown**
