# Zerodha Kite Connect v3 — Historical Candle Data (reference)

> **Source:** https://kite.trade/docs/connect/v3/historical/ (official live doc — NOW FETCHED live-verbatim)
> **Fetched-Verified:** 2026-07-14 — live-verbatim via GH-runner 2026-07-14 (UTC timestamps in `00-COVERAGE-MANIFEST.md`): the official docs page, 8 kite.trade forum threads (2875, 3018, 6292, 8722, 11460, 14149, 14806, 15111, 15015), and the Kite-API support articles (`historical-data-and-live-market-data-payment-plan`, `can-i-subscribe-to-historical-api-without-subscribing-to-kite-connect-api`, `what-are-the-charges-for-kite-apis`) were all fetched HTTP-200 by the runner (per-URL sha256 + timestamps in the crawl manifest). Original 2026-07-13 cross-check evidence retained: CLIENT-LIB-SOURCE (pykiteconnect@master + gokiteconnect@master), OFFICIAL-MOCK (zerodha/kiteconnect-mocks@main).
> **Evidence tiers:** see README legend. Per-file shorthand: **Verified (LIVE-DOC runner 2026-07-14 …)** = confirmed against the runner-fetched live page (top tier for this pack); **Verified (LIVE-FORUM runner 2026-07-14 …)** = confirmed against a runner-fetched live kite.trade forum page — Zerodha-operated, but a forum post (staff-authored posts noted as such; user-quoted server error messages noted as such); **Verified (CLIENT-LIB-SOURCE …)** / **Verified (OFFICIAL-MOCK …)** = SDK source / official mock JSON cross-check; **ARITH** = derived, derivation shown; **Assumed** / **Unknown** per legend.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. Any TickVault consumer of this API requires its own dated operator grant per `no-rest-except-live-feed-2026-06-27.md` FIRST. (Same banner class as the Dhan full-market-depth stub / groww-ref `11-historical-candles.md`.)
> **Related:** `99-UNKNOWNS.md` (several rows below now close — see OPEN QUESTIONS "RESOLVED by live fetch" notes); `14-option-chain-composition.md` §5 (India-F&O/BFO coverage note).

---

## 1. Endpoint

**Verified (LIVE-DOC runner 2026-07-14 — https://kite.trade/docs/connect/v3/historical/ , in fetch-log), verbatim:**

```
GET /instruments/historical/:instrument_token/:interval
```

> "Retrieve historical candle records for a given instrument." … "The historical data API provides archived data (up to date as of the time of access) for instruments across various exchanges spanning back several years. A historical record is presented in the form of a **candle** (Timestamp, Open, High, Low, Close, Volume, OI)…"

- Base URL `https://api.kite.trade` — Verified (LIVE-DOC: the page's own curl examples hit `https://api.kite.trade/instruments/historical/5633/minute?...`; cross-check CLIENT-LIB-SOURCE pykiteconnect `_default_root_uri = "https://api.kite.trade"`).
- `:instrument_token` — LIVE-DOC verbatim: "Identifier for the instrument whose historical records you want to fetch. This is obtained with the instrument list API." It is a **path segment**, as is `:interval`.
- Headers (LIVE-DOC: both curl examples carry them verbatim): `X-Kite-Version: 3` + `Authorization: token api_key:access_token`. Cross-check Verified (CLIENT-LIB-SOURCE pykiteconnect `_request`).
- SDK default HTTP timeout: `_default_timeout = 7  # In seconds` (Verified, CLIENT-LIB-SOURCE pykiteconnect — SDK-only fact, not on the docs page).

**Compare: Dhan/Groww.** Dhan's historical surface is `POST` with a JSON body and securityId as a body field (`docs/dhan-ref/05-historical-data.md`); Groww's is `GET` with query params and a composite `groww_symbol` string (`docs/groww-ref/11-historical-candles.md` §1–§2). Kite is the only one of the three that puts BOTH the instrument identity and the interval in the URL path.

## 2. Request parameters (query string)

**Verified (LIVE-DOC runner 2026-07-14 — historical/ page "Request parameters" table), verbatim:**

| Param | Live-docs wording (verbatim) |
|---|---|
| `from` | "`yyyy-mm-dd hh:mm:ss` formatted date indicating the start date of records" |
| `to` | "`yyyy-mm-dd hh:mm:ss` formatted date indicating the end date of records" |
| `continuous` | "Accepts `0` or `1`. Pass `1` to get continuous data" |
| `oi` | "Accepts `0` or `1`. Pass `1` to get OI data" |

- Cross-check Verified (CLIENT-LIB-SOURCE pykiteconnect `historical_data`): the SDK sends exactly `{"from", "to", "interval", "continuous": 1|0, "oi": 1|0}`; gokiteconnect formats dates `2006-01-02 15:04:05` — both match the live wire format.
- **`to` boundary is INCLUSIVE (candle whose open == `to` is returned)** — Verified (LIVE-DOC, example-derived): the docs example requests `from=2017-12-15 09:15:00&to=2017-12-15 09:20:00` and the shown response contains SIX candles, `09:15:00` through `09:20:00` **including** the `09:20:00` candle; the OI example shows the same (`to=…09:20:00` → last candle `09:20:00+0530`). Residual caveat: demonstrated at `minute` interval only; `day`-interval boundary behaviour Assumed same. (This closes the former "inclusivity Unknown" flag — contrast Dhan, whose daily `toDate` is documented NON-inclusive, `docs/dhan-ref/05-historical-data.md`.)
- **Sub-day granular windows are official** — Verified (LIVE-DOC, "Note" box verbatim): "It is possible to retrieve candles for small time intervals by making the `from` and `to` calls granular. For instance `from = 2017-01-01 09:15:00` and `to = 2017-01-01 09:30:00` to fetch candles for just 15 minutes between those timestamps." (This is the shape a per-minute REST leg would use — see §10.)
- **Date-only `from`/`to` strings** — a Zerodha staff example in the live-fetched forum uses date-only strings through the Python SDK (`from_date='2017-01-01', to_date='2019-01-01'` — Verified, LIVE-FORUM runner 2026-07-14, https://kite.trade/forum/discussion/6292/historical-data-for-expired-contracts , staff rakeshr Aug 2019, in fetch-log). The SDK passes strings through verbatim, so the server accepts date-only; the documented canonical format remains `yyyy-mm-dd hh:mm:ss`.
- **Interval also in query — SDK wart (Verified, CLIENT-LIB-SOURCE):** pykiteconnect redundantly sends `interval` as a query param even though it is already a path segment. The live docs define it as a URI parameter only.

## 3. Intervals — the full set

**Verified (LIVE-DOC runner 2026-07-14 — historical/ page "URI parameters" `:interval`), verbatim:**

> "The candle record interval. Possible values are: · `minute` · `day` · `3minute` · `5minute` · `10minute` · `15minute` · `30minute` · `60minute`"

- Exactly **8 documented values** — the former SEARCH-tier interval list is now LIVE-DOC, byte-confirmed.
- ⚠ **CORRECTION vs the 2026-07-13 draft:** this file previously stated "the `2minute` value does NOT exist on Kite". The live-fetched forum shows the SERVER recognizes several UNDOCUMENTED interval strings — a June 2024 user post quotes the server's own per-interval cap error messages for `2minute`, `4minute`, `hour`, `2hour`, `3hour`, `4hour`, `week` (each answered with "interval exceeds max limit: N days", not an invalid-interval error) — **Verified (LIVE-FORUM runner 2026-07-14 — https://kite.trade/forum/discussion/14149/historical-data-retention-policy , user-quoted server error messages, staff replied without disputing them; in fetch-log)**. So: 8 documented intervals; additional undocumented ones exist server-side (see §4 table). Treat undocumented intervals as UNSUPPORTED-BUT-PRESENT — do not build on them.
- **Verified (CLIENT-LIB-SOURCE pykiteconnect):** the SDK defines NO interval enum/constants; all interval validation is server-side.
- **Compare: Dhan** exposes intraday intervals as bare strings `"1"/"5"/"15"/"25"/"60"` on a separate intraday endpoint + a distinct daily endpoint (`docs/dhan-ref/05-historical-data.md`); Kite uses ONE endpoint with the interval in the path. **Compare: Groww**'s 12-value enum includes `2minute`/`1hour`/`4hour`/`1week`/`1month` as DOCUMENTED values (`docs/groww-ref/11-historical-candles.md` §3).

## 4. THE PER-INTERVAL MAX RANGE CAPS (per request)

⚠ **Key live finding (CORRECTION):** the official docs page https://kite.trade/docs/connect/v3/historical/ carries **NO caps table at all** (Verified-absence, LIVE-DOC runner 2026-07-14 — the full page was fetched and read; its only sections are URI parameters / Request parameters / Response structure / Continuous data / Examples / OI Data). The 2026-07-13 draft's assumption that "the official caps table lives on the blocked docs page" was WRONG. The caps are **server-enforced** and surface only as the error message `interval exceeds max limit: N days` — the forum is the only official written record.

**The CURRENT caps — Verified (LIVE-FORUM runner 2026-07-14 — https://kite.trade/forum/discussion/14149/historical-data-retention-policy , June 2024 user post quoting the server's own error message per interval, verbatim below; staff sujith replied in-thread without disputing any number; in fetch-log):**

```
# minute   | interval exceeds max limit: 60 days
# 2minute  | interval exceeds max limit: 60 days
# 3minute  | interval exceeds max limit: 100 days
# 4minute  | interval exceeds max limit: 100 days
# 5minute  | interval exceeds max limit: 100 days
# 10minute | interval exceeds max limit: 100 days
# 15minute | interval exceeds max limit: 200 days
# 30minute | interval exceeds max limit: 200 days
# 60minute | interval exceeds max limit: 400 days
# hour     | interval exceeds max limit: 400 days
# 2hour    | interval exceeds max limit: 400 days
# 3hour    | interval exceeds max limit: 400 days
# 4hour    | interval exceeds max limit: 400 days
# day      | interval exceeds max limit: 2000 days
# week     | interval exceeds max limit: 2000 days
```

Restricted to the 8 DOCUMENTED intervals:

| Interval | Max days per request | Independent live corroboration |
|---|---|---|
| `minute` | **60 days** | forum 15111 (June 2025): user quotes live API error "the maximum limit allowed for 1-minute data is only 60 days" — Verified (LIVE-FORUM runner 2026-07-14, https://kite.trade/forum/discussion/15111/historical-data-limit-60-days , in fetch-log) |
| `3minute` | **100 days** | 14149 error list |
| `5minute` | **100 days** | 14149 error list |
| `10minute` | **100 days** | 14149 error list |
| `15minute` | **200 days** | forum 11460 (June 2022): user observed "200 days on 15 min candle" — Verified (LIVE-FORUM runner 2026-07-14, https://kite.trade/forum/discussion/11460/kite-historical-data-interval-date-range , in fetch-log) |
| `30minute` | **200 days** | 14149 error list |
| `60minute` | **400 days** | 11460: user observed 400 days on a "3hours candle" (the undocumented `3hour`, same 400-day bucket) |
| `day` | **2000 days** | 14149 error list + staff table below (2000 unchanged since 2018) |

- **Tier honesty:** the former SEARCH-tier 8-row table is now UPGRADED — all 8 rows match the live-fetched forum evidence exactly. The strongest form is user-quoted SERVER ERROR MESSAGES on the official Zerodha-operated forum (June 2024), corroborated by a June-2025 error quote (minute=60) and a June-2022 observation (15minute=200, hourly-class=400). It is still NOT a docs-page table — none exists; the messages are the API's own enforcement text. Before hard-coding, a live probe (request cap+1 days per interval) remains the final ratchet.
- **Historical caps were RAISED over time — Verified (LIVE-FORUM runner 2026-07-14 — https://kite.trade/forum/discussion/2875/is-there-any-limitation-on-getting-historical-data , staff sujith, January 2018, verbatim):** "Yes, the limit is as follows All intervals are in days, "minute": 30, "hour": 365, "day": 2000, "3minute": 90, "5minute": 90, "10minute": 90, "15minute": 180, "30minute": 180, "60minute": 365". This 2018 staff table is SUPERSEDED by the 2024/2025 evidence above (minute 30→60, 3/5/10minute 90→100, 15/30minute 180→200, 60minute 365→400; day unchanged at 2000) — recorded here so a stale citation of the 2018 numbers is recognized as such. (Same 2018 thread, staff: "You can make up to 3 requests per second." — see §12.)
- **Behaviour on exceeding a cap: the API ERRORS (does not silently truncate)** — the error MESSAGE text is now known verbatim (`interval exceeds max limit: N days`, above); the surrounding envelope (`error_type`, HTTP status) is still **Unknown** (not shown in any fetched page; presumed the standard `error` envelope of `02-rest-conventions…`). Flagged.
- Longer ranges = client-side chunking loops (multiple requests with sliding windows) — staff-confirmed pattern (2875: "You can get complete historical data but in multiple requests").
- **ARITH sanity check:** 60 days of minute candles ≈ 60 × 375 session minutes = 22,500 candles/request — same order as Dhan's 90-day intraday cap (~33,750) and Groww's 30-day 1-minute cap (~11,250). All three vendors cap intraday requests at the 10⁴-candles-per-response order. (Forum 2875 staff aside, Jan 2018: "For a minute interval data, there will be 360 rows in a day for NSE instruments" — the 360 vs 375 discrepancy is theirs, quoted for the record.)
- **Compare: Dhan** — flat **90 days** per intraday request (`docs/dhan-ref/05-historical-data.md`). **Compare: Groww** — 30/90/180-day caps by interval bucket (`docs/groww-ref/11-historical-candles.md` §4).

## 5. Response shape

**Verified (LIVE-DOC runner 2026-07-14 — historical/ page "Examples", verbatim first 2 candles; byte-identical to the OFFICIAL-MOCK `historical_minute.json` cross-check):**

```json
{
  "status": "success",
  "data": {
    "candles": [
      ["2017-12-15T09:15:00+0530", 1704.5, 1705, 1699.25, 1702.8, 2499],
      ["2017-12-15T09:16:00+0530", 1702, 1702, 1698.15, 1698.15, 1271]
    ]
  }
}
```

- **LIVE-DOC verbatim:** "The response is an array of records, where each record in turn is an array of the following values — `[timestamp, open, high, low, close, volume]`." → **6 elements** without OI; **7 elements with OI** when `oi=1` (see §8; the page's intro names the full candle tuple "(Timestamp, Open, High, Low, Close, Volume, OI)").
- The raw API returns positional arrays, NOT keyed objects — LIVE-DOC (the wording above) + the SDK docstring. SDK parser cross-check Verified (CLIENT-LIB-SOURCE pykiteconnect `_format_historical`): maps `d[0..5]` → `date/open/high/low/close/volume`, `d[6]` → `oi` when present.
- Types (Verified, CLIENT-LIB-SOURCE gokiteconnect `market.go` struct): `Open/High/Low/Close float64`, `Volume int`, `OI int`, `Date` = parsed time.
- Envelope: `{"status": "success", "data": {...}}` (LIVE-DOC example + OFFICIAL-MOCK). Note lowercase `"success"` — contrast Groww's uppercase `"SUCCESS"` (`docs/groww-ref/11-historical-candles.md` §5).
- **Compare: Dhan** returns **columnar parallel arrays** (`open: [...], high: [...]` …) — a completely different shape (`docs/dhan-ref/05-historical-data.md`). Kite and Groww both return row-arrays; only Kite's rows carry a timezone offset.

## 6. Timestamp format & timezone

**Verified (LIVE-DOC runner 2026-07-14 — both docs-page examples):** candle timestamps are ISO-8601 with an explicit numeric IST offset and NO colon in the offset: `"2017-12-15T09:15:00+0530"`.

**Verified (LIVE-FORUM runner 2026-07-14 — forum 3018, staff sujith, Jan 2018, verbatim):** "The params you send is like this `yyyy-mm-dd hh:mm:ss` but the response will be `yyyy-mm-ddThh:mm:ss+0530`. It has always been like this." — the request/response format asymmetry is staff-confirmed.

**Cross-check Verified (CLIENT-LIB-SOURCE gokiteconnect `market.go`):** `time.Parse("2006-01-02T15:04:05-0700", ds)` — wire format `%Y-%m-%dT%H:%M:%S%z` (4-digit offset, no colon). pykiteconnect uses lenient `dateutil.parser.parse`.

- This is the only one of the three vendors whose historical timestamps carry an explicit timezone. **Compare: Dhan** returns UNIX epoch seconds (`docs/dhan-ref/05-historical-data.md`); **Compare: Groww** returns naive strings/epoch with NO timezone stated (Assumed IST) — `docs/groww-ref/11-historical-candles.md` §5. Kite is the least ambiguous of the three here.
- Candle stamp = candle OPEN time — Verified (LIVE-DOC, example-derived): the `from=…09:15:00 to=…09:20:00` request returns candles stamped `09:15:00…09:20:00`, i.e. one per minute stamped at open; day candles in the forum-3018 continuous example are stamped `T09:15:00+0530` (session open).

## 7. `continuous=1` — expired futures contracts

**Verified (LIVE-DOC runner 2026-07-14 — historical/ page "Continuous data" section, verbatim):**

> "…the exchanges flush the `instrument_token` for futures and options contracts for every expiry. … The instrument master API only returns instrument_tokens for contracts that are live. It is not possible to retrieve instrument_tokens for expired contracts from the API, unless you regularly download and cache them. This is where `continuous` API comes in **which works for NFO and MCX futures contracts**. Given a live contract's `instrument_token`, **the API will return `day` candle records** for the same instrument's expired contracts. For instance, assuming the current month is January and you pass NIFTYJAN18FUT's `instrument_token` along with `continuous=1`, you can fetch day candles for December, November ... contracts by simply changing the `from` and `to` dates."

- **Futures only + `day` interval only — now LIVE-DOC** (was a SEARCH-vs-docstring discrepancy). The pykiteconnect docstring's "futures and options instruments" wording is a stale SDK docstring, RESOLVED against it by: (a) the live docs paragraph above; (b) staff sujith, Feb 2018 (forum 3018, verbatim): "We don't have minute level data for expired instruments. We only have day candles for expired futures instruments. We don't have historical data for expired options instruments."; (c) staff sujith, **Feb 2025** (forum 14806, verbatim): "Kite Connect only provides day candle data for expired futures instruments." + staff Sravanthi_bh, Feb 2025: "Historical data won't be available for the expired option contract." — all Verified (LIVE-FORUM runner 2026-07-14, in fetch-log).
- **Expired-futures day-candle depth: from JUL 2011** — Verified (LIVE-FORUM 3018, staff sujith, Feb 2018, verbatim): "We have day's historical data for expired instruments from JUL 2011."
- MCX futures explicitly covered — Verified (LIVE-FORUM 6292, staff rakeshr, Aug 2019): expired MCX future day candles via a live contract's token + `continuous=1`, multi-year `from`/`to` example shown.
- Wire behaviour detail (forum 3018 user-pasted curl transcripts, 2018): `continuous=1` responses are the same candle-array shape; a live contract's own recent candles appear either way — the flag extends the window BACK through prior expiries.
- **Compare: Dhan** has no continuous-contract mechanism; its historical API does not support F&O contracts at all in our tested envelope (`docs/dhan-ref/05-historical-data.md` + `historical-candles-cross-verify.md` "Derivatives: SKIPPED"). **Compare: Groww** serves expired contracts via explicit expiries/contracts discovery endpoints rather than a continuous flag (`docs/groww-ref/11-historical-candles.md` §8).

## 8. `oi=1` — open interest column

- **Verified (LIVE-DOC runner 2026-07-14):** `oi` — "Accepts `0` or `1`. Pass `1` to get OI data" (request-parameters table); the "OI Data" example requests `…&oi=1` and returns 7-element candles, verbatim first candle:

```json
["2019-12-04T09:15:00+0530", 12009.9, 12019.35, 12001.25, 12001.5, 163275, 13667775]
```

(byte-identical to the OFFICIAL-MOCK `historical_oi.json` cross-check). The 7th element (13667775, repeated on consecutive minutes) is the open interest; OI is an int (Verified, gokiteconnect struct `OI int`).
- SDK mapping `d[6]` → `"oi"` — Verified (CLIENT-LIB-SOURCE pykiteconnect `_format_historical`).
- **Compare: Dhan** returns OI as a parallel `open_interest` array gated by an `oi: true` body flag, all-zeros for equities; **Compare: Groww** always carries a 7th element that is `null` for non-FNO.

## 9. Data availability depth & retention

**Verified (LIVE-FORUM runner 2026-07-14 — staff statements, in fetch-log):**

- **Minute-level data starts ~early 2015; anchor date 2015-02-02** — forum 8722 (staff rakeshr, Oct 2020, verbatim): "we have intraday candle from **2015-02-02** for most of the contracts."; staff sujith (same thread): "for some we might have intraday data from early 2015 but for some, we have data from the last quarter of 2015" (back-populated from different sources; per-instrument variance — "The only way to find out is by user fetching data"). Reconfirmed forum 14149 (staff sujith, **June 2024**, verbatim): "There is no fixed date for day candle data but for minute level data it starts somewhere around early 2015".
- **Day candles: no fixed start** — forum 14149 (staff sujith, June 2024, verbatim): "For some NSE stocks, day candles are back filled till late 1990s as well."
- **The former "3-year rolling retention" conflict is RESOLVED:** forum 8722 shows its origin — a stale 2018 FAQ post said intraday history was "up to 3 years"; when pressed, staff corrected to the from-2015-02-02 answer above (Oct 2020), reconfirmed 2024. There is NO rolling minute-data retention window; availability is from ~2015 onward and does not slide.
- Special sessions exist in the data (forum 3018, staff, 2018): Muhurat-day and the Saturday 2015-02-28 budget session have day data; minute data "from FEB 2015" with partial back-population before that.
- Note also §7: expired-futures day candles from JUL 2011.
- **Compare: Dhan** — intraday last **5 years**, daily to inception (`docs/dhan-ref/05-historical-data.md`). **Compare: Groww** — everything from **2020** only (`docs/groww-ref/11-historical-candles.md` §4). Kite has the deepest minute history of the three (~2015) and the deepest daily history (1990s for some NSE stocks).

## 10. SPECIAL SECTION — just-closed-minute freshness: Unknown (live probe required)

**Unknown — the 2026-07-14 live fetch did NOT close this.** The live docs page describes the data as "archived data (up to date as of the time of access)" and shows that granular sub-day windows are official (§2 Note), but states NOTHING about how quickly the just-closed minute becomes available, nor whether the IN-PROGRESS (forming) candle of the current minute is included in a response whose `to` extends past `now`. (One forum-15111 tag string — "Fetching open of running candle … using kite historical data on 1 minute dataframe" — implies users do see a running candle, but that is a tag, not an answer.) Same absence class as Groww (`docs/groww-ref/11-historical-candles.md` §6) and the hole that bit the Dhan spot-1m leg live (the 2026-07-13 SPOT1M-01 window-shape incident, `rest-1m-pipeline-error-codes.md` §0).

**Why it matters:** this is THE gating fact for any future Kite per-minute REST leg mirroring the tickvault `spot_1m_rest` pattern (fetch the just-closed minute ~1s after the boundary, bounded re-poll ladder, measured close-to-data latency). Design consequence if adopted: assume nothing, use a day-granular window + client-side minute filter + measured latency histogram from day one (the Dhan #1499 lesson, the Groww §38 pattern) — even though Kite officially supports narrow windows, the freshness of the newest candle inside one is unproven.

**Probe design (first live session):** at minute close T+1s, request `from=T-1min to=T` at `interval=minute`; ladder re-polls at ~0.7/1.5/3/6s; record when the T-1 candle first appears AND whether a forming candle for the current minute appears (and whether its OHLC mutates across polls — the `to`-inclusive boundary rule of §2 makes a forming-candle appearance plausible).

## 11. Subscription requirement & pricing

**All former SEARCH-tier pricing claims are now LIVE-verified:**

- **Historical add-on FREE since 2025-02-08** — Verified (LIVE-FORUM runner 2026-07-14 — https://kite.trade/forum/discussion/14806/historical-data-is-now-free-with-base-kite-connect-subscription , staff Matti, Feb 2025, verbatim): "We will no longer charge an additional ₹2000 fee for historical data on Kite Connect. With effect from **February 8, 2025**, we've stopped charging for the historical data add-on on Kite Connect. All existing connect apps have been granted historical data permission and new apps will come with the subscription auto-enabled. We've also removed the UI option to subscribe to the historical add-on from the developer console." Staff sujith, same thread: "The add-on historical data subscription is free but you will still need the base Kite Connect subscription to fetch historical data." → **no separate historical entitlement flag exists anymore** (the former Assumed is confirmed: auto-enabled on every app).
- **Base fee ₹2,000 → ₹500/month (announced May 2025)** — Verified (LIVE-FORUM runner 2026-07-14 — https://kite.trade/forum/discussion/15015/revising-kite-connect-fees-from-2000-to-500-per-month , staff Matti, May 2025, verbatim): "we're lowering Kite Connect's price to **₹500 per month for the data (real-time + historical) APIs**. The order placement and account management APIs (view holdings, positions, etc.) **have been free since March 2025**."
- **Current plan structure — Verified (LIVE-DOC runner 2026-07-14 — support article https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/historical-data-and-live-market-data-payment-plan , verbatim):** "No, you do not need to pay separately for historical data and live market data … **Kite Connect Personal API (Free):** … does NOT include live market data or historical data. **Kite Connect (Paid) API:** You get all Personal API features plus access to live market data and historical data at no additional cost. **This plan costs ₹500 per month per API key.**" Corroborated by the charges article (https://support.zerodha.com/…/articles/what-are-the-charges-for-kite-apis , verbatim): "Connect - ₹500/month … Available for retail users at **₹500 per app each month**"; startups building mass-retail products get the APIs free (contact kiteconnect@zerodha.com).
- **Historical cannot be bought standalone** — Verified (LIVE-DOC runner 2026-07-14 — support article https://support.zerodha.com/…/articles/can-i-subscribe-to-historical-api-without-subscribing-to-kite-connect-api , verbatim, entire body): "No, you must subscribe to Kite Connect API first to subscribe to historical API."
- Net current state (2026-07-14): **historical candle data requires the paid Kite Connect plan, ₹500/month per API key/app, which bundles historical + real-time WebSocket data; the free Personal tier has neither.**
- **Compare: Dhan** — Data APIs are a paid plan (`data_plan` gate, `docs/dhan-ref/01-introduction-and-rate-limits.md`); **Compare: Groww** — historical rides the single "active Trading API Subscription" (`docs/groww-ref/11-historical-candles.md` §1).

## 12. Rate limit for this endpoint

- **Historical candle: 3 req/s** — staff-stated TWICE on live-fetched forum pages: forum 2875 (staff sujith, Jan 2018): "You can make up to 3 requests per second."; forum 14806 (staff Sravanthi_bh, **Feb 2025**): "You can make up to 3 requests per second." — Verified (LIVE-FORUM runner 2026-07-14, in fetch-log). The full per-endpoint table (Quote 1/s, Historical 3/s, orders 10/s, others 10/s; breach → HTTP 429) remains per forum 13397 + the exceptions page — see `02-rest-conventions-…` §6b for the cross-file treatment.
- **ARITH:** at 3 req/s a full 60-day minute-candle backfill of ~250 instruments = 250 requests ≈ 84s minimum wall-clock.
- **Compare: Dhan** Data APIs 5/s + 100K/day; **Compare: Groww** rate family for `/historical/*` officially UNASSIGNED (`docs/groww-ref/15-rate-limits-and-capacity.md` §3). Kite is the only vendor of the three with an explicit historical-endpoint-specific published rate.

## 13. SDK method signatures (reference facts; no code vendored)

- **pykiteconnect (Verified, CLIENT-LIB-SOURCE @master):** `historical_data(instrument_token, from_date, to_date, interval, continuous=False, oi=False)` → list of dicts `{date, open, high, low, close, volume[, oi]}` with `date` parsed to an aware datetime via `dateutil`. Default request timeout 7s; no client-side rate limiter or retry in the base client. ⚠ Its docstring's "continuous … for futures and options instruments" is STALE vs the live docs (§7 — futures only).
- **gokiteconnect (Verified, CLIENT-LIB-SOURCE @master `market.go`):** `GetHistoricalData(instrumentToken int, interval string, fromDate time.Time, toDate time.Time, continuous bool, OI bool) ([]HistoricalData, error)`; struct `{Date models.Time, Open/High/Low/Close float64, Volume int, OI int}`; request dates formatted `2006-01-02 15:04:05`; response parsed with layout `2006-01-02T15:04:05-0700`.
- Both SDKs pass `interval` through unvalidated — all enum/caps enforcement is server-side (LIVE-confirmed by §3/§4: the server even accepts undocumented intervals and enforces caps via its own error message).

## OPEN QUESTIONS (for 99-UNKNOWNS)

- ~~Per-interval max-range caps table — official byte-verbatim copy~~ — **RESOLVED by live fetch (2026-07-14):** the live docs page carries NO caps table (Verified-absence); the caps are server-enforced and the full current table is live-verified via the server's own error messages quoted on the official forum (14149 June 2024, corroborated by 15111 June 2025 + 11460 June 2022) — §4. The 99-UNKNOWNS caps row can CLOSE. Residual (non-blocking, ops hygiene): a live cap+1 probe per interval before hard-coding, since the strongest artifact is user-quoted error text, not a docs table.
- ~~Interval enum byte-verbatim~~ — **RESOLVED by live fetch (2026-07-14):** the docs page lists exactly `minute, day, 3minute, 5minute, 10minute, 15minute, 30minute, 60minute` (§3). NEW nuance: the server additionally recognizes UNDOCUMENTED intervals (`2minute`, `4minute`, `hour`, `2hour`, `3hour`, `4hour`, `week`) per the 14149 error list — documented-8 is the supported contract.
- ~~`to` boundary inclusivity~~ — **RESOLVED by live fetch (2026-07-14):** the docs-page examples show the candle whose open time equals `to` IS included (§2). Residual: demonstrated at `minute` interval; `day`-boundary Assumed same (trivial live confirm).
- ~~Minute-data retention: from 2015-02-02 vs "3 years rolling"~~ — **RESOLVED by live fetch (2026-07-14):** staff-confirmed from ~2015-02-02 / early-2015 (2020 + 2024 staff posts); the "3 years" was a stale 2018 FAQ phrase — no rolling window (§9). The 99-UNKNOWNS retention row can CLOSE.
- ~~`continuous=1` exact semantics~~ — **RESOLVED by live fetch (2026-07-14):** live docs paragraph + 2018/2019/2025 staff posts: NFO+MCX FUTURES only, `day` candles only, expired-futures depth from JUL 2011; expired options unavailable (§7). The pykiteconnect docstring is stale.
- ~~Current pricing confirmation~~ — **RESOLVED by live fetch (2026-07-14):** ₹500/month per API key incl. historical + live data; historical add-on free since 2025-02-08; order/account APIs free since March 2025; Personal (free) tier has NO data APIs; historical not purchasable standalone (§11). The 99-UNKNOWNS pricing row can CLOSE.
- **Just-closed-minute freshness + forming-candle behaviour** — STILL Unknown after the live fetch (nothing on the docs page or in any fetched thread answers it); FIRST-LIVE-SESSION probe per §10 ladder. Decides whether a Kite per-minute REST leg (the `spot_1m_rest` pattern) is viable at ~1s latency. Blocking: pipeline (for any per-minute leg).
- **Cap-exceeded error ENVELOPE** — partially resolved: the message text is now known verbatim (`interval exceeds max limit: N days`, §4); the wrapping `error_type` + HTTP status are still Unknown. Live probe: request 61 days of `minute`, capture the full JSON. Blocking: ops (error classification) — minor.
- **`day`-interval `to`-boundary + date-only-string acceptance byte-confirmation** — near-certain (staff example uses date-only strings via SDK, §2), one trivial live request confirms. Blocking: none.

## CLAIMS (for README reconciled table)

- Endpoint is `GET https://api.kite.trade/instruments/historical/:instrument_token/:interval` with query `from,to,continuous,oi`; auth = `X-Kite-Version: 3` + `Authorization: token api_key:access_token` — **Verified (LIVE-DOC runner 2026-07-14; cross-check CLIENT-LIB-SOURCE pykiteconnect)** — https://kite.trade/docs/connect/v3/historical/
- `from`/`to` wire format is `yyyy-mm-dd hh:mm:ss`; sub-day granular windows official; `to`-boundary candle INCLUDED (example-derived) — **Verified (LIVE-DOC runner 2026-07-14; cross-check CLIENT-LIB-SOURCE both SDKs)** — https://kite.trade/docs/connect/v3/historical/
- Documented interval set is exactly `minute, day, 3minute, 5minute, 10minute, 15minute, 30minute, 60minute` (server additionally recognizes undocumented `2minute/4minute/hour/2hour/3hour/4hour/week` — CORRECTION of the former "no 2minute" claim) — **Verified (LIVE-DOC runner 2026-07-14 + LIVE-FORUM 14149)** — https://kite.trade/docs/connect/v3/historical/
- Per-request caps: minute 60d · 3/5/10minute 100d · 15/30minute 200d · 60minute 400d · day 2000d — server-enforced, error `interval exceeds max limit: N days`; NO caps table exists on the docs page (former SEARCH table CONFIRMED; former 2018 staff table 30/90/90/90/180/180/365/2000 SUPERSEDED) — **Verified (LIVE-FORUM runner 2026-07-14 — 14149 server-error quotes + 15111 + 11460; UPGRADED from SEARCH)** — https://kite.trade/forum/discussion/14149/historical-data-retention-policy
- Response = `{"status":"success","data":{"candles":[[ts,o,h,l,c,volume(,oi)] …]}}` — positional arrays, 6 elements (7 with `oi=1`) — **Verified (LIVE-DOC runner 2026-07-14 examples, byte-identical to OFFICIAL-MOCK; CLIENT-LIB-SOURCE `_format_historical`)** — https://kite.trade/docs/connect/v3/historical/
- Candle timestamps are ISO-8601 with explicit `+0530` offset (layout `%Y-%m-%dT%H:%M:%S%z`); request-vs-response format asymmetry staff-confirmed — **Verified (LIVE-DOC runner 2026-07-14 + LIVE-FORUM 3018 staff; cross-check OFFICIAL-MOCK + gokiteconnect)**
- `continuous=1` returns EXPIRED futures contracts' `day` candles via a live contract's token — NFO + MCX FUTURES ONLY, day interval only, expired-futures depth from JUL 2011; expired options unavailable (SDK docstring "futures and options" is stale) — **Verified (LIVE-DOC runner 2026-07-14 docs paragraph + LIVE-FORUM 3018/6292/14806 staff; UPGRADED from SEARCH)** — https://kite.trade/docs/connect/v3/historical/
- `oi=1` appends open interest as int element 7 — **Verified (LIVE-DOC runner 2026-07-14 OI example; cross-check OFFICIAL-MOCK + both SDK parsers)**
- Historical endpoint rate limit = 3 req/s — staff-stated Jan 2018 AND re-stated Feb 2025 — **Verified (LIVE-FORUM runner 2026-07-14 — 2875 + 14806; UPGRADED from SEARCH)** — https://kite.trade/forum/discussion/2875/is-there-any-limitation-on-getting-historical-data
- Pricing: historical add-on (was ₹2,000/mo) FREE since 2025-02-08, auto-enabled on all apps; base Kite Connect revised ₹2,000 → **₹500/month per API key** (May 2025) bundling historical + real-time data; order/account APIs free since March 2025; free Personal tier has NO data APIs; historical not purchasable standalone — **Verified (LIVE-FORUM 14806 + 15015 staff posts; LIVE-DOC support articles `historical-data-and-live-market-data-payment-plan` + `what-are-the-charges-for-kite-apis` + `can-i-subscribe-to-historical-api-without-subscribing-to-kite-connect-api`; UPGRADED from SEARCH)** — https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/historical-data-and-live-market-data-payment-plan
- Minute-level history from ~2015-02-02 (per-instrument variance early-2015…Q4-2015); day candles have no fixed start (some NSE stocks to late 1990s); NO rolling retention window (the "3-year" claim was a stale 2018 FAQ) — **Verified (LIVE-FORUM runner 2026-07-14 — 8722 + 14149 staff posts; UPGRADED from SEARCH, conflict RESOLVED)** — https://kite.trade/forum/discussion/8722/is-intraday-1-minute-historical-data-available-until-2015
- Just-closed-minute availability/latency: UNDOCUMENTED in every fetched source incl. the live docs page — **Unknown** (first-live-session probe; the tickvault per-minute-REST gating fact)
