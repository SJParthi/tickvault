# Zerodha Kite Connect v3 — Overview, Authentication & Token Lifecycle

> **Source:** https://kite.trade/docs/connect/v3/ · https://kite.trade/docs/connect/v3/user/ (both UNREACHABLE live from this sandbox — see provenance)
> **Fetched-Verified:** 2026-07-13 via CLIENT-LIB-SOURCE (pykiteconnect@master + gokiteconnect@master via raw.githubusercontent.com), OFFICIAL-MOCK (zerodha/kiteconnect-mocks@main), and SEARCH (kite.trade-domain result snippets). ARCHIVE-DOC and MIRROR-LIVE routes were BOTH BLOCKED at the egress proxy (web.archive.org CONNECT 403; r.jina.ai CONNECT 403; kite.trade direct CONNECT 403 — per `zerodha-probe.md` 2026-07-13), so NOTHING in this file carries those tiers. The official docs HTML was never read verbatim; SDK source + official mocks are the strongest available evidence.
> **Evidence tiers:** see README legend — Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) / SEARCH / Assumed / Unknown. SEARCH-tier facts here come from search-engine snippets of kite.trade pages (URL-cited, NOT byte-verbatim) and are the weakest Verified-class evidence in this file — re-verify against the live pages before contracting.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. Adding Zerodha as a feed/broker requires its own dated operator lock per `.claude/rules/project/websocket-connection-scope-lock.md` re-approval protocol.
> **Related:** `99-UNKNOWNS.md` (this file's open questions feed it), `docs/dhan-ref/02-authentication.md` + `docs/groww-ref/17-token-lifecycle.md` (the comparison baselines).

---

## 1. What Kite Connect v3 is

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master README.md, fetched 2026-07-13** — https://raw.githubusercontent.com/zerodha/pykiteconnect/master/README.md**):** verbatim from the official Python client README:

> "Kite Connect is a set of REST-like APIs that expose many capabilities required to build a complete investment and trading platform. Execute orders in real time, manage user portfolio, stream live market data (WebSockets), and more, with the simple HTTP API collection."

The README names the official doc surfaces: HTTP API docs at https://kite.trade/docs/connect/v3 and Python client docs at https://kite.trade/docs/pykiteconnect/v4. The current official Python client is **v5.x** (v4 renamed ticker fields per the v3 WebSocket doc; v5 dropped Python 2.7; v5.2 added autoslice orders) — same README. (Note: the README's own H1 still reads "The Kite Connect API Python client **- v4**" — a stale title; the changelog sections v5/v5.2 in the same file are the version authority. Recorded so a future reader doesn't false-refute the v5.x claim.)

**Vendor:** Zerodha Technology (the SDK copyright line; broker code `ZERODHA` in every mock response). Kite Connect is the API product of Zerodha, India's largest retail broker (**Assumed** — market-position statement not sourced in this pack; the API-product relationship is Verified via the SDKs' org `github.com/zerodha`).

## 2. Base URLs and API root

| Item | Value | Tier |
|---|---|---|
| REST API root | `https://api.kite.trade` | **Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py `_default_root_uri`, line 36; cross-checked gokiteconnect connect.go `baseURI`, line 31 — both fetched 2026-07-13)** |
| Login endpoint | `https://kite.zerodha.com/connect/login` | **Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py `_default_login_uri` line 37; gokiteconnect connect.go `kiteBaseURI` line 32 + `GetLoginURL` line 203)** — note BOTH official SDKs point at **kite.zerodha.com**, not kite.trade |
| `kite.trade/connect/login` as a login alias | **Assumed** — commonly cited in third-party material; the live docs page that would confirm/deny any redirect alias is unreachable (→ OPEN QUESTIONS) |
| API version discriminator | `3` — sent as the `v=3` login-URL param AND the `X-Kite-Version: 3` header | **Verified (CLIENT-LIB-SOURCE pykiteconnect `kite_header_version = "3"` line 41 + `login_url()` line 248; gokiteconnect `kiteHeaderVersion = "3"` line 34)** |
| SDK default HTTP timeout | 7 seconds | **Verified (CLIENT-LIB-SOURCE pykiteconnect `_default_timeout = 7`, line 38)** — SDK default, not a server contract |

**Compare: Dhan/Groww —** Dhan splits auth and API hosts (`auth.dhan.co` mint vs `api.dhan.co/v2` API — `docs/dhan-ref/02-authentication.md`); Groww uses one host `api.groww.in` for mint + API (`docs/groww-ref/17-token-lifecycle.md` §3). Kite splits similarly to Dhan: interactive login on `kite.zerodha.com`, API on `api.kite.trade`.

## 3. Required headers on every API request

**Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py `_request`, lines 936–945; byte-consistent with gokiteconnect connect.go lines 236–241):**

```
X-Kite-Version: 3
Authorization: token <api_key>:<access_token>
User-Agent: Kiteconnect-python/<version>       (SDK-added, not a server requirement)
```

- The `Authorization` scheme literal is the word `token` (lowercase), then `api_key` and `access_token` joined by a single colon. Both SDKs construct exactly `"token {api_key}:{access_token}"`.
- The header is only attached when BOTH api_key and access_token are set (pre-login calls — i.e. the token exchange itself — go out without it; the token-exchange POST instead carries `api_key` as a form field, §5).
- `X-Kite-Version: 3` is sent unconditionally by both SDKs on every request.

**Compare: Dhan/Groww —** materially different header schemes: Dhan uses a bare `access-token: <JWT>` header (never `Authorization` — `.claude/rules/dhan/api-introduction.md` rule 3); Groww uses standard `Authorization: Bearer <token>` + `X-API-VERSION: 1.0` (`docs/groww-ref/17-token-lifecycle.md` §4). Kite's `token key:secret`-style composite in `Authorization` is a third, distinct scheme — a Kite client cannot reuse either existing header builder.

## 4. Response envelope and error surfaces

**Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py `_request`, lines 972–996):**

- JSON success envelope: `{"status": "success", "data": …}` — the SDK returns `data` unwrapped. Confirmed against every fetched mock (**Verified (OFFICIAL-MOCK generate_session.json / profile.json / session_logout.json, kiteconnect-mocks@main, fetched 2026-07-13)**).
- JSON error envelope: `status == "error"` with fields `error_type` + `message`. The SDK maps `error_type` by name onto its exception classes, defaulting to `GeneralException`.
- Some endpoints return `text/csv` (instrument dumps) — the SDK returns raw bytes for those; any other content type raises `DataException`.
- Exception taxonomy with default HTTP codes (**Verified (CLIENT-LIB-SOURCE pykiteconnect exceptions.py, fetched 2026-07-13)**): `TokenException` (403, "all token and authentication related errors"), `PermissionException` (403), `InputException` (400), `OrderException` (500), `GeneralException` (500), `DataException` (502, "bad response from the backend Order Management System (OMS)"), `NetworkException` (503, "network issue between Kite and the backend OMS").

**Session-death detection contract (load-bearing):** **Verified (CLIENT-LIB-SOURCE connect.py lines 981–988):** the SDK fires a user-registered `session_expiry_hook` exactly when a response has HTTP status **403** AND `error_type == "TokenException"`. That `(403, TokenException)` pair is therefore the wire signature of "your access_token died" — the Kite analog of Dhan's DH-901 / Data-807 and Groww's 401-at-06:00.

## 5. The login flow (3 steps) and token exchange

The flow reconstructed from both official SDKs + the official mock (docs page unreadable — wording below is SDK-derived, not doc-verbatim):

**Step 1 — redirect the user to the login URL.**
**Verified (CLIENT-LIB-SOURCE pykiteconnect `login_url()` line 246–248; gokiteconnect `GetLoginURL` line 203):**

```
https://kite.zerodha.com/connect/login?api_key=<api_key>&v=3
```

gokiteconnect additionally exposes `GetLoginURLWithparams` appending `&redirect_params=<url-encoded key=val pairs>` (connect.go lines 207–210) — a documented mechanism for round-tripping state through the redirect.

**Step 2 — receive `request_token` on the registered redirect URL.**
**Verified (CLIENT-LIB-SOURCE pykiteconnect `generate_session` docstring, line 260):** "`request_token` is the token obtained from the GET paramers [sic] after a successful login redirect." The README (lines 111–117) confirms the sequence: redirect user to `login_url()` → "receive the request_token from the registered redirect url after the login flow" → exchange it. The interactive step is a REAL browser login by the account holder (2FA included) — there is NO documented API to obtain a `request_token` programmatically (**Verified-absence across both SDK surfaces**; the exchange-mandate rationale is SEARCH tier, §7). `request_token` TTL and single-use semantics: **Unknown** (→ OPEN QUESTIONS).

**Step 3 — POST the token exchange.**
**Verified (CLIENT-LIB-SOURCE pykiteconnect `generate_session` lines 250–278; cross-checked gokiteconnect `GenerateSession` user.go lines 145–168):**

```
POST https://api.kite.trade/session/token        (route "api.token": /session/token)
Content-Type: application/x-www-form-urlencoded  (SDK sends form data, not JSON)
X-Kite-Version: 3

api_key=<api_key>&request_token=<request_token>&checksum=<checksum>
```

**Checksum construction (exact):** `checksum = SHA-256 hex digest of the UTF-8 concatenation api_key + request_token + api_secret` — byte-identical in both SDKs (connect.py lines 263–264: `hashlib.sha256(api_key + request_token + api_secret).hexdigest()`; user.go lines 147–148). On success both SDKs auto-set the returned `access_token` on the client.

**Compare: Dhan/Groww —** Groww's checksum is `sha256(api_secret + epoch_seconds)` with a 10-minute timestamp validity (`docs/groww-ref/17-token-lifecycle.md` §3.2) — different input recipe, same SHA-256-hex idea. Dhan has no checksum at all (TOTP + PIN query params — `docs/dhan-ref/02-authentication.md`). Kite is the only one of the three that binds the checksum to a per-login `request_token`.

## 6. The session response (what `POST /session/token` returns)

**Verified (OFFICIAL-MOCK generate_session.json, kiteconnect-mocks@main, fetched 2026-07-13** — https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/generate_session.json**):** the full `data` payload, field-for-field:

| Field | Mock value / shape | Notes |
|---|---|---|
| `user_id` | `"XX0000"` | broker client id |
| `user_type` | `"individual"` | |
| `email`, `user_name`, `user_shortname` | strings | |
| `broker` | `"ZERODHA"` | |
| `exchanges` | `["NSE","NFO","BFO","CDS","BSE","MCX","BCD","MF"]` | enabled segments for the account |
| `products` | `["CNC","NRML","MIS","BO","CO"]` | |
| `order_types` | `["MARKET","LIMIT","SL","SL-M"]` | |
| `avatar_url` | string | |
| `api_key` | echoed back | |
| **`access_token`** | string | THE credential for all subsequent calls (§3 header) |
| `public_token` | string | for public/websocket-adjacent use — semantics **Unknown** (docs unreadable; → OPEN QUESTIONS) |
| `enctoken` | string | undocumented in SDK; used by Kite web internally — **Unknown** semantics |
| **`refresh_token`** | `""` — EMPTY STRING in the official mock | see §8 — individuals do not receive one |
| `silo` | `""` | **Unknown** semantics |
| `login_time` | `"2021-01-01 16:15:14"` — 19-char `YYYY-MM-DD HH:MM:SS` string | pykiteconnect parses it with dateutil only when `len == 19` (connect.py lines 275–276); NO timezone on the wire — **Assumed IST** (Indian broker convention; unconfirmed) |
| `meta` | `{"demat_consent": "physical"}` | |

gokiteconnect models the same shape as `UserSession` = embedded `UserProfile` + `user_id/api_key/public_token/access_token/refresh_token/login_time` (**Verified (CLIENT-LIB-SOURCE gokiteconnect user.go lines 13–26)**).

**Compare: Dhan/Groww —** Dhan's mint returns `accessToken` + `expiryTime` (ISO, IST-based); Groww's returns `token` + a machine-readable `expiry` field. **Kite's session response carries NO expiry field at all** — `login_time` is the only timestamp; the client must KNOW the daily-flush rule (§7) rather than read a TTL off the wire. This is materially worse for automated lifecycle management than both Dhan and Groww.

## 7. ACCESS TOKEN DAILY LIFECYCLE (load-bearing)

The official docs wording could NOT be captured verbatim (kite.trade + archive.org + jina all proxy-blocked). What IS established:

1. **Valid for ~one whole day.** **SEARCH (kite.trade forum + docs snippets, fetched 2026-07-13):** "An access token is valid for one whole day. Unless you log out." (aggregated snippet across kite.trade/forum/discussion/896, /7759, /14312 — snippet-level attribution to a single thread not possible; re-verify).
2. **Daily flush at ~07:30 AM IST.** **SEARCH (same result set):** "Access tokens are flushed every day at 7:30 AM and new one is generated every day at 7:35 AM"; another snippet: "valid until the next day morning till around 07:00 AM or 07:30 AM"; and: "If you create an access token post 07:30 AM it should be valid for the whole day." Sources: kite.trade/forum/discussion/3468 ("access token expiry time everyday"), /4816, /2581, /6383, /13884 (thread-level; snippets are forum-answer text, most likely by Zerodha staff, but NOT the docs page). The exact flush instant and whether it is documented on /docs/connect/v3/user/ verbatim: **Unknown** (→ OPEN QUESTIONS #1 — this is THE load-bearing fact for feed/token-minter scheduling).
3. **Manual login is mandated daily.** **SEARCH:** "it is mandatory by the exchange that a user has to log in manually at least once a day" (kite.trade forum, refresh-token threads /5394, /14052). Consequence: for an individual account there is NO fully-unattended token path — every trading day needs one interactive `kite.zerodha.com` login to produce a fresh `request_token` (§5 Step 2).
4. **One active token per api_key (probable).** **SEARCH (weak):** "Previous access token gets expired by generating a new access token, as only one access token will be active" (forum snippet, thread attribution unclear). Treat as **Assumed** pending live confirmation — but plan for it (Dhan's identical one-active-token rule burned this project before: `dual-instance-lock-2026-07-04.md`).
5. **No wire-visible TTL.** **Verified-absence (OFFICIAL-MOCK generate_session.json + both SDK session structs):** no `expiry`/`expires_in`/TTL field exists anywhere in the session payload (§6).

**Feed-decision synthesis (ARITH/Assumed on top of the above):** a Kite-based feed must (a) obtain a fresh token once per morning AFTER ~07:35 IST and before 09:15 market open — a tighter window than Groww's 06:00 reset + 06:05 mint; (b) tolerate that the mint step is interactive-login-shaped (browser automation or human), unlike Dhan's TOTP mint and Groww's TOTP/approval mint which are pure API calls; (c) detect death via the `(403, TokenException)` signature (§4), not via a TTL clock.

**Compare: Dhan/Groww —**

| | Kite | Dhan | Groww |
|---|---|---|---|
| Token life | ~1 day, flushed ~07:30 AM IST (SEARCH) | 24h from mint (Verified — `docs/dhan-ref/02-authentication.md`) | dies daily 06:00 AM IST (Verified LIVE-DOC — `docs/groww-ref/17-token-lifecycle.md` §2) |
| Unattended mint possible? | **NO** (manual login mandated; SEARCH) | YES — TOTP+PIN API mint | YES — TOTP or checksum API mint |
| Renewal without re-login | only via `refresh_token` — NOT issued to individuals (§8) | `GET /v2/RenewToken` extends 24h (active tokens only) | re-mint anytime via API |
| Wire TTL field | none | `expiryTime` in mint response | `expiry` in mint response |
| One-active-token | Assumed yes (SEARCH) | Verified yes (documented) | Unknown (groww-ref §5.2) |

## 8. Refresh tokens — endpoints exist, individuals don't get them

**Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py):** the SDK carries a complete refresh-token surface:

- `api.token.renew` → `POST /session/refresh_token` with form fields `api_key`, `refresh_token`, `checksum` where `checksum = sha256_hex(api_key + refresh_token + api_secret)` (`renew_access_token`, lines 292–311; docstring: "Renew expired `refresh_token` using valid `refresh_token`" [sic — the docstring's own wording; it renews the *access token*]). gokiteconnect's `RenewAccessToken` (user.go lines 192–212) returns `{user_id, access_token, refresh_token}` (`UserSessionTokens`).
- `invalidate_refresh_token` → `DELETE /session/token` with `api_key` + `refresh_token` (lines 313–322).

**Who actually receives one:** **Verified (OFFICIAL-MOCK generate_session.json):** `"refresh_token": ""` — empty for the mocked individual session. **SEARCH (kite.trade forum /5394 "Refresh token generation", /14052 "How to get refresh_token", fetched 2026-07-13):** refresh_token "is provided only for the platforms that are built for mass and not for individual users" (e.g. exchange-approved platforms like smallcase); individuals "have to log in manually every day". So: the endpoints are real, but for this project's account class they are dead surface. **Compare: Dhan —** Dhan's `RenewToken` works for every account (extends the 24h JWT) — Kite's renewal path being platform-gated is a material downgrade for individual automation.

## 9. Logout / token invalidation

**Verified (CLIENT-LIB-SOURCE pykiteconnect `invalidate_access_token`, lines 280–290; gokiteconnect `InvalidateAccessToken` user.go lines 186–189):**

```
DELETE https://api.kite.trade/session/token
    params: api_key=<api_key>&access_token=<access_token>
```

(Same route name-pair as the exchange: `api.token.invalidate` maps to the same `/session/token` path with DELETE.) Response: **Verified (OFFICIAL-MOCK session_logout.json):** `{"status": "success", "data": true}`.

Voluntary invalidation matters because §7's "valid one whole day **unless you log out**" — an accidental logout (e.g. from another integration on the same api_key) kills the feed's token mid-session. **Compare: Groww —** Groww has NO documented programmatic revoke (UI-only — groww-ref 17 §5.3); Kite documents one.

## 10. User profile endpoints

- `GET /user/profile` — **Verified (CLIENT-LIB-SOURCE pykiteconnect route `user.profile` line 115 + `profile()` lines 334–336)**. Response shape **Verified (OFFICIAL-MOCK profile.json):** `user_id, user_type, email, user_name, user_shortname, broker, exchanges[], products[], order_types[], avatar_url, meta.demat_consent` — i.e. the §6 session payload minus the token fields.
- `GET /user/profile/full` — **Verified (CLIENT-LIB-SOURCE gokiteconnect connect.go `URIFullUserProfile` line 102)**; response **Verified (OFFICIAL-MOCK full_profile.json):** adds `twofa_type` ("totp"), `phone` (masked), `bank_accounts[]` (name/branch/account, masked), `dp_ids[]`, and more. NOTE: pykiteconnect@master has NO route for it — Go-SDK-only surface (**Verified-absence in connect.py `_routes`**).
- No `tokenValidity`-style field appears in either profile mock (**Verified-absence**) — unlike Dhan's `/v2/profile` which returns `tokenValidity` in `DD/MM/YYYY HH:MM` IST (`.claude/rules/dhan/authentication.md` rule 9). A Kite watchdog probing `/user/profile` gets liveness (200 vs 403/TokenException) but NOT remaining-validity.

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Exact official access-token expiry wording + flush instant.** Open https://kite.trade/docs/connect/v3/user/ and paste the "Authentication and token exchange" section verbatim. The ~07:30 AM IST daily flush is currently forum-snippet (SEARCH) only; the token-minter/feed schedule would be built on this number. (blocking: pipeline)
- **`request_token` TTL and single-use semantics.** Same page — how long is a `request_token` exchangeable, and does a second exchange fail? Matters for login-automation retry design. (blocking: pipeline)
- **One-active-access-token semantics.** Does `generate_session` #2 invalidate access_token #1 (forum snippet says yes)? Paste the official line if it exists on /docs/connect/v3/user/, else mark live-probe: mint twice, test token #1. Dual-instance-lock design depends on it. (blocking: pipeline)
- **Login-URL alias.** Does https://kite.trade/connect/login exist/redirect to https://kite.zerodha.com/connect/login? Open it in a browser and note the redirect chain. Cosmetic, but the pack should not repeat an unverified alias. (blocking: none)
- **`public_token`, `enctoken`, `silo` field semantics** in the session response. Paste the response-attribute table from /docs/connect/v3/user/. (blocking: none)
- **`login_time` timezone.** Confirm the docs state IST (wire format has no zone). (blocking: ops)
- **Redirect-URL params on login return** (e.g. `action`/`status` params alongside `request_token`). Paste the login-flow section of /docs/connect/v3/user/. (blocking: none)
- **Whether daily manual login can be satisfied by TOTP-automated browser login without ToS breach.** This is a policy question for the operator/Zerodha support, not a docs paste. (blocking: pipeline)

## CLAIMS (for README reconciled table)

- API root is `https://api.kite.trade`; login endpoint is `https://kite.zerodha.com/connect/login?api_key=…&v=3` — Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py lines 36–37, 246–248 + gokiteconnect connect.go lines 31–34, 203; fetched 2026-07-13)
- Every authenticated request carries `X-Kite-Version: 3` + `Authorization: token <api_key>:<access_token>` — Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py lines 936–945; gokiteconnect connect.go lines 236–241)
- Token exchange: `POST /session/token` with form fields `api_key`, `request_token`, `checksum = sha256_hex(api_key + request_token + api_secret)` — Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py lines 250–278; gokiteconnect user.go lines 145–168)
- The session response carries `access_token`, `public_token`, `enctoken`, `refresh_token`, `login_time` (19-char, no timezone) + full user profile — but NO expiry/TTL field — Verified (OFFICIAL-MOCK generate_session.json, kiteconnect-mocks@main) / Verified-absence for the TTL
- `refresh_token` is empty for individual accounts; refresh tokens are issued only to exchange-approved mass platforms — individuals must log in manually daily — Verified (OFFICIAL-MOCK generate_session.json `"refresh_token": ""`) + SEARCH (kite.trade/forum/discussion/5394, /14052)
- Access token is valid ~one whole day and is flushed daily at ~07:30 AM IST (~07:35 new-session availability); a token minted after 07:30 lasts the whole day — SEARCH (kite.trade forum /3468, /4816, /896, /13884; fetched 2026-07-13) — NOT doc-verbatim; top open question
- There is NO programmatic `request_token` mint: the daily interactive login on kite.zerodha.com is exchange-mandated — Verified-absence (both SDK surfaces) + SEARCH (kite.trade forum /5394)
- Session death signature: HTTP 403 + `error_type == "TokenException"`; pykiteconnect fires `session_expiry_hook` on exactly that pair — Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py lines 981–988, exceptions.py)
- Logout: `DELETE /session/token` (api_key + access_token) → `{"status":"success","data":true}` — Verified (CLIENT-LIB-SOURCE both SDKs) + Verified (OFFICIAL-MOCK session_logout.json)
- Refresh-token renewal endpoint exists: `POST /session/refresh_token` with `checksum = sha256_hex(api_key + refresh_token + api_secret)` — Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py lines 292–311; gokiteconnect user.go lines 192–212) — dead surface for individuals per the claim above
- Error envelope: `{"status":"error","error_type","message"}`; success is `{"status":"success","data":…}`; some endpoints return CSV — Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py lines 972–996 + OFFICIAL-MOCK files)
- `GET /user/profile` (both SDKs) and `GET /user/profile/full` (gokiteconnect only) return profile shapes with NO token-validity field — Verified (CLIENT-LIB-SOURCE + OFFICIAL-MOCK profile.json/full_profile.json) / Verified-absence for validity
