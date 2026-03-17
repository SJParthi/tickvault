# Dhan V2 Introduction & Rate Limits

> **Source**: https://dhanhq.co/docs/v2/
> **Extracted**: 2026-03-13
> **Related**: `08-annexure-enums.md` (error codes, enums)

---

## 1. Overview

DhanHQ API v2 is a set of REST-like APIs + WebSockets for trading and market data. JSON request/response for REST. Binary response for WebSocket feeds.

**Base URL**: `https://api.dhan.co/v2/`

All requests require `access-token` header. GET/DELETE params go as query params. POST/PUT params as JSON body.

```
curl --request POST \
--url https://api.dhan.co/v2/ \
--header 'Content-Type: application/json' \
--header 'access-token: JWT' \
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
| per hour    | 500       | —         | Unlimited  | Unlimited        |
| per day     | 5000      | 100000    | Unlimited  | Unlimited        |

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
