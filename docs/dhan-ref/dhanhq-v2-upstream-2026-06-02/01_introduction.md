# Introduction — DhanHQ v2

> Source: https://dhanhq.co/docs/v2/ · API v2 · Audited 2 Jun 2026

DhanHQ API is a set of REST-like APIs for building trading and investment services. It lets you execute & modify orders in real time, manage portfolio, and access live market data. URLs are resource-based and accept JSON (or form-encoded) requests; responses are JSON with standard HTTP status codes.

## Getting started
- **Developer Kit / interactive spec:** https://api.dhan.co/v2/
- **Python client:** https://pypi.org/project/dhanhq/

## Structure

### REST
GET and DELETE parameters go as **query parameters**; POST and PUT parameters as **form-encoded / JSON body**. An access token must be sent in the header on every request.
```bash
curl --request POST \
  --url https://api.dhan.co/v2/ \
  --header 'Content-Type: application/json' \
  --header 'access-token: JWT' \
  --data '{Request JSON}'
```

### Python
```bash
pip install dhanhq
```
```python
from dhanhq import dhanhq
dhan = dhanhq("client_id", "access_token")
```
(See `15_python_sdk_dhanhq.md` — the current 2.x SDK uses a `DhanContext` instead of raw strings.)

## Errors
Error responses carry an internally generated code and message:
```json
{ "errorType": "", "errorCode": "", "errorMessage": "" }
```
Full code list in `08_annexure.md` (Trading API errors `DH-9xx`, Data API errors `8xx`).

## Rate limit
| Window | Order APIs | Data APIs | Quote APIs | Non-Trading APIs |
|---|---|---|---|---|
| per second | 10 | 5 | 1 | 20 |
| per minute | 250 | — | Unlimited | Unlimited |
| per hour | 1000 | — | Unlimited | Unlimited |
| per day | 7000 | 100000 | Unlimited | Unlimited |

- Order modifications are capped at **25 modifications per order**.

## Account access (summary)
- All Dhan users get **Trading APIs free**.
- **Data APIs are a paid add-on** (charges shown on the platform).
- Partners (algo platforms, fintechs, banks, PMS) onboard via the form at https://dhanhq.co/trading-apis.
