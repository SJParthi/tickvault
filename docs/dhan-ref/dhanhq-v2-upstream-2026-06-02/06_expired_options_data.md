# Expired Options Data — DhanHQ v2

> Source: https://dhanhq.co/docs/v2/expired-options-data/ · API v2 · Audited 2 Jun 2026

Expired options contract data, pre-processed on a **rolling basis** so you request by **strike relative to spot** (ATM, ATM±n) instead of finding security IDs for expired contracts. Minute-level, **up to 5 years** back, both Index and Stock options. Returns OHLC, implied volatility, volume, open interest, strike and spot.

| Method | Path | Use |
|---|---|---|
| POST | `/charts/rollingoption` | Continuous (rolling) expired options contract data |

## Historical Rolling Data — `POST /charts/rollingoption`
**Up to 30 days of data per call.** Data stored on a minute level, by strike relative to spot (ATM, ATM+1, ATM-1, …).
```bash
curl --request POST --url https://api.dhan.co/v2/charts/rollingoption \
  --header 'Accept: application/json' --header 'Content-Type: application/json' \
  --header 'access-token: ' --data '{}'
```
**Request**
```json
{
  "exchangeSegment": "NSE_FNO",
  "interval": "1",
  "securityId": 13,
  "instrument": "OPTIDX",
  "expiryFlag": "MONTH",
  "expiryCode": 1,
  "strike": "ATM",
  "drvOptionType": "CALL",
  "requiredData": ["open","high","low","close","volume"],
  "fromDate": "2021-08-01",
  "toDate": "2021-09-01"
}
```
| Field | Type | Description |
|---|---|---|
| `exchangeSegment` (req) | enum string | See `08_annexure.md` |
| `interval` (req) | enum int | `1`, `5`, `15`, `25`, `60` |
| `securityId` (req) | string/int | **Underlying** security ID (e.g. 13 = Nifty) |
| `instrument` (req) | enum string | See `08_annexure.md` |
| `expiryCode` (req) | enum int | Expiry (OpenAPI restricts to `1`/`2`/`3`) |
| `expiryFlag` (req) | enum string | `WEEK` or `MONTH` |
| `strike` (req) | enum string | `ATM`; up to `ATM+10`/`ATM-10` for index near-expiry; up to `ATM+3`/`ATM-3` otherwise |
| `drvOptionType` (req) | enum string | `CALL` or `PUT` |
| `requiredData` (req) | array | any of `open` `high` `low` `close` `iv` `volume` `strike` `oi` `spot` |
| `fromDate` (req) | string | Start (`YYYY-MM-DD`) |
| `toDate` (req) | string | End — **non-inclusive** |

**Response** — the requested side is populated; the other is `null`:
```json
{
  "data": {
    "ce": {
      "iv": [], "oi": [], "strike": [], "spot": [],
      "open": [354, 360.3], "high": [], "low": [], "close": [], "volume": [],
      "timestamp": [1756698300, 1756699200]
    },
    "pe": null
  }
}
```
| Field | Type | Description |
|---|---|---|
| `open/high/low/close` | float | OHLC of the timeframe |
| `volume` | int | Volume traded |
| `timestamp` | int | EPOCH (Unix) seconds |

(From the OpenAPI spec, element types are: `iv/strike/spot/open/high/low/close` = double; `oi/volume/timestamp` = int64. The response always carries both `ce` and `pe` keys; the non-requested side is `null`.)

For enum descriptions, see `08_annexure.md`. Machine-schema view: `12_chart_optionchart.md`.
