<!-- ═══════════════ REPO-ADDED NOTE 2026-07-13 (snapshot body below is UNTOUCHED) ═══════════════ -->
> **⚠ REPO NOTE 2026-07-13 — THIS CAPTURE IS UNRELIABLE; canonical limits stand.**
> The daily values in the Rate Limit Table below (~lines 20-21) are **TRANSPOSED** versus the
> canonical `docs/dhan-ref/01-introduction-and-rate-limits.md` AND the older snapshot
> `dhanhq-v2-upstream-2026-06-02/01_introduction.md`: both say **Order APIs 7,000/day** and
> **Data APIs 100,000/day**; this file says the opposite. This file's `RATE_LIMIT_ERROR`/`RL001`
> error shape also appears NOWHERE else (the canonical rate-limit error is **DH-904**). Verdict
> (2026-07-13 verification, independently reproduced): an isolated capture-quality defect in
> Dhan's docs.dhanhq.co "Export .md for LLMs" generator — **NOT evidence of a real limit
> change**. Treat this capture (and the whole `dhanhq-v2-upstream-2026-07-03/` LLM-export
> snapshot dir) as suspect; the canonical limits stand (Order 7,000/day; Data 100,000/day).
> See `docs/dhan-ref/verification-2026-07-13.md` §4 flag 1. Snapshot body preserved verbatim below.
>
> **2026-07-14 update (runner-crawled):** the transposition is now PROVEN LIVE on the portal
> itself — the rendered `docs.dhanhq.co/markdown/api/v2/guides/rate-limits.md` (fetched
> 2026-07-14T08:01:34Z) AND the OpenAPI yaml's intro description carry the same swapped
> dailies, while BOTH surfaces' PRIMARY tables (classic dhanhq.co/docs/v2 root + the portal's
> own api/v2.md index) confirm the canonical values verbatim (Order 7,000/day; Data
> 100,000/day). So: live portal-page corruption on the dedicated guide, not merely an export
> artifact — and still contradicted by both surfaces' primary tables; the canonical limits
> stand unchanged. See `docs/dhan-ref/01-introduction-and-rate-limits.md` "2026-07-14
> Upstream Update".
<!-- ═══════════════════════════════ END REPO-ADDED NOTE 2026-07-13 ═══════════════════════════════ -->

# DhanHQ API v2 — Rate Limits

> **Source:** Official DhanHQ documentation (docs.dhanhq.co) — captured from DhanHQ's own "Export .md for LLMs" file, generated 2026-06-30.
> **Pages covered in this file:**
> - https://docs.dhanhq.co/api/v2/guides/rate-limits


---

## Rate Limits

DhanHQ API rate limits and best practices

DhanHQ enforces rate limits to ensure fair usage and platform stability.

## Rate Limit Table

| API Category | Per Second | Daily Limit |
|--------------|------------|-------------|
| **Order APIs** | 10 requests | 100,000 |
| **Data APIs** | 5 requests | 7,000 |
| **Market Quote** | 1 request | — |
| **Option Chain** | 1 per 3 sec | — |

## Market Quote Limits

- Maximum **1,000 instruments** per request for LTP/OHLC/Quote endpoints
- **1 request per second** across all market quote endpoints

## Option Chain Limits

- **1 request per 3 seconds** per underlying/expiry combination

## Handling Rate Limits

When you exceed the rate limit, the API returns:

```json
{
  "status": "failure",
  "errorType": "RATE_LIMIT_ERROR",
  "errorCode": "RL001",
  "errorMessage": "Too many requests. Please retry after 1 second."
}
```

### Best Practices

1. **Batch requests** — Use multi-instrument endpoints (e.g., LTP for multiple instruments in one call)
2. **Cache data** — Cache market data locally and refresh at appropriate intervals
3. **Exponential backoff** — When rate limited, wait and retry with increasing delays
4. **Queue orders** — Don't fire all orders simultaneously; space them out

### Python Example with Rate Limiting

```python
import time
from dhanhq import DhanContext, dhanhq

context = DhanContext("client_id", "access_token")
dhan = dhanhq(context)

# Get LTP for multiple instruments in a single call
instruments = [
    ("NSE_EQ", "1333"),
    ("NSE_EQ", "11536"),
    ("NSE_EQ", "2885"),
]
ltp = dhan.get_ltp(instruments)

# Space out order placements
orders = [...]  # your order list
for order in orders:
    response = dhan.place_order(**order)
    print(response)
    time.sleep(0.1)  # 100ms delay between orders
```
