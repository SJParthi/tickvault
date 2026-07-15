# GDF 04 — Snapshot Bars (SubscribeSnapshot / GetSnapshot / GetExchangeSnapshot)

> **Source:** https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/function-subscribesnapshot/ (slug RESOLVED 2026-07-14) · …/websockets-api-documentation/function-getsnapshot-2/ · …/rest-api-documentation/function-getsnapshot/ · …/rest-api-documentation/getexchangesnapshot/ · …/delayed-api/{subscribesnapshot-delayed, getsnapshot-delayed, getexchangesnapshot-delayed}/
> **Fetched:** 2026-07-13 (pack) + 2026-07-14 (machine crawl) · **Evidence tier:** **RUNNER-DOC (2026-07-14, URLs above)** for everything cited in the reconciliation section below; CLIENT-LIB-SOURCE(wsgfdl-py@1.3.5) + GITHUB-SAMPLE(js-2020, dhelm-2018) retained for SDK-only claims. Verbatim JSON from official artifacts.

---

## 2026-07-14 RUNNER-DOC reconciliation (machine-fetched verbatim corpus, docs-fetch/gdf-2026-07-14 @ 7d354a8d)

The five live pages covering this file were captured verbatim on 2026-07-14. Outcome summary:

| Claim | Verdict | Evidence |
|---|---|---|
| SubscribeSnapshot page slug | **NEW/CLOSED** — `…/websockets-api-documentation/function-subscribesnapshot/` (never captured before) | RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/function-subscribesnapshot/) |
| Push result `MessageType:"RealtimeSnapshotResult"`; returned fields Exchange, InstrumentIdentifier, Periodicity, Period, LastTradeTime, O/H/L/C, TradedQty, OpenInterest; LastTradeTime = epoch SECONDS | **CONFIRMED** | same page |
| SubscribeSnapshot `Periodicity` includes Day | **DRIFT-REFUTED** — the live page lists `["MINUTE"]/["HOUR"]` ONLY; the earlier SEARCH-tier "…and Day (end-of-day)" snippet does NOT appear on this page (Day IS doc-listed on REST GetExchangeSnapshot — see §5) | same page + …/rest-api-documentation/getexchangesnapshot/ |
| Unsubscribe param casing | **DRIFT (qualified)** — the doc param is **`Unsubscribe`** (capital U), `[true]/[false], default = [false]`; wsgfdl-py 1.3.5 sends lowercase `"unsubscribe":"false"` — server tolerance of both is still Assumed (U-16) | same page |
| Bar-stamp convention (U-6) | **CLOSED — open-stamp (bucket-start)**, now DOC-STATED byte-verbatim on two pages; see §2 | …/function-getsnapshot-2/ + …/delayed-api/getsnapshot-delayed/ |
| No-trade bars silently skipped | **CONFIRMED, upgraded to RUNNER-DOC verbatim**: "Also if a user subscribes for snapshot data of any symbol for a period and if no trades happen during that period, no data is sent by the server" | …/function-getsnapshot-2/ |
| "bar `LastTradeTime` ALWAYS :00-aligned in samples" | **DRIFT (qualified)** — the SubscribeSnapshot page's own 2017-era sample has `LastTradeTime:1508835184` (`% 60 == 4`, NOT aligned) with `TradedQty:0` and O=H=L=C; the 2024-era delayed samples (1726206960/1726207020) ARE minute-aligned, 60s apart, delivered ~1s after the boundary. Treat alignment as the live-era norm, not an invariant of every doc sample | function-subscribesnapshot + subscribesnapshot-delayed pages |
| GetSnapshot `Period` list `1,2,3,5,10,15,20,30` (SDK comment) | **DRIFT (doc is open-ended)** — WS doc says "Numerical value 1, 2, 3…", REST doc says "1, 2, 5…", both `default = 1`, MINUTE/HOUR only | …/function-getsnapshot-2/ + …/rest-api-documentation/function-getsnapshot/ |
| GetExchangeSnapshot periodicity `Minute, Hour` | **DRIFT-REFUTED** — REST doc: `Minute, Hour, Day`; BOTH `periodicity` and `period` (1,2,5,10,15,30) are **Mandatory** (pack said defaults) | …/rest-api-documentation/getexchangesnapshot/ |
| `nonTraded` semantics | **NEW fact** — "this parameter should be sent with value **"true" for NSE_IDX** Exchange (NSE Indices)"; indices otherwise vanish from exchange snapshots | same page (+ identical note on the delayed twin) |
| GetExchangeSnapshot From/To, 5-snapshot cap | **CONFIRMED** — From/To optional epoch SECONDS (1567655100 = 2019-09-05 09:15:00 IST, doc-verbatim); `To` mandatory if `From` present; "Maximum 5 snapshots can be requested with single request" | same page |
| `TokenNumber` on SnapshotResult rows | **CONFIRMED (2024-era)** — the GetSnapshot (Delayed) 2024 sample rows carry `"TokenNumber":null` | …/delayed-api/getsnapshot-delayed/ |
| GetSnapshot (Delayed) | **NEW fact** — "(only active during market hours)" per its page title | same page |
| REST output options | **NEW facts** — REST GetSnapshot/GetExchangeSnapshot support `xml=[true]/[false]` and `format=CSV` (mutually exclusive); REST JSON is a BARE ALL-CAPS array with `LASTTRADETIME` in epoch **milliseconds** (e.g. 1434710918000) even though the doc TEXT says "seconds" — doc-internal contradiction, observed wire = ms | …/rest-api-documentation/function-getsnapshot/ |
| WS GetExchangeSnapshot page | sidebar names slug `…/websockets-api-documentation/getexchangesnapshot-2/` — page NOT in the 2026-07-14 crawl; SDK-tier claims for the WS form stand un-upgraded | sidebar of every fetched page |
| `GetExchangeSnapshotAfterMarket` (U-20) | **STILL OPEN** — zero mentions across the 215-page corpus (grep 2026-07-14); the doc GetLimitation sample's `AllowedFunctions` lists only 9 functions and does not name it. It remains an SDK-observed live-response name with UNKNOWN wire shape | corpus-wide grep + …/websockets-api-documentation/function-getlimitation-2/ |

## 1. SubscribeSnapshot — per-symbol STREAMED bar closes

Server pushes one bar per symbol "whenever 1 minute completes" (verbatim GFDL comment) — i.e. delivery at bar completion. One request per symbol; re-subscribe after disconnect. **Official page (RUNNER-DOC 2026-07-14):** `…/websockets-api-documentation/function-subscribesnapshot/` — "SubscribeSnapshot : Subscribe to Realtime Snapshots data, returns data snapshot as per Periodicity & Period values".

Doc-verbatim request (RUNNER-DOC 2026-07-14 — JavaScript sample, `Unsubscribe` omitted = default false):

```json
{"MessageType":"SubscribeSnapshot","Exchange":"NFO","InstrumentIdentifier":"FUTIDX_BANKNIFTY_24NOV2016_XX_0","Periodicity":"MINUTE","Period":1}
```

SDK form (wsgfdl-py 1.3.5 builder uses lowercase `unsubscribe` as a STRING, unlike the doc's `Unsubscribe` boolean param; older ws-gfdl omitted it — dual-case server tolerance still Assumed, U-16):

```json
{"MessageType":"SubscribeSnapshot","Exchange":"NFO","Periodicity":"Minute","Period":1,"InstrumentIdentifier":"NIFTY-I","unsubscribe":"false"}
```

Parameters (doc table, RUNNER-DOC 2026-07-14):
- `Periodicity`: `["MINUTE"]/["HOUR"]` — the live SubscribeSnapshot page lists ONLY these two. **2026-07-14 correction:** the earlier SEARCH-tier snippet claiming "…Hour and Day (end-of-day)" for SubscribeSnapshot does NOT appear on this page; `Day` is doc-listed only on REST GetExchangeSnapshot (§5). Do NOT send Day to SubscribeSnapshot without a live probe.
- `Period`: `1,2,5,10,15,30 default = 1` (doc-verbatim).
- `Unsubscribe`: `[true]/[false], default = [false]` — "If [true] instrumentidentfier is unsubscribed" (doc-verbatim, typo included).
- No backfill/catch-up flag exists on SubscribeSnapshot (verified absence in every SDK signature AND the doc param table) — gap-fill is via GetSnapshot (latest bar) or GetHistory.

Response push (SDK capture, `MessageType:"RealtimeSnapshotResult"`):

```json
{"Exchange":"NFO","InstrumentIdentifier":"NIFTY-I","Periodicity":"MINUTE","Period":1,"LastTradeTime":1669265340,"TradedQty":11300,"OpenInterest":5608700,"Open":18351.0,"High":18353.95,"Low":18350.25,"Close":18353.95,"MessageType":"RealtimeSnapshotResult"}
```

Doc-verbatim response sample (RUNNER-DOC 2026-07-14 — note: 2017-era sample; `LastTradeTime:1508835184` is NOT minute-aligned (`% 60 == 4`) and carries `TradedQty:0` with O=H=L=C — see §2 alignment note):

```json
{"Exchange":"NFO","InstrumentIdentifier":"FUTIDX_BANKNIFTY_24NOV2016_XX_0","Periodicity":"MINUTE","Period":1,"LastTradeTime":1508835184,"TradedQty":0,"OpenInterest":0,"Open":20162.0,"High":20162.0,"Low":20162.0,"Close":20162.0,"MessageType":"RealtimeSnapshotResult"}
```

### 1a. Delayed twin — SubscribeSnapshot (Delayed) with a worked timing example (RUNNER-DOC 2026-07-14)

`…/delayed-api/subscribesnapshot-delayed/`: "works exactly in same manner like SubscribeSnapshot … Only difference is that the data is delayed which will be visible only from the trade timestamp received in the response (field : LastTradeTime) and comparing it with the current time." Same `MessageType:"SubscribeSnapshot"` on the wire (delay is per-API-key backend config, no request change). Doc-verbatim worked example (15-minute NFO delay, 2024-09-13):

```
Request sent at September 13, 2024 11:40:00 (IST)
Response received at
11:41:01 - Last Trade Time (IST): September 13, 2024 11:26:00 AM
11:42:01 - Last Trade Time (IST): September 13, 2024 11:27:00 AM
```

```json
{"Exchange":"NFO","InstrumentIdentifier":"NIFTY-I","Periodicity":"MINUTE","Period":1,"LastTradeTime":1726206960,"TradedQty":10450,"OpenInterest":14626675,"Open":25391.75,"High":25392.5,"Low":25386.0,"Close":25386.2,"MessageType":"RealtimeSnapshotResult"}
{"Exchange":"NFO","InstrumentIdentifier":"NIFTY-I","Periodicity":"MINUTE","Period":1,"LastTradeTime":1726207020,"TradedQty":13475,"OpenInterest":14636550,"Open":25386.25,"High":25388.4,"Low":25378.5,"Close":25380.0,"MessageType":"RealtimeSnapshotResult"}
```

(1726206960 = 2024-09-13 11:26:00 IST, ARITH — matches the doc's own IST gloss; bars minute-aligned, 60s apart, delivered ~1s after each minute boundary. The label 11:26:00 for the bar delivered while the 11:41 minute completes, under a 15-min delay, is bucket-START labeling — the §2 open-stamp corroboration.)

### Bar field table

| Field | Type | Meaning | Notes |
|---|---|---|---|
| `LastTradeTime` | integer | bar label, epoch seconds UTC-base | ALWAYS :00-aligned in samples (1669265340 = 10:19:00 IST, ARITH). Stamp convention → §2 |
| `Open`/`High`/`Low`/`Close` | f64 | bar OHLC | ⚠ here `Close` = the BAR close (NOT prev-day close — dual semantics vs the tick family) |
| `TradedQty` | integer | BAR volume (not cumulative) | |
| `OpenInterest` | integer | OI at bar time | |
| `Periodicity`/`Period` | string/integer | echo of bar spec (`"MINUTE"`, 1) | 2018-era rows lack `Period` → Option-al |
| No bid/ask fields | — | bar payload has no Buy/Sell fields | |

### 1b. NO-TRADE BARS ARE SILENTLY SKIPPED

**RUNNER-DOC verbatim (2026-07-14, …/function-getsnapshot-2/):** "Also if a user subscribes for snapshot data of any symbol for a period and if no trades happen during that period, no data is sent by the server" — upgraded from the earlier SEARCH-tier snippet; note the sentence itself says "subscribes", so it covers SubscribeSnapshot, not just the pull. **Consumers must handle missing bars** — a gap in the stream is NOT a feed failure for an illiquid symbol; conversely a dense index will normally emit every minute. Related: for whole-exchange snapshots of indices, `nonTraded="true"` is REQUIRED on NSE_IDX (§5).

## 2. Bar-stamp convention — open-stamp **CLOSED (U-6, doc-stated 2026-07-14)**

**Status: CLOSED — bars are OPEN-STAMPED (bucket-start labeling).** Two live pages state it explicitly, byte-verbatim (RUNNER-DOC 2026-07-14):

- WS GetSnapshot (…/function-getsnapshot-2/): "GetSnapshot , the time when request is sent is very important. For example : If user sends request for 1minute snapshot data at 9:20:01, he will get record of trade data between 9:19:00 to 9:20:00. However, if request is sent at 9:19:58 i.e. before minute is complete, user will get trade data between 9:18:00 to 9:19:00. **In both cases, timestamp of the returned data will be start timestamp of the period. So when data between 9:15:00 to 9:20:00 is returned, it will have timestamp of 9:15:00.**"
- GetSnapshot (Delayed) (…/delayed-api/getsnapshot-delayed/): repeats the same rule with a 2024 worked example ("…timestamp of the returned data will be start timestamp of the period. So when data between 14:25:00 to 14:26:00 is returned, it will have timestamp of 14:25:00").
- Streamed-bar corroboration: the SubscribeSnapshot (Delayed) worked example (§1a) labels the bar delivered under a 15-min delay at 11:41:01 as `11:26:00` — the START of the completed minute.

Honesty notes: (a) the SubscribeSnapshot page ITSELF is silent on stamping — the explicit statement lives on the GetSnapshot pages, whose Important Note's final sentence explicitly extends to subscribers; a first-live-session sanity check (compare one push's wall-clock arrival vs its label) remains cheap insurance but is no longer a blocking probe. (b) Alignment: live-era samples are minute-aligned (`% 60 == 0`; §1a), but the 2017-era SubscribeSnapshot doc sample (`1508835184`, `% 60 == 4`, TradedQty 0) is NOT — do not hard-assert alignment as a wire invariant. (c) Historical caution retained: an earlier "decisive" arithmetic example in this pack was WRONG (1669265340 decodes to 10:19:00 IST, not the claimed 11:39).

## 3. Daily-bar stamp anomaly — 18:00 IST (date label ONLY)

The ENTIRE 10-row daily series in the official gfdl-rest dump is stamped **18:00:00 IST** (`LASTTRADETIME: 1724243400000` = 2024-08-21 18:00:00 IST, ARITH); the 2018 dhelm daily row (1536280200) decodes to **06:00:00 IST** — a second era-variant. Verdict (ARITH-verified, supersedes any earlier "daily stamped 09:15 session open" claim — that was arithmetic-refuted): **daily `LASTTRADETIME` is a vendor EOD-process stamp, era-dependent. Use it as a DATE label only, never a time key.**

## 4. GetSnapshot — pull, latest completed bar, ≤25 symbols

Doc-verbatim request (RUNNER-DOC 2026-07-14, …/function-getsnapshot-2/ — "Returns latest Snapshot Data of multiple Symbols – max 25 in single call"):

```json
{"MessageType":"GetSnapshot","Exchange":"MCX","Periodicity":"MINUTE","Period":1,"isShortIdentifiers":"true","InstrumentIdentifiers":[{"Value":"CRUDEOIL19MAR21FUT"},{"Value":"NATURALGAS26MAR21FUT"}]}
```

SDK form (GFDL comments: Exchange `NFO, NSE, NSE_IDX, CDS, MCX` mandatory; Period `1,2,3,5,10,15,20,30`):

```json
{"MessageType":"GetSnapshot","Exchange":"NFO","Periodicity":"MINUTE","Period":1,"isShortIdentifiers":"false","InstrumentIdentifiers":[{"Value":"NIFTY-I"},{"Value":"BANKNIFTY-I"}]}
```

Doc param table (RUNNER-DOC 2026-07-14): `Periodicity` `["MINUTE"]/["HOUR"], default = ["MINUTE"]`; `Period` "Numerical value 1, 2, 3…" default 1 (the REST twin says "1, 2, 5…") — the doc is OPEN-ENDED, so the SDK's `1,2,3,5,10,15,20,30` comment is a client-side hint, not a doc-pinned list; `InstrumentIdentifiers` max 25 per request; `isShortIdentifiers` `[true]/[false], default = [false]`.

Doc-verbatim response (RUNNER-DOC 2026-07-14, `MessageType:"SnapshotResult"`):

```json
{"Result":[
{"InstrumentIdentifier":"CRUDEOIL19MAR21FUT","Exchange":"MCX","LastTradeTime":1593008580,"TradedQty":167,"OpenInterest":3760,"Open":3001.0,"High":3003.0,"Low":2998.0,"Close":3000.0},
{"InstrumentIdentifier":"NATURALGAS26MAR21FUT","Exchange":"MCX","LastTradeTime":1593008580,"TradedQty":57,"OpenInterest":10824,"Open":129.8,"High":129.9,"Low":129.7,"Close":129.8}],
"MessageType":"SnapshotResult"}
```

(NOTE this doc sample echoes back the SHORT identifiers as requested — the earlier "REST GetSnapshot echoes back the LONG identifier even when short was requested" observation came from the 2022 README capture; treat echo-format as response-era-dependent, verify live.)

2022 single-row variant: `{"InstrumentIdentifier":"FUTIDX_NIFTY_28JUL2022_XX_0","Exchange":"NFO","LastTradeTime":1658204580,"TradedQty":25550,"OpenInterest":11313250,"Open":16288.45,"High":16291.75,"Low":16286.5,"Close":16287.4,"TokenNumber":null}`. The GetSnapshot (Delayed) page's 2024 sample rows ALSO carry `"TokenNumber":null` (RUNNER-DOC 2026-07-14) — keep `TokenNumber` `Option`-al.

### 4a. Delayed + REST twins (RUNNER-DOC 2026-07-14)

- **GetSnapshot (Delayed)** (…/delayed-api/getsnapshot-delayed/): same wire (`MessageType:"GetSnapshot"`, delay = per-key backend config), "**only active during market hours**" (page title), identifiers may be sent as `NIFTY-I`-style continuous while the response echoes the LONG contract form (2024 sample: sent `NATURALGAS-I`, received `FUTCOM_NATURALGAS_25SEP2024__0`).
- **REST GetSnapshot** (…/rest-api-documentation/function-getsnapshot/): `GET /GetSnapshot/?accessKey=…&exchange=…&periodicity=MINUTE&period=1&instrumentIdentifiers=A+B[&xml=true|&format=CSV][&isShortIdentifiers=true]`. Response is a **BARE ALL-CAPS JSON array** (no `Result` envelope): `[{"CLOSE":17763.25,"EXCHANGE":"NFO","HIGH":…,"INSTRUMENTIDENTIFIER":…,"TRADEDQTY":200,"LASTTRADETIME":1434710918000,…}]` — `LASTTRADETIME` is epoch **milliseconds** in the sample even though the doc text says "seconds" (doc-internal contradiction; observed wire = ms, matching 09's REST convention). XML root `SnapshotArray` with human-readable datetimes; CSV via `format=CSV` (never combined with `xml`).

## 5. GetExchangeSnapshot — pull, ENTIRE exchange, max 5 bars/call

"Returns entire Exchange Snapshot in realtime. This function can return maximum 5 snapshots in single call. You will need to use 'From' and 'To' parameters to control the dataset required." (verbatim GFDL comment)

WS request — JS-sample form (verbatim) and SDK form (note the SDK builder uses lowercase `periodicity`/`period` — casing drift, U-16):

```json
{"MessageType":"GetExchangeSnapshot","Exchange":"NFO","Periodicity":"Minute","Period":1}
{"MessageType":"GetExchangeSnapshot","Exchange":"NFO","periodicity":"MINUTE","period":"1","instrumentType":"FUTIDX","From":1567655100,"To":1567655400,"nonTraded":"false"}
```

- `periodicity` **Mandatory**: `Minute, Hour, Day` (RUNNER-DOC 2026-07-14, REST page — supersedes the pack's "Minute, Hour (default Minute)"; End-Of-Day sample downloads confirm Day). `period` **Mandatory**: `1,2,5,10,15,30` (both marked "Mandatory Parameter" on the doc — the earlier "default" wording was SDK-side).
- `instrumentType` optional (FUTIDX, FUTSTK, OPTIDX, OPTSTK, FUTCOM, FUTCUR, …) — absent → all types.
- `From`/`To` optional, epoch SECONDS (doc-verbatim: 1567655100 = "Thursday, September 5, 2019 9:15:00 AM in GMT+05:30" — a second independent true-UTC-epoch proof). `To` is **mandatory if `From` is mentioned** (RUNNER-DOC 2026-07-14).
- `nonTraded` optional, default `"false"`; `"true"` includes non-traded instruments. **⚠ NEW (RUNNER-DOC 2026-07-14): "this parameter should be sent with value "true" for NSE_IDX Exchange (NSE Indices)"** — indices are otherwise ABSENT from exchange snapshots. ⚠ gfdl-rest 1.0.3 BUG: sends the nonTraded value as query param `max` (09 §5).
- REST extras (RUNNER-DOC 2026-07-14): `xml=[true]/[false]` and `format=CSV` output options (mutually exclusive); example URL `http://endpoint:port/GetExchangeSnapshot/?accessKey=0a0b0c&exchange=NFO&periodicity=Minute&period=1` (accessKey in query string — the R#35 token-in-URL surface). Returned fields: InstrumentIdentifier, Exchange, LastTradeTime, TradedQty, OpenInterest, Open, High, Low, Close.
- Response `MessageType:"ExchangeSnapshotResult"` (WS). The WS page slug is `…/websockets-api-documentation/getexchangesnapshot-2/` (sidebar-known, NOT in the 2026-07-14 crawl — WS-form specifics stay SDK-tier). This is the whole-exchange mechanism that exists TODAY without the Streaming API (10). `GetLimitation.AllowedFunctions` (live SDK response) also names a server-side **GetExchangeSnapshotAfterMarket** — wire shape **Unknown** (U-20; 2026-07-14 corpus-wide grep: ZERO doc mentions, and the doc GetLimitation sample lists only 9 functions without it).
- Delayed twin: `GetExchangeSnapshot (Delayed)` (…/delayed-api/getexchangesnapshot-delayed/, RUNNER-DOC 2026-07-14) — identical param table incl. the NSE_IDX nonTraded note and `Minute, Hour, Day`; same `MessageType:"GetExchangeSnapshot"` on the wire; backend-configured per-exchange delay (01 §3), visible only by comparing `LastTradeTime` with the clock.

## 6. tickvault relevance (bounded note)

`SubscribeSnapshot(Minute,1)` is a native official-1-minute-candle stream (relevant to the `spot_1m_rest` use-case class) and `GetHistory` 1m is its backfill — BUT the bars are vendor-built from GDF's own 1s feed (Assumed — 12 §3), so exact-match parity vs exchange-official or tick-built candles must NOT be expected on High/Low.

## What this prevents

- Treating a missing bar as a dead feed (no-trade bars are silently skipped).
- Keying storage on daily-bar timestamps (18:00/06:00 vendor stamps would corrupt time keys).
- Close-stamping bars: labels are the bucket START (doc-stated 2026-07-14 — U-6 closed); a close-stamped consumer would shift every candle one bucket.
- Paging through GetExchangeSnapshot without From/To (5-snapshot cap per call).
- Silently missing every NSE index from an exchange snapshot (NSE_IDX requires `nonTraded="true"` — doc-stated 2026-07-14).
- Sending `Periodicity:"Day"` to SubscribeSnapshot (doc lists MINUTE/HOUR only; Day is a REST GetExchangeSnapshot value).
