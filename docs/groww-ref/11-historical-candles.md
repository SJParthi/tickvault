# Groww Trade API — Historical Candles (full verbatim reference)

> **Source:** https://groww.in/trade-api/docs/curl/backtesting · https://groww.in/trade-api/docs/curl/historical-data · https://groww.in/trade-api/docs/python-sdk/backtesting · https://groww.in/trade-api/docs/curl/annexures
> **Fetched/Verified:** 2026-07-13 (via the 2026-07-03 lossless capture + 2026-07-13 live cross-checks; groww.in direct fetch proxy-blocked from the sandbox — provenance stated honestly)
> **Evidence tiers:** Verified / Assumed / Unknown per README legend. "Verified (capture 2026-07-03)" = verbatim from the 26-page lossless official capture; "Verified (live 2026-07-13)" = independently matched by same-day live-page search extraction or mirror forensics; "Verified (SDK 1.5.0)" = read from the official `growwapi` 1.5.0 wheel (PyPI latest as of 2026-07-13).
> **Related:** `14-option-chain.md` (chain + greeks), `15-rate-limits-and-capacity.md` (family attribution for these endpoints), `99-UNKNOWNS.md` (U-1, U-2, U-3, U-4, U-6, U-7, U-16)

> **⚠ SCOPE — REFERENCE DOCUMENTATION ONLY, NOT FETCH AUTHORIZATION.** This file documents the Groww historical-candles REST surface for completeness. Groww historical/backtest fetching from TickVault is governed by `.claude/rules/project/groww-second-feed-scope-2026-06-19.md` §33 (LIVE-FEED-ONLY) / §37 (the narrow BruteX-S3 CSV consumption grant — TickVault reads BruteX-produced artifacts from our own bucket, never this API). Any future TickVault REST consumer of these endpoints requires its own dated operator grant recorded in that rule file FIRST. (Same banner class as the Dhan full-market-depth stub.)

---

## 1. Current endpoint — Get Historical Candle Data

**Verified (capture 2026-07-03 + live 2026-07-13):**

```
GET https://api.groww.in/v1/historical/candles
```

Documented on the **Backtesting** page (`/trade-api/docs/curl/backtesting#get-historical-candle-data`), NOT on the "Historical Data" page — that page hosts only the deprecated endpoint (§7).

Full request example, verbatim (capture 2026-07-03; the identical example was independently returned by a live-page search extraction 2026-07-13):

```
# You can also use wget
curl -X GET 'https://api.groww.in/v1/historical/candles?exchange=NSE&segment=CASH&groww_symbol=NSE-WIPRO&start_time=2025-09-24 10:56:00&end_time=2025-09-24 15:21:00&candle_interval=5minute' \
  -H 'Accept: application/json' \
  -H 'Authorization: Bearer {ACCESS_TOKEN}' \
  -H 'X-API-VERSION: 1.0'
```

**Headers (Verified):** `Authorization: Bearer {ACCESS_TOKEN}` + `X-API-VERSION: 1.0` + `Accept: application/json` — the mandatory trio documented on the REST intro page ("All headers are mandatory."). The SDK additionally sends `x-request-id: <uuid4>`, `Content-Type: application/json`, `x-client-id: growwapi`, `x-client-platform: growwapi-python-client`, `x-client-platform-version: 1.5.0`, `x-api-version: 1.0` (Verified, SDK 1.5.0 `_build_headers`).

**Purpose text, verbatim (capture 2026-07-03; "available from 2020" matched live 2026-07-13):**

> Fetch historical candle data for backtesting trading strategies. This API provides
>
> - Historical OHLC (Open, High, Low, Close) data for all instruments
> - Volume for tradable instruments (Equities and FNO)
> - Open Interest (OI) for FNO
>   Data of Equities, Indices and FNO instruments are available from 2020.

Segment scope, verbatim note (both backtesting page variants): "**Note:** Currently, Backtesting APIs only support CASH and FNO segments."

Prerequisite (Verified, REST intro capture): "Having an active Trading API Subscription."

## 2. Request parameters

Request schema, verbatim (capture 2026-07-03, REST variant — all 6 required):

| Name | Type | Description |
| --- | --- | --- |
| exchange `*` | string | Stock exchange (NSE, BSE) |
| segment `*` | string | Segment of the instrument such as CASH, FNO etc. |
| groww\_symbol `*` | string | Groww symbol of the instrument for which historical data is required |
| start\_time `*` | string | Start time in yyyy-MM-dd HH:mm:ss or epoch seconds format from which data is required |
| end\_time `*` | string | End time in yyyy-MM-dd HH:mm:ss or epoch seconds format until which data is required |
| candle\_interval `*` | string | Interval for which data is required. |

- **Epoch unit wart (Verified both sources):** the CURRENT endpoint's docs say epoch **seconds**; the DEPRECATED endpoint's SDK docstring says "epoch **milliseconds** or yyyy-MM-dd HH:mm:ss". The unit differs between old (ms) and new (s).
- **Timezone: NOT stated anywhere in the docs (Verified-absence).** All examples use exchange-session wall-clock times (`09:15:00`, `15:30:00`). **Assumed: IST.** No UTC mention on any captured page.

**`groww_symbol` format (Verified, capture — the page defines it as "formed by concatenating" Exchange + Trading Symbol + Expiry Date ("format: DDMmmYY, example: 23Jan25"; derivatives only) + Strike Price (options only) + Option Type; the compressed form here is a faithful compaction of that bulleted definition, not a verbatim string):**

| Class | Examples (verbatim) |
| --- | --- |
| Equity | `NSE-WIPRO`, `BSE-RELIANCE` |
| Index | `NSE-NIFTY`, `BSE-SENSEX` |
| Future | `NSE-NIFTY-30Sep25-FUT`, `BSE-SENSEX-25Sep25-FUT` |
| Call Option | `NSE-NIFTY-30Sep25-24650-CE`, `BSE-SENSEX-25Sep25-79500-CE` |
| Put Option | `NSE-NIFTY-30Sep25-24650-PE`, `BSE-SENSEX-25Sep25-79500-PE` |

"Groww symbol also exists in the instruments csv file and it can be obtained from the Get Instruments API." (verbatim)

## 3. Candle intervals — ALL 12 values

Annexure table, verbatim (capture 2026-07-03; the 12-value enum independently confirmed live 2026-07-13 via search + SDK 1.5.0 constants + two third-party SDK ports):

| Value | Description |
| --- | --- |
| 1minute | 1 minute interval |
| 2minute | 2 minute interval |
| 3minute | 3 minute interval |
| 5minute | 5 minute interval |
| 10minute | 10 minute interval |
| 15minute | 15 minute interval |
| 30minute | 30 minute interval |
| 1hour | 1 hour interval |
| 4hour | 4 hour interval |
| 1day | 1 day interval |
| 1week | 1 week interval |
| 1month | 1 month interval |

SDK constants carry the exact same strings: `CANDLE_INTERVAL_MIN_1 = "1minute"` … `CANDLE_INTERVAL_MONTH = "1month"` (Verified, SDK 1.5.0 + annexures capture).

## 4. Per-request range caps — "Backtesting Data Limits"

Verbatim (capture 2026-07-03; independently CONFIRMED-VIA-MIRROR by two verbatim page scrapes dated 2026-03-05 and 2026-03-22 — the numeric rows do not surface in live search snippets, so live re-verification on 2026-07-13 was not possible):

| Candle Intervals | Max Duration per Request |
| --- | --- |
| **1 min, 2 min, 3 min, 5 min** | 30 days |
| **10 min, 15 min, 30 min** | 90 days |
| **1 hour, 4 hours, 1 day, 1 week, 1 month** | 180 days |

- Availability window, verbatim: "Data of Equities, Indices and FNO instruments are available from 2020" (Verified live 2026-07-13).
- **Known internal doc inconsistency (Verified):** the trade-api LANDING page separately says historical OHLC can be fetched "for up to 3 months" — marketing shorthand that CONFLICTS with the from-2020 statement + the caps table above. Treat the docs tables as authoritative; live search extraction demonstrably mis-concludes "max duration is 3 months" from that landing-page line.
- **Unknown:** whether exceeding a cap errors (GA001-class) or silently truncates — never documented; live probe listed in `99-UNKNOWNS.md` U-3.

## 5. Response shape

Verbatim (capture 2026-07-03, truncated to 2 candles; page states "All prices in rupees."):

```json
{
    "status": "SUCCESS",
    "payload": {
        "candles": [
            [
                "2025-09-24T10:30:00", // candle timestamp in yyyy-MM-dd HH:mm:ss format
                245.95, // open price
                246.15, // high price
                245.05, // low price
                245.6,  // close price
                735060, // volume
                null // open interest (only for FNO instruments, null for others)
            ],
            [ "2025-09-24T11:00:00", 245.64, 245.66, 244.8, 244.94, 682373, null ]
        ],
        "closing_price": 244.6,
        "start_time": "2025-09-24 10:30:00",
        "end_time": "2025-09-24 13:30:00",
        "interval_in_minutes": 30
    }
}
```

- **Candle field order (Verified):** `[timestamp, open, high, low, close, volume, open_interest]` — 7 elements. The python-sdk page's schema sentence reads verbatim: "Each candle contains: timestamp (yyyy-MM-dd HH:mm:ss), open, high, low, close, volume, open interest"; the REST page's schema row omits "open interest" from the sentence while the example carries the 7th element — doc-internal sloppiness, byte-verified in the capture.
- **Timestamp wire-format wart (Verified, byte-verified in the capture):** the example candle carries an ISO-`T` string (`"2025-09-24T10:30:00"`) while the inline comment on the SAME line AND the schema say "yyyy-MM-dd HH:mm:ss" (space separator). The envelope `start_time`/`end_time` use the space form. **A parser MUST accept both forms.** No timezone suffix anywhere — Assumed IST wall-clock.
- Types: open/high/low/close float (rupees); volume int; open interest int-or-`null` ("only for FNO instruments, null for others"); `closing_price` float; `interval_in_minutes` int (echo of the request).
- The SDK returns `payload` unwrapped — `_parse_response` returns `response_map["payload"]` when present (Verified, SDK 1.5.0).
- **OI is FNO-only (Verified):** equity/index rows always carry `null` in position 7.

## 6. SPECIAL SECTION — Current-day serving & minute freshness: UNDOCUMENTED

**The docs contain ZERO statements about current-day availability or just-closed-minute latency.** Grep-verified across ALL 26 pages of the 2026-07-03 lossless capture on 2026-07-13 (terms: current day / same day / today / in-progress / latest candle / delay / latency / freshness — zero relevant hits; the only "today" hits are a Smart Orders prose line and the smart-order-list `start_date_time` window default). A groww.in-domain-restricted live search on 2026-07-13 surfaced nothing further. Every documented example uses past dates.

The ONLY adjacent statement in the whole corpus is the Live Data page's OHLC note, verbatim:

> Note: The OHLC data retrieved using the OHLC API reflects the current time's OHLC (i.e., real-time snapshot).
> For interval-based OHLC data (e.g., 1-minute, 5-minute candles), please refer to the Historical Data API.

— which POINTS intraday-interval-candle users AT the historical surface, implying (but never stating) same-day service. Oddity worth noting for probe design: that link targets the Historical Data page, which hosts the DEPRECATED endpoint (§7), not the current one.

Internal empirical context (Assumed for doc purposes, internal): BruteX produces same-day post-close 1m CSVs from this API (the §37 cross-verify pipeline), so same-day data after close is empirically served. Whether MID-SESSION minutes of the in-progress day are served, and how quickly a just-closed minute appears, are **Unknown** — the two central live probes. Cross-ref: `99-UNKNOWNS.md` **U-1** (current-day serving) and **U-2** (just-closed-minute freshness ladder).

## 7. DEPRECATED endpoint — Get Historical Data (`/candle/range`)

Verbatim deprecation note (capture 2026-07-03; the same wording surfaced live 2026-07-13):

> **Note**
>
> This API request is deprecated and will NOT work in the future. Use Get Historical Candle Data instead.

```
GET https://api.groww.in/v1/historical/candle/range
```

Request params (verbatim): `exchange`\*, `segment`\*, `trading_symbol`\* (NOT groww_symbol), `start_time`\*, `end_time`\* ("yyyy-MM-dd HH:mm:ss or epoch seconds format"), `interval_in_minutes` (string, optional).

Response candle differs from the current endpoint (verbatim schema): "Each candle has candle timestamp (epoch second), open (float), high (float), low (float), close (float) , volume (int) in that order." — **6 elements, NO OI, epoch-second int timestamp** (example: `1633072800`).

Its DIFFERENT limits table, verbatim (capture 2026-07-03 — appears on both /historical-data page variants; DEPRECATED alongside the endpoint):

| Candle Interval | Max Duration per Request | Historical Data Available |
| --- | --- | --- |
| **1 min** | 7 days | Last 3 months |
| **5 min** | 15 days | Last 3 months |
| **10 min** | 30 days | Last 3 months |
| **1 hour (60 min)** | 150 days | Last 3 months |
| **4 hours (240 min)** | 365 days | Last 3 months |
| **1 day (1440 min)** | 1080 days (~3 years) | Full history |
| **1 week (10080 min)** | No Limit | Full history |

The "Last 3 months" intraday availability applies to this OLD endpoint; the NEW endpoint claims from-2020 availability with the §4 caps. The SDK's `get_historical_candle_data` emits a `DeprecationWarning` verbatim: "`get_historical_candle_data` is deprecated and will be removed in future releases. Please use `get_historical_candles` method instead." (Verified, SDK 1.5.0).

## 8. Companion F&O discovery endpoints (same `/historical/` family)

**Get Expiries (Verified, capture verbatim + SDK 1.5.0):**

```
GET https://api.groww.in/v1/historical/expiries?exchange=NSE&underlying_symbol=NIFTY&year=2024&month=1
```

| Name | Type | Description (verbatim) |
| --- | --- | --- |
| exchange `*` | string | Stock exchange (NSE, BSE) |
| underlying\_symbol `*` | string | Underlying symbol for which expiry dates are required (e.g., NIFTY, BANKNIFTY, RELIANCE) |
| year | integer | Year for which expiry dates are required (2020 - current year). If year is not specified, current year is considered. |
| month | integer | Month for which expiry dates are required (1-12). If month is not specified, expiries of the entire year is returned. |

Response: `{"status":"SUCCESS","payload":{"expiries":["2024-01-25", ...]}}` — YYYY-MM-DD strings. "Data of FNO instruments are available from 2020."

**Discrepancy (Verified both sources, byte-verified):** the docs say `year` is "2020 - current year"; the SDK docstring says "between 2000 and 5000". Both accurate to their sources; the server-accepted range is **Unknown** — `99-UNKNOWNS.md` U-16.

**Get Contracts (Verified, capture verbatim + SDK 1.5.0):**

```
GET https://api.groww.in/v1/historical/contracts?exchange=NSE&underlying_symbol=NIFTY&expiry_date=2025-01-25
```

Params: `exchange`\*, `underlying_symbol`\* ("1-20 characters"), `expiry_date`\* (YYYY-MM-DD). Response: `{"contracts": ["NSE-NIFTY-02Jan25-28500-PE", ...]}` — groww_symbols passable directly into the candles API.

**Per-contract F&O candles workflow (Verified, capture verbatim):** expiries → contracts → candles, ending in
`GET .../historical/candles?exchange=NSE&segment=FNO&groww_symbol=NSE-NIFTY-04Jan24-19200-CE&start_time=2024-01-01 09:15:00&end_time=2024-01-10 15:30:00&candle_interval=15minute`. Expired historical contracts are included (the expiries API takes years back to 2020). No F&O-specific range caps are documented — the §4 table is not segment-qualified.

## 9. SDK method signature (reference facts; no code vendored)

`get_historical_candles(exchange, segment, groww_symbol, start_time, end_time, candle_interval, timeout=None) -> dict` → `GET {domain}/historical/candles` where `self.domain = "https://api.groww.in/v1"` (Verified, SDK 1.5.0). Docstring: "start_time (str): The start time in yyyy-MM-dd HH:mm:ss format."

- **ZERO client-side validation on market-data params (Verified by reading the full method bodies):** no `candle_interval` enum check, no date-format check, no range-length math — everything is passed through verbatim; all enforcement is server-side. (Nuance for honesty: `get_access_token` DOES raise `ValueError` on totp/secret mutual-exclusion — "zero validation" holds for the candles/chain/LTP/OHLC market-data scope, not the SDK universally.)
- Default `timeout=None` = infinite `requests` timeout ("Defaults to None (infinite)") — a black-holed endpoint hangs the caller forever unless a bounded timeout is passed.
- No client-side rate limiter, no retry/backoff, no `Retry-After` handling anywhere in the REST client.
- Changelog (Verified, capture): "Historical data retrieval APIs" added in [1.0.0] — 3rd October, 2025.

## 10. Error envelope

- **No endpoint-specific error section** exists on the historical/backtesting pages (Verified-absence, capture).
- Failure envelope, verbatim (REST intro capture): `{"status": "FAILURE", "error": {"code": "GA001", "message": "Invalid trading symbol.", "metadata": null}}`.
- Global API Error Codes table, verbatim (REST intro, capture 2026-07-03):

| Code | Message |
| --- | --- |
| GA000 | Internal error occurred |
| GA001 | Bad request |
| GA003 | Unable to serve request currently |
| GA004 | Requested entity does not exist |
| GA005 | User not authorised to perform this operation |
| GA006 | Cannot process this request |
| GA007 | Duplicate order reference id |

- HTTP-status → SDK exception mapping (Verified, SDK 1.5.0 `_ERROR_MAP`): 400→BadRequest, 401→Authentication, 403→Authorisation, 404→NotFound, **429→GrowwAPIRateLimitException**, 504→Timeout; any other non-2xx → generic `GrowwAPIException`. A body with `status == "FAILURE"` raises `GrowwAPIException(code, msg)` FIRST (GA-coded business failures win over the HTTP-status map).
- Rate-limit family for `/historical/*` is UNASSIGNED in the official table — see `15-rate-limits-and-capacity.md` §3 and `99-UNKNOWNS.md` U-4.
