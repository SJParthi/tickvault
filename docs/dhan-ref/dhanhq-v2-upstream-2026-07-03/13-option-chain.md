# DhanHQ API v2 — Option Chain (with Greeks)

> **Source:** Official DhanHQ documentation (docs.dhanhq.co) — captured from DhanHQ's own "Export .md for LLMs" file, generated 2026-06-30.
> **Pages covered in this file:**
> - https://docs.dhanhq.co/api/v2/option-chain
> - https://docs.dhanhq.co/api/v2/option-chain/get-expiry-list
> - https://docs.dhanhq.co/api/v2/option-chain/get-option-chain


---

## Option Chain

Option Chain

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/optionchain` | Get Option Chain of any instrument |
| POST | `/optionchain/expirylist` | Expiry List for Options of Underlying |

> **Info:** Rate limit for Option Chain API is set to one unique request every 3 seconds. This means you can fetch entire option chain for multiple different underlying instrument or multiple expiries of same instrument concurrently every 3 seconds.

---

## Get Expiry List

`POST` https://api.dhan.co/v2/optionchain/expirylist

Get available expiry dates for an underlying.

Retrieve dates of all expiries of any underlying, for which Options Instruments are active.


### Request Body Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| UnderlyingScrip | int | Yes | Security ID of underlying instrument (see Annexure) |
| UnderlyingSeg | string | No | Exchange and segment of underlying (see Annexure) |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| data[] | array | No | All expiry dates of underlying in YYYY-MM-DD |


### Status Codes

| Code | Description |
|------|-------------|
| 200 | List of expiry dates |


### Example Request

```bash
curl -X POST https://api.dhan.co/v2/optionchain/expirylist \
  -H "access-token: <your-access-token>" \
  -H "client-id: <your-client-id>" \
  -H "Content-Type: application/json"
```

---

## Get Option Chain

`POST` https://api.dhan.co/v2/optionchain

Retrieve real-time Option Chain across exchanges for all underlying. You can fetch OI, Greeks, Volume, LTP, best bid/ask and IV across all strikes.

Retrieve real-time Option Chain across exchanges for all underlying. You can fetch Open Interest (OI), Greeks, Volume, Last Traded Price, Best Bid/Ask and Implied Volatility (IV) across all strikes for any underlying.


### Request Body Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| UnderlyingScrip | int | Yes | Security ID of underlying instrument (see Annexure) |
| UnderlyingSeg | string | No | Exchange and segment of underlying (see Annexure) |
| Expiry | string | No | Expiry date of option in YYYY-MM-DD (active expiries can be fetched from Expiry List API) |


### Status Codes

| Code | Description |
|------|-------------|
| 200 | Option chain data |


### Example Request

```bash
curl -X POST https://api.dhan.co/v2/optionchain \
  -H "access-token: <your-access-token>" \
  -H "client-id: <your-client-id>" \
  -H "Content-Type: application/json"
```
