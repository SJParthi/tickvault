# Option Chain — DhanHQ v2

> Source: https://dhanhq.co/docs/v2/option-chain/ · API v2 · Audited 2 Jun 2026

Entire option chain for any option underlying across NSE, BSE and MCX. Returns OI, Greeks, volume, top bid/ask and price for **every strike** of the underlying.

| Method | Path | Use |
|---|---|---|
| POST | `/optionchain` | Option chain of an instrument |
| POST | `/optionchain/expirylist` | Expiry list for an underlying |

> **Rate limit: 1 unique request / 3 seconds** (OI updates slowly vs LTP). You can fetch chains for multiple underlyings — or multiple expiries of the same underlying — concurrently within that cadence.
> Both endpoints require **both** `access-token` and `client-id` headers.

## Option Chain — `POST /optionchain`
```bash
curl --request POST --url https://api.dhan.co/v2/optionchain \
  --header 'Content-Type: application/json' --header 'access-token: JWT' \
  --header 'client-id: 1000000001' --data '{Request Body}'
```
**Headers:** `access-token` (req), `client-id` (req).
**Request**
```json
{ "UnderlyingScrip": 13, "UnderlyingSeg": "IDX_I", "Expiry": "2024-10-31" }
```
| Field | Type | Description |
|---|---|---|
| `UnderlyingScrip` (req) | int | Security ID of underlying (from `09_instruments.md`) |
| `UnderlyingSeg` | enum string | Exchange/segment of underlying (`08_annexure.md`) |
| `Expiry` | string | Expiry date (`YYYY-MM-DD`); from the expiry-list call |

**Response** — `oc` is a map keyed by a float-string strike, each with `ce` and `pe`:
```json
{
  "data": {
    "last_price": 25642.8,
    "oc": {
      "25650.000000": {
        "ce": {
          "average_price": 146.99,
          "greeks": { "delta": 0.53871, "theta": -15.1539, "gamma": 0.00132, "vega": 12.18593 },
          "implied_volatility": 9.789193798280868,
          "last_price": 134,
          "oi": 3786445,
          "previous_close_price": 244.85,
          "previous_oi": 402220,
          "previous_volume": 31931705,
          "security_id": 42528,
          "top_ask_price": 134, "top_ask_quantity": 1365,
          "top_bid_price": 133.55, "top_bid_quantity": 1625,
          "volume": 117567970
        },
        "pe": { "... same shape ..." }
      }
    }
  },
  "status": "success"
}
```
**Top-level**
| Field | Type | Description |
|---|---|---|
| `data.last_price` | float | LTP of the underlying |
| `data.oc` | map | Option chain, keyed by strike |
| `data.oc.{strike}.ce` | object | Call data |
| `data.oc.{strike}.pe` | object | Put data |

**Per-side (ce/pe) fields**
| Field | Type | Description |
|---|---|---|
| `average_price` | float | Day average price *(added v2.5)* |
| `greeks.delta` | float | ₹ premium change per ₹1 underlying move |
| `greeks.theta` | float | Time decay |
| `greeks.gamma` | float | Rate of change of delta |
| `greeks.vega` | float | Premium change per 1% IV change |
| `implied_volatility` | float | IV |
| `last_price` | float | LTP of the option |
| `oi` | int | Open Interest |
| `previous_close_price` | float | Prev-day close |
| `previous_oi` | int | Prev-day OI |
| `previous_volume` | int | Prev-day volume |
| `security_id` | int | Option's security ID *(added v2.5)* |
| `top_ask_price` | float | Best ask price |
| `top_ask_quantity` | int | Qty at best ask |
| `top_bid_price` | float | Best bid price |
| `top_bid_quantity` | int | Qty at best bid |
| `volume` | int | Day volume |

## Expiry List — `POST /optionchain/expirylist`
```bash
curl --request POST --url https://api.dhan.co/v2/optionchain/expirylist \
  --header 'Content-Type: application/json' --header 'access-token: JWT' \
  --header 'client-id: 1000000001' --data '{}'
```
**Request**
```json
{ "UnderlyingScrip": 13, "UnderlyingSeg": "IDX_I" }
```
| Field | Type | Description |
|---|---|---|
| `UnderlyingScrip` (req) | int | Underlying security ID |
| `UnderlyingSeg` | enum string | Exchange/segment of underlying |

**Response**
```json
{ "data": ["2024-10-17","2024-10-24","2024-10-31", "..."], "status": "success" }
```
| Field | Type | Description |
|---|---|---|
| `data[]` | array | All expiry dates of the underlying (`YYYY-MM-DD`) |

For enum descriptions, see `08_annexure.md`.
