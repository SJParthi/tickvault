# DhanHQ API v2 — Historical Data (Daily + Intraday OHLC)

> **Source:** Official DhanHQ documentation (docs.dhanhq.co) — captured from DhanHQ's own "Export .md for LLMs" file, generated 2026-06-30.
> **Pages covered in this file:**
> - https://docs.dhanhq.co/api/v2/historical-data
> - https://docs.dhanhq.co/api/v2/historical-data/get-daily-historical
> - https://docs.dhanhq.co/api/v2/historical-data/get-intraday-historical


---

## Historical Data

Historical Data

This API gives you historical candle data for the desired scrip across segments & exchange. This data is presented in the form of a candle and gives you timestamp, open, high, low, close & volume.

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/charts/historical` | Get OHLC for daily timeframe |
| POST | `/charts/intraday` | Get OHLC for minute timeframe |

---

## Get Daily Historical Data

`POST` https://api.dhan.co/v2/charts/historical

Get daily OHLC candle data for backtesting and analysis.

Retrieve OHLC & Volume of daily candle for desired instrument. The data for any scrip is available back upto the date of its inception.


### Request Body Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| securityId | string | No | Exchange standard ID for each scrip. refer here |
| exchangeSegment | enum | No | Exchange & segment (see Annexure) Values: `NSE_EQ`, `NSE_FNO`, `BSE_EQ`, `BSE_FNO`, `MCX_COMM`, `IDX_I`. |
| instrument | string | No | Instrument type of the scrip (see Annexure) |
| expiryCode | int | No | Expiry of the instrument (for derivatives) |
| oi | string | No | Open Interest data for F&O: true or false |
| fromDate | string | No | Start date (YYYY-MM-DD) |
| toDate | string | No | End date (YYYY-MM-DD, non-inclusive) |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| open | float | No | Open price of the timeframe |
| high | float | No | High price in the timeframe |
| low | float | No | Low price in the timeframe |
| close | float | No | Close price of the timeframe |
| volume | int | No | Volume traded in the timeframe |
| open_interest | int | No | Open interest (for F&O instruments) |
| timestamp | int | No | Epoch timestamp |


### Status Codes

| Code | Description |
|------|-------------|
| 200 | Historical OHLC data |


### Example Request

```bash
curl -X POST https://api.dhan.co/v2/charts/historical \
  -H "access-token: <your-access-token>" \
  -H "Content-Type: application/json"
```

---

## Get Intraday Historical Data

`POST` https://api.dhan.co/v2/charts/intraday

Get intraday OHLC candle data (1, 5, 15, 30, 60 minute intervals).

Retrieve Open, High, Low, Close, OI & Volume of 1, 5, 15, 30 and 60 min candle for desired instrument for last 5 years. This data available for all exchanges and segments for all active instruments.


### Request Body Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| securityId | string | No | Exchange standard ID for each scrip. refer here |
| exchangeSegment | enum | No | Exchange & segment (see Annexure) Values: `NSE_EQ`, `NSE_FNO`, `BSE_EQ`, `BSE_FNO`, `MCX_COMM`, `IDX_I`. |
| instrument | string | No | Instrument type of the scrip (see Annexure) |
| interval | enum | No | Minute intervals Values: `1`, `5`, `15`, `30`, `60`. Default: `5`. |
| oi | string | No | Open Interest data for F&O: true or false |
| fromDate | string | No | Start date (YYYY-MM-DD) |
| toDate | string | No | End date (YYYY-MM-DD) |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| open | float | No | Open price of the timeframe |
| high | float | No | High price in the timeframe |
| low | float | No | Low price in the timeframe |
| close | float | No | Close price of the timeframe |
| volume | int | No | Volume traded in the timeframe |
| open_interest | int | No | Open interest (for F&O instruments) |
| timestamp | int | No | Epoch timestamp |


### Status Codes

| Code | Description |
|------|-------------|
| 200 | Intraday OHLC data |


### Example Request

```bash
curl -X POST https://api.dhan.co/v2/charts/intraday \
  -H "access-token: <your-access-token>" \
  -H "Content-Type: application/json"
```
