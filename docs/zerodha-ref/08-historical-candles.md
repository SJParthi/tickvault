# Zerodha Kite Connect v3 — Historical Candle Data (reference)

> **Source:** https://kite.trade/docs/connect/v3/historical/ (official live doc — UNREACHABLE from this sandbox; see provenance)
> **Fetched-Verified:** 2026-07-13 via CLIENT-LIB-SOURCE (pykiteconnect@master + gokiteconnect@master over raw.githubusercontent.com), OFFICIAL-MOCK (zerodha/kiteconnect-mocks@main), and SEARCH (result snippets naming kite.trade forum/docs pages). ARCHIVE-DOC and MIRROR-LIVE routes were BOTH proxy-blocked on 2026-07-13 (kite.trade, web.archive.org and r.jina.ai all CONNECT-403 at the egress proxy — see `zerodha-probe.md`), so NOTHING in this file is byte-verbatim from the official docs page; the strongest tier available is official SDK source + official mock JSON.
> **Evidence tiers:** see README legend. Per-file shorthand: **Verified (CLIENT-LIB-SOURCE …)** = read from the official Zerodha SDK source; **Verified (OFFICIAL-MOCK …)** = read from Zerodha's official mock-response repo; **SEARCH (…)** = search-engine extraction naming a kite.trade page — substantive but NOT byte-verbatim, sub-Verified; **ARITH** = derived, derivation shown; **Assumed** / **Unknown** per legend.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. Any TickVault consumer of this API requires its own dated operator grant per `no-rest-except-live-feed-2026-06-27.md` FIRST. (Same banner class as the Dhan full-market-depth stub / groww-ref `11-historical-candles.md`.)
> **Related:** `99-UNKNOWNS.md` (the caps-table re-verification, just-closed-minute freshness, inclusivity, retention rows below); `14-option-chain-composition.md` §5 (India-F&O/BFO coverage note — index/BFO historical coverage is Unknown there).

---

## 1. Endpoint

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master `kiteconnect/connect.py`, fetched 2026-07-13 — https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/connect.py):**

```
GET https://api.kite.trade/instruments/historical/:instrument_token/:interval
```

- Route dict entry, verbatim: `"market.historical": "/instruments/historical/{instrument_token}/{interval}"`.
- Base URL, verbatim: `_default_root_uri = "https://api.kite.trade"`.
- `instrument_token` is the NUMERIC token from the instruments dump (`instruments()` call) — it is a **path segment**, as is `interval`.
- Headers (Verified, same file, `_request`): `X-Kite-Version: 3` (`kite_header_version = "3"`) + `Authorization: token {api_key}:{access_token}` (verbatim format `"token {}".format(api_key + ":" + access_token)`).
- SDK default HTTP timeout: `_default_timeout = 7  # In seconds` (Verified, same file).

**Compare: Dhan/Groww.** Dhan's historical surface is `POST` with a JSON body and securityId as a body field (`docs/dhan-ref/05-historical-data.md`); Groww's is `GET` with query params and a composite `groww_symbol` string (`docs/groww-ref/11-historical-candles.md` §1–§2). Kite is the only one of the three that puts BOTH the instrument identity and the interval in the URL path.

## 2. Request parameters (query string)

**Verified (CLIENT-LIB-SOURCE pykiteconnect `connect.py::historical_data`, fetched 2026-07-13)** — the SDK sends exactly these query params (verbatim from the method body):

```python
params={
    "from": from_date_string,
    "to": to_date_string,
    "interval": interval,
    "continuous": 1 if continuous else 0,
    "oi": 1 if oi else 0
}
```

| Param | Format | Notes |
|---|---|---|
| `from` | `yyyy-mm-dd HH:MM:SS` (SDK: `strftime("%Y-%m-%d %H:%M:%S")` on a `datetime`, or a pass-through string) | Verified (CLIENT-LIB-SOURCE pykiteconnect). Go SDK identical: `fromDate.Format("2006-01-02 15:04:05")` (Verified, CLIENT-LIB-SOURCE gokiteconnect@master `market.go`, fetched 2026-07-13 — https://raw.githubusercontent.com/zerodha/gokiteconnect/master/market.go). Docstring: "From date (datetime object or string in format of yyyy-mm-dd HH:MM:SS" — a date-only `yyyy-mm-dd` string is also commonly used; whether the server accepts date-only is **Assumed yes** (the docstring's own example wording implies both), not source-verified here. |
| `to` | same as `from` | **Inclusivity of `to` is Unknown** — neither SDK documents whether the `to` boundary candle is included (contrast Dhan, whose daily `toDate` is documented NON-inclusive — `docs/dhan-ref/05-historical-data.md`). Flagged in OPEN QUESTIONS. |
| `interval` | string; also appears in the URL path (the SDK sends it in BOTH places — path via `url_args` and query via `params`; verbatim in the method body) | see §3 |
| `continuous` | `1`/`0` int | see §7 |
| `oi` | `1`/`0` int | see §8 |

**Interval also in query — wart (Verified, CLIENT-LIB-SOURCE):** the SDK redundantly sends `interval` as a query param even though it is already a path segment. Harmless, but a faithful client mirror should note the path segment is what the route template uses.

## 3. Intervals — the full set

**SEARCH (kite.trade forum + docs result extraction, fetched 2026-07-13 — result set incl. https://kite.trade/forum/discussion/11460/kite-historical-data-interval-date-range and https://kite.trade/docs/connect/v3/historical/):** the documented interval strings are:

`minute`, `3minute`, `5minute`, `10minute`, `15minute`, `30minute`, `60minute`, `day`

- **Verified (CLIENT-LIB-SOURCE pykiteconnect):** the SDK defines NO interval enum/constants — "No INTERVAL_* constants are defined in the class"; the docstring cites free strings ("minute, day, 5 minute etc."). All interval validation is server-side.
- The `2minute` value does NOT exist on Kite (contrast Groww's 12-value enum including `2minute`/`1hour`/`4hour`/`1week`/`1month` — `docs/groww-ref/11-historical-candles.md` §3). **Verified-absence only at SEARCH tier** — the official docs page could not be fetched to byte-verify the enum; operator-paste requested (OPEN QUESTIONS).
- **Compare: Dhan** exposes intraday intervals as bare strings `"1"/"5"/"15"/"25"/"60"` on a separate intraday endpoint + a distinct daily endpoint (`docs/dhan-ref/05-historical-data.md`); Kite uses ONE endpoint with the interval in the path.

## 4. THE PER-INTERVAL MAX RANGE CAPS (per request)

**SEARCH (kite.trade forum result extraction, fetched 2026-07-13 — result set incl. https://kite.trade/forum/discussion/2875/is-there-any-limitation-on-getting-historical-data , https://kite.trade/forum/discussion/11460/kite-historical-data-interval-date-range , https://kite.trade/forum/discussion/15111/historical-data-limit-60-days ; the same numbers were independently returned by a second search pass on data-depth):**

| Interval | Max days per request |
|---|---|
| `minute` | **60 days** |
| `3minute` | **100 days** |
| `5minute` | **100 days** |
| `10minute` | **100 days** |
| `15minute` | **200 days** |
| `30minute` | **200 days** |
| `60minute` | **400 days** |
| `day` | **2000 days** |

- ⚠ **Tier honesty:** these are the load-bearing numbers of this file and they are SEARCH-tier only (the official caps table lives on the blocked https://kite.trade/docs/connect/v3/historical/ page). Two independent search extractions agreed on all 8 rows, but NO fetched artifact carries them byte-verbatim. **Operator paste of the live docs table is the #1 open question of this pack.** Do NOT hard-code these into any client without that re-verification.
- Behaviour on exceeding a cap: the API errors (does not silently truncate) per forum discussion titles in the result set — but the exact error envelope is **Unknown** (not fetched). Flagged.
- Longer ranges = client-side chunking loops (multiple requests with sliding windows) — the universal pattern in every result.
- **ARITH sanity check:** 60 days of minute candles ≈ 60 × 375 session minutes = 22,500 candles/request — same order as Dhan's 90-day intraday cap (~90 × 375 = 33,750) and Groww's 30-day 1-minute cap (~11,250). All three vendors cap intraday requests at the 10⁴-candles-per-response order.
- **Compare: Dhan** — flat **90 days** per intraday request, 5-year intraday depth, daily back to inception (`docs/dhan-ref/05-historical-data.md`). **Compare: Groww** — 30/90/180-day caps by interval bucket, data from 2020 (`docs/groww-ref/11-historical-candles.md` §4).

## 5. Response shape

**Verified (OFFICIAL-MOCK zerodha/kiteconnect-mocks@main `historical_minute.json`, fetched 2026-07-13 — https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/historical_minute.json), verbatim (first 2 candles):**

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

- **Candle array = `[timestamp, open, high, low, close, volume]` — 6 elements** without OI; **7 elements with OI** when `oi=1` (see §8). Field order Verified twice: by the mock and by the SDK's `_format_historical` parser, verbatim:

```python
record = {
    "date": dateutil.parser.parse(d[0]),
    "open": d[1], "high": d[2], "low": d[3], "close": d[4], "volume": d[5],
}
if len(d) == 7:
    record["oi"] = d[6]
```

(Verified, CLIENT-LIB-SOURCE pykiteconnect `connect.py::_format_historical`.)
- The raw API returns positional arrays, NOT keyed objects — the SDK docstring states this explicitly ("the actual response JSON from the API does not have field names such has 'open', 'high' etc.").
- Types (Verified, CLIENT-LIB-SOURCE gokiteconnect `market.go` struct): `Open/High/Low/Close float64`, `Volume int`, `OI int`, `Date` = parsed time.
- Envelope: `{"status": "success", "data": {...}}` (Verified, OFFICIAL-MOCK). Note lowercase `"success"` — contrast Groww's uppercase `"SUCCESS"` (`docs/groww-ref/11-historical-candles.md` §5).
- **Compare: Dhan** returns **columnar parallel arrays** (`open: [...], high: [...]` …) — a completely different shape (`docs/dhan-ref/05-historical-data.md`). Kite and Groww both return row-arrays; only Kite's rows carry a timezone offset.

## 6. Timestamp format & timezone

**Verified (OFFICIAL-MOCK `historical_minute.json` + `historical_oi.json`):** candle timestamps are ISO-8601 with an explicit numeric IST offset and NO colon in the offset: `"2017-12-15T09:15:00+0530"`.

**Verified (CLIENT-LIB-SOURCE gokiteconnect `market.go`):** the Go SDK parses exactly this layout — `time.Parse("2006-01-02T15:04:05-0700", ds)` — confirming the wire format is `%Y-%m-%dT%H:%M:%S%z` (4-digit offset, no colon). pykiteconnect uses lenient `dateutil.parser.parse`.

- This is the only one of the three vendors whose historical timestamps carry an explicit timezone. **Compare: Dhan** returns UNIX epoch seconds (daily stamped at UTC 18:30 prev-day = IST midnight; intraday at candle open UTC) — `docs/dhan-ref/05-historical-data.md`; **Compare: Groww** returns naive strings/epoch with NO timezone stated anywhere (Assumed IST) — `docs/groww-ref/11-historical-candles.md` §5. Kite is the least ambiguous of the three here.
- Candle stamp = candle OPEN time (Assumed from the mock: first candle of the session is `09:15:00`, NSE open; not doc-verified).

## 7. `continuous=1` — expired futures contracts

**Verified (CLIENT-LIB-SOURCE pykiteconnect):** boolean flag, sent as `continuous: 1|0`; docstring verbatim: "`continuous` is a boolean flag to get continuous data for futures and options instruments."

**SEARCH (kite.trade forum result extraction, fetched 2026-07-13 — result set incl. https://kite.trade/forum/discussion/3018/how-to-get-historical-data-for-expired-futures and https://kite.trade/forum/discussion/6292/historical-data-for-expired-contracts):**
- Works for NFO and MCX **futures** contracts: pass a LIVE contract's `instrument_token` with `continuous=1` and past `from`/`to` dates to get the SAME underlying's EXPIRED contracts' candles.
- **`day` interval only** — "The continuous data option gives only day candles… no intra-day data for expired future instruments."
- NOT available for options contracts (futures only), per the same result set. (Note the pykiteconnect docstring says "futures and options instruments" — a doc-vs-behaviour discrepancy between the SDK docstring and the forum-stated behaviour; flag as **Unknown** which is authoritative, operator paste requested.)
- **Compare: Dhan** has no continuous-contract mechanism; its historical API does not support F&O contracts at all in our tested envelope (`docs/dhan-ref/05-historical-data.md` + `historical-candles-cross-verify.md` "Derivatives: SKIPPED"). **Compare: Groww** serves expired contracts via explicit expiries/contracts discovery endpoints rather than a continuous flag (`docs/groww-ref/11-historical-candles.md` §8).

## 8. `oi=1` — open interest column

- **Verified (CLIENT-LIB-SOURCE pykiteconnect):** `oi` boolean → query `oi=1`; when present the candle array grows to 7 elements and the SDK maps `d[6]` → `"oi"`.
- **Verified (OFFICIAL-MOCK zerodha/kiteconnect-mocks@main `historical_oi.json`, fetched 2026-07-13):** verbatim first candle:

```json
["2019-12-04T09:15:00+0530", 12009.9, 12019.35, 12001.25, 12001.5, 163275, 13667775]
```

7 elements; the 7th (13667775, repeated on consecutive minutes) is the open interest. OI is an int (Verified, gokiteconnect struct `OI int`).
- **Compare: Dhan** returns OI as a parallel `open_interest` array gated by an `oi: true` body flag, all-zeros for equities; **Compare: Groww** always carries a 7th element that is `null` for non-FNO.

## 9. Data availability depth & retention

**SEARCH (kite.trade forum result extraction, fetched 2026-07-13 — result set incl. https://kite.trade/forum/discussion/8722/is-intraday-1-minute-historical-data-available-until-2015 and https://kite.trade/forum/discussion/14149/historical-data-retention-policy):**
- Intraday (minute-level) candle data available **from ~2015-02-02** for most contracts.
- Day candles: no fixed start — some NSE stocks back-filled to the late 1990s.
- ⚠ **Conflicting retention signal:** one extraction also stated "for 1-minute candles, up to 3 years of data can be fetched" — which conflicts with from-2015 availability. Whether a rolling minute-data retention window exists is **Unknown**; both claims are SEARCH-tier from forum threads of different dates. Flagged for operator paste (the retention-policy thread).
- **Compare: Dhan** — intraday last **5 years**, daily to inception (`docs/dhan-ref/05-historical-data.md`). **Compare: Groww** — everything from **2020** only (`docs/groww-ref/11-historical-candles.md` §4).

## 10. SPECIAL SECTION — just-closed-minute freshness: Unknown (live probe required)

**Unknown.** No fetched source states whether the JUST-CLOSED minute candle is served immediately after its boundary, nor whether the IN-PROGRESS (forming) candle of the current day is included in a response whose `to` extends past `now`. The official docs page (blocked) is not expected to document latency either — the same absence Groww showed (`docs/groww-ref/11-historical-candles.md` §6 grep-verified absence) and the same hole that bit the Dhan spot-1m leg live (the 2026-07-13 SPOT1M-01 window-shape incident, `rest-1m-pipeline-error-codes.md` §0).

**Why it matters:** this is THE gating fact for any future Kite per-minute REST leg mirroring the tickvault `spot_1m_rest` pattern (fetch the just-closed minute ~1s after the boundary, bounded re-poll ladder, measured close-to-data latency). Design consequence if adopted: assume nothing, use a day-granular window + client-side minute filter + measured latency histogram from day one (the Dhan #1499 lesson, the Groww §38 pattern).

**Probe design (first live session):** at minute close T+1s, request `from=T-1min to=T` at `interval=minute`; ladder re-polls at ~0.7/1.5/3/6s; record when the T-1 candle first appears AND whether a forming candle for the current minute appears (and whether its OHLC mutates across polls).

## 11. Subscription requirement & pricing

**SEARCH (fetched 2026-07-13 — result set incl. https://kite.trade/forum/discussion/14806/historical-data-is-now-free-with-base-kite-connect-subscription , https://kite.trade/forum/discussion/15015/revising-kite-connect-fees-from-2000-to-500-per-month , https://www.marketcalls.in/fintech/zerodha-makes-trading-api-free-for-personal-use-bundles-historical-data-with-connect-api.html , https://zerodha.com/products/api/):**
- **Historically:** Kite Connect base ₹2,000/month + a SEPARATE historical-data add-on ₹2,000/month.
- **As of 2025-02-08:** the historical add-on was made FREE — historical data bundled with the base Kite Connect subscription; existing apps auto-granted historical permission.
- **Subsequently:** base Kite Connect fee revised **₹2,000 → ₹500/month** (real-time WebSocket + historical candle APIs included); trading APIs free for personal use per the same announcements.
- ⚠ All pricing is SEARCH-tier (announcement pages 403-blocked from the sandbox); amounts and effective dates need operator confirmation from https://zerodha.com/products/api/ before entering any cost table. No add-on entitlement flag exists anymore per these results — the historical endpoint works on any active Kite Connect app subscription (**Assumed** from the bundling announcements).
- **Compare: Dhan** — Data APIs are a paid plan (`data_plan` gate, `docs/dhan-ref/01-introduction-and-rate-limits.md`); **Compare: Groww** — historical rides the single "active Trading API Subscription" (`docs/groww-ref/11-historical-candles.md` §1).

## 12. Rate limit for this endpoint

**SEARCH (fetched 2026-07-13 — result set incl. https://kite.trade/forum/discussion/13397/rate-limits and https://kite.trade/docs/connect/v3/exceptions/):** official per-endpoint rate limits: Quote **1 req/s**, **Historical candle 3 req/s**, order placement 10 req/s (+ per-minute/day RMS order caps — out of this file's scope; see `02-rest-conventions-…` §6b, which records the conflicting 200-vs-400/min readings), all other endpoints 10 req/s. No per-minute/per-day cap on historical; exceeding → HTTP **429**.
- **ARITH:** at 3 req/s a full 60-day minute-candle backfill of ~250 instruments = 250 requests ≈ 84s minimum wall-clock.
- **Compare: Dhan** Data APIs 5/s + 100K/day; **Compare: Groww** rate family for `/historical/*` officially UNASSIGNED (`docs/groww-ref/15-rate-limits-and-capacity.md` §3). Kite is the only vendor of the three with an explicit historical-endpoint-specific published rate.

## 13. SDK method signatures (reference facts; no code vendored)

- **pykiteconnect (Verified, CLIENT-LIB-SOURCE @master):** `historical_data(instrument_token, from_date, to_date, interval, continuous=False, oi=False)` → list of dicts `{date, open, high, low, close, volume[, oi]}` with `date` parsed to an aware datetime via `dateutil`. Default request timeout 7s; no client-side rate limiter or retry in the base client.
- **gokiteconnect (Verified, CLIENT-LIB-SOURCE @master `market.go`):** `GetHistoricalData(instrumentToken int, interval string, fromDate time.Time, toDate time.Time, continuous bool, OI bool) ([]HistoricalData, error)`; struct `{Date models.Time, Open/High/Low/Close float64, Volume int, OI int}`; request dates formatted `2006-01-02 15:04:05`; response parsed with layout `2006-01-02T15:04:05-0700`.
- Both SDKs pass `interval` through unvalidated — all enum/caps enforcement is server-side (same posture as Groww's SDK, `docs/groww-ref/11-historical-candles.md` §9).

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Per-interval max-range caps table — official byte-verbatim copy.** Paste https://kite.trade/docs/connect/v3/historical/ (the whole page). The §4 8-row table is SEARCH-tier only; it is the most load-bearing numeric table of the pack (chunking-loop design depends on it). Blocking: pipeline.
- **Interval enum byte-verbatim** (does the docs page list exactly `minute,3minute,5minute,10minute,15minute,30minute,60minute,day`, and no others?). Same paste as above. Blocking: pipeline.
- **`to` boundary inclusivity** (is the candle at exactly `to` included?). Same paste + live probe (request `to` = a known candle's open time; check presence). Matters because Dhan's non-inclusive `toDate` already caused off-by-one-day bugs. Blocking: pipeline.
- **Just-closed-minute freshness + forming-candle behaviour** — undocumented everywhere; FIRST-LIVE-SESSION probe per §10 ladder. Decides whether a Kite per-minute REST leg (the `spot_1m_rest` pattern) is viable at ~1s latency. Blocking: pipeline (for any per-minute leg).
- **Minute-data retention: from 2015-02-02 vs "3 years rolling"** — conflicting forum claims. Paste https://kite.trade/forum/discussion/14149/historical-data-retention-policy + probe (request a 2016 minute range on RELIANCE). Blocking: capacity (backfill design).
- **`continuous=1` exact semantics** — docstring says "futures and options"; forum says futures-only + day-interval-only. Paste the §7 docs paragraph from https://kite.trade/docs/connect/v3/historical/ . Blocking: none (tickvault doesn't trade expired contracts).
- **Cap-exceeded error envelope** (exact `error_type`/message for an over-range request — `InputException`?). Live probe: request 61 days of `minute`. Blocking: ops (error classification).
- **Current pricing page confirmation** — paste https://zerodha.com/products/api/ (₹500/month incl. historical, free-personal-use terms, effective dates). Blocking: none (cost planning only).

## CLAIMS (for README reconciled table)

- Endpoint is `GET https://api.kite.trade/instruments/historical/{instrument_token}/{interval}` with query `from,to,interval,continuous,oi`; auth = `X-Kite-Version: 3` + `Authorization: token api_key:access_token` — **Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py)** — https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/connect.py
- `from`/`to` wire format is `yyyy-mm-dd HH:MM:SS` (both official SDKs format exactly this) — **Verified (CLIENT-LIB-SOURCE pykiteconnect + gokiteconnect market.go)** — https://raw.githubusercontent.com/zerodha/gokiteconnect/master/market.go
- Response = `{"status":"success","data":{"candles":[[ts,o,h,l,c,volume(,oi)] …]}}` — positional arrays, 6 elements (7 with `oi=1`) — **Verified (OFFICIAL-MOCK historical_minute.json + historical_oi.json; CLIENT-LIB-SOURCE `_format_historical`)** — https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/historical_minute.json
- Candle timestamps are ISO-8601 with explicit `+0530` offset (layout `%Y-%m-%dT%H:%M:%S%z`) — **Verified (OFFICIAL-MOCK + CLIENT-LIB-SOURCE gokiteconnect `time.Parse("2006-01-02T15:04:05-0700", …)`)**
- Interval set: `minute,3minute,5minute,10minute,15minute,30minute,60minute,day`; no SDK-side enum (server-validated) — **SEARCH (kite.trade docs/forum results, 2026-07-13) + Verified-absence (CLIENT-LIB-SOURCE: no INTERVAL constants)**
- Per-request caps: minute 60d · 3/5/10minute 100d · 15/30minute 200d · 60minute 400d · day 2000d — **SEARCH (two independent extractions over kite.trade forum results, 2026-07-13; official table on the blocked docs page — operator paste pending)** — https://kite.trade/forum/discussion/2875/is-there-any-limitation-on-getting-historical-data
- `continuous=1` returns EXPIRED futures contracts' candles via the live contract's token, day interval only, futures only (SDK docstring says "futures and options" — discrepancy flagged) — **SEARCH (kite forum, 2026-07-13) + Verified (CLIENT-LIB-SOURCE docstring)** — https://kite.trade/forum/discussion/3018/how-to-get-historical-data-for-expired-futures
- `oi=1` appends open interest as int element 7 — **Verified (OFFICIAL-MOCK historical_oi.json + both SDK parsers)**
- Historical endpoint rate limit = 3 req/s (Quote 1/s, others 10/s); breach → HTTP 429 — **SEARCH (kite forum rate-limits threads + docs/exceptions page results, 2026-07-13)** — https://kite.trade/forum/discussion/13397/rate-limits
- Historical add-on (was ₹2,000/mo) FREE since 2025-02-08, bundled into Kite Connect; base fee later revised ₹2,000 → ₹500/mo — **SEARCH (kite forum announcements + marketcalls result, 2026-07-13)** — https://kite.trade/forum/discussion/14806/historical-data-is-now-free-with-base-kite-connect-subscription
- Minute-level history available from ~2015-02-02 (day candles further back; a conflicting "3-year rolling" forum claim exists → Unknown/probe) — **SEARCH (kite forum, 2026-07-13)** — https://kite.trade/forum/discussion/8722/is-intraday-1-minute-historical-data-available-until-2015
- Just-closed-minute availability/latency: UNDOCUMENTED in every fetched source — **Unknown** (first-live-session probe; the tickvault per-minute-REST gating fact)
