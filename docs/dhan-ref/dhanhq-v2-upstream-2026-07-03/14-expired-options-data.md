# DhanHQ API v2 — Expired Options Data (Historical Rolling Options)

> **Source:** Official DhanHQ documentation (docs.dhanhq.co) — captured from DhanHQ's own "Export .md for LLMs" file, generated 2026-06-30.
> **Pages covered in this file:**
> - https://docs.dhanhq.co/api/v2/expired-options-data
> - https://docs.dhanhq.co/api/v2/guides/expired-options-data
> - https://docs.dhanhq.co/api/v2/expired-options-data/get-expired-options-data


---

## Expired Options Data

Expired Options Data

This API gives you expired options contract data. We have pre processed data for you to get it on rolling basis i.e. you can fetch last 5 years of strike wise data based on ATM and upto 10 strikes above and below. In addition to that, the data values are open, high, low, close, implied volatility, volume, open interest and spot information as well.

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/charts/rollingoption` | Get Continuous Expired Options Contract data |

---

## Expired Options Data

Fetch expired options contract data on a rolling basis with OHLC, IV, OI and spot data

This API gives you expired options contract data. Pre-processed data is available on a rolling basis — fetch the last 5 years of strike-wise data based on ATM and up to 10 strikes above and below.

Data values include: **open, high, low, close, implied volatility, volume, open interest** and **spot** information.

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/charts/rollingoption` | Get Continuous Expired Options Contract data |

---

## Historical Rolling Data

Fetch expired options data on a rolling basis, along with OI, IV, OHLC, Volume and spot information. You can fetch up to **30 days** of data per API call. Data is stored on a minute level, based on strike price relative to spot (e.g., ATM, ATM+1, ATM-1).

Both **Index Options** and **Stock Options** data are available for the last 5 years.

```bash
curl --request POST \
  --url https://api.dhan.co/v2/charts/rollingoption \
  --header 'Accept: application/json' \
  --header 'Content-Type: application/json' \
  --header 'access-token: {JWT}' \
  --data '{}'
```

**Request Structure**

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
    "requiredData": ["open", "high", "low", "close", "volume"],
    "fromDate": "2021-08-01",
    "toDate": "2021-09-01"
}
```


**Response Structure**

```json
{
    "data": {
        "ce": {
            "iv": [],
            "oi": [],
            "strike": [],
            "spot": [],
            "open": [354, 360.3],
            "high": [],
            "low": [],
            "close": [],
            "volume": [],
            "timestamp": [1756698300, 1756699200]
        },
        "pe": null
    }
}
```


For description of enum values, refer to the [Annexure](/api/v2/guides/annexure).


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| exchangeSegment | enum | Yes | Exchange & segment ([Annexure](/api/v2/guides/annexure#exchange-segment)) |
| interval | enum int | Yes | Minute intervals — , , , , Values: `1`, `5`, `15`, `25`, `60`. |
| securityId | string | Yes | Underlying exchange standard ID ([Instruments](/api/v2/guides/instruments)) |
| instrument | enum | Yes | Instrument type ([Annexure](/api/v2/guides/annexure#instrument)) |
| expiryCode | enum int | Yes | Expiry ([Annexure](/api/v2/guides/annexure#expiry-code)) |
| expiryFlag | enum | Yes | or Values: `WEEK`, `MONTH`. |
| strike | enum | Yes | for At the Money, up to / for Index Options near expiry, up to / for other contracts Values: `ATM`, `ATM+10`, `ATM-10`, `ATM+3`, `ATM-3`. |
| drvOptionType | enum | Yes | or Values: `CALL`, `PUT`. |
| requiredData | array | Yes | Requested fields — Values: `open`, `high`, `low`, `close`, `iv`, `volume`, `strike`, `oi`, `spot`. |
| fromDate | string | Yes | Start date (YYYY-MM-DD) |
| toDate | string | Yes | End date (non-inclusive) |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| open | float | No | Open price of the timeframe |
| high | float | No | High price |
| low | float | No | Low price |
| close | float | No | Close price |
| volume | int | No | Volume traded |
| timestamp | int | No | Epoch timestamp |

---

## Historical Rolling Options Data

`POST` https://api.dhan.co/v2/charts/rollingoption

Fetch expired options contract data on a rolling basis with OHLC, IV, OI and spot information. Up to 45 days per call. Last 5 years available.

Fetch expired options data on a rolling basis, along with the Open Interest, Implied Volatility, OHLC, Volume as well as information about the spot. You can fetch for upto 45 days of data in a single API call. Expired options data is stored on a minute level, based on strike price relative to spot (example ATM, ATM+10, ATM-10, etc.).

You can fetch data upto last 5 years. We have added both Index Options and Stock Options data on this.


### Request Body Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| exchangeSegment | enum | No | Exchange & Segment Values: `NSE_FNO`, `BSE_FNO`. Default: `NSE_FNO`. |
| interval | enum | No | Minute intervals Values: `1`, `5`, `15`, `30`, `60`. Default: `5`. |
| securityId | string | No | Underlying security ID |
| instrument | enum | No | Instrument type Values: `OPTIDX`, `OPTSTK`. Default: `OPTIDX`. |
| expiryFlag | enum | No | Expiry Flag Values: `WEEK`, `MONTH`. Default: `WEEK`. |
| expiryCode | enum | No | 1=Near, 2=Next, 3=Far Values: `1`, `2`, `3`. Default: `1`. |
| strike | string | No | ATM, ATM+10, ATM-10 |
| drvOptionType | enum | No | Option Type Values: `CALL`, `PUT`. Default: `CALL`. |
| requiredData | array | No | Array of data fields: open, high, low, close, iv, volume, strike, oi, spot Values: `open`, `high`, `low`, `close`, `iv`, `volume`, `strike`, `oi`, `spot`. |
| fromDate | string | No | Start Date (YYYY-MM-DD) |
| toDate | string | No | End Date (YYYY-MM-DD) |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| data.ce.open | array | No | Array of open prices for CE |
| data.ce.high | array | No | Array of high prices for CE |
| data.ce.low | array | No | Array of low prices for CE |
| data.ce.close | array | No | Array of close prices for CE |
| data.ce.volume | array | No | Array of volumes for CE |
| data.ce.iv | array | No | Array of implied volatilities for CE |
| data.ce.oi | array | No | Array of open interest for CE |
| data.ce.spot | array | No | Array of underlying spot prices for CE |
| data.ce.strike | array | No | Array of strike prices for CE |
| data.ce.timestamp | array | No | Array of epoch timestamps for CE |
| data.pe | object | No | Object containing PE data points analogous to CE |


### Status Codes

| Code | Description |
|------|-------------|
| 200 | Expired options data |
| 401 | Unauthorized |


### Example Request

```bash
curl -X POST https://api.dhan.co/v2/charts/rollingoption \
  -H "access-token: <your-access-token>" \
  -H "Content-Type: application/json"
```
