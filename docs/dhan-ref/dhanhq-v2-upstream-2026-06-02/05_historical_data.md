# Historical Data — DhanHQ v2

> Source: https://dhanhq.co/docs/v2/historical-data/ · API v2 · Audited 2 Jun 2026

Historical candle data (timestamp, open, high, low, close, volume, + optional OI) across segments & exchanges. Responses are **column-oriented arrays** (positionally aligned: `open[i]`/`high[i]`/… share `timestamp[i]`).

| Method | Path | Use |
|---|---|---|
| POST | `/charts/historical` | OHLC for daily timeframe |
| POST | `/charts/intraday` | OHLC for minute timeframe |

## Daily Historical Data — `POST /charts/historical`
OHLC & volume of daily candles. Data is available **back to the instrument's inception date**.
```bash
curl --request POST --url https://api.dhan.co/v2/charts/historical \
  --header 'Content-Type: application/json' --header 'access-token: JWT' --data '{}'
```
**Request**
```json
{
  "securityId": "1333",
  "exchangeSegment": "NSE_EQ",
  "instrument": "EQUITY",
  "expiryCode": 0,
  "oi": false,
  "fromDate": "2022-01-08",
  "toDate": "2022-02-08"
}
```
| Field | Type | Description |
|---|---|---|
| `securityId` (req) | string | From instrument master |
| `exchangeSegment` (req) | enum string | See `08_annexure.md` |
| `instrument` (req) | enum string | See `08_annexure.md` |
| `expiryCode` (optional) | enum int | Derivatives expiry (`0`/`1`/`2`) |
| `oi` (optional) | boolean | Open Interest for F&O |
| `fromDate` (req) | string | Start date (`YYYY-MM-DD`) |
| `toDate` (req) | string | End date — **non-inclusive** |

**Response** (`open/high/low/close/volume/timestamp/open_interest`, all arrays):
```json
{
  "open": [3978, 3856, ...],
  "high": [3978, 3925, ...],
  "low":  [3861, 3856, ...],
  "close":[3879.85, 3915.9, ...],
  "volume":[3937092, 1906106, ...],
  "timestamp":[1326220200, 1326306600, ...],
  "open_interest":[0,0, ...]
}
```
| Field | Type | Description |
|---|---|---|
| `open/high/low/close` | float | OHLC of the timeframe |
| `volume` | int | Volume traded |
| `timestamp` | int | EPOCH (Unix) seconds — see note below |

## Intraday Historical Data — `POST /charts/intraday`
OHLC + OI + volume of **1, 5, 15, 25, 60-minute** candles for **the last ~5 years**, all exchanges/segments, all active instruments.
```bash
curl --request POST --url https://api.dhan.co/v2/charts/intraday \
  --header 'Accept: application/json' --header 'Content-Type: application/json' \
  --header 'access-token: ' --data '{}'
```
**Request**
```json
{
  "securityId": "1333",
  "exchangeSegment": "NSE_EQ",
  "instrument": "EQUITY",
  "interval": "1",
  "oi": false,
  "fromDate": "2024-09-11 09:30:00",
  "toDate": "2024-09-15 13:00:00"
}
```
| Field | Type | Description |
|---|---|---|
| `securityId` (req) | string | From instrument master |
| `exchangeSegment` (req) | enum string | See `08_annexure.md` |
| `instrument` (req) | enum string | See `08_annexure.md` |
| `interval` (req) | enum int | `1`, `5`, `15`, `25`, `60` |
| `oi` (optional) | boolean | Open Interest for F&O |
| `fromDate` (req) | string | Start (`YYYY-MM-DD` or `YYYY-MM-DD HH:MM:SS` IST) |
| `toDate` (req) | string | End (same formats) |

> **Limit:** only **90 days of data per call** for any interval. Store data locally for day-to-day analysis.

**Response:** same shape as daily (`open/high/low/close/volume/timestamp/open_interest`).

---
**Timestamp note:** the docs and release notes describe `timestamp` as **EPOCH / UNIX time (seconds since 1970)**, and the Python lib converts it as Unix epoch → IST. The machine OpenAPI spec text says "since 1980" — treat that as a spec error. Convert to **IST (UTC+5:30)**. (The example arrays above use placeholder dates that don't line up with the request range — don't infer the epoch from them.)
For enum descriptions, see `08_annexure.md`.
