# DhanHQ API v2 — Market Quote (REST Snapshots: LTP / OHLC / Full Quote)

> **Source:** Official DhanHQ documentation (docs.dhanhq.co) — captured from DhanHQ's own "Export .md for LLMs" file, generated 2026-06-30.
> **Pages covered in this file:**
> - https://docs.dhanhq.co/api/v2/market-quote
> - https://docs.dhanhq.co/api/v2/market-quote/get-ltp
> - https://docs.dhanhq.co/api/v2/market-quote/get-ohlc
> - https://docs.dhanhq.co/api/v2/market-quote/get-quote


---

## Market Quote

Market Quote

This API gives you snapshots of multiple instruments at once. You can fetch LTP, Quote or Market Depth of instruments via API which sends real time data at the time of API request.

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/marketfeed/ltp` | Get ticker data of instruments |
| POST | `/marketfeed/ohlc` | Get OHLC data of instruments |
| POST | `/marketfeed/quote` | Get market depth data of instruments |

> **Info:** You can fetch upto 1000 instruments in single API request with rate limit of 1 request per second.

---

## Get LTP

`POST` https://api.dhan.co/v2/marketfeed/ltp

Get last traded price for multiple instruments.

Retrieve LTP for list of instruments with single API request


### Request Body Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| NSE_EQ | int-array | No | List of NSE Equity security IDs as integers (comma-separated, e.g. 1333, 11536) |
| NSE_FNO | int-array | No | List of NSE F&O security IDs as integers (comma-separated, e.g. 49081, 49082) |
| BSE_EQ | int-array | No | List of BSE Equity security IDs as integers (comma-separated) |


### Status Codes

| Code | Description |
|------|-------------|
| 200 | LTP data |


### Example Request

```bash
curl --request POST \
  --url https://api.dhan.co/v2/marketfeed/ltp \
  --header 'Accept: application/json' \
  --header 'Content-Type: application/json' \
  --header 'access-token: <your-access-token>' \
  --header 'client-id: <your-client-id>' \
  --data '{
    "NSE_EQ": [11536],
    "NSE_FNO": [49081, 49082]
  }'
```

---

## Get OHLC

`POST` https://api.dhan.co/v2/marketfeed/ohlc

Get OHLC (Open, High, Low, Close) data for instruments.

Retrieve the Open, High, Low and Close price along with LTP for specified list of instruments.


### Request Body Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| NSE_EQ | int-array | No | List of NSE Equity security IDs as integers (comma-separated, e.g. 1333, 11536) |
| NSE_FNO | int-array | No | List of NSE F&O security IDs as integers (comma-separated, e.g. 49081, 49082) |
| BSE_EQ | int-array | No | List of BSE Equity security IDs as integers (comma-separated) |


### Status Codes

| Code | Description |
|------|-------------|
| 200 | OHLC data |


### Example Request

```bash
curl --request POST \
  --url https://api.dhan.co/v2/marketfeed/ohlc \
  --header 'Accept: application/json' \
  --header 'Content-Type: application/json' \
  --header 'access-token: <your-access-token>' \
  --header 'client-id: <your-client-id>' \
  --data '{
    "NSE_EQ": [11536]
  }'
```

---

## Get Full Quote

`POST` https://api.dhan.co/v2/marketfeed/quote

Get full market quote including market depth (5 levels).

Retrieve full details including market depth, OHLC data, Open Interest and Volume along with LTP for specified instruments.


### Request Body Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| NSE_EQ | int-array | No | List of NSE Equity security IDs as integers (comma-separated, e.g. 1333, 11536) |
| NSE_FNO | int-array | No | List of NSE F&O security IDs as integers (comma-separated, e.g. 49081, 49082) |
| BSE_EQ | int-array | No | List of BSE Equity security IDs as integers (comma-separated) |


### Status Codes

| Code | Description |
|------|-------------|
| 200 | Full quote with depth |


### Example Request

```bash
curl --request POST \
  --url https://api.dhan.co/v2/marketfeed/quote \
  --header 'Accept: application/json' \
  --header 'Content-Type: application/json' \
  --header 'access-token: <your-access-token>' \
  --header 'client-id: <your-client-id>' \
  --data '{
    "NSE_EQ": [11536]
  }'
```
