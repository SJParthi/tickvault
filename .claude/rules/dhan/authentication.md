# Dhan Authentication Enforcement

> **Ground truth:** `docs/dhan-ref/02-authentication.md`
> **Scope:** Any file touching token generation, renewal, TOTP, static IP, user profile, or auth headers.
> **Cross-reference:** `docs/dhan-ref/01-introduction-and-rate-limits.md` (header format), `docs/dhan-ref/08-annexure-enums.md` (DH-901 error)

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any token handling, auth flow, or profile check: `Read docs/dhan-ref/02-authentication.md`.

2. **Token is JWT, 24h validity.** Format starts with `eyJ...`. Header name: see `dhan-api-introduction.md` rule 3. Goes in HTTP header ONLY — never in URL query params for REST, never in request body, never in logs.

3. **Token must NEVER be logged, stored in DB, or exposed.** Not in QuestDB, not in Valkey keys (only values, encrypted), not in Prometheus metrics, not in tracing spans, not in error messages. Use `Secret<String>` from secrecy crate.

4. **Generate token via API.**
   - Endpoint: `POST https://auth.dhan.co/app/generateAccessToken?dhanClientId={ID}&pin={PIN}&totp={TOTP}`
   - Response fields: `dhanClientId`, `dhanClientName`, `dhanClientUcc`, `givenPowerOfAttorney`, `accessToken`, `expiryTime`
   - `expiryTime` format: ISO-like `"2026-01-01T00:00:00.000"` (IST-based)

5. **RenewToken — exact endpoint and behavior.**
   - `GET https://api.dhan.co/v2/RenewToken` with headers: `access-token: {JWT}`, `dhanClientId: {Client ID}`
   - Only renews ACTIVE tokens. Expired tokens return error.
   - Extends validity by another 24 hours.
   - **One active token at a time** — generating new token invalidates the old one.
   - **2026-07-13 note (UNVERIFIED-LIVE — search-index + SDK relayed):** the new
     docs.dhanhq.co renew-token page describes renewal as **expiring the current token and
     issuing a NEW one** (adopt the response token; the one-active-token invariant is
     unchanged) and states plainly it works **only for tokens generated via Dhan Web**. One
     search summary claimed the verb is POST, but the official SDK (2.3.0rc1) and the docs
     curl example both indicate **GET** — so GET stands here; live-probe BOTH verbs from the
     prod box before changing this rule or any code. Runtime risk is bounded by the existing
     RenewToken→generateAccessToken fallback. See `docs/dhan-ref/02-authentication.md`
     "2026-07-13 Upstream Update" + `docs/dhan-ref/verification-2026-07-13.md` §4 flag 3.

6. **API Key & Secret flow (OAuth-style, 12-month validity).**
   - Step 1: `POST https://auth.dhan.co/app/generate-consent?client_id={dhanClientId}` with headers `app_id`, `app_secret`. Max 25 consentAppId per day.
   - Step 2: Browser login at `https://auth.dhan.co/login/consentApp-login?consentAppId={consentAppId}`
   - Step 3: `POST/GET https://auth.dhan.co/app/consumeApp-consent?tokenId={Token ID}` with headers `app_id`, `app_secret`. Note: The DhanHQ Python SDK uses GET. The official docs suggest POST. Both may work.

7. **Static IP — MANDATORY for Order APIs (ENFORCED April 1, 2026).**
   - Set: `POST https://api.dhan.co/v2/ip/setIP` — body: `{ "dhanClientId": "...", "ip": "...", "ipFlag": "PRIMARY" }`
   - Modify: `PUT https://api.dhan.co/v2/ip/modifyIP` — **7-day cooldown** after modification
   - Get: `GET https://api.dhan.co/v2/ip/getIP` — response includes `detectedIP`, `ipMatchStatus`, `ordersAllowed`
   - Supports IPv4 and IPv6. Primary + Secondary IP per account.
   - NOT required for Data APIs, Market Feed, Portfolio, or non-order APIs.
   - **April 1, 2026 enforcement:** Orders from unregistered IPs are REJECTED by the exchange. No grace period. Pre-market check MUST call GetIP and verify `ordersAllowed == true`.

8. **TOTP — 6-digit code, 30-second window, RFC 6238.**
   - Secret in environment/SSM only. Never hardcoded. Never in .env committed to git.
   - Generate code close to API call time (within 30s window).
   - Required for programmatic token generation (Section 2.1 of ground truth).

9. **User Profile — validation endpoint.**
   - `GET https://api.dhan.co/v2/profile` with `access-token` header
   - Response: `dhanClientId`, `tokenValidity` (format: `"DD/MM/YYYY HH:MM"` IST — NOT ISO format), `activeSegment`, `ddpi`, `mtf`, `dataPlan`, `dataValidity`

10. **Pre-market check at 08:45 AM IST must verify:**
    - `dataPlan == "Active"`
    - `activeSegment` contains `"Derivative"`
    - Token has > 4 hours until expiry
    - If any fail → rotate token or CRITICAL alert

11. **DH-901 auth-specific handling:** Rotate token → retry ONCE → if still fails → HALT + CRITICAL alert. (Error code details: `dhan-annexure-enums.md` rule 11.)

## What This Prevents

- Token leak → unauthorized trading on your account
- Expired token during market hours → system goes dark, missed signals
- Silent retry with expired token → infinite 401 loop
- Wrong `tokenValidity` parsing (DD/MM/YYYY vs ISO) → incorrect expiry check → surprise token death
- TOTP secret in code → secret committed to git → compromised account
- Missing pre-market check → data plan expired → WebSocket rejected → no market data
- IP modification during trading → 7-day cooldown → locked out of order APIs

## Trigger

This rule activates when editing files matching:
- `crates/core/src/auth/*.rs`
- `crates/common/src/config.rs`
- `crates/core/src/network/ip_monitor.rs`
- `crates/core/src/network/ip_verifier.rs`
- `crates/core/src/websocket/connection.rs` (token usage in WS auth)
- `crates/trading/src/oms/api_client.rs` (token usage in REST)
- `scripts/setup-secrets.sh`
- Any file containing `access_token`, `RenewToken`, `generateAccessToken`, `UserProfileResponse`, `GenerateTokenResponse`, `totp`, `dataPlan`, `tokenValidity`, `token_manager`, `token_cache`, `secret_manager`, `ip_setIP`, `ip_modifyIP`
