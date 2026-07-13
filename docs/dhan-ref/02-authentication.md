# Dhan V2 Authentication — Complete Reference

> **Source**: https://dhanhq.co/docs/v2/authentication/
> **Extracted**: 2026-03-13
> **Related**: `01-introduction-and-rate-limits.md`

---

## 1. Overview

All DhanHQ APIs require `access-token` header. Two user types: Individual (direct traders) and Partners (platforms).

Trading APIs: **free** for all Dhan users. Data APIs: **additional charges** required.

---

## 2. Access Token (Individual — Simple Method)

Generate directly from Dhan Web → My Profile → Access DhanHQ APIs. Valid for **24 hours**.

### 2.1 Generate Token via API (requires TOTP enabled)

```
POST https://auth.dhan.co/app/generateAccessToken?dhanClientId={ID}&pin={PIN}&totp={TOTP}
```

| Parameter     | Description                        |
|---------------|------------------------------------|
| `dhanClientId`| Client ID                          |
| `pin`         | 6-digit Dhan Pin                   |
| `totp`        | TOTP from authenticator app        |

**Response:**
```json
{
    "dhanClientId": "1000000401",
    "dhanClientName": "JOHN DOE",
    "dhanClientUcc": "ABCD12345E",
    "givenPowerOfAttorney": false,
    "accessToken": "eyJ...",
    "expiryTime": "2026-01-01T00:00:00.000"
}
```

### 2.2 Renew Token

```
curl --location 'https://api.dhan.co/v2/RenewToken' \
--header 'access-token: {JWT Token}' \
--header 'dhanClientId: {Client ID}'
```

- Only renews **active** tokens. Expired tokens return error.
- Extends validity by another 24 hours.

> **Important**: RenewToken may only work for tokens generated via Dhan Web, not for tokens generated programmatically via the `generateAccessToken` API (TOTP method). For TOTP-generated tokens, prefer regenerating via `generateAccessToken` instead. Our code correctly falls back to `acquire_token()` when renewal fails.

---

## 3. API Key & Secret (Individual — OAuth Flow)

Generate API key & secret from Dhan Web. Valid for **12 months**.

### Step 1: Generate Consent

```
POST https://auth.dhan.co/app/generate-consent?client_id={dhanClientId}
Headers: app_id: {API key}, app_secret: {API secret}
```

Response: `{ "consentAppId": "uuid", "consentAppStatus": "GENERATED" }`

> Max 25 `consentAppId` per day. Only one active token at a time.

### Step 2: Browser Login

```
https://auth.dhan.co/login/consentApp-login?consentAppId={consentAppId}
```

User logs in → redirected to your URL with `tokenId` as query param.

### Step 3: Consume Consent

```
curl --location 'https://auth.dhan.co/app/consumeApp-consent?tokenId={Token ID}' \
--header 'app_id: {API Key}' \
--header 'app_secret: {API Secret}'
```

Response: access token + expiry time.

> **SDK note:** Dhan API (Python SDK ref) (v2.2.0) uses GET for this endpoint instead of POST (the curl example above omits `-X POST`). The HTTP method may be flexible; verify with actual API behavior.

---

## 4. Partner Authentication

Three-step flow similar to OAuth. Uses `partner_id` and `partner_secret`.

### Step 1: Generate Consent
```
POST https://auth.dhan.co/partner/generate-consent
Headers: partner_id, partner_secret
```

### Step 2: Browser Login
```
https://auth.dhan.co/consent-login?consentId={consentId}
```

### Step 3: Consume Consent
```
POST https://auth.dhan.co/partner/consume-consent?tokenId={Token ID}
Headers: partner_id, partner_secret
```

---

## 5. Static IP Whitelisting

**Mandatory** per SEBI/exchange guidelines for Order Placement APIs only. Not required for data fetching or non-order APIs.

### Set IP
```
POST https://api.dhan.co/v2/ip/setIP
Headers: access-token
Body: { "dhanClientId": "...", "ip": "10.200.10.10", "ipFlag": "PRIMARY" }
```

### Modify IP
```
PUT https://api.dhan.co/v2/ip/modifyIP
```
Once set, cannot modify for **7 days**.

### Get IP
```
GET https://api.dhan.co/v2/ip/getIP
```

Response:
```json
{
    "modifyDatePrimary": "yyyy-MM-dd",
    "modifyDateSecondary": "yyyy-MM-dd",
    "primaryIP": "10.200.10.10",
    "secondaryIP": "",
    "detectedIP": "203.0.113.42",
    "ipMatchStatus": "PRIMARY_MATCH",
    "ordersAllowed": true
}
```

| Field                | Type    | Description                                           |
|----------------------|---------|-------------------------------------------------------|
| `modifyDatePrimary`  | string  | Date when primary IP can next be modified (yyyy-MM-dd)|
| `modifyDateSecondary`| string  | Date when secondary IP can next be modified           |
| `primaryIP`          | string  | Whitelisted primary IP address                        |
| `secondaryIP`        | string  | Whitelisted secondary IP address (empty if not set)   |
| `detectedIP`         | string  | IP address detected by Dhan API for this request      |
| `ipMatchStatus`      | string  | `PRIMARY_MATCH`, `SECONDARY_MATCH`, or mismatch value |
| `ordersAllowed`      | boolean | `true` if orders can be placed from current IP        |

> Supports both IPv4 and IPv6. Primary + Secondary IP per account. Each user needs unique static IP.

### Static IP Enforcement — MANDATORY April 1, 2026

**SEBI/Exchange enforcement change:** Effective April 1, 2026, orders submitted from IPs
NOT on the whitelist are **REJECTED by the exchange**. No grace period.

Affected endpoints: Place/Modify/Cancel orders, Super Orders, Forever Orders.

**Pre-market check must verify:** Call `GET /v2/ip/getIP` and assert `ordersAllowed == true`
before market open. If `false` → CRITICAL Telegram alert.

---

## 6. TOTP Setup

- Go to Dhan Web → DhanHQ Trading APIs → Setup TOTP
- Scan QR in authenticator app
- 6-digit code refreshes every 30 seconds (RFC 6238)
- Required for programmatic token generation (Section 2.1)

---

## 7. User Profile (Test API)

Great for validating access token.

```
GET https://api.dhan.co/v2/profile
Headers: access-token: {JWT}
```

```json
{
    "dhanClientId": "1100003626",
    "tokenValidity": "30/03/2025 15:37",
    "activeSegment": "Equity, Derivative, Currency, Commodity",
    "ddpi": "Active",
    "mtf": "Active",
    "dataPlan": "Active",
    "dataValidity": "2024-12-05 09:37:52.0"
}
```

> **SDK Note**: The Dhan API (Python SDK ref) (v2.2.0+) sends both `access-token` AND `dhanClientId` headers for the profile endpoint, although the official docs only require `access-token`.

---

## 8. Implementation Notes

1. **Token expiry is IST-based** — `expiryTime` field is in IST.
2. **One active token at a time** — generating new token invalidates the old one.
3. **Static IP only for orders** — data APIs, market feed, portfolio all work without IP whitelisting.
4. **TOTP enables automation** — without TOTP, token must be manually generated from web UI daily.
5. **For tickvault**: Use TOTP method for daily auto-renewal. Generate token at pre-market (before 9:00 AM IST), renew before expiry.

---

## 2026-07-13 Upstream Update — RenewToken (§2.2) — UNVERIFIED-LIVE

Per repo convention this dated section supersedes without rewriting §2.2 above. Evidence is
search-index-relayed (Dhan hosts proxy-blocked from the verifying sandbox) plus direct fetches of
the official Python SDK; **no live API call was made — live-probe before ANY code change**. Full
evidence chain: `verification-2026-07-13.md` §4 flag 3.

1. **New docs portal page:** RenewToken is now also documented at
   `https://docs.dhanhq.co/api/v2/authentication/renew-token` (alongside dhanhq.co/docs/v2).
2. **Semantics — renew ISSUES A NEW token** (2 independent sources; UNVERIFIED-LIVE): the
   live-indexed page states "This API **expires your current token and provides you with a new
   token** with another 24 hours of validity"; the official SDK docstring likewise returns "the
   **new** access token". This supersedes §2.2's "Extends validity by another 24 hours"
   same-token reading. The one-active-token invariant (§8.2) is unaffected — consumers must
   simply adopt the RESPONSE token rather than assuming the old JWT string stays valid.
3. **Web-tokens-only now stated plainly:** "You can use this only for tokens generated from
   Dhan Web" — upgrades §2.2's hedged Important note from inference to (indexed) upstream text.
4. **HTTP verb DISPUTED — GET remains best-supported; do NOT flip to POST without a live
   probe:** one search summary of the new page claimed "Method: POST", but a second summary of
   the SAME page said its curl example (no `-X`) indicates GET; the official SDK — main branch
   AND the released 2.3.0rc1 sdist (PyPI direct fetch) — calls `requests.get` on
   `/v2/RenewToken`; a community POST attempt (DhanHQ-py issue #108, 2025-10-25) received HTTP
   400. §2.2's curl (default GET) therefore stands. Any future change requires probing BOTH
   verbs from the prod box (never from a sandbox — no token exists there and minting one would
   invalidate the live prod token).
5. **Runtime risk is bounded either way:** tickvault's TokenManager already runs
   RenewToken-then-`generateAccessToken` fallback, so a wrong verb or the new-token semantics
   degrades to one extra mint, not an outage.
