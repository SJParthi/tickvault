# Dhan V2 Introduction & Rate Limits

> **Source**: https://dhanhq.co/docs/v2/
> **Extracted**: 2026-03-13
> **Related**: `08-annexure-enums.md` (error codes, enums)

---

## 1. Overview

DhanHQ API v2 is a set of REST-like APIs + WebSockets for trading and market data. JSON request/response for REST. Binary response for WebSocket feeds.

**Base URL**: `https://api.dhan.co/v2/`

All requests require `access-token` header. GET/DELETE params go as query params. POST/PUT params as JSON body.

> **SDK note (v2.2.0rc1):** The Dhan API (Python SDK ref) now sends both `access-token` AND `client-id` headers on ALL requests by default (not just Market Quote / Option Chain). Consider sending `client-id` on all requests for forward compatibility.

```
curl --request POST \
--url https://api.dhan.co/v2/ \
--header 'Content-Type: application/json' \
--header 'access-token: JWT' \
--header 'client-id: DHAN_CLIENT_ID' \
--data '{Request JSON}'
```

---

## 2. Error Response Structure

```json
{
    "errorType": "",
    "errorCode": "",
    "errorMessage": ""
}
```

See `08-annexure-enums.md` Sections 10–11 for all error codes.

---

## 3. Rate Limits

| Rate Limit  | Order APIs | Data APIs | Quote APIs | Non Trading APIs |
|-------------|-----------|-----------|------------|------------------|
| per second  | 10        | 5         | 1          | 20               |
| per minute  | 250       | —         | Unlimited  | Unlimited        |
| per hour    | 1000      | —         | Unlimited  | Unlimited        |
| per day     | 7000      | 100000    | Unlimited  | Unlimited        |

- Order modifications capped at **25 per order**
- Quote APIs: 1/sec but unlimited per minute/hour/day
- Data APIs (Historical, Option Chain): 5/sec, 100K/day

---

## 4. API Categories

**Trading APIs** (free for all Dhan users):
Orders, Super Order, Forever Order, Conditional Trigger, Portfolio & Positions, EDIS, Trader's Control, Funds & Margin, Statement, Postback, Live Order Update

**Data APIs** (additional charges):
Market Quote, Live Market Feed, Full Market Depth, Historical Data, Expired Options Data, Option Chain

---

## 5. Python SDK Quick Start

```bash
pip install dhanhq
```

```python
from dhanhq import DhanContext, dhanhq

dhan_context = DhanContext("client_id", "access_token")
dhan = dhanhq(dhan_context)
```

---

## 2026-07-14 Upstream Update (runner-crawled live pages) — rate limits verbatim on both surfaces; the transposed table is LIVE portal corruption

**Evidence tier: Verified-live.** Raw HTML of `https://dhanhq.co/docs/v2/` (runs 1–3, sha256
`d777e139` content-identical, latest 2026-07-14T07:57:28Z) + the portal artifacts
`docs.dhanhq.co/markdown/api/v2.md` (07:58:50Z), `…/markdown/api/v2/guides/rate-limits.md`
(08:01:34Z), `docs-export.md` (07:58:44Z), and the OpenAPI yaml (07:58:48Z). Full manifest:
`00-COVERAGE-MANIFEST.md`.

1. **The canonical rate-limit table is CONFIRMED verbatim on BOTH surfaces' primary pages.**
   Classic root page, text-extracted verbatim:

   | Rate Limit | Order APIs | Data APIs | Quote APIs | Non Trading APIs |
   |---|---|---|---|---|
   | per second | 10 | 5 | 1 | 20 |
   | per minute | 250 | - | Unlimited | Unlimited |
   | per hour | 1000 | - | Unlimited | Unlimited |
   | per day | 7000 | 100000 | Unlimited | Unlimited |

   plus "Order Modifications are capped at 25 modifications/order". The portal's own
   `api/v2.md` Getting-Started index carries the IDENTICAL table + the 25-mods info box
   (also present verbatim inside docs-export.md at ~line 478).
2. **The TRANSPOSED table is LIVE portal corruption, not merely an export artifact:** the
   portal's dedicated `guides/rate-limits.md` page STILL serves Order 100,000/day + Data
   7,000/day (the Order↔Data dailies swapped), omits the per-min/per-hr Order tiers, omits
   Non-Trading 20/sec and the 25-mods cap, and invents an `RL001 / RATE_LIMIT_ERROR` shape
   seen nowhere else; the OpenAPI yaml's intro description carries the SAME transposed table;
   docs-export.md contains BOTH tables (canonical ~line 478, transposed ~line 6293) — the
   portal is internally inconsistent. Trust order: classic root = portal `api/v2.md` index >
   `guides/rate-limits.md` (demonstrably corrupt). The repo limiter's numbers are unchanged
   and correct.
3. **Prose wording drift (LOW):** the live classic page says "POST and PUT parameters as
   form-encoded" (while also saying the APIs accept "JSON or form-encoded" and its own curl
   sends `Content-Type: application/json` JSON) — JSON remains sanctioned; the repo's
   JSON-body guidance stands.
4. **Per-clientId reading:** neither surface says "per dhanClientId" explicitly; the classic
   annexure DH-904 text says "single user" and the live-feed docs say "per user" — the repo's
   per-clientId reading is consistent with "per user", not contradicted.
5. **Python quick-start:** the classic page still shows the OLD constructor
   (`dhan = dhanhq("client_id","access_token")`); the repo's DhanContext snippet matches the
   current SDK + the new portal — a live-legacy-page lag; keep the repo snippet.
