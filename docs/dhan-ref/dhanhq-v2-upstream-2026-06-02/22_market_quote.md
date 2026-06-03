# Market Quote — DhanHQ v2

> Source: https://dhanhq.co/docs/v2/market-quote/ · API v2 (Data API) · Audited 2 Jun 2026

REST snapshots of multiple instruments at once (real-time at request time). Unlike the WebSocket feed, this is request/response. Three modes:

| Method | Path | Use | Python |
|---|---|---|---|
| POST | `/marketfeed/ltp` | LTP (ticker) | `dhan.ticker_data(securities)` |
| POST | `/marketfeed/ohlc` | OHLC + LTP | `dhan.ohlc_data(securities)` |
| POST | `/marketfeed/quote` | Full: depth + OHLC + OI + volume + LTP | `dhan.quote_data(securities)` |

> **Up to 1000 instruments per request; rate limit 1 request/second.**
> Both `access-token` and `client-id` headers are required on all three.
> Request body = a map of `ExchangeSegment` → array of security IDs.

## Ticker Data — `POST /marketfeed/ltp`
```bash
curl --request POST --url https://api.dhan.co/v2/marketfeed/ltp \
  --header 'Accept: application/json' --header 'Content-Type: application/json' \
  --header 'access-token: JWT' --header 'client-id: 1000000001' --data '{}'
```
**Request**
```json
{ "NSE_EQ": [11536], "NSE_FNO": [49081, 49082] }
```
Keys are exchange-segment enums (`08_annexure.md`); values are arrays of security IDs.
**Response**
```json
{
  "data": {
    "NSE_EQ": { "11536": { "last_price": 4520 } },
    "NSE_FNO": { "49081": { "last_price": 368.15 }, "49082": { "last_price": 694.35 } }
  },
  "status": "success"
}
```
`last_price` (float) — LTP of the instrument.

## OHLC Data — `POST /marketfeed/ohlc`
Same request shape.
**Response**
```json
{
  "data": {
    "NSE_EQ": {
      "11536": {
        "last_price": 4525.55,
        "ohlc": { "open": 4521.45, "close": 4507.85, "high": 4530, "low": 4500 }
      }
    }
  },
  "status": "success"
}
```
| Field | Type | Description |
|---|---|---|
| `last_price` | float | LTP |
| `ohlc.open` | float | Day open |
| `ohlc.close` | float | Day close |
| `ohlc.high` | float | Day high |
| `ohlc.low` | float | Day low |

## Market Depth Data — `POST /marketfeed/quote`
Full snapshot: 5-level depth + OHLC + OI + volume + LTP + circuit limits.
**Request**
```json
{ "NSE_FNO": [49081] }
```
**Response**
```json
{
  "data": {
    "NSE_FNO": {
      "49081": {
        "average_price": 0,
        "buy_quantity": 1825,
        "depth": {
          "buy": [ { "quantity": 1800, "orders": 1, "price": 77 }, { "quantity": 25, "orders": 1, "price": 50 }, {"quantity":0,"orders":0,"price":0}, {"quantity":0,"orders":0,"price":0}, {"quantity":0,"orders":0,"price":0} ],
          "sell": [ {"quantity":0,"orders":0,"price":0}, {"quantity":0,"orders":0,"price":0}, {"quantity":0,"orders":0,"price":0}, {"quantity":0,"orders":0,"price":0}, {"quantity":0,"orders":0,"price":0} ]
        },
        "last_price": 368.15,
        "last_quantity": 0,
        "last_trade_time": "01/01/1980 00:00:00",
        "lower_circuit_limit": 48.25,
        "net_change": 0,
        "ohlc": { "open": 0, "close": 368.15, "high": 0, "low": 0 },
        "oi": 0, "oi_day_high": 0, "oi_day_low": 0,
        "sell_quantity": 0,
        "upper_circuit_limit": 510.85,
        "volume": 0
      }
    }
  },
  "status": "success"
}
```
| Field | Type | Description |
|---|---|---|
| `average_price` | float | Day VWAP |
| `buy_quantity` | int | Total pending buy qty at exchange |
| `sell_quantity` | int | Total pending sell qty at exchange |
| `depth.buy[].quantity` | int | Qty at this buy depth |
| `depth.buy[].orders` | int | Open BUY orders at this depth |
| `depth.buy[].price` | float | Price of this buy depth |
| `depth.sell[].quantity` | int | Qty at this sell depth |
| `depth.sell[].orders` | int | Open SELL orders at this depth |
| `depth.sell[].price` | float | Price of this sell depth |
| `last_price` | float | Last traded price |
| `last_quantity` | int | Last traded quantity |
| `last_trade_time` | string | Last trade time, formatted `dd/MM/yyyy HH:mm:ss` |
| `lower_circuit_limit` | float | Lower circuit |
| `upper_circuit_limit` | float | Upper circuit |
| `net_change` | float | Absolute change in LTP from prev close |
| `volume` | int | Day total volume |
| `oi` | int | Open Interest (derivatives) |
| `oi_day_high` | int | Highest OI for the day (NSE_FNO only) |
| `oi_day_low` | int | Lowest OI for the day (NSE_FNO only) |
| `ohlc.open/close/high/low` | float | Day OHLC |

> `depth.buy`/`depth.sell` are **5-level** here (REST snapshot). For 20/200-level depth use the WebSocket in `04_full_market_depth.md`.
> `last_trade_time` is a **formatted datetime string** (not an epoch). When an instrument hasn't traded, it defaults to `01/01/1980 00:00:00` — a zero-value placeholder, distinct from the chart-endpoint integer timestamps (see the epoch note in `05_historical_data.md`).
For enum descriptions, see `08_annexure.md`.
