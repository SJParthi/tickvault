# Zerodha Kite Connect v3 — REST Conventions, Exceptions & Rate Limits

> **Source:** https://kite.trade/docs/connect/v3/ (conventions / response structure: https://kite.trade/docs/connect/v3/response-structure/) · https://kite.trade/docs/connect/v3/exceptions/ · https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/kite-connect-api-faqs
> **Fetched-Verified:** 2026-07-13 via CLIENT-LIB-SOURCE (raw.githubusercontent.com fetch of official `zerodha/pykiteconnect@master` + shallow clone of official `zerodha/gokiteconnect@master`), OFFICIAL-MOCK (`zerodha/kiteconnect-mocks@main`), and SEARCH (WebSearch snippets of the named live kite.trade / support.zerodha.com pages). **kite.trade, support.zerodha.com, web.archive.org and r.jina.ai were ALL egress-proxy-blocked (CONNECT 403)** — no ARCHIVE-DOC or MIRROR-LIVE tier was attainable; every docs-page claim below is therefore SEARCH-tier at best and flagged for operator verbatim re-verification (OPEN QUESTIONS).
> **Evidence tiers:** see README legend — Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) / SEARCH / Assumed / Unknown.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. Any Zerodha integration requires its own operator-dated scope lock per `.claude/rules/project/websocket-connection-scope-lock.md` re-approval protocol.
> **Related:** `01-*` (auth/session), `03-*` (market quotes), `05-*` (historical), `99-UNKNOWNS.md`.

---

## 1. Base URL, versioning, mandatory headers

All facts in this section are **Verified (CLIENT-LIB-SOURCE pykiteconnect@master `kiteconnect/connect.py`, fetched 2026-07-13, https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/connect.py)**, cross-checked against **gokiteconnect@master `connect.go`** (identical values):

| Item | Value | Where in source |
|---|---|---|
| REST root | `https://api.kite.trade` | py `_default_root_uri` / go `baseURI` |
| Login root | `https://kite.zerodha.com/connect/login` | py `_default_login_uri` / go `kiteBaseURI` + `/connect/login` |
| API version header | `X-Kite-Version: 3` on EVERY request | py `kite_header_version = "3"` set in `_request`; go `kiteHeaderVersion = "3"` in `doEnvelope`/`do`/`doRaw` |
| Auth header | `Authorization: token <api_key>:<access_token>` (literal word `token`, colon-joined) | py `_request`: `"token {}".format(api_key + ":" + access_token)`; go: `fmt.Sprintf("token %s:%s", …)` |
| Version also on login URL | `?api_key=<key>&v=3` query param on the login URL | py `login_url()`; go `GetLoginURL` |
| Default client timeout | 7 seconds in BOTH official SDKs | py `_default_timeout = 7`; go `requestTimeout = 7000ms` |
| User-Agent convention | `Kiteconnect-python/<ver>` / `gokiteconnect/<ver>` | py `_user_agent()`; go `userAgent()` |

- **Versioning is header-based, not URL-based** — the REST paths carry no `/v3/` segment (**Verified (CLIENT-LIB-SOURCE pykiteconnect `_routes` dict)**: `/orders`, `/quote`, `/session/token`, …). Compare: Dhan puts the version in the URL (`https://api.dhan.co/v2/…`) and uses an `access-token` header, not `Authorization` (`docs/dhan-ref/01-introduction-and-rate-limits.md`).
- **The token never travels in a URL** on the REST surface (Verified — SDK puts it only in the `Authorization` header). (The WebSocket URL is the exception, covered in the WS section file.)

## 2. Request conventions

All **Verified (CLIENT-LIB-SOURCE pykiteconnect `connect.py::_request` + gokiteconnect `http.go::DoRaw`)**:

1. **Methods:** GET / POST / PUT / DELETE (the `_routes` usage: place=POST, modify=PUT, cancel=DELETE, reads=GET).
2. **GET/DELETE parameters go as URL query params** (py: `query_params = params` for GET/DELETE; go: `req.URL.RawQuery`).
3. **POST/PUT bodies are `application/x-www-form-urlencoded` by DEFAULT** — NOT JSON (py: `data=params` unless `is_json`; go: explicit `Content-Type: application/x-www-form-urlencoded` default). This is a material difference from Dhan and Groww, both of which POST JSON bodies.
4. **Exactly three routes POST JSON instead** (py `is_json=True` call sites): `POST /margins/orders` (order margins), `POST /margins/basket` (basket margins), `POST /charges/orders` (virtual contract note). GTT place/modify are form-encoded but with JSON-STRING field values (`condition` and `orders` fields are `json.dumps(...)` strings inside the form body — Verified, `place_gtt`).
5. **Repeated-key list params:** the quote family passes instrument lists as repeated `i=` query params (py `params={"i": ins}` with a list under requests semantics).
6. **No retry, no backoff, no rate-limiter, and no `Retry-After` handling exists in either official SDK** (**Verified-absence**, grep over `connect.py` + `gokiteconnect/*.go`: zero hits for `429`, `Retry-After`, `sleep`-based retry on the REST path). Throttling is entirely the caller's job.

## 3. Response envelope

### 3a. Success envelope

**Verified (OFFICIAL-MOCK kiteconnect-mocks@main `ltp.json` + `order_response.json` + `generate_session.json`, fetched 2026-07-13, raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/…)** — verbatim `ltp.json`:

```json
{
  "status": "success",
  "data": {
    "NSE:INFY": { "instrument_token": 408065, "last_price": 1074.35 }
  }
}
```

- Every JSON success response is `{"status": "success", "data": <payload>}`; both SDKs unwrap and return only `data` (**Verified (CLIENT-LIB-SOURCE)**: py `return data["data"]`; go `envelope{Data}` unmarshal in `http.go::readEnvelope`).
- **SEARCH (kite.trade/docs/connect/v3/response-structure/, snippets 2026-07-13):** "A successful 200 OK response always has a JSON response body with a status key with the value success. The data key contains the full response payload." Consistent with the mocks; verbatim page re-check is an OPEN QUESTION formality only.

### 3b. Error envelope

**Verified (CLIENT-LIB-SOURCE gokiteconnect `http.go::errorEnvelope`)** — the exact wire struct the official Go SDK deserializes on any HTTP ≥ 400:

```go
type errorEnvelope struct {
    Status    string      `json:"status"`     // "error"
    ErrorType string      `json:"error_type"` // exception name, e.g. "TokenException"
    Message   string      `json:"message"`    // human-readable description
    Data      interface{} `json:"data"`       // usually null
}
```

- pykiteconnect detects an error by `data.get("status") == "error" or data.get("error_type")` and raises the class named by `error_type` (**Verified (CLIENT-LIB-SOURCE pykiteconnect `connect.py::_request`)**).
- **SEARCH (response-structure page):** "A failure response is preceded by the corresponding 40x or 50x HTTP header. status contains error. message contains a textual description; error_type contains the name of the exception."
- Compare: Dhan's error body is `{errorType, errorCode, errorMessage}` — three strings, with a SEPARATE machine code (`DH-904` etc.). **Kite has NO numeric/machine error code in the body** — the `error_type` NAME + HTTP status are the whole taxonomy (Verified from both SDK parsers, which extract nothing else).

### 3c. Content types, gzip, timestamps

- Responses are `application/json` except the instrument master dumps, which are **CSV**: pykiteconnect branches on the response `Content-Type` — `"json"` → envelope parse, `"csv"` → raw bytes returned, anything else → `DataException("Unknown Content-Type …")` (**Verified (CLIENT-LIB-SOURCE pykiteconnect `_request` tail)**).
- **Gzip: "The responses may be Gzipped"** — **SEARCH** (response-structure page snippet, 2026-07-13). Neither SDK contains explicit gzip code (**Verified-absence**) because `requests`/`net/http` transparently decompress; a hand-rolled client must send `Accept-Encoding: gzip` handling or accept identity (**Assumed** — standard HTTP semantics; exact server behavior unprobed).
- **Timestamps:** "Timestamp (datetime) strings in the responses are represented in the form yyyy-mm-dd hh:mm:ss, set under the Indian timezone (IST)" — **SEARCH** (response-structure page). Corroborated by pykiteconnect, which date-parses exactly the 19-char (`len == 19`) fields `order_timestamp`, `exchange_timestamp`, `fill_timestamp`, `last_trade_time`, … (**Verified (CLIENT-LIB-SOURCE `_format_response`)**). Compare: Dhan REST historical uses UTC epoch seconds and Dhan WS uses IST epoch seconds (`docs/dhan-ref/08-annexure-enums.md` rule 13) — Kite's REST convention is IST STRINGS, a third convention.
- Historical candles return **positional arrays**, not keyed objects (`data.candles[i] = [ts, o, h, l, c, vol(, oi)]`) — Verified (CLIENT-LIB-SOURCE `_format_historical`); detailed in the historical section file.

## 4. HTTP status codes in use

| Code | Meaning (per the SDK mappings) | Tier |
|---|---|---|
| 200 | success envelope | Verified (OFFICIAL-MOCK + both SDKs) |
| 400 | `InputException` default (go maps 400→InputError both directions) | Verified (CLIENT-LIB-SOURCE both `exceptions.py` + `errors.go`) |
| 403 | `TokenException` / `PermissionException` default; go also maps 401→TokenError | Verified (CLIENT-LIB-SOURCE both) |
| 429 | rate limit exceeded ("subsequent requests will fail with error code 429") | SEARCH (kite FAQ / forum, 2026-07-13) |
| 500 | `GeneralException` / `OrderException` default (py; go gives OrderError 400 — see §5 note) | Verified (CLIENT-LIB-SOURCE both) |
| 502 | `DataException` — "a bad response from the backend Order Management System (OMS)" (py docstring; go maps DataError→504) | Verified (CLIENT-LIB-SOURCE both — WITH the 502-vs-504 SDK divergence noted) |
| 503 | `NetworkException` — "a network issue between Kite and the backend OMS" | Verified (CLIENT-LIB-SOURCE both) |
| 504 | go `GetErrorName` maps 504→NetworkError (alongside 503) | Verified (CLIENT-LIB-SOURCE gokiteconnect `errors.go`) |

## 5. Exceptions taxonomy

Docs page: https://kite.trade/docs/connect/v3/exceptions/ (proxy-blocked; taxonomy reconstructed from BOTH official SDKs, which cite that exact URL in-source — gokiteconnect `errors.go` line 9: "Check documantation to learn about individual exception: https://kite.trade/docs/connect/v3/exceptions/").

### 5a. pykiteconnect classes — Verified (CLIENT-LIB-SOURCE pykiteconnect@master `kiteconnect/exceptions.py`, fetched 2026-07-13)

| Class | Default HTTP code | Docstring (verbatim) |
|---|---|---|
| `KiteException` (base) | 500 | "Base exception class … exposes `.code` (HTTP error code) and `.message`" |
| `GeneralException` | 500 | "An unclassified, general error." |
| `TokenException` | 403 | "Represents all token and authentication related errors." |
| `PermissionException` | 403 | "Represents permission denied exceptions for certain calls." |
| `OrderException` | 500 | "Represents all order placement and manipulation errors." |
| `InputException` | 400 | "Represents user input errors such as missing and invalid parameters." |
| `DataException` | 502 | "Represents a bad response from the backend Order Management System (OMS)." |
| `NetworkException` | 503 | "Represents a network issue between Kite and the backend Order Management System (OMS)." |

### 5b. gokiteconnect `error_type` string constants — Verified (CLIENT-LIB-SOURCE gokiteconnect@master `errors.go`)

`GeneralException`, `TokenException`, `PermissionError` (sic — the Go constant's VALUE is the string `"PermissionError"`, not `"PermissionException"`), `UserException`, `TwoFAException`, `OrderException`, `InputException`, `DataException`, `NetworkException`.

- **The union of the two official SDKs therefore attests these server `error_type` values:** GeneralException, TokenException, UserException, TwoFAException, OrderException, InputException, DataException, NetworkException, plus a permission variant whose exact wire spelling the two SDKs disagree on (`PermissionException` py class vs `PermissionError` go constant) — flagged as an OPEN QUESTION.
- `UserException` / `TwoFAException` exist ONLY in the Go list — pykiteconnect has no matching class, so in Python they fall through to `GeneralException` (see 5c). **`MarginException` is additionally attested ON THE WIRE by the official mock** `autoslice_response.json` (child error `{"code": 400, "error_type": "MarginException", …}` — Verified (OFFICIAL-MOCK), see `03-orders.md` §3b/§14) even though it appears in NEITHER SDK's class/constant list — in pykiteconnect it dispatches to `GeneralException` via the unknown-name fallback. Whether the docs page lists further types (e.g. `HoldingException`) is **Unknown** — OPEN QUESTION.
- Go default-code mapping (Verified, `errors.go::NewError`): General→500, Token/Permission/User/TwoFA→403, **Order→400 and Data→504** — both DIFFERENT from pykiteconnect's Order→500 / Data→502. The real per-response code always comes from the HTTP status; the SDK defaults only matter when synthesizing errors locally.

### 5c. Dispatch semantics (how a client should behave) — all Verified (CLIENT-LIB-SOURCE)

1. **Dispatch is by body `error_type`, not by HTTP code**: py `getattr(ex, data.get("error_type"), ex.GeneralException)` — an UNKNOWN/new `error_type` name degrades safely to `GeneralException` (never a KeyError). Go `NewError` likewise defaults unknown types to `GeneralError`. A Rust port MUST keep this unknown-name fallback arm (same no-panic-on-unknown contract as `dhan/annexure-enums.md` rule 15).
2. **Session-expiry hook fires ONLY on `HTTP 403` AND `error_type == "TokenException"`** (py `_request`): the sanctioned signal for "access token is dead, re-login". `PermissionException` (also 403) deliberately does NOT fire it.
3. Go treats ANY `status >= 400` as an error envelope and parses `errorEnvelope` from the body; a non-JSON error body becomes `DataError("Error parsing response.")`.
4. Non-JSON, non-CSV success content-type → `DataException` (py).

## 6. RATE LIMITS (load-bearing — read the tier labels)

> ⚠ **Every number in this section is SEARCH-tier** (WebSearch snippets of kite.trade FAQ/forum + support.zerodha.com articles, fetched 2026-07-13). The official pages are proxy-blocked and NEITHER official SDK encodes any rate-limit constant (**Verified-absence**, grep). Operator verbatim re-verification is MANDATORY before any capacity design — see OPEN QUESTIONS. Where sources conflict, BOTH readings are recorded.

### 6a. Per-endpoint-class API limits — SEARCH (kite.trade FAQ / forum discussions 13397, 8577, 2354; snippets 2026-07-13)

| Endpoint class | Limit |
|---|---|
| Quote (`/quote`, `/quote/ohlc`, `/quote/ltp`) | **1 req/second** |
| Historical candle (`/instruments/historical/...`) | **3 req/second** |
| Order placement (`POST /orders/{variety}`) | **10 req/second** |
| All other endpoints | **10 req/second** |

- Scope: limits are widely described as **per `api_key`** (SEARCH, forum: "Combined requests from a unique API key shouldn't exceed 10 req/sec"; "The 3 connections limit is per api_key and not based on user"). Per-key-vs-per-user pooling nuance is an OPEN QUESTION (same class as Groww's U-5).
- "There is no cap on the maximum number of requests per day for any API excluding order placement" — SEARCH (kite FAQ snippet). Contrast BOTH comparators: Dhan Data APIs carry a 100,000/day cap; Groww documents no per-day column either.

### 6b. Order-count caps (RMS layer, distinct from the HTTP rate limit) — SEARCH, with recorded conflicts

| Cap | Reading A | Reading B | Sources (SEARCH 2026-07-13) |
|---|---|---|---|
| Orders per second | 10/s | — (consistent) | kite FAQ + support.zerodha.com order-rate-limits article |
| Orders per minute | **200/min** (kite.trade FAQ-era figure) | **400/min** (support.zerodha.com "order rate limits on Kite" article) | both snippets surfaced 2026-07-13. **Recency weighting (adversarial re-source 2026-07-13, ×3 passes):** the 400/min figure appears in the support article that explicitly covers the API-key level AND in a current exceptions-docs extraction; the 200/min figure is attributed to forum-era/older documentation and flagged as outdated in the same extraction set. 200/min appears to be the SUPERSEDED figure; 400/min the current one — operator paste still mandatory, OPEN QUESTION |
| Orders per day | **3,000/day** (forum "Changes to per day order limits", discussion/12851: "daily order limit is 3000 orders max … contact RMS to increase for the day" — a change "effective July 22, 2023") | **5,000/day** (support.zerodha.com kite-connect-api-faqs: "a single user/API key will not be able to place more than 5000 orders per day, … across all segments and varieties") | plausibly a limit raise over time (the Groww 15→10/s precedent shows these move). **Recency weighting:** the 2026-07 extraction passes consistently return 5,000 as the live-FAQ figure and tie 3,000 to the 2023-era change — 5,000 is the likelier CURRENT number; which is live is still an OPEN QUESTION |
| Modifications per order | **25 max**, then "Maximum allowed order modifications exceeded" | — (consistent) | support.zerodha.com order-modification-failed article |

Day-cap semantics (all SEARCH, forum discussion/12851): the cap is described as **Zerodha ACCOUNT-level, counting ALL platforms** (Kite web/app + API) per forum 12851 — but note a wording tension: the live support-FAQ phrase is "**a single user/API key**" (per-key phrasing). Both scopes appear in sources; treat the pooling scope as unresolved (folded into the pooling-scope OPEN QUESTION). **RMS-blocked order requests still count**; an iceberg with 5 legs counts as 5; a GTT trigger's resulting order counts as 1; **modify / cancel / GTT-place calls do NOT count** toward the day cap.

Compare: Dhan Order APIs = 10/s, 250/min, 1,000/hr, **7,000/day**, 25 modifications per order, per `dhanClientId` (`docs/dhan-ref/01-introduction-and-rate-limits.md`). Groww Orders = 10/s, 250/min, NO per-day column (`docs/groww-ref/15-rate-limits-and-capacity.md` §1). Kite's 1/s quote limit is the TIGHTEST quote budget of the three (Dhan Quote REST = 1/s too; Groww Live Data = 10/s + 300/min) and its 3/s historical class has no Dhan/Groww analog (both leave historical unassigned or under Data APIs).

### 6c. Per-call instrument caps on the quote family — SEARCH (kite.trade/docs/connect/v3/market-quotes/ snippets, 2026-07-13)

- `/quote` (full): **up to 500 instruments** per request.
- `/quote/ohlc` and `/quote/ltp`: **up to 1000 instruments** per request.
- Practical wart (SEARCH, forum): instruments ride as repeated `i=` GET query params, so URL-length limits bite before 1000 on long `exchange:tradingsymbol` names (~825 reported); batch lower or use instrument tokens.
- Whether one 500-instrument call bills as 1 request against the 1/s quote limit is not stated in any fetched snippet (**Assumed 1** — it is one HTTP request; same assumption class as Groww's U-9).

### 6d. WebSocket caps — SEARCH (kite.trade/docs/connect/v3/websocket/ + forum discussions 11179, 15708, 3620; snippets 2026-07-13)

- **3 WebSocket connections per `api_key`** (not per user). **Enforcement character (SEARCH, forum 11179-class threads):** described by forum staff as a **SOFT limit** — "currently there is a soft limit of up to 3 WebSocket connections per API account… repeatedly breaching these limits or abnormal usage patterns may lead to the application being flagged / temporary suspension of the app." Capacity designs must treat 3 as the ceiling AND respect the abuse-flagging penalty class (the Groww adaptive-penalty lesson, `docs/groww-ref/15-…` §5).
- **3,000 instruments (tokens) per connection** → 9,000 across the 3 connections.
- Over-subscription behavior (SEARCH, forum 15708): tokens beyond 3,000 draw an error for the excess; the existing connection stays alive.
- Compare: Dhan allows 5 WS connections × 5,000 instruments (`docs/dhan-ref/01-introduction-and-rate-limits.md` / `03-live-market-feed-websocket.md`); Groww documents "upto 1000 subscriptions" with per-connection-vs-account unstated, plus a MEASURED undocumented adaptive connection cap (`docs/groww-ref/15-rate-limits-and-capacity.md` §1, §5).

### 6e. 429 / throttle behavior

- Exceeding a limit → **HTTP 429** (SEARCH, kite FAQ "subsequent requests will fail with error code 429"; forum threads "Solution for error code-429", "Error 429: Too many requests\Network exception").
- The 429 body's `error_type` is suggested by forum thread titles to surface as `NetworkException` ("Too many requests"; a second independent title-level corroboration: forum 11564 "kiteconnect.exceptions.NetworkException: Too many requests") — **Assumed**, no fetched verbatim body; the raw 429 wire shape (headers incl. any `Retry-After`, body JSON) is an OPEN QUESTION (probe class — same as Groww U-7).
- **Neither official SDK retries, backs off, or reads `Retry-After`** (Verified-absence, §2.6) — a 429 simply raises. Any Kite client must own its own pacing; the Dhan DH-904 exponential-ladder discipline (`dhan-ref` rule 8) is the house pattern to mirror.
- No ban/cooldown/fair-use policy language surfaced in any fetched snippet (**Unknown** — absence of evidence only; the Groww WS adaptive-penalty precedent, measured 2026-07-06, mandates live-probe humility here too).

## 7. Compare: Dhan / Groww (one-glance)

| Dimension | Kite Connect v3 (this file) | Dhan v2 (`docs/dhan-ref/01-…`) | Groww (`docs/groww-ref/15-…`) |
|---|---|---|---|
| Auth header | `Authorization: token key:token` + `X-Kite-Version: 3` | `access-token` (+ `client-id` on some) | `Authorization: Bearer` + `x-api-version: 1.0` |
| Version | header-based | URL `/v2/` | URL `/v1/` |
| POST body | form-urlencoded (3 JSON exceptions) | JSON | JSON |
| Error body | `{status:"error", error_type, message, data}` — name-only taxonomy | `{errorType, errorCode, errorMessage}` — machine codes (DH-9xx/8xx) | HTTP-status-mapped SDK exceptions |
| Rate-limit model | per-endpoint-CLASS req/s (1/3/10) + RMS order counts | per-category /s /min /hr /day table | 3 type-pooled buckets /s + /min |
| Quote budget | 1/s (500–1000 instruments/call) | Quote APIs 1/s (1000 instruments/call) | Live Data 10/s · 300/min (50/call) |
| Order day cap | 3,000 or 5,000 (conflicting sources) | 7,000/day | none documented |
| WS caps | 3 conns × 3,000 tokens | 5 conns × 5,000 SIDs | 1000 subscriptions; adaptive conn cap (measured) |

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Rate-limit table verbatim** — open https://kite.trade/docs/connect/v3/exceptions/ (the "API rate limits" anchor) and paste the table verbatim. Every §6a number is SEARCH-tier; a capacity design against regurgitated snippets repeats the Groww stale-15/s hazard documented in `groww-ref/15` §2. **Blocking: capacity.**
- **Orders/minute: 200 or 400?** — open https://support.zerodha.com/category/trading-and-markets/alerts-and-nudges/kite-error-messages/articles/order-rate-limits-on-kite AND the kite.trade FAQ; paste both. Determines burst pacing for any order engine. **Blocking: capacity (order path only).**
- **Orders/day: 3,000 or 5,000 (current)?** — open https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/kite-connect-api-faqs and https://kite.trade/forum/discussion/12851/changes-to-per-day-order-limits; paste the current figures + dates. **Blocking: capacity (order path only).**
- **Exceptions page full taxonomy** — open https://kite.trade/docs/connect/v3/exceptions/ and paste the exception table verbatim: is `MarginException` (mock-attested on the wire, in neither SDK) documented there, and does `HoldingException` exist? Is the permission type's wire spelling `PermissionException` or `PermissionError` (the two official SDKs disagree)? **Blocking: none (fallback-to-GeneralException absorbs unknowns).**
- **429 wire shape** — first live 429: capture status headers (any `Retry-After`?) + raw JSON body (`error_type` value — NetworkException?). Probe-class. **Blocking: ops.**
- **Rate-limit pooling scope** — per api_key vs per user/account when one account holds multiple API keys; forum answers say per-key, docs verbatim unconfirmed. Paste from the FAQ. **Blocking: capacity (multi-app only).**
- **Gzip** — response-structure page verbatim ("The responses may be Gzipped") + whether `Accept-Encoding: identity` is honored: https://kite.trade/docs/connect/v3/ (response structure section). **Blocking: none.**

## CLAIMS (for README reconciled table)

- REST root `https://api.kite.trade`; auth = `Authorization: token api_key:access_token` + `X-Kite-Version: 3` header on every request; version is header-based, no `/v3/` in paths — **Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py + gokiteconnect connect.go, fetched 2026-07-13)** — raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/connect.py
- Success envelope `{"status":"success","data":…}`; SDKs return only `data` — **Verified (OFFICIAL-MOCK ltp.json/order_response.json + CLIENT-LIB-SOURCE both SDKs)** — raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/ltp.json
- Error envelope `{"status":"error","error_type":…,"message":…,"data":…}`; dispatch is by `error_type` NAME with unknown names degrading to GeneralException; no numeric machine code exists (vs Dhan's DH-9xx) — **Verified (CLIENT-LIB-SOURCE gokiteconnect http.go errorEnvelope + pykiteconnect connect.py _request)**
- POST/PUT bodies are form-urlencoded by default; only `/margins/orders`, `/margins/basket`, `/charges/orders` POST JSON — **Verified (CLIENT-LIB-SOURCE pykiteconnect is_json call sites + gokiteconnect http.go default Content-Type)**
- Exception classes + default codes: General(500)/Token(403)/Permission(403)/Order(500)/Input(400)/Data(502)/Network(503); Go adds UserException + TwoFAException `error_type` strings; `MarginException` is mock-attested on the wire (autoslice child error) but in NEITHER SDK — **Verified (CLIENT-LIB-SOURCE pykiteconnect exceptions.py + gokiteconnect errors.go + OFFICIAL-MOCK autoslice_response.json)**
- Session-expiry hook contract: fires only on HTTP 403 + `error_type == "TokenException"` — **Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py)**
- Instrument dumps are served as CSV content-type (SDK branches json/csv/other→DataException) — **Verified (CLIENT-LIB-SOURCE pykiteconnect _request)**
- API rate limits: Quote 1/s · Historical 3/s · Order placement 10/s · everything else 10/s, per api_key; no daily cap outside order placement — **SEARCH** (kite.trade FAQ/forum snippets 2026-07-13: kite.trade/forum/discussion/13397/rate-limits et al.) — verbatim re-check mandatory
- Order RMS caps: 10/s; 200/min vs 400/min (conflicting — 400 = the currently-extracted figure, 200 = superseded FAQ-era per 2026-07-13 re-sourcing ×3); 3,000/day (forum 12851, tied to a 2023 change) vs 5,000/day (live support FAQ — the likelier current figure); pooling scope account-level (forum) vs "single user/API key" (FAQ wording) unresolved; 25 modifications per order; blocked requests count, modify/cancel don't — **SEARCH** (support.zerodha.com + kite.trade forum snippets 2026-07-13)
- Quote-family per-call caps: 500 instruments on `/quote`, 1000 on `/quote/ohlc` + `/quote/ltp` — **SEARCH** (kite.trade/docs/connect/v3/market-quotes/ snippets 2026-07-13)
- WebSocket caps: 3 connections per api_key (a SOFT limit with abuse-flagging per forum staff) × 3,000 instruments per connection — **SEARCH** (kite.trade docs/forum snippets 2026-07-13)
- Exceeding limits → HTTP 429; neither official SDK retries/backs off/reads Retry-After (caller-owned pacing) — **SEARCH** (429) + **Verified-absence (CLIENT-LIB-SOURCE grep, both SDKs)**
- Timestamps in REST responses are IST strings `yyyy-mm-dd hh:mm:ss` (19-char), parsed as such by pykiteconnect — **SEARCH (response-structure page) + Verified (CLIENT-LIB-SOURCE _format_response len==19 parse)**
- Responses may be gzipped — **SEARCH** (response-structure page snippet 2026-07-13); no explicit SDK handling (transparent decompression)
