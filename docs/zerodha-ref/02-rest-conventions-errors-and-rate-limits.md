# Zerodha Kite Connect v3 — REST Conventions, Exceptions & Rate Limits

> **Source:** https://kite.trade/docs/connect/v3/ (introduction) · https://kite.trade/docs/connect/v3/response-structure/ · https://kite.trade/docs/connect/v3/exceptions/ (carries the "API rate limit" table) · https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/kite-connect-api-faqs · https://support.zerodha.com/category/trading-and-markets/alerts-and-nudges/kite-error-messages/articles/order-rate-limits-on-kite · kite.trade forum discussions 2354 / 8577 / 12851 / 13397 / 15708
> **Fetched-Verified:** 2026-07-13 via CLIENT-LIB-SOURCE (raw.githubusercontent.com fetch of official `zerodha/pykiteconnect@master` + shallow clone of official `zerodha/gokiteconnect@master`) and OFFICIAL-MOCK (`zerodha/kiteconnect-mocks@main`); **upgraded 2026-07-14 live-verbatim via GH-runner (UTC timestamps in 00-COVERAGE-MANIFEST.md)** — the previously proxy-blocked kite.trade + support.zerodha.com pages were fetched raw by the LIVE-DOC runner (147× HTTP 200, sha256+timestamp per URL in the crawl manifest), so every former SEARCH-tier docs claim below has been re-checked against the live page and re-tiered.
> **Evidence tiers:** see README legend — **Verified (LIVE-DOC runner 2026-07-14)** [top tier: confirmed against the runner-fetched live page] / Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) / SEARCH / Assumed / Unknown. Live forum threads are cited as LIVE-DOC (forum) — the PAGE is verbatim-verified, but a forum answer (even staff) is weaker authority than the docs/support pages.
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

Items 1–6 **Verified (CLIENT-LIB-SOURCE pykiteconnect `connect.py::_request` + gokiteconnect `http.go::DoRaw`)**; items 2–3 ADDITIONALLY **Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/response-structure/)** — live verbatim: "All GET and DELETE request parameters go as query parameters, and POST and PUT parameters as form-encoded (application/x-www-form-urlencoded) parameters, responses from the API are always JSON."

1. **Methods:** GET / POST / PUT / DELETE (the `_routes` usage: place=POST, modify=PUT, cancel=DELETE, reads=GET).
2. **GET/DELETE parameters go as URL query params** (py: `query_params = params` for GET/DELETE; go: `req.URL.RawQuery`).
3. **POST/PUT bodies are `application/x-www-form-urlencoded` by DEFAULT** — NOT JSON (py: `data=params` unless `is_json`; go: explicit `Content-Type: application/x-www-form-urlencoded` default). This is a material difference from Dhan and Groww, both of which POST JSON bodies. The live introduction page (LIVE-DOC, https://kite.trade/docs/connect/v3/): "All inputs are form-encoded parameters and responses are JSON (apart from a couple exceptions)."
4. **Exactly three routes POST JSON instead** (py `is_json=True` call sites): `POST /margins/orders` (order margins), `POST /margins/basket` (basket margins), `POST /charges/orders` (virtual contract note). GTT place/modify are form-encoded but with JSON-STRING field values (`condition` and `orders` fields are `json.dumps(...)` strings inside the form body — Verified, `place_gtt`).
5. **Repeated-key list params:** the quote family passes instrument lists as repeated `i=` query params (py `params={"i": ins}` with a list under requests semantics).
6. **No retry, no backoff, no rate-limiter, and no `Retry-After` handling exists in either official SDK** (**Verified-absence**, grep over `connect.py` + `gokiteconnect/*.go`: zero hits for `429`, `Retry-After`, `sleep`-based retry on the REST path). Throttling is entirely the caller's job.

## 3. Response envelope

### 3a. Success envelope

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/response-structure/)** — live verbatim: "All responses from the API server are JSON with the content-type application/json unless explicitly stated otherwise. A successful 200 OK response always has a JSON response body with a status key with the value success. The data key contains the full response payload." Cross-checked **Verified (OFFICIAL-MOCK kiteconnect-mocks@main `ltp.json` + `order_response.json` + `generate_session.json`, fetched 2026-07-13)** — verbatim `ltp.json`:

```json
{
  "status": "success",
  "data": {
    "NSE:INFY": { "instrument_token": 408065, "last_price": 1074.35 }
  }
}
```

- Every JSON success response is `{"status": "success", "data": <payload>}`; both SDKs unwrap and return only `data` (**Verified (CLIENT-LIB-SOURCE)**: py `return data["data"]`; go `envelope{Data}` unmarshal in `http.go::readEnvelope`).

### 3b. Error envelope

**Verified (LIVE-DOC runner 2026-07-14, response-structure/ + exceptions/ pages)** — live verbatim example (identical on both pages):

```
HTTP/1.1 500 Server error
Content-Type: application/json

{
    "status": "error",
    "message": "Error message",
    "error_type": "GeneralException"
}
```

Live verbatim: "A failure response is preceded by the corresponding 40x or 50x HTTP header. The status key in the response envelope contains the value error. The message key contains a textual description of the error and error_type contains the name of the exception. **There may be an optional data key with additional payload.**"

- Cross-check **Verified (CLIENT-LIB-SOURCE gokiteconnect `http.go::errorEnvelope`)** — the exact wire struct the official Go SDK deserializes on any HTTP ≥ 400:

```go
type errorEnvelope struct {
    Status    string      `json:"status"`     // "error"
    ErrorType string      `json:"error_type"` // exception name, e.g. "TokenException"
    Message   string      `json:"message"`    // human-readable description
    Data      interface{} `json:"data"`       // usually null — "optional" per the live page
}
```

- pykiteconnect detects an error by `data.get("status") == "error" or data.get("error_type")` and raises the class named by `error_type` (**Verified (CLIENT-LIB-SOURCE pykiteconnect `connect.py::_request`)**).
- Compare: Dhan's error body is `{errorType, errorCode, errorMessage}` — three strings, with a SEPARATE machine code (`DH-904` etc.). **Kite has NO numeric/machine error code in the body** — the `error_type` NAME + HTTP status are the whole taxonomy (Verified from both SDK parsers AND the live exceptions page, which documents only exception names: "error responses come with the name of the exception generated internally by the API server … raise them by doing a switch on the returned exception name").

### 3c. Content types, gzip, timestamps

- Responses are `application/json` except the instrument master dumps, which are **CSV**: pykiteconnect branches on the response `Content-Type` — `"json"` → envelope parse, `"csv"` → raw bytes returned, anything else → `DataException("Unknown Content-Type …")` (**Verified (CLIENT-LIB-SOURCE pykiteconnect `_request` tail)**). Live response-structure page: "responses … are always JSON … unless explicitly stated otherwise" (LIVE-DOC).
- **Gzip: "The responses may be Gzipped"** — **Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/ — the INTRODUCTION page)**. CORRECTION vs the 2026-07-13 draft: the earlier SEARCH snippet attributed this sentence to the response-structure page; the live response-structure page does NOT carry it — it lives on the introduction page ("All inputs are form-encoded parameters and responses are JSON (apart from a couple exceptions). The responses may be Gzipped."). Neither SDK contains explicit gzip code (**Verified-absence**) because `requests`/`net/http` transparently decompress; a hand-rolled client must handle `Accept-Encoding: gzip` or accept identity (**Assumed** — standard HTTP semantics; exact server behavior unprobed).
- **Timestamps:** **Verified (LIVE-DOC runner 2026-07-14, response-structure/ "Data types")** — live verbatim: "Values in JSON responses are of types string, int, float, or bool. Timestamp (datetime) strings in the responses are represented in the form yyyy-mm-dd hh:mm:ss, set under the Indian timezone (IST) — UTC+5.5 hours. A date string is represented in the form yyyy-mm-dd." Corroborated by pykiteconnect, which date-parses exactly the 19-char (`len == 19`) fields `order_timestamp`, `exchange_timestamp`, `fill_timestamp`, `last_trade_time`, … (**Verified (CLIENT-LIB-SOURCE `_format_response`)**). Compare: Dhan REST historical uses UTC epoch seconds and Dhan WS uses IST epoch seconds (`docs/dhan-ref/08-annexure-enums.md` rule 13) — Kite's REST convention is IST STRINGS, a third convention.
- Historical candles return **positional arrays**, not keyed objects (`data.candles[i] = [ts, o, h, l, c, vol(, oi)]`) — Verified (CLIENT-LIB-SOURCE `_format_historical`); detailed in the historical section file.

## 4. HTTP status codes in use

The live exceptions page carries a "Common HTTP error codes" table — **Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/exceptions/)**, live meanings verbatim:

| Code | Live docs meaning (verbatim) | SDK mapping cross-check | Tier |
|---|---|---|---|
| 200 | success envelope | both SDKs + mocks | Verified (LIVE-DOC + OFFICIAL-MOCK + both SDKs) |
| 400 | "Missing or bad request parameters or values" | `InputException` default (go maps 400→InputError both directions) | Verified (LIVE-DOC + CLIENT-LIB-SOURCE both) |
| 403 | "Session expired or invalidate. Must relogin" | `TokenException` / `PermissionException` default; go also maps 401→TokenError | Verified (LIVE-DOC + CLIENT-LIB-SOURCE both) |
| 404 | "Request resource was not found" | (no dedicated SDK class — GeneralException fallback) | Verified (LIVE-DOC) |
| 405 | "Request method (GET, POST etc.) is not allowed on the requested endpoint" | (fallback) | Verified (LIVE-DOC) |
| 410 | "The requested resource is gone permanently" | (fallback) | Verified (LIVE-DOC) |
| 429 | "Too many requests to the API (rate limiting)" | no SDK handling (Verified-absence, §2.6) | Verified (LIVE-DOC) |
| 500 | "Something unexpected went wrong" | `GeneralException` / `OrderException` default (py; go gives OrderError 400 — see §5 note) | Verified (LIVE-DOC + CLIENT-LIB-SOURCE both) |
| 502 | "The backend OMS is down and the API is unable to communicate with it" | py `DataException` default 502 (docstring says "bad response from OMS" — slightly different framing); go maps DataError→504 | Verified (LIVE-DOC + CLIENT-LIB-SOURCE — WITH the 502-vs-504 SDK divergence noted) |
| 503 | "Service unavailable; the API is down" | `NetworkException` default 503 | Verified (LIVE-DOC + CLIENT-LIB-SOURCE both) |
| 504 | "Gateway timeout; the API is unreachable" | go `GetErrorName` maps 504→NetworkError (alongside 503) | Verified (LIVE-DOC + CLIENT-LIB-SOURCE gokiteconnect `errors.go`) |

## 5. Exceptions taxonomy

Docs page: https://kite.trade/docs/connect/v3/exceptions/ — **now fetched live (LIVE-DOC runner 2026-07-14)**; both official SDKs cite this exact URL in-source (gokiteconnect `errors.go` line 9).

### 5a. The DOCUMENTED taxonomy — Verified (LIVE-DOC runner 2026-07-14, exceptions/ page, descriptions verbatim)

The live page documents exactly NINE exception types:

| `error_type` (wire name) | Live docs description (verbatim) |
|---|---|
| `TokenException` | "Preceded by a 403 header, this indicates the expiry or invalidation of an authenticated session. This can be caused by the user logging out, a natural expiry, or the user logging into another Kite instance. When you encounter this error, you should clear the user's session and re-initiate a login." |
| `UserException` | "Represents user account related errors" |
| `OrderException` | "Represents order related errors such placement failures, a corrupt fetch etc" |
| `InputException` | "Represents missing required fields, bad values for parameters etc." |
| `MarginException` | "Represents insufficient funds, required for the order placement" |
| `HoldingException` | "Represents insufficient holdings, available to place sell order for specified instrument" |
| `NetworkException` | "Represents a network error where the API was unable to communicate with the OMS (Order Management System)" |
| `DataException` | "Represents an internal system error where the API was unable to understand the response from the OMS to inturn respond to a request" |
| `GeneralException` | "Represents an unclassified error. This should only happen rarely" |

**Resolutions vs the 2026-07-13 SDK-only reconstruction:**
- `MarginException` **IS documented** (previously mock-attested only — the `autoslice_response.json` child error; see `03-orders.md` §3b/§14). RESOLVED.
- `HoldingException` **EXISTS and is documented** (previously Unknown). RESOLVED.
- **NO permission type appears on the live page at all** — neither `PermissionException` (the py class) nor `PermissionError` (the go constant). The permission variant is SDK-attested only and its wire spelling remains unresolved (low stakes: both SDKs' unknown-name fallback absorbs it; see 5c).
- `TwoFAException` (go constant) is also NOT documented on the live exceptions page — it belongs to the login/session surface (see `01-*`).

### 5b. SDK class/constant lists (cross-check evidence)

**pykiteconnect classes — Verified (CLIENT-LIB-SOURCE pykiteconnect@master `kiteconnect/exceptions.py`, fetched 2026-07-13):**

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

**gokiteconnect `error_type` string constants — Verified (CLIENT-LIB-SOURCE gokiteconnect@master `errors.go`):** `GeneralException`, `TokenException`, `PermissionError` (sic), `UserException`, `TwoFAException`, `OrderException`, `InputException`, `DataException`, `NetworkException`.

- Note the SDK lists LAG the live docs both ways: pykiteconnect has classes for a permission type the docs don't list, and NEITHER SDK has a class/constant for the documented `MarginException` / `HoldingException` — in pykiteconnect those dispatch to `GeneralException` via the unknown-name fallback (5c). A Rust port should define ALL NINE documented types + tolerate undocumented extras.
- Go default-code mapping (Verified, `errors.go::NewError`): General→500, Token/Permission/User/TwoFA→403, **Order→400 and Data→504** — both DIFFERENT from pykiteconnect's Order→500 / Data→502. The real per-response code always comes from the HTTP status; the SDK defaults only matter when synthesizing errors locally.

### 5c. Dispatch semantics (how a client should behave) — all Verified (CLIENT-LIB-SOURCE)

1. **Dispatch is by body `error_type`, not by HTTP code**: py `getattr(ex, data.get("error_type"), ex.GeneralException)` — an UNKNOWN/new `error_type` name degrades safely to `GeneralException` (never a KeyError). Go `NewError` likewise defaults unknown types to `GeneralError`. A Rust port MUST keep this unknown-name fallback arm (same no-panic-on-unknown contract as `dhan/annexure-enums.md` rule 15). The live page endorses the switch-on-name pattern (LIVE-DOC: "raise them by doing a switch on the returned exception name").
2. **Session-expiry hook fires ONLY on `HTTP 403` AND `error_type == "TokenException"`** (py `_request`): the sanctioned signal for "access token is dead, re-login". The live TokenException description confirms the semantics ("clear the user's session and re-initiate a login" — LIVE-DOC).
3. Go treats ANY `status >= 400` as an error envelope and parses `errorEnvelope` from the body; a non-JSON error body becomes `DataError("Error parsing response.")`.
4. Non-JSON, non-CSV success content-type → `DataException` (py).

## 6. RATE LIMITS

> The 2026-07-13 draft carried this section as SEARCH-tier with two deliberately-unresolved conflicts (orders/min 200-vs-400; orders/day 3,000-vs-5,000). **The 2026-07-14 live fetch SETTLES both** — every number below is now cited against the runner-fetched live page. Neither official SDK encodes any rate-limit constant (**Verified-absence**, grep), so the docs/support pages are the sole authority.

### 6a. Official per-endpoint-class API rate-limit table — Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/exceptions/ "API rate limit")

Live table verbatim:

| end-point | rate-limit |
|---|---|
| Quote | **1req/second** |
| Historical candle | **3req/second** |
| Order placement | **10req/second** |
| All other endpoints | **10req/second** |

The table is immediately followed by this live note (verbatim): "There are limitations at 400 orders per minute and 10 orders per second. As a risk management measure, at Zerodha, a single user/API key will not be able to place more than 5000 orders per day. This restriction is across all segments and varieties. Rate limitations also apply for order modification where a maximum of 25 modifications are allowed per order. Post that user has to cancel the order and place it again."

- **Scope of the req/s limits:** the general 10 req/s is a COMBINED count per API key, not per-endpoint — **Verified (LIVE-DOC (forum) runner 2026-07-14, https://kite.trade/forum/discussion/8577/api-rate-limits, staff answers)**: sujith (Sep 2020) "It is not a restriction per API call. It is a combined count."; rakeshr (Sep 2020) "combined requests from a unique API key shouldn't exceed 10 req/sec." The ORDER-specific pooling scope is different — see 6b.
- **No per-day cap outside order placement** — **Verified (LIVE-DOC (forum) runner 2026-07-14, https://kite.trade/forum/discussion/13397/rate-limits, staff sujith, Nov 2023)**: "There are no per minute or per day limits for any endpoints except for order placement API." (The docs pages themselves publish no daily column for non-order endpoints.) Contrast BOTH comparators: Dhan Data APIs carry a 100,000/day cap; Groww documents no per-day column either.

### 6b. Order-count caps (RMS layer, distinct from the HTTP rate limit) — SETTLED 2026-07-14

| Cap | Settled value | Citations (LIVE-DOC runner 2026-07-14) |
|---|---|---|
| Orders per second | **10/s, per client (trading) ACCOUNT, not per app; excess → HTTP 429** | kite-connect-api-faqs support article, verbatim: "The maximum allowed limit is 10 orders per second (OPS) per client (trading) account. Requests beyond this are rejected with a 429 HTTP response code. The limit applies per client account, not per app." + exceptions/ docs note |
| Orders per minute | **400/min** | exceptions/ docs note ("limitations at 400 orders per minute") + order-rate-limits-on-kite support article ("There is also a limitation of 400 orders per minute") |
| Orders per day | **5,000/day** | exceptions/ docs note ("a single user/API key will not be able to place more than 5000 orders per day … across all segments and varieties") + order-rate-limits-on-kite support article ("you cannot place more than 5000 orders per day at Zerodha. This restriction applies across all segments") |
| Modifications per order | **25 max**, then cancel-and-re-place | exceptions/ docs note + order-rate-limits-on-kite support article ("a maximum of 25 modifications per order") |

**Previously conflicting (record kept for the 99-UNKNOWNS rebuilder):**
- *Orders/min 200-vs-400:* **RESOLVED by live fetch — 400/min is the current official figure** (live docs + live support article, both 2026-07-14). 200/min was the docs figure as of Nov 2023 (a user quotes it verbatim in forum 13397) and still echoes in forum replies as late as Jul 2025 (forum 8577: "200 orders per minute is the order placement limit") — treat any 200/min sighting as the superseded figure.
- *Orders/day 3,000-vs-5,000:* **RESOLVED by live fetch — 5,000/day is the current official figure** (live docs + live support articles, 2026-07-14). 3,000/day was the account-level limit announced effective 2023-07-22 (forum 12851, staff rakeshr: "the number of orders per account will be limited to 3000 across all varieties" — itself unifying the older 3,000-regular + 2,000-cover split). The 3,000→5,000 raise date is unannounced in any fetched page (**Unknown** — the live pages simply state 5,000).
- *Pooling scope (per-key vs per-account):* **substantially RESOLVED — order limits pool at the Zerodha CLIENT-ACCOUNT level, across ALL platforms.** The FAQ is explicit for the 10/s OPS limit ("per client account, not per app" — LIVE-DOC). For the day cap, forum staff repeatedly state account-level counting across platforms (LIVE-DOC (forum) 12851: sujith Jul 2023 "It is a Zerodha account level restriction"; sujith Apr 2024 "It includes the requests made from all the platforms. It is the Zerodha account level limit."; Nov 2024 reply: Kite web/app orders count too), and the support article says "at Zerodha". The docs-page phrase "a single user/API key" is the loosest wording but is consistent with account-level pooling (one user = one account). Residual nuance: the staff account-level statements date from the 3,000-era; no post-raise page re-states the scope in one sentence — flagged, low risk.

**Day-cap semantics — Verified (LIVE-DOC (forum) runner 2026-07-14, forum 12851 staff answers + the two live support articles):**
- **RMS-blocked / invalid order requests still count** — support article why-am-i-getting-the-error-maximum-allowed-order-request-exceeded (LIVE-DOC): "the maximum order request limit counts both successful orders and blocked invalid requests. Invalid orders don't appear in your order book but still count"; forum 12851 sujith (Jan 2024): counts include "orders that don't show up in the orderbook but rejected by our mini RMS with 400 http status code", pre/post-market included.
- **Iceberg legs:** toward the DAY cap, 5 legs = 5 orders (forum 12851 staff, Jan 2024). Toward the per-second OPS limit, the live FAQ says "Iceberg orders count as a single order, regardless of the number of legs" — DIFFERENT counters, both recorded; do not conflate.
- **A GTT trigger's resulting order counts as 1**; multiple fills of one order = 1 order (forum 12851 staff).
- **Modify / cancel / GTT-place calls do NOT count** toward the day cap (forum 12851 sujith, Apr 2024: "The order limit will only consider the order place request only and not modify or cancel or place GTT API calls.").
- **Once the day cap is hit you cannot even exit positions** — contact Zerodha RMS/support to raise the limit for the day (forum 12851 sujith, Jul 2023; support article: "Reach out to Zerodha's support desk if you have any difficulty in exiting open positions due to order limitations").

Compare: Dhan Order APIs = 10/s, 250/min, 1,000/hr, **7,000/day**, 25 modifications per order, per `dhanClientId` (`docs/dhan-ref/01-introduction-and-rate-limits.md`). Groww Orders = 10/s, 250/min, NO per-day column (`docs/groww-ref/15-rate-limits-and-capacity.md` §1). Kite's 1/s quote limit is the TIGHTEST quote budget of the three (Dhan Quote REST = 1/s too; Groww Live Data = 10/s + 300/min) and its 3/s historical class has no Dhan/Groww analog (both leave historical unassigned or under Data APIs).

### 6c. Per-call instrument caps on the quote family — Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/market-quotes/)

- `/quote` (full): **up to 500 instruments** per request (live verbatim: "the complete market data snapshot of up to 500 instruments in one go").
- `/quote/ohlc`: **up to 1000 instruments** ("OHLC + LTP snapshots of up to 1000 instruments in one go").
- `/quote/ltp`: **up to 1000 instruments** ("LTPs of up to 1000 instruments in one go").
- Practical wart (SEARCH, forum): instruments ride as repeated `i=` GET query params, so URL-length limits bite before 1000 on long `exchange:tradingsymbol` names (~825 reported); batch lower or use instrument tokens.
- Whether one 500-instrument call bills as 1 request against the 1/s quote limit is not stated on the live page either (**Assumed 1** — it is one HTTP request; same assumption class as Groww's U-9).

### 6d. WebSocket caps — Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/websocket/ + forum 15708)

- **3 WebSocket connections per `api_key`** × **3,000 instruments per connection** (→ 9,000 across the 3) — live docs verbatim: "You can subscribe for up to 3000 instruments on a single WebSocket connection and receive live quotes for them. Single API key can have upto 3 websocket connections."
- **Enforcement character: SOFT limit with abuse flagging** — **Verified (LIVE-DOC (forum) runner 2026-07-14, https://kite.trade/forum/discussion/15708/, staff salim_chisty, Dec 2025–Jan 2026)**: "Currently, there is a soft limit of up to 3 WebSocket connections per API account. … repeatedly breaching these limits or exhibiting abnormal usage patterns may lead to the application being flagged, which can potentially result in the temporary suspension of the app." Capacity designs must treat 3 as the ceiling AND respect the abuse-flagging penalty class (the Groww adaptive-penalty lesson, `docs/groww-ref/15-…` §5).
- Over-subscription behavior — same thread (LIVE-DOC (forum)): "If a subscription request exceeds the 3,000-instrument limit on a single connection, the API will return an error for the additional tokens. However, the existing WebSocket connection will remain active and unaffected."
- Compare: Dhan allows 5 WS connections × 5,000 instruments (`docs/dhan-ref/01-introduction-and-rate-limits.md` / `03-live-market-feed-websocket.md`); Groww documents "upto 1000 subscriptions" with per-connection-vs-account unstated, plus a MEASURED undocumented adaptive connection cap (`docs/groww-ref/15-rate-limits-and-capacity.md` §1, §5).

### 6e. 429 / throttle behavior

- Exceeding a limit → **HTTP 429** — **Verified (LIVE-DOC runner 2026-07-14)**: exceptions/ page HTTP table ("429 — Too many requests to the API (rate limiting)"), the FAQ's OPS answer ("rejected with a 429 HTTP response code"), and forum-staff history back to 2017 (forum 2354, tonystark: "If request is rate limited you will get HTTP error response with status code 429").
- The 429 body's `error_type` is suggested by forum thread titles to surface as `NetworkException` ("Too many requests"; forum 11564 title-level corroboration) — **Assumed**, no fetched verbatim 429 body; the raw 429 wire shape (headers incl. any `Retry-After`, body JSON) remains an OPEN QUESTION (probe class — same as Groww U-7).
- **Neither official SDK retries, backs off, or reads `Retry-After`** (Verified-absence, §2.6) — a 429 simply raises. Any Kite client must own its own pacing; the Dhan DH-904 exponential-ladder discipline (`dhan-ref` rule 8) is the house pattern to mirror.
- No ban/cooldown/fair-use policy language exists on the live REST-docs pages (**Verified-absence on the fetched pages** for REST; the WS surface DOES carry the forum-staff abuse-flagging warning, §6d). The Groww WS adaptive-penalty precedent (measured 2026-07-06) mandates live-probe humility regardless.

### 6f. Historical evolution (context for stale third-party sources)

Limits have moved substantially over the years — any number found in an old forum thread or blog must be dated before use (all LIVE-DOC (forum), fetched 2026-07-14):

| Era | Figures then in force | Source |
|---|---|---|
| 2017 | 3 req/s ALL HTTP calls combined; order placement buffered to 5/s; NO daily order cap | forum 2354 (staff sujith/tonystark, Oct 2017) |
| Nov 2023 | docs said: Quote 1/s, Historical 3/s, Orders 10/s + **200/min + 3,000/day** | forum 13397 (user quoting the then-docs; staff confirms only order placement has min/day caps) |
| Jul 2023 | day cap unified at 3,000/account (was 3,000 regular + 2,000 CO), effective 2023-07-22 | forum 12851 (staff announcement) |
| 2026-07-14 (live) | Quote 1/s, Historical 3/s, Orders 10/s + **400/min + 5,000/day**, 25 mods/order | exceptions/ docs + both support articles (LIVE-DOC) |

## 7. Compare: Dhan / Groww (one-glance)

| Dimension | Kite Connect v3 (this file) | Dhan v2 (`docs/dhan-ref/01-…`) | Groww (`docs/groww-ref/15-…`) |
|---|---|---|---|
| Auth header | `Authorization: token key:token` + `X-Kite-Version: 3` | `access-token` (+ `client-id` on some) | `Authorization: Bearer` + `x-api-version: 1.0` |
| Version | header-based | URL `/v2/` | URL `/v1/` |
| POST body | form-urlencoded (3 JSON exceptions) | JSON | JSON |
| Error body | `{status:"error", error_type, message[, data]}` — name-only taxonomy (9 documented types) | `{errorType, errorCode, errorMessage}` — machine codes (DH-9xx/8xx) | HTTP-status-mapped SDK exceptions |
| Rate-limit model | per-endpoint-CLASS req/s (1/3/10) + RMS order counts | per-category /s /min /hr /day table | 3 type-pooled buckets /s + /min |
| Quote budget | 1/s (500–1000 instruments/call) | Quote APIs 1/s (1000 instruments/call) | Live Data 10/s · 300/min (50/call) |
| Order caps | 10/s (per client account) · 400/min · **5,000/day** (settled 2026-07-14) | 10/s · 250/min · 1,000/hr · 7,000/day | 10/s · 250/min; no day cap documented |
| WS caps | 3 conns × 3,000 tokens (soft + abuse-flagging) | 5 conns × 5,000 SIDs | 1000 subscriptions; adaptive conn cap (measured) |

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- ~~**Rate-limit table verbatim**~~ — **RESOLVED by live fetch 2026-07-14**: the exceptions/ page table is pasted verbatim in §6a (Quote 1/s · Historical 3/s · Orders 10/s · all other 10/s). Close the matching U-row.
- ~~**Orders/minute: 200 or 400?**~~ — **RESOLVED by live fetch 2026-07-14: 400/min** (exceptions/ docs + order-rate-limits support article; 200 = the pre-2024 superseded figure, still echoed in old forum replies). Close the matching U-row.
- ~~**Orders/day: 3,000 or 5,000 (current)?**~~ — **RESOLVED by live fetch 2026-07-14: 5,000/day** (exceptions/ docs + both live support articles; 3,000 was the July-2023 figure per forum 12851). Residual sub-unknown: the DATE the 3,000→5,000 raise happened is unannounced anywhere fetched. **Blocking: none.**
- ~~**Exceptions page full taxonomy**~~ — **RESOLVED by live fetch 2026-07-14**: nine documented types pasted verbatim in §5a; `MarginException` and `HoldingException` ARE documented. Residual sub-unknown: the permission type's wire spelling (`PermissionException` py vs `PermissionError` go) — the live page lists NO permission type at all, so the docs cannot arbitrate; only a live 403-permission response would. **Blocking: none (fallback-to-GeneralException absorbs it).**
- ~~**Gzip**~~ — **RESOLVED by live fetch 2026-07-14**: "The responses may be Gzipped" is live-verbatim on the INTRODUCTION page (https://kite.trade/docs/connect/v3/), not response-structure (attribution corrected). Whether `Accept-Encoding: identity` is honored remains unprobed (Assumed-standard). **Blocking: none.**
- **429 wire shape** — first live 429: capture status headers (any `Retry-After`?) + raw JSON body (`error_type` value — NetworkException?). The live pages document the 429 STATUS but never show a 429 BODY. Probe-class. **Blocking: ops.**
- **Rate-limit pooling scope (residual)** — order limits are settled as client-account-level (§6b: FAQ "per client account, not per app"; forum staff "Zerodha account level … all platforms"), and the general 10 req/s is per-api_key combined (forum staff 8577). Residual: whether the QUOTE 1/s and HISTORICAL 3/s classes pool per key or per account when one account holds multiple API keys — no live page states it (forum answers say per-key for the general limits). **Blocking: capacity (multi-app only).**

## CLAIMS (for README reconciled table)

- REST root `https://api.kite.trade`; auth = `Authorization: token api_key:access_token` + `X-Kite-Version: 3` header on every request; version is header-based, no `/v3/` in paths — **Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py + gokiteconnect connect.go, fetched 2026-07-13)** — raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/connect.py
- Success envelope `{"status":"success","data":…}`; SDKs return only `data` — **Verified (LIVE-DOC runner 2026-07-14, kite.trade/docs/connect/v3/response-structure/) + Verified (OFFICIAL-MOCK ltp.json/order_response.json + CLIENT-LIB-SOURCE both SDKs)**
- Error envelope `{"status":"error","error_type":…,"message":…[, "data":…]}` — the `data` key is OPTIONAL per the live page; dispatch is by `error_type` NAME with unknown names degrading to GeneralException; no numeric machine code exists (vs Dhan's DH-9xx) — **Verified (LIVE-DOC runner 2026-07-14, response-structure/ + exceptions/) + Verified (CLIENT-LIB-SOURCE gokiteconnect http.go errorEnvelope + pykiteconnect connect.py _request)**
- GET/DELETE params as query params; POST/PUT bodies form-urlencoded; only `/margins/orders`, `/margins/basket`, `/charges/orders` POST JSON — **Verified (LIVE-DOC runner 2026-07-14, response-structure/ first line) + Verified (CLIENT-LIB-SOURCE pykiteconnect is_json call sites + gokiteconnect http.go default Content-Type)**
- Documented exception taxonomy = exactly 9 types: TokenException, UserException, OrderException, InputException, MarginException, HoldingException, NetworkException, DataException, GeneralException; NO permission type documented (SDK-only variant, wire spelling unresolved); MarginException/HoldingException are in NEITHER SDK (unknown-name fallback absorbs) — **Verified (LIVE-DOC runner 2026-07-14, kite.trade/docs/connect/v3/exceptions/) + CLIENT-LIB-SOURCE both SDKs + OFFICIAL-MOCK autoslice_response.json**
- Common HTTP error codes on the live page: 400/403/404/405/410/429/500/502/503/504 with the §4 verbatim meanings — **Verified (LIVE-DOC runner 2026-07-14, exceptions/)**
- Session-expiry hook contract: fires only on HTTP 403 + `error_type == "TokenException"` — **Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py)**; live TokenException description endorses clear-session-and-relogin (**LIVE-DOC**)
- Instrument dumps are served as CSV content-type (SDK branches json/csv/other→DataException) — **Verified (CLIENT-LIB-SOURCE pykiteconnect _request)**
- API rate limits: Quote 1/s · Historical candle 3/s · Order placement 10/s · all other endpoints 10/s — **Verified (LIVE-DOC runner 2026-07-14, kite.trade/docs/connect/v3/exceptions/ "API rate limit" table, verbatim)**; the general 10/s is a combined per-api_key count (**LIVE-DOC (forum) 8577 staff**); no per-day cap outside order placement (**LIVE-DOC (forum) 13397 staff, Nov 2023**)
- Order RMS caps SETTLED: 10 orders/s per client (trading) ACCOUNT (not per app; excess → 429) · **400/min** · **5,000/day across all segments and varieties** · 25 modifications per order — **Verified (LIVE-DOC runner 2026-07-14: exceptions/ docs note + kite-connect-api-faqs + order-rate-limits-on-kite support articles)**. Previously conflicting: 200/min and 3,000/day were the pre-2024 figures (forum 13397 Nov-2023 docs quote; forum 12851 July-2023 change) — superseded.
- Day-cap semantics: account-level across ALL platforms (Kite web/app + API); RMS-blocked/invalid requests count; iceberg = 5 legs → 5 toward the DAY cap but 1 toward the per-second OPS limit; GTT-trigger order = 1; modify/cancel/GTT-place do NOT count; once hit, even exits are blocked (call RMS) — **Verified (LIVE-DOC runner 2026-07-14: forum 12851 staff answers + both support articles + FAQ)**
- Quote-family per-call caps: 500 instruments on `/quote`, 1000 on `/quote/ohlc` + `/quote/ltp` — **Verified (LIVE-DOC runner 2026-07-14, kite.trade/docs/connect/v3/market-quotes/, verbatim)**
- WebSocket caps: 3 connections per api_key × 3,000 instruments per connection; SOFT limit with abuse-flagging; over-subscription errors only the excess tokens, connection stays alive — **Verified (LIVE-DOC runner 2026-07-14, kite.trade/docs/connect/v3/websocket/ + forum 15708 staff)**
- Exceeding limits → HTTP 429 (documented on the live exceptions page + FAQ); neither official SDK retries/backs off/reads Retry-After (caller-owned pacing); the 429 BODY shape remains unprobed — **Verified (LIVE-DOC runner 2026-07-14) + Verified-absence (CLIENT-LIB-SOURCE grep, both SDKs)**
- Timestamps in REST responses are IST (UTC+5.5) strings `yyyy-mm-dd hh:mm:ss`; date strings `yyyy-mm-dd`; JSON value types string/int/float/bool — **Verified (LIVE-DOC runner 2026-07-14, response-structure/ "Data types", verbatim) + Verified (CLIENT-LIB-SOURCE _format_response len==19 parse)**
- Responses may be gzipped — **Verified (LIVE-DOC runner 2026-07-14, kite.trade/docs/connect/v3/ introduction page — attribution CORRECTED from response-structure/)**; no explicit SDK handling (transparent decompression)
