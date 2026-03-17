# Dhan API Introduction & Rate Limits Enforcement

> **Ground truth:** `docs/dhan-ref/01-introduction-and-rate-limits.md`
> **Scope:** Any file touching Dhan REST API base URLs, request headers, error response parsing, rate limiting, or HTTP client configuration.
> **Cross-reference:** `docs/dhan-ref/08-annexure-enums.md` (error codes, rate limit table)

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any Dhan REST client, HTTP request builder, error handler, or rate limiter: `Read docs/dhan-ref/01-introduction-and-rate-limits.md`.

2. **Base URL must be v2.** Always `https://api.dhan.co/v2/`. Never v1. Auth endpoint is `https://auth.dhan.co/`. WebSocket is `wss://api-feed.dhan.co`.

3. **Header must be exactly `access-token`.** Not `Authorization`, not `Bearer`, not `X-Access-Token`, not `token`. Lowercase, hyphenated, exactly `access-token`.

4. **Token must NEVER appear in URLs.** No query params containing token in REST calls. No URL logging that could expose tokens. (WebSocket connection URL is the one exception — token is in query string per Dhan's design.)

5. **GET/DELETE params go as query params. POST/PUT params as JSON body.** Content-Type for POST/PUT: `application/json`.

6. **Error response structure — exactly 3 string fields.** `errorType`, `errorCode`, `errorMessage` — all strings, all always present. Parse `errorCode` for handling action.

7. **Rate limits must match the table exactly.**
   - Order APIs: 10/sec, 250/min, 500/hr, 5000/day
   - Data APIs: 5/sec, 100,000/day
   - Quote APIs: 1/sec (unlimited per min/hr/day)
   - Non-Trading APIs: 20/sec (unlimited per min/hr/day)
   - Order modifications: max 25 per order
   - All limits are per `dhanClientId`, NOT per IP.

8. **DH-904 must trigger exponential backoff.** 10s → 20s → 40s → 80s → give up + CRITICAL alert. Never immediate retry.

9. **Data API 805 must stop ALL connections for 60s.** Then audit connection count (max 5), then reconnect one at a time.

10. **API categories matter for rate limiting.**
    - Trading APIs (free): Orders, Super Order, Forever Order, Conditional Trigger, Portfolio, EDIS, Trader's Control, Funds, Statement, Postback, Live Order Update
    - Data APIs (paid): Market Quote, Live Market Feed, Full Market Depth, Historical Data, Expired Options Data, Option Chain

## What This Prevents

- Wrong base URL → all API calls fail silently or hit deprecated endpoints
- Wrong header name → 401 on every request → system unusable
- Token in URL → token leaked in server logs, proxy logs, HTTP client logs
- Missing error field parsing → wrong handling action → API suspension risk
- Wrong rate limits → DH-904 flood → account suspension
- Immediate retry after 904 → compounding rate limit violations

## Trigger

This rule activates when editing files matching:
- `crates/trading/src/oms/api_client.rs`
- `crates/trading/src/oms/rate_limiter.rs`
- `crates/trading/src/oms/mod.rs`
- `crates/common/src/constants.rs`
- `crates/common/src/config.rs`
- `crates/common/src/error.rs`
- `crates/core/src/historical/candle_fetcher.rs`
- `crates/core/src/websocket/connection_pool.rs`
- `crates/api/src/handlers/quote.rs`
- Any file containing `DhanErrorResponse`, `access-token`, `api.dhan.co`, `RateLimiter`, `DH-904`, `DH-901`, `DHAN_API_BASE_URL`, `rate_limit`
