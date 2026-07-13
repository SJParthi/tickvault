# 17 — Token Lifecycle (Auth Flows, Daily 06:00 Expiry, Mint Wire Shapes) — CORRECTED 2026-07-13

> **Source:** https://groww.in/trade-api/docs/curl (REST intro — the auth-lifecycle authority) · https://groww.in/trade-api/docs/python-sdk (SDK intro twin)
> **Fetched/Verified:** 2026-07-13 (compiled this date; base tier = the 2026-07-03 lossless capture; live cross-checks via search-mediated reads 2026-07-13 — direct groww.in fetch 403-blocked at the sandbox egress proxy, see `16-orders-margins-portfolio.md` provenance note)
> **Related:** `01-introduction-auth-ratelimits.md` + `01-introduction-auth.md` (superseded on lifecycle — banners added 2026-07-13), `15-rate-limits-and-capacity.md`, `SSM-SETUP.md` (superseded by the token-minter lock), `.claude/rules/project/groww-shared-token-minter-2026-07-02.md` (the binding TickVault-side contract), `99-UNKNOWNS.md`

> **⚠ THIS FILE CORRECTS TWO LONG-STANDING MIS-READINGS** (2026-07-13 adversarial refutation, `refute-endpoints` claim 6): (1) the docs define **THREE** auth approaches, not two — the older files counted only the python-sdk page's two; (2) **the ~06:00 IST daily token expiry IS OFFICIALLY DOCUMENTED** — it was previously labeled "third-party only / Unknown (official)". Both corrections are byte-verified against the 2026-07-03 lossless capture of the official REST intro page.

---

## 1. THREE documented auth approaches (not two)

The **REST intro page** (https://groww.in/trade-api/docs/curl, LIVE-DOC capture 2026-07-03) documents three approaches; the **python-sdk page** documents only the last two (its "two ways you can interact with GrowwAPI"). The capture pack's own index (`00-INDEX.md`: "Auth: three approaches — direct Access Token, API Key + Secret (checksum flow), API Key + TOTP") always agreed with the REST page — the "two flows" reading in `01-introduction-auth*.md` was under-counting.

| # (REST page) | Approach | Verbatim parenthetical (LIVE-DOC, capture 2026-07-03) | Mechanism |
|---|---|---|---|
| 1st | **Access Token** (generated in the web UI) | "**(Expires daily at 6:00 AM)**" | Log in → profile → Settings → 'Trading APIs' → 'Generate API keys' → 'Access Token'. "You can create, revoke and manage all your tokens from this page" (https://groww.in/user/profile/trading-apis). No mint API call — the UI hands you the bearer token directly. |
| 2nd | **API Key and Secret Flow** | "(Uses API Key and Secret — Requires daily approval on Groww Cloud Api Keys Page)" | `POST /v1/token/api/access` with `key_type: "approval"` + SHA-256 checksum (§3.2) |
| 3rd | **TOTP Flow** | "(Uses API Key and Totp code — Requires daily approval on [Groww Cloud Api keys page])" — **but see the §5 contradiction**: the python-sdk page says "(Uses TOTP token and TOTP QR code — **No Expiry**)" | `POST /v1/token/api/access` with `key_type: "totp"` (§3.1) |

Prerequisite for all three (verbatim): "Having an active Trading API Subscription. You can purchase a subscription from this page" (https://groww.in/user/profile/trading-apis).

---

## 2. The daily 06:00 expiry — OFFICIALLY DOCUMENTED

Verbatim, LIVE-DOC (capture 2026-07-03 of https://groww.in/trade-api/docs/curl, §"1st Approach: Access Token" — capture file line 32):

> (Expires daily at 6:00 AM)

Corroboration ladder (strongest → weakest):

| Tier | Evidence |
|---|---|
| LIVE-DOC (capture 2026-07-03) | The verbatim parenthetical above, on the official REST intro page |
| LIVE-DOC (capture 2026-07-03) | The token-mint response's machine-readable **`expiry`** field (§4) — per-token TTL on the wire |
| WS (2026-07-13) | groww.in-domain-restricted live search returned "The API access token expires daily at 6:00 AM" + the same token-response fields from official pages |
| Internal production (since 2026-07-02) | The bruteX minter Lambda runs `cron(35 0 * * ? *)` ≈ **06:05 IST "right after Groww's 06:00 IST daily token reset"**; the WS side observes auth-class rejects at ~06:00 IST that self-heal after the ~06:05 mint (`groww-shared-token-minter-2026-07-02.md`) |
| 3RD-PARTY | "Token expires daily at 6:00 AM IST" (karthik1729/groww-mcp README, fetched 2026-07-13) |

Residual **Unknowns**: whether the reset is exactly 06:00:00 IST or jittered; whether a token minted at 05:59 survives past 06:00 (the §4 `expiry` field answers this — read it once live); whether the parenthetical (stated on the 1st Approach) applies with identical timing to approval/TOTP-minted tokens (Assumed yes — the wire `expiry` field is the per-token truth either way).

---

## 3. Mint wire shapes — VERBATIM (LIVE-DOC, capture 2026-07-03)

One mint endpoint serves approaches 2 and 3: `POST https://api.groww.in/v1/token/api/access`, header `Authorization: Bearer <USER_API_KEY>` (the API key — NOT an access token) + `Content-Type: application/json`. The `key_type` field selects the mode (doc note verbatim: "Use the correct type in the request body to select the authentication mode. 'approval' for api key and secret, 'totp' for api key and totp. All headers are mandatory.").

### 3.1 TOTP flow (`key_type: "totp"`)

```
curl -X POST "https://api.groww.in/v1/token/api/access" \
  -H "Authorization: Bearer <USER_API_KEY>" \
  -H "Content-Type: application/json" \
  -d '{
    "key_type": "totp",
    "totp": "<TOTP_CODE>"
  }'
```

| Parameter | Type | Description | Required |
| --- | --- | --- | --- |
| `key_type` | String | "totp" | Yes |
| `totp` | String | TOTP code generated by the user using a third-party authenticator app | Yes |

### 3.2 API Key + Secret flow (`key_type: "approval"`, checksum)

```
curl -X POST "https://api.groww.in/v1/token/api/access" \
  -H "Authorization: Bearer <USER_API_KEY>" \
  -H "Content-Type: application/json" \
  -d '{
    "key_type": "approval",
    "checksum": "<Checksum>",
    "timestamp": "1719830400"
  }'
```

| Parameter | Type | Description | Required |
| --- | --- | --- | --- |
| `key_type` | String | "approval" | Yes |
| `checksum` | String | HMAC or checksum signature | Yes |
| `timestamp` | String | Epoch seconds (10 digits) | Yes |

Checksum rule (verbatim): "Checksum should be a SHA256 hash of api secret and and latest timestamp in epoch second concatenated together." [sic — the doc's own doubled "and"] — i.e. `checksum = sha256_hex(secret + str(epoch_seconds))`. The doc's parameter note: the timestamp is "Valid for 10 minutes. Provide the same value in request." SDK equivalent (official `growwapi` 1.5.0 wheel, `client.py`, PyPI-fetched 2026-07-13): `_generate_checksum(secret, str(int(time.time())))` = `sha256((data+salt).encode("utf-8")).hexdigest()`.

---

## 4. Mint response — the machine-readable fields the SDK DISCARDS

Verbatim response + schema, LIVE-DOC (capture 2026-07-03, REST intro §"2nd Approach" Response block):

```json
{
  "token": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...",
  "tokenRefId": "ref-123",
  "sessionName": "my-session",
  "expiry": "2024-07-01T12:34:56",
  "isActive": true
}
```

| Parameter | Type | Description |
| --- | --- | --- |
| `token` | String | The generated access token |
| `tokenRefId` | String | Reference ID for the token |
| `sessionName` | String | Name of the session |
| `expiry` | String | Expiry date-time (ISO format) |
| `isActive` | Boolean | Token status |

> **⚑ Load-bearing flag:** the **`expiry`** field is a DIRECTLY CONSUMABLE per-token TTL — no consumer needs to hardcode "06:00 IST" folklore; read `expiry` at mint time. The official python SDK **discards everything but the token** (`get_access_token` returns only `response.json()["token"]` — wheel source, 1.5.0), which is why this field went unnoticed by every pre-2026-07-13 research pass. Probe: read `expiry` from one live mint to confirm it stamps 06:00 next-day.

Usage after mint — every API call: `Authorization: Bearer {ACCESS_TOKEN}` + `Accept: application/json` + `X-API-VERSION: 1.0` (all mandatory). SDK error map for the mint (and all calls): `400 → BadRequest` (surfaces `error.displayMessage`), `401 → Authentication`, `403 → Authorisation`, `429 → RateLimit`, `504 → Timeout`.

---

## 5. Documented contradictions & unknowns (recorded, not resolved)

| # | Item | The two sides | Status |
|---|---|---|---|
| 1 | **TOTP flow expiry wording** | python-sdk page (capture 2026-07-03): TOTP flow "(Uses TOTP token and TOTP QR code — **No Expiry**)" vs REST intro page (same capture date), 3rd Approach: "**Requires daily approval** on Groww Cloud Api keys page" | Contradiction in Groww's own docs. Empirically the python-sdk wording is operative: TickVault production has run the TOTP flow since 2026-07-02 with NO daily human approval (the minter Lambda mints unattended daily). The "No Expiry" describes the TOTP *credential* (key/QR seed), never the minted access token — which dies daily per §2. |
| 2 | **One-active-token semantics** | Nothing in the captured docs or SDK states whether minting a new access token invalidates the previous one (contrast: Dhan documents one-active-token explicitly). The web UI supports multiple named tokens ("create, revoke and manage all your tokens"); `sessionName`/`isActive` in §4 hint at multi-session support. Empirically BruteX + TickVault concurrently USE one shared minted token without conflict. | **Unknown** — probe: mint twice in succession; test whether token #1 still authenticates. |
| 3 | Revocation API | The UI page revokes tokens; no documented programmatic revoke endpoint. | **Unknown** |
| 4 | Auth-failure body at the reset instant | Raw HTTP status (401 vs 403) + JSON body Groww returns the moment a token dies at 06:00. Internal WS-side evidence: NATS `Authorization Violation` / connect-time rejects at ~06:00 IST. | **Unknown** (REST side) — capture one live 06:00 crossing. |

---

## 6. TickVault-side contract (binding — do not re-derive from this file)

**TickVault NEVER mints a Groww access token.** Per the operator lock `.claude/rules/project/groww-shared-token-minter-2026-07-02.md`:

- The bruteX-owned Lambda `groww-token-minter` is the SOLE minter — EventBridge `cron(35 0 * * ? *)` ≈ **06:05 IST**, which this file now shows aligns with the OFFICIALLY DOCUMENTED 06:00 daily expiry (§2) rather than mere operational lore.
- TickVault only ever **READS** the minted token from SSM `/tickvault/<env>/groww/access-token` (read-only IAM role; the api-key/totp-secret params are unreadable to TickVault by IAM).
- On any auth-class failure: drop the cached token → re-read SSM at ≥60s pacing → alert once after ~10 min → keep retrying — **never mint** (a second mint from TickVault could destabilize the shared session; §5.2's one-active-token Unknown is exactly why the lock forbids it).
- `SSM-SETUP.md` (2026-06-19) predates this lock and is superseded on Groww-credential guidance — banner added 2026-07-13.

The §3 wire shapes above are therefore REFERENCE-ONLY for TickVault (the minter Lambda and any future authorized session are the consumers); the per-session WS socket-token mint (`POST /v1/api/apex/v1/socket/token/create/`) is a separate, allowed live-feed-AUTH surface documented in `02-verified-endpoints.md` + `no-rest-except-live-feed-2026-06-27.md` §3.
