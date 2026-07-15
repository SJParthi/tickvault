# Zerodha Kite Connect v3 — Overview, Authentication & Token Lifecycle

> **Source:** https://kite.trade/docs/connect/v3/ · https://kite.trade/docs/connect/v3/user/ · https://kite.trade/docs/connect/v3/apps/ (auth-relevant redirect mechanics)
> **Fetched-Verified:** 2026-07-14 — live-verbatim via GH-runner 2026-07-14 (UTC timestamps in `00-COVERAGE-MANIFEST.md`; per-URL sha256 in the crawl fetch-log). The 2026-07-13 CLIENT-LIB-SOURCE (pykiteconnect@master + gokiteconnect@master) and OFFICIAL-MOCK (zerodha/kiteconnect-mocks@main) citations are RETAINED as cross-check evidence. SEARCH-tier claims that the live pages confirm are UPGRADED to **Verified (LIVE-DOC runner 2026-07-14)**; one SEARCH claim was CONTRADICTED by the live page and is corrected in §7 (the live page wins).
> **Evidence tiers:** see README legend — Verified (LIVE-DOC runner 2026-07-14) is the TOP tier for this pack; Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) / SEARCH / Assumed / Unknown follow.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. Adding Zerodha as a feed/broker requires its own dated operator lock per `.claude/rules/project/websocket-connection-scope-lock.md` re-approval protocol.
> **Related:** `99-UNKNOWNS.md` (this file's open questions feed it), `docs/dhan-ref/02-authentication.md` + `docs/groww-ref/17-token-lifecycle.md` (the comparison baselines).

---

## 1. What Kite Connect v3 is

**Verified (LIVE-DOC runner 2026-07-14 — https://kite.trade/docs/connect/v3/):** verbatim from the live Introduction page:

> "Kite Connect is a set of REST-like HTTP APIs that expose many capabilities required to build a complete stock market investment and trading platform. It lets you execute orders in real time (equities, commodities, mutual funds), manage user portfolios, stream live market data over WebSockets, and more."

And the transport contract, same page, verbatim:

> "All inputs are form-encoded parameters and responses are JSON (apart from a couple exceptions). The responses may be Gzipped. Standard HTTP codes are used to indicate success and error states with accompanying JSON data. The API endpoints are not cross site request enabled, hence cannot be called directly from browsers."

**App-credential model (LIVE-DOC, same page, verbatim):** "An api_key + api_secret pair is issued and you have to register a redirect url where a user is sent after the login flow. For mobile and desktop applications, there has to be a remote backend which does the handshake on behalf of the mobile app and the api_secret should never be embedded in the app."

**Prerequisites (LIVE-DOC, same page):** an active Zerodha trading account **with 2FA TOTP enabled**, plus a developer-portal account where an app is created to obtain `api_key`/`api_secret` and the redirect URL is registered.

Cross-check: the pykiteconnect README carries near-identical wording ("REST-like APIs … Execute orders in real time, manage user portfolio, stream live market data (WebSockets)") — **Verified (CLIENT-LIB-SOURCE pykiteconnect@master README.md, fetched 2026-07-13)**. The README names the doc surfaces https://kite.trade/docs/connect/v3 (now live-verified) and https://kite.trade/docs/pykiteconnect/v4. The current official Python client is **v5.x** (README changelog sections v5/v5.2 are the version authority; the README H1 still says "- v4", a stale title — recorded so a future reader doesn't false-refute the v5.x claim).

**Vendor:** Zerodha Technology Pvt. Ltd. — **Verified (LIVE-DOC runner 2026-07-14):** page footer "Copyright © 2015 - 2025, Zerodha Technology Pvt. Ltd."; broker code `ZERODHA` appears in the live session-response sample AND every fetched mock. Kite Connect is the API product of Zerodha, India's largest retail broker (**Assumed** — market-position statement not sourced in this pack).

## 2. Base URLs and API root

| Item | Value | Tier |
|---|---|---|
| REST API root | `https://api.kite.trade` | **Verified (LIVE-DOC runner 2026-07-14 — https://kite.trade/docs/connect/v3/user/):** every live curl example targets it (`curl https://api.kite.trade/session/token …`, `/user/profile`, `/user/margins`). Cross-check: pykiteconnect `_default_root_uri` line 36; gokiteconnect `baseURI` line 31 (CLIENT-LIB-SOURCE, 2026-07-13) |
| Login endpoint | `https://kite.zerodha.com/connect/login?v=3&api_key=xxx` | **Verified (LIVE-DOC runner 2026-07-14 — user/ page, verbatim):** "The login flow starts by navigating to the public Kite login endpoint." Note it is **kite.zerodha.com**, not kite.trade. Cross-check: both SDKs' login-URL builders (CLIENT-LIB-SOURCE). The apps/ page shows the same endpoint without `v=3` (`…/connect/login?api_key=xxx`) — the `v=3` param appears optional there, but send it (the user/ page canonical form carries it) |
| `kite.trade/connect/login` as a login alias | **Assumed** — commonly cited in third-party material; NOT present anywhere in the live-crawled docs pages (both live pages use kite.zerodha.com exclusively); redirect behaviour untested (→ OPEN QUESTIONS) |
| API version discriminator | `3` — the `v=3` login-URL param AND the `X-Kite-Version: 3` header | **Verified (LIVE-DOC runner 2026-07-14):** every live curl example carries `-H "X-Kite-Version: 3"`. Cross-check: pykiteconnect `kite_header_version = "3"`; gokiteconnect `kiteHeaderVersion = "3"` (CLIENT-LIB-SOURCE) |
| SDK default HTTP timeout | 7 seconds | **Verified (CLIENT-LIB-SOURCE pykiteconnect `_default_timeout = 7`, line 38)** — SDK default, not a server contract; not mentioned on the live pages |

**Compare: Dhan/Groww —** Dhan splits auth and API hosts (`auth.dhan.co` mint vs `api.dhan.co/v2` API — `docs/dhan-ref/02-authentication.md`); Groww uses one host `api.groww.in` for mint + API (`docs/groww-ref/17-token-lifecycle.md` §3). Kite splits similarly to Dhan: interactive login on `kite.zerodha.com`, API on `api.kite.trade`.

## 3. Required headers on every API request

**Verified (LIVE-DOC runner 2026-07-14 — user/ page "Signing requests", verbatim):**

> "Once the authentication is complete, all requests should be signed with the HTTP Authorization header with token as the authorisation scheme, followed by a space, and then the api_key:access_token combination. For example: `curl -H "Authorization: token api_key:access_token"` … `curl -H "Authorization: token xxx:yyy"`"

```
X-Kite-Version: 3
Authorization: token <api_key>:<access_token>
User-Agent: Kiteconnect-python/<version>       (SDK-added, not a server requirement)
```

- The `Authorization` scheme literal is the word `token` (lowercase), then `api_key` and `access_token` joined by a single colon — now doc-verbatim, and byte-consistent with both SDKs' `"token {api_key}:{access_token}"` construction (**Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py lines 936–945; gokiteconnect connect.go lines 236–241)**).
- The header is only attached when BOTH api_key and access_token are set (pre-login calls — i.e. the token exchange itself — go out without it; the live token-exchange curl example carries only `X-Kite-Version: 3` plus the form fields, §5).
- `X-Kite-Version: 3` appears on every live curl example including the token exchange (**LIVE-DOC**); both SDKs send it unconditionally.

**Compare: Dhan/Groww —** materially different header schemes: Dhan uses a bare `access-token: <JWT>` header (never `Authorization` — `.claude/rules/dhan/api-introduction.md` rule 3); Groww uses standard `Authorization: Bearer <token>` + `X-API-VERSION: 1.0` (`docs/groww-ref/17-token-lifecycle.md` §4). Kite's `token key:secret`-style composite in `Authorization` is a third, distinct scheme — a Kite client cannot reuse either existing header builder.

## 4. Response envelope and error surfaces

- JSON success envelope: `{"status": "success", "data": …}` — **Verified (LIVE-DOC runner 2026-07-14):** every response sample on the live user/ page (session, profile, margins, logout) is wrapped exactly so. Inputs are form-encoded, responses JSON (a couple of exceptions, e.g. CSV instrument dumps) and may be gzipped — LIVE-DOC, §1 verbatim. Cross-check: the SDK returns `data` unwrapped (**CLIENT-LIB-SOURCE pykiteconnect connect.py `_request` lines 972–996**) and every fetched mock matches (**OFFICIAL-MOCK generate_session.json / profile.json / session_logout.json**).
- JSON error envelope: `status == "error"` with fields `error_type` + `message`. The SDK maps `error_type` by name onto its exception classes, defaulting to `GeneralException` (**Verified (CLIENT-LIB-SOURCE)**; the live exceptions/ page is file `02`'s domain).
- Some endpoints return `text/csv` (instrument dumps) — the SDK returns raw bytes for those; any other content type raises `DataException`.
- Exception taxonomy with default HTTP codes (**Verified (CLIENT-LIB-SOURCE pykiteconnect exceptions.py, fetched 2026-07-13)**): `TokenException` (403, "all token and authentication related errors"), `PermissionException` (403), `InputException` (400), `OrderException` (500), `GeneralException` (500), `DataException` (502, "bad response from the backend Order Management System (OMS)"), `NetworkException` (503, "network issue between Kite and the backend OMS").

**Session-death detection contract (load-bearing):** **Verified (CLIENT-LIB-SOURCE connect.py lines 981–988):** the SDK fires a user-registered `session_expiry_hook` exactly when a response has HTTP status **403** AND `error_type == "TokenException"`. That `(403, TokenException)` pair is therefore the wire signature of "your access_token died" — the Kite analog of Dhan's DH-901 / Data-807 and Groww's 401-at-06:00.

## 5. The login flow (3 steps) and token exchange

**Verified (LIVE-DOC runner 2026-07-14 — https://kite.trade/docs/connect/v3/user/ "Login flow", verbatim summary):**

> "Navigate to the Kite Connect login page with the api_key → A successful login comes back with a request_token to the registered redirect URL → POST the request_token and checksum (SHA-256 of api_key + request_token + api_secret) to /session/token → Obtain the access_token and use that with all subsequent requests"

**Step 1 — redirect the user to the login URL.** **Verified (LIVE-DOC):**

```
https://kite.zerodha.com/connect/login?v=3&api_key=xxx
```

**`redirect_params` round-tripping (LIVE-DOC, verbatim):** "An optional redirect_params param can be appended to public Kite login endpoint, that will be sent back to the redirect URL. The value is URL encoded query params string, eg: some=X&more=Y). eg: `https://kite.zerodha.com/connect/login?v=3&api_key=xxx&redirect_params=some%3DX%26more%3DY`". (gokiteconnect's `GetLoginURLWithparams` is the SDK wrapper for this — CLIENT-LIB-SOURCE.)

**Step 2 — receive `request_token` on the registered redirect URL.**
**Verified (LIVE-DOC — user/ page):** "A successful login comes back with a request_token as a URL query parameter to the redirect URL registered on the developer console for that api_key." **Redirect-URL shape (LIVE-DOC — https://kite.trade/docs/connect/v3/apps/):** the redirect lands as `https://yoursite.com/kite-redirect?request_token=yyy&status=zzz` — i.e. a `status` param accompanies `request_token`. The apps/ page also documents: webview-based capture for mobile/desktop (monitor URL changes, extract the token), cookie + 3rd-party-cookie support required in the webview, `127.0.0.1` local-web-server redirect hosts allowed "for personal desktop apps", and (verbatim warning, both pages): never embed `api_secret` in a distributed app — "Use a server backend to do the token exchange on behalf of your application."

**`request_token` TTL + single-use (LIVE-DOC — user/ "Request parameters" table, verbatim):** "The one-time token obtained after the login flow. This token's lifetime is only a few minutes and it is meant to be exchanged for an access_token immediately after being obtained" — so: ONE-TIME use, few-minutes lifetime, exchange immediately. (Exact minute count not stated — residual in OPEN QUESTIONS.)

The interactive step is a REAL browser login by the account holder (2FA TOTP included, per the §1 prerequisites) — there is NO documented API to obtain a `request_token` programmatically (**Verified-absence across the live user/ + apps/ pages AND both SDK surfaces**).

**Step 3 — POST the token exchange.** **Verified (LIVE-DOC — user/ "Authentication and token exchange", live curl example verbatim):**

```
curl https://api.kite.trade/session/token \
   -H "X-Kite-Version: 3" \
   -d "api_key=xxx" \
   -d "request_token=yyy" \
   -d "checksum=zzz"
```

(Form-encoded POST — `-d` fields, not JSON. Route name in the SDKs: `api.token` → `/session/token`.)

**Checksum construction (exact, LIVE-DOC verbatim):** "checksum — SHA-256 hash of (api_key + request_token + api_secret)". Byte-identical in both SDKs: `hashlib.sha256(api_key + request_token + api_secret).hexdigest()` (**CLIENT-LIB-SOURCE pykiteconnect connect.py lines 263–264; gokiteconnect user.go lines 147–148**) — i.e. the hex digest of the UTF-8 concatenation. On success both SDKs auto-set the returned `access_token` on the client.

**Compare: Dhan/Groww —** Groww's checksum is `sha256(api_secret + epoch_seconds)` with a 10-minute timestamp validity (`docs/groww-ref/17-token-lifecycle.md` §3.2) — different input recipe, same SHA-256-hex idea. Dhan has no checksum at all (TOTP + PIN query params — `docs/dhan-ref/02-authentication.md`). Kite is the only one of the three that binds the checksum to a per-login `request_token`.

## 6. The session response (what `POST /session/token` returns)

**Verified (LIVE-DOC runner 2026-07-14 — user/ page response sample + "Response attributes" table).** The live sample is field-for-field identical to the official mock (**OFFICIAL-MOCK generate_session.json**, cross-check retained):

| Field | Live sample value / shape | Live-doc attribute description (verbatim where quoted) |
|---|---|---|
| `user_id` | `"XX0000"` | "The unique, permanent user id registered with the broker and the exchanges" |
| `user_type` | `"individual"` | "User's registered role at the broker. This will be individual for all retail users" |
| `email`, `user_name`, `user_shortname` | strings | |
| `broker` | `"ZERODHA"` | "The broker ID" |
| `exchanges` | `["NSE","NFO","BFO","CDS","BSE","MCX","BCD","MF"]` | "Exchanges enabled for trading on the user's account" |
| `products` | `["CNC","NRML","MIS","BO","CO"]` | "Margin product types enabled for the user" |
| `order_types` | `["MARKET","LIMIT","SL","SL-M"]` | "Order types enabled for the user" |
| `avatar_url` | string | "Full URL to the user's avatar (PNG image) if there's one" |
| `api_key` | echoed back | "The API key for which the authentication was performed" |
| **`access_token`** | string | "The authentication token that's used with every subsequent request" — expiry semantics in §7 (LIVE-DOC verbatim there) |
| `public_token` | string | **"A token for public session validation where requests may be exposed to the public"** (LIVE-DOC — was Unknown, now resolved) |
| `enctoken` | string, in the live sample | NOT in the live Response-attributes table — **Verified-absence (LIVE-DOC)**: appears in the sample JSON, remains undocumented; semantics **Unknown** |
| **`refresh_token`** | `''` — EMPTY in the live sample (and the mock) | **"A token for getting long standing read permissions. This is only available to certain approved platforms"** (LIVE-DOC — see §8) |
| `silo` | `''`, in the live sample | NOT in the live Response-attributes table — undocumented; **Unknown** semantics |
| `login_time` | `"2021-01-01 16:15:14"` — 19-char `YYYY-MM-DD HH:MM:SS` | "User's last login time" (LIVE-DOC) — NO timezone stated on the wire or in the doc; **Assumed IST** (→ OPEN QUESTIONS). pykiteconnect parses it with dateutil only when `len == 19` |
| `meta` | `{"demat_consent": "physical"}` | "demat_consent: empty, consent or physical" (LIVE-DOC) |

gokiteconnect models the same shape as `UserSession` = embedded `UserProfile` + `user_id/api_key/public_token/access_token/refresh_token/login_time` (**Verified (CLIENT-LIB-SOURCE gokiteconnect user.go lines 13–26)**).

**Compare: Dhan/Groww —** Dhan's mint returns `accessToken` + `expiryTime` (ISO, IST-based); Groww's returns `token` + a machine-readable `expiry` field. **Kite's session response carries NO expiry field at all** (**Verified-absence — LIVE-DOC sample + attributes table + OFFICIAL-MOCK + both SDK structs**) — `login_time` is the only timestamp; the expiry rule lives in doc prose (§7), not on the wire. This is materially worse for automated lifecycle management than both Dhan and Groww.

## 7. ACCESS TOKEN DAILY LIFECYCLE (load-bearing)

**THE official wording is now captured. Verified (LIVE-DOC runner 2026-07-14 — https://kite.trade/docs/connect/v3/user/, `access_token` response-attribute row, verbatim):**

> "The authentication token that's used with every subsequent request. Unless this is invalidated using the API, or invalidated by a master-logout from the Kite Web trading terminal, it'll expire at `6 AM` on the next day (regulatory requirement)."

1. **Expiry instant = 6 AM on the next day.** **CORRECTION (2026-07-14, live page wins):** the previous SEARCH-tier claim in this file — "flushed every day at 7:30 AM / new one at 7:35 AM" (kite.trade forum snippets /3468, /4816, /2581, /6383, /13884, /896) — is CONTRADICTED by the official docs page, which says **6 AM on the next day**. The forum figures were either stale or approximate; the doc wording is the contract. Practical reading: a token minted on day D dies at 06:00 (doc gives no timezone label on "6 AM", **Assumed IST** — Indian broker/regulatory context) on day D+1, i.e. any token minted after 06:00 covers the full trading day it was minted on.
2. **Three death paths (LIVE-DOC, same sentence):** (a) API invalidation (`DELETE /session/token`, §9); (b) **a master-logout from the Kite Web trading terminal** — NEW fact: the account holder logging out of Kite Web with master-logout kills the API access_token too; (c) the daily 6 AM expiry. The expiry is labelled a **"regulatory requirement"** — the Kite analog of Dhan's SEBI-mandated 24h JWT.
3. **Manual login is required daily.** Consequence of 1+2 plus §8 (no refresh_token for individuals): every trading day needs one interactive `kite.zerodha.com` login (2FA TOTP) to produce a fresh `request_token`. The live docs express this structurally (daily expiry + login-flow-only mint); the explicit "mandatory by the exchange that a user has to log in manually at least once a day" wording remains **SEARCH** (kite.trade forum /5394, /14052) — consistent with, and now anchored by, the LIVE-DOC "regulatory requirement" label.
4. **One active token per api_key (probable).** **SEARCH (weak):** "Previous access token gets expired by generating a new access token, as only one access token will be active" (forum snippet, thread attribution unclear). The live user/ page is SILENT on this. Treat as **Assumed** pending live probe — but plan for it (Dhan's identical one-active-token rule burned this project before: `dual-instance-lock-2026-07-04.md`).
5. **No wire-visible TTL.** **Verified-absence (LIVE-DOC session sample + attributes table; OFFICIAL-MOCK; both SDK session structs):** no `expiry`/`expires_in` field exists anywhere in the session payload (§6). The 6 AM rule must be hard-coded client-side.

**Feed-decision synthesis (ARITH/Assumed on top of the above):** a Kite-based feed must (a) obtain a fresh token once per morning AFTER 06:00 IST and before 09:15 market open — a similar window to Groww's 06:00 reset + 06:05 mint (the previous "after ~07:35" schedule note is void with the corrected 6 AM figure); (b) tolerate that the mint step is interactive-login-shaped (browser webview or human — apps/ page mechanics), unlike Dhan's TOTP mint and Groww's TOTP/approval mint which are pure API calls; (c) detect death via the `(403, TokenException)` signature (§4), not via a TTL clock; (d) additionally tolerate the master-logout death path — an operator logging out of Kite Web mid-session can kill the feed token (death path 2b).

**Compare: Dhan/Groww —**

| | Kite | Dhan | Groww |
|---|---|---|---|
| Token life | until 6 AM next day, unless API-invalidated or web master-logout (Verified LIVE-DOC runner 2026-07-14) | 24h from mint (Verified — `docs/dhan-ref/02-authentication.md`) | dies daily 06:00 AM IST (Verified LIVE-DOC — `docs/groww-ref/17-token-lifecycle.md` §2) |
| Unattended mint possible? | **NO** (login-flow-only mint — LIVE-DOC structural; "manual login mandated" wording SEARCH) | YES — TOTP+PIN API mint | YES — TOTP or checksum API mint |
| Renewal without re-login | only via `refresh_token` — "only available to certain approved platforms" (LIVE-DOC, §8) | `GET /v2/RenewToken` extends 24h (active tokens only) | re-mint anytime via API |
| Wire TTL field | none (Verified-absence LIVE-DOC) | `expiryTime` in mint response | `expiry` in mint response |
| One-active-token | Assumed yes (SEARCH; live docs silent) | Verified yes (documented) | Unknown (groww-ref §5.2) |

Interesting alignment: Kite's 6 AM and Groww's 06:00 daily death are the SAME instant — one morning mint window covers both brokers' patterns.

## 8. Refresh tokens — endpoints exist, individuals don't get them

**Verified (LIVE-DOC runner 2026-07-14 — user/ response-attributes table, verbatim):** `refresh_token` = "A token for getting long standing read permissions. This is only available to certain approved platforms". This upgrades the previous forum-only sourcing: platform-gating is now doc-verbatim.

**Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py):** the SDK carries a complete refresh-token surface:

- `api.token.renew` → `POST /session/refresh_token` with form fields `api_key`, `refresh_token`, `checksum` where `checksum = sha256_hex(api_key + refresh_token + api_secret)` (`renew_access_token`, lines 292–311; docstring: "Renew expired `refresh_token` using valid `refresh_token`" [sic — the docstring's own wording; it renews the *access token*]). gokiteconnect's `RenewAccessToken` (user.go lines 192–212) returns `{user_id, access_token, refresh_token}` (`UserSessionTokens`). NOTE: this endpoint does NOT appear in the live user/ page's endpoint table (**Verified-absence LIVE-DOC**) — SDK-only documentation.
- `invalidate_refresh_token` → `DELETE /session/token` with `api_key` + `refresh_token` (lines 313–322).

**Who actually receives one:** `"refresh_token": ''` — empty in the live doc sample AND the official mock (**LIVE-DOC + OFFICIAL-MOCK**). **SEARCH (kite.trade forum /5394, /14052):** issued "only for the platforms that are built for mass and not for individual users" (e.g. exchange-approved platforms); individuals "have to log in manually every day" — now consistent with the LIVE-DOC "certain approved platforms" wording, and note the live doc scopes it to **read permissions**. So: the endpoints are real, but for this project's account class they are dead surface. **Compare: Dhan —** Dhan's `RenewToken` works for every account (extends the 24h JWT) — Kite's renewal path being platform-gated is a material downgrade for individual automation.

## 9. Logout / token invalidation

**Verified (LIVE-DOC runner 2026-07-14 — user/ "Logout", verbatim):**

> "This call invalidates the access_token and destroys the API session. After this, the user should be sent through a new login flow before further interactions. This does not log the user out of the official Kite web or mobile applications."

```
curl --request DELETE \
  -H "X-Kite-Version: 3" \
  "https://api.kite.trade/session/token?api_key=xxx&access_token=yyy"
```

Response (LIVE-DOC sample, matching **OFFICIAL-MOCK session_logout.json**): `{"status": "success", "data": true}`. (SDK route `api.token.invalidate` → same `/session/token` path with DELETE, params as query string — **CLIENT-LIB-SOURCE** both SDKs.)

Asymmetry worth pinning: API logout does NOT touch the Kite web/mobile session (LIVE-DOC above), but a Kite Web **master-logout** DOES kill the API access_token (§7 death path 2b — LIVE-DOC). Voluntary/accidental invalidation matters because an accidental `DELETE /session/token` from another integration on the same api_key — or the operator's own web master-logout — kills the feed's token mid-session. **Compare: Groww —** Groww has NO documented programmatic revoke (UI-only — groww-ref 17 §5.3); Kite documents one.

## 10. User profile endpoints

The live user/ page's endpoint table (**LIVE-DOC**): `POST /session/token` · `GET /user/profile` · `GET /user/margins/:segment` · `DELETE /session/token`.

- `GET /user/profile` — **Verified (LIVE-DOC — user/ "User profile", verbatim):** "While a successful token exchange returns the full user profile, it's possible to retrieve it any point of time with the /user/profile API. **Do note that the profile API does not return any of the tokens.**" Live response sample: `user_id, user_type, email, user_name, user_shortname, broker, exchanges[], products[], order_types[], avatar_url, meta.demat_consent` — the §6 session payload minus the token fields. (Cross-check: **OFFICIAL-MOCK profile.json**, identical shape.)
- `GET /user/margins` and `GET /user/margins/:segment` (`equity` | `commodity`) — **Verified (LIVE-DOC — user/ "Funds and margins")**: funds/cash/margin per segment (`net`, `available.{cash,opening_balance,live_balance,intraday_payin,adhoc_margin,collateral}`, `utilised.{debits,span,exposure,option_premium,m2m_realised,m2m_unrealised,holding_sales,delivery,stock_collateral,liquid_collateral,turnover,payout}`) — detail belongs to `06-margins-and-charges.md`; listed here because it shares the user/ page.
- `GET /user/profile/full` — **Verified (CLIENT-LIB-SOURCE gokiteconnect connect.go `URIFullUserProfile` line 102)**; response **Verified (OFFICIAL-MOCK full_profile.json):** adds `twofa_type` ("totp"), `phone` (masked), `bank_accounts[]`, `dp_ids[]`, and more. NOTE: NOT in the live user/ page's endpoint table (**Verified-absence LIVE-DOC**) and pykiteconnect@master has NO route for it — Go-SDK-only surface.
- No `tokenValidity`-style field appears in either profile response (**Verified-absence — LIVE-DOC sample + OFFICIAL-MOCK**) — unlike Dhan's `/v2/profile` which returns `tokenValidity` in `DD/MM/YYYY HH:MM` IST (`.claude/rules/dhan/authentication.md` rule 9). A Kite watchdog probing `/user/profile` gets liveness (200 vs 403/TokenException) but NOT remaining-validity — the 6 AM rule (§7) is the only clock.

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **RESOLVED by live fetch (2026-07-14): exact official access-token expiry wording.** https://kite.trade/docs/connect/v3/user/ verbatim: "Unless this is invalidated using the API, or invalidated by a master-logout from the Kite Web trading terminal, it'll expire at `6 AM` on the next day (regulatory requirement)." The prior ~07:30 AM forum figure was WRONG (corrected in §7). Residual: the doc does not label "6 AM"'s timezone — Assumed IST. (99-UNKNOWNS rebuilder: close the matching U-row; keep only the timezone residual.)
- **RESOLVED by live fetch (2026-07-14): `request_token` TTL and single-use semantics.** LIVE-DOC verbatim: "The one-time token … lifetime is only a few minutes and it is meant to be exchanged for an access_token immediately after being obtained." Residual: the exact minute count is unstated (cosmetic — design for "exchange immediately"). (Close the matching U-row.)
- **RESOLVED by live fetch (2026-07-14): redirect-URL params on login return.** LIVE-DOC (apps/ page): the redirect lands as `…?request_token=yyy&status=zzz` — a `status` param accompanies the token. (Close the matching U-row.)
- **PARTIALLY RESOLVED by live fetch (2026-07-14): session-response token-field semantics.** `public_token` = "public session validation where requests may be exposed to the public"; `refresh_token` = "long standing read permissions … certain approved platforms" (both LIVE-DOC). STILL OPEN: `enctoken` and `silo` appear in the live sample JSON but have NO row in the live Response-attributes table — semantics remain Unknown. (blocking: none)
- **One-active-access-token semantics.** Does `generate_session` #2 invalidate access_token #1 (forum snippet says yes)? The live user/ page is SILENT — live-probe required: mint twice, test token #1. Dual-instance-lock design depends on it. (blocking: pipeline)
- **Login-URL alias.** Does https://kite.trade/connect/login exist/redirect to https://kite.zerodha.com/connect/login? Not in the crawl set; both live pages use kite.zerodha.com only. Cosmetic. (blocking: none)
- **`login_time` timezone + the "6 AM" timezone.** Live doc says "User's last login time" and "6 AM" with no zone label on either — Assumed IST; confirm via live probe or support. (blocking: ops)
- **Whether daily manual login can be satisfied by TOTP-automated browser login without ToS breach.** Policy question for the operator/Zerodha support, not a docs paste. (blocking: pipeline)

## CLAIMS (for README reconciled table)

- API root is `https://api.kite.trade`; login endpoint is `https://kite.zerodha.com/connect/login?v=3&api_key=…` — **Verified (LIVE-DOC runner 2026-07-14 — kite.trade/docs/connect/v3/user/)**; cross-check CLIENT-LIB-SOURCE (both SDKs, 2026-07-13)
- Optional `redirect_params` (URL-encoded query-string value) round-trips through the login redirect — **Verified (LIVE-DOC user/)**; cross-check gokiteconnect `GetLoginURLWithparams`
- The login redirect returns `request_token` AND a `status` param on the registered redirect URL — **Verified (LIVE-DOC apps/)** — was Unknown
- `request_token` is ONE-TIME and lives "only a few minutes"; exchange immediately — **Verified (LIVE-DOC user/ request-parameters table)** — was Unknown
- Every authenticated request carries `X-Kite-Version: 3` + `Authorization: token <api_key>:<access_token>` (scheme word `token`, space, `api_key:access_token`) — **Verified (LIVE-DOC user/ "Signing requests")**; cross-check CLIENT-LIB-SOURCE
- Token exchange: `POST /session/token`, form-encoded fields `api_key`, `request_token`, `checksum` = "SHA-256 hash of (api_key + request_token + api_secret)" — **Verified (LIVE-DOC user/)**; cross-check CLIENT-LIB-SOURCE (hex digest per both SDKs)
- The session response carries `access_token`, `public_token`, `enctoken`, `refresh_token`, `login_time` (19-char, no timezone) + full user profile — but NO expiry/TTL field — **Verified (LIVE-DOC user/ sample + attributes table)** + OFFICIAL-MOCK / Verified-absence for the TTL
- `public_token` = "a token for public session validation where requests may be exposed to the public" — **Verified (LIVE-DOC user/)** — was Unknown; `enctoken`/`silo` remain undocumented (in sample, absent from attributes table — Verified-absence LIVE-DOC)
- **access_token expires at `6 AM` on the next day (regulatory requirement), unless invalidated via the API or by a master-logout from the Kite Web trading terminal — Verified (LIVE-DOC runner 2026-07-14, user/ verbatim). CORRECTION: supersedes this pack's earlier SEARCH-tier "~07:30 AM daily flush / ~07:35 new session" forum figure — the live page wins**
- A Kite Web master-logout kills the API access_token; API logout (`DELETE /session/token`) does NOT log the user out of Kite web/mobile — **Verified (LIVE-DOC user/)** — new fact
- `refresh_token` grants "long standing read permissions", "only available to certain approved platforms"; empty for individual accounts — individuals must log in manually daily — **Verified (LIVE-DOC user/ + sample `''`)** + SEARCH (forum /5394, /14052) for the daily-manual-login wording
- There is NO programmatic `request_token` mint: the daily interactive login on kite.zerodha.com (2FA TOTP account prerequisite) is the only mint path — **Verified-absence (LIVE-DOC user/ + apps/ + both SDK surfaces)**; "exchange-mandated" wording SEARCH
- Session death signature: HTTP 403 + `error_type == "TokenException"`; pykiteconnect fires `session_expiry_hook` on exactly that pair — Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py lines 981–988, exceptions.py); live pages silent on the pair
- Logout: `DELETE /session/token` (api_key + access_token as query params) → `{"status":"success","data":true}` — **Verified (LIVE-DOC user/ curl + sample)** + CLIENT-LIB-SOURCE + OFFICIAL-MOCK
- Refresh-token renewal endpoint exists: `POST /session/refresh_token` with `checksum = sha256_hex(api_key + refresh_token + api_secret)` — Verified (CLIENT-LIB-SOURCE both SDKs) — NOT in the live user/ endpoint table (Verified-absence LIVE-DOC); dead surface for individuals per the claim above
- Envelope/transport: success is `{"status":"success","data":…}`; inputs are form-encoded; responses JSON (a couple of exceptions), may be Gzipped; endpoints are not CORS-enabled — **Verified (LIVE-DOC v3 root + user/ samples)**; error envelope `{"status":"error","error_type","message"}` Verified (CLIENT-LIB-SOURCE + mocks; live exceptions page → file 02)
- `GET /user/profile` "does not return any of the tokens" (LIVE-DOC verbatim) and `GET /user/margins/:segment` (`equity`/`commodity`) are the other user/ endpoints; `GET /user/profile/full` is Go-SDK-only and absent from the live endpoint table — **Verified (LIVE-DOC user/)** + CLIENT-LIB-SOURCE/OFFICIAL-MOCK; no token-validity field anywhere (Verified-absence)
