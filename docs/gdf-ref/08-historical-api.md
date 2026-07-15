# GDF 08 — Historical API (GetHistory / GetHistoryAfterMarket / GetHistoryGreeks)

> **Source:** https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/function-gethistory-2/ · …/rest-api-documentation/function-gethistory/ · …/websockets-api-documentation/gethistoryaftermarket-3/ · …/rest-api-documentation/gethistoryaftermarket-2/ · …/greeks-api/gethistorygreeks-2/ · …/faqs/faqs/
> **Fetched:** 2026-07-13 · **Evidence tier:** CLIENT-LIB-SOURCE(wsgfdl-py@1.3.5 :: history*.py; gfdl-rest@1.0.3 :: GetHistory*.py + READMEs) + GITHUB-SAMPLE(js-2020, dhelm-2018) + SEARCH + ARITH. Verbatim JSON from official artifacts.

---

## 2026-07-14 RUNNER-DOC reconciliation (machine-fetched live docs, GitHub runner @ 7d354a8d)

Every GetHistory-family live doc page was fetched verbatim 2026-07-14; the claims below upgrade
to **RUNNER-DOC (2026-07-14, \<url\>)**. Corrections are also integrated in place below, dated.

| # | Verdict | Fact | Source |
|---|---|---|---|
| 1 | CONFIRMED | WS GetHistory params (Exchange, InstrumentIdentifier, Periodicity `TICK/MINUTE/HOUR/DAY/WEEK/MONTH`, Period, From, To, Max, UserTag, IsShortIdentifier, AdjustSplits default **true**); From/To = "no. of seconds since Epoch time" with worked examples `1388534400 (01-01-2014 0:0:0)` / `1412121600 (10-01-2014 0:0:0)` — both are **00:00:00 UTC** instants (ARITH), re-confirming the plain-UTC epoch base (U-23) | RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/function-gethistory-2/) |
| 2 | **DRIFT-CORRECTED** | Doc defaults: **Periodicity default = TICK** and **Max default = 0 ("means all available data")** — the §1 "SDK defaults Max=10, MINUTE" are wsgfdl-py wrapper defaults, NOT server/doc defaults | same page |
| 3 | **DRIFT-CORRECTED** | `Period`: doc says "Numerical value 1, 2, 3…, default = 1 … Can be applied for [MINUTE]/[HOUR]/[DAY]". The §1 list `1,2,3,4,5,10,12,15,20,30` is superseded by the per-periodicity retention table: Minute periods 1,2,3,4 / 5,6,10,12 / 15,20,30; **Hour periods 1,2,3,4,6,8,12** | RUNNER-DOC (2026-07-14, …/introduction/type-of-data-available/) |
| 4 | CONFIRMED | Returned row = LastTradeTime, QuotationLot (LotSize), TradedQty, OpenInterest, Open, High, Low, Close; WS LastTradeTime epoch-seconds | function-gethistory-2 page |
| 5 | CONFIRMED | Request/Result envelope + `HistoryOHLCResult`/`HistoryTickResult`, rows **newest-first** (official 2024-07 Python AllFunctions PDF: DAY rows 1719923400→1719405000, MINUTE 1723784940→1723784400, TICK 1723785033→1723785018, all descending); Request echo carries `"AdjustSplits":true` | RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/wp-content/uploads/2024/07/GFDL_Websocket_Python_Sample_AllFunctions.pdf) |
| 6 | CONFIRMED | DAY bar vendor stamp: PDF DAY row `1719923400` = 2024-07-02 **18:00:00 IST** (ARITH) — the 04 §3 18:00-IST daily-stamp claim holds on official sample data | same PDF |
| 7 | **U-7 PARTIAL-CLOSE** | Tick history granularity is officially "**Every 1 Sec**" (retention table row "Tick \| Every 1 Sec \| 1 Calendar Week") — tick history IS the 1-second L1 archive, never sub-second trades. PDF TICK sample rows are irregularly spaced (…018, …020, …033 — 2s and 13s gaps), so rows exist only for seconds with activity. Byte-parity with the live 1s stream: still a live probe | type-of-data page + AllFunctions PDF |
| 8 | CONFIRMED (tier↑) | Doc-ceiling depth table now RUNNER-DOC, incl. the **contractwise splits** the pack lacked: NFO continuous futures D/W/M since 2010, **futures contractwise D/W/M = 3 calendar months, options contractwise = 1 calendar month** (bounded expired-contract lookback — U-8-relevant) | type-of-data page |
| 9 | CONFIRMED | REST GetHistory params mirror WS in camelCase query params + `xml`/`format=CSV` switches; **REST request `from`/`to` are epoch SECONDS** (doc example URL `from=1716269700&to=1718861700`) — see the §4 correction | RUNNER-DOC (2026-07-14, …/rest-api-documentation/function-gethistory/) |
| 10 | **DISCREPANCY (live-verify)** | REST GetHistory response `LastTradeTime`: the doc PROSE says "In JSON Response, this value is expressed as no. of seconds since Epoch" — but the gfdl-rest SDK dump shows 13-digit **milliseconds**, and sibling REST doc pages' own JSON samples show ms (GetSnapshot sample `"LASTTRADETIME": 1434710918000`). Doc prose vs doc samples conflict; treat REST responses as ms until live-probed, accept both defensively | function-gethistory (REST) + function-getsnapshot (REST) pages |
| 11 | CONFIRMED | GetHistoryAfterMarket exists on BOTH transports with identical verbatim purpose text: history "till **previous working day**"; "To receive current day's historical data via this function, you will need to send request after market is closed"; positioned as cheaper for backtesting-class use | RUNNER-DOC (2026-07-14, …/websockets-api-documentation/gethistoryaftermarket-3/ + …/rest-api-documentation/gethistoryaftermarket-2/) — see §5 update (U-22) |
| 12 | CONFIRMED + extended | GetHistoryGreeks (WS): field list Token…DTR verbatim; **Periodicity = MINUTE or TICK only** (Period applies to MINUTE/TICK); From/To epoch-seconds; Max default 0; verbatim `HistoryGreeksResult` sample — Request echo + `Result[]` newest-first (1731999380→1731998888 descending), `Token` a STRING (`"48093"`), `Timestamp` epoch-seconds | RUNNER-DOC (2026-07-14, …/greeks-api/gethistorygreeks-2/) — see §6 update |
| 13 | NEW | A dedicated **REST GetHistoryGreeks doc page exists** in the live nav (`…/greeks-api-global-datafeeds-apis/gethistorygreeks-returns-historical-greeks-data/` — not fetched this run), i.e. GDF documents a real REST GetHistoryGreeks function; the gfdl-rest SDK posting to `GetHistory/` (§6/§7) reads as an SDK bug, not server routing (U-22 PARTIAL) | nav links on every 2026-07-14 page |
| 14 | NEW | Official sample artifacts: 2024-07 WS AllFunctions PDFs/zips (JavaScript + Python) and REST GetHistory response-format PDFs `GFDL_REST_GetHistory_{JSON,XML,CSV}_FORMAT.pdf` under `globaldatafeeds.in/resources/API_Example_pdf/` | function-gethistory pages |
| 15 | NEW (U-2-adjacent) | The REST GetHistory doc's own example URL is `https://endpoint:port/GetHistory/?accessKey=…` — **https shown in an official example** (the connect page still shows `http://`); TLS availability remains a live/sales question | function-gethistory (REST) page |
| 16 | CONFIRMED (tier↑) | Delayed-API lane has its own GetHistory (Delayed) pages on both transports with the same param table (REST: `…/delayed-api-rest-api-documentation/gethistory-delayed-returns-historical-data-tick-minute-eod/`) | RUNNER-DOC (2026-07-14) |
| 17 | CONFIRMED (tier↑) | Quota tiers "1800, 3600, or 7200 requests per hour" + subscribe/unsubscribe/resubscribe each consuming quota are now doc-verbatim (was SEARCH-Assumed) | RUNNER-DOC (2026-07-14, …/faqs/faqs/) |

Not found in the 2026-07-14 corpus (stay at prior tier): the "GetHistoryAfterMarket not enabled
together with GetHistory on one key" claim (§5 — still Assumed, no doc support either way); the
per-key GetLimitation numbers (§3 layer (iii) — unchanged, per-key truth).

---

## 1. GetHistory — WS request (three wrapper forms, verbatim)

```json
{"MessageType":"GetHistory","Exchange":"NFO","InstrumentIdentifier":"NIFTY-I","Periodicity":"MINUTE","Period":1,"From":1658115000,"To":1658138400,"Max":10,"UserTag":"x","isShortIdentifier":"false"}
```

| Form | Fields | Notes |
|---|---|---|
| by period | From + To (epoch SECONDS) | `From:0` → "entire possible history" (verbatim); omit `To` or send a future time → till latest |
| by count | `Max:<N>` (no From/To) | SDK-WRAPPER defaults: Max=10, Periodicity `MINUTE`, Period 1. **Doc/server defaults differ (RUNNER-DOC 2026-07-14): Periodicity default = TICK, Max default = 0 "means all available data"** |
| adjusted (1.3.5 NEW) | adds `"AdjustSplits":<bool>` between To and Max | Parameter-table default **"true"** — split-adjusted rows by default (RUNNER-DOC-confirmed on WS + REST pages, 2026-07-14; the PDF response echo carries `"AdjustSplits":true`) |

- `Periodicity` (verbatim): `Tick, Minute, Hour, Day, Week, Month`. `Period` doc-verbatim (2026-07-14): "Numerical value 1, 2, 3…, default = 1", applicable to MINUTE/HOUR/DAY; the per-periodicity supported sets (type-of-data page) are Minute `1,2,3,4 / 5,6,10,12 / 15,20,30` and Hour `1,2,3,4,6,8,12` (the older flat list `1,2,3,4,5,10,12,15,20,30` was the SDK-era rendering).
- `isShortIdentifier:"true"` with contractwise symbols; `UserTag` = free-form echo tag.
- Rows are returned **NEWEST-FIRST** (descending time) — reverse before folding into time-ordered stores.

## 2. Responses — `HistoryOHLCResult` / `HistoryTickResult` (dialect drift!)

Three envelope dialects exist for the SAME function — the decoder must accept ALL:

1. **Flat rows** (2022 SDK README): `{"LastTradeTime":1658138400,"QuotationLot":50,"TradedQty":89800,"OpenInterest":11179850,"Open":…,"High":…,"Low":…,"Close":…}`.
2. **Request/Result envelope** (2018 mirror, verbatim):

```json
{"Request":{"Exchange":"NSE","InstrumentIdentifier":"SBIN","From":1536019200,"To":1536477902,"Max":0,"Periodicity":"DAY","Period":0,"MessageType":"GetHistory"},
"Result":[
{"LastTradeTime":1536280200,"QuotationLot":1,"TradedQty":23716678,"OpenInterest":0,"Open":295.9,"High":295.9,"Low":289.45,"Close":291.65}],
"MessageType":"HistoryOHLCResult"}
```

3. **REST wrapper**: `{"USERTAG":<tag>,"OHLC":[…]}` / `{"USERTAG":…,"TICK":[…]}` with ALL-CAPS keys + epoch-milliseconds.

TICK row (verbatim, per-second granularity; `TradedQty` can be 0 — quote-only seconds):

```json
{"LastTradeTime":1536205500,"LastTradePrice":298.0,"QuotationLot":1,"TradedQty":0,"OpenInterest":0,"BuyPrice":298.1,"BuyQty":2980,"SellPrice":298.45,"SellQty":1835}
```

Consecutive official TICK rows are 1–2 s apart (…008, …010, …012, …013 in the gfdl-rest dump) — consistent with the tick history being the archived 1s L1 stream, NOT sub-second trades. **2026-07-14 (U-7 PARTIAL-CLOSE):** the live retention table labels tick history "Tick | Every 1 Sec" (RUNNER-DOC, …/introduction/type-of-data-available/) — officially 1-second granularity; the 2024-07 AllFunctions PDF TICK sample shows irregular spacing (…018, …020, …033 — rows only for seconds with activity). Byte-parity with the live 1s stream remains a live probe.

### Row field table (OHLC)

| Field | Type | Meaning | Timezone/precision |
|---|---|---|---|
| `LastTradeTime` (WS) / `LASTTRADETIME` (REST) | int | bar label | WS epoch-SECONDS; REST epoch-MILLISECONDS always ending `000` (second resolution). UTC-base; minute bars :00-aligned (open-stamp Assumed, U-6); DAILY bars = vendor stamp 18:00/06:00 IST — date label only (04 §3) |
| `Open/High/Low/Close` | f64 | bar OHLC (`Close` = BAR close here) | |
| `TradedQty` | int | bar volume | |
| `OpenInterest` | int | OI | |
| `QuotationLot` | f64/int | lot size | |

No VWAP/turnover per-bar fields exist.

## 3. History depth — THREE layers (never quote a single set of numbers)

| Layer | Values | Tier |
|---|---|---|
| (i) Doc-page ceiling | tick ("Every 1 Sec") **1 calendar week**; 1–4 min **3 calendar months**; 5,6,10,12 min 4.5 months; 15,20,30 min + hourly (periods 1,2,3,4,6,8,12) 6 months; Day/Week/Month **since 2010**. NFO splits (2026-07-14): continuous futures D/W/M since 2010; **futures CONTRACTWISE D/W/M = 3 calendar months; options CONTRACTWISE = 1 calendar month** (bounded expired-contract lookback) | **RUNNER-DOC (2026-07-14, …/introduction/type-of-data-available/)** — was SEARCH |
| (ii) Plan tier (ProPlus/reseller figures) | tick ~2 days; 1-min ~60 calendar days; EOD from 2010 | SEARCH (FAQ via 2026-07-03 eval) — Assumed |
| (iii) **Per-key truth: `GetLimitation`** | observed `MaxTicks` **2** (2018 real key) / **5** (ws-gfdl README) / **7** (gfdl-rest README); `MaxIntraday: 44` (≈44 trading days ≈ 60 calendar days — explains the "60d" tier); `MaxEOD: 100000`; plus per-periodicity `Minute_xEnabled`/`Hour_xEnabled` booleans | CLIENT-LIB-SOURCE + GITHUB-SAMPLE(dhelm-2018) |

**GetLimitation is the ONLY authoritative per-account answer** (11 §1). Actual purchased-plan values: **Unknown** until a key exists (U-9).

## 4. Timestamp dialects (load-bearing)

- WS `From`/`To`/`LastTradeTime`: epoch **SECONDS**, standard UTC epoch. Doc verbatim: "'1658138400' (18-07-2022 15:30:00) … expressed as no. of seconds since Epoch time (i.e. 1st January 1970)" — 1658138400 UTC = 10:00:00 UTC = 15:30:00 IST exactly (ARITH). Second worked example: 1567655100 = 09:15:00 IST. The 2026-07-14 live pages carry `1388534400 (01-01-2014 0:0:0)` / `1412121600 (10-01-2014 0:0:0)` — both midnight **UTC** instants (ARITH). **NOT the Dhan IST-shifted convention** (U-23 re-confirmed at RUNNER-DOC tier).
- REST **requests**: `from`/`to` are epoch **SECONDS** too (RUNNER-DOC 2026-07-14 — the REST GetHistory doc example URL sends `from=1716269700&to=1718861700`; the prior "epoch-ms on REST" note applied only to responses).
- REST **responses**: SDK dumps + sibling REST doc samples show epoch **MILLISECONDS** (usually trailing `000` = second resolution), but the 2026-07-14 REST GetHistory doc PROSE says "In JSON Response … no. of seconds since Epoch" — doc-internal conflict; parse both defensively, live-verify. Any 13-digit `ServerTime` seen in doc snippets is the REST dialect — never import into WS parsing.
- REST GetHistoryGreeks responds in XML with HUMAN timestamps `DD-MM-YYYY HH:MM:SS` (§6).

## 5. GetHistoryAfterMarket

```json
{"MessageType":"GetHistoryAfterMarket","Exchange":"NFO","InstrumentIdentifier":"NIFTY-I","Periodicity":"MINUTE","Period":1,"From":0,"To":0,"Max":10,"UserTag":"","isShortIdentifier":"false"}
```

- Official purpose (RUNNER-DOC 2026-07-14, verbatim on BOTH transport pages): returns history "till **previous working day**"; "To receive current day's historical data via this function, you will need to send request after market is closed"; useful for backtesting-class services, "should also save their API costs". NOT enabled together with GetHistory on one key (still Assumed — the 2026-07-14 pages say nothing either way).
- ⚠ The official SDK module opens its OWN WS connection (endpoint+apikey args) for this function — hints GDF may serve it from a separate endpoint/key class (Assumed).
- REST (U-22, updated 2026-07-14): a dedicated REST doc page EXISTS — https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/rest-api-documentation/gethistoryaftermarket-2/ — whose body defers to the REST GetHistory param table ("You can find details of the function request here" → function-gethistory). Per the universal `http://endpoint:port/<FunctionName>/` scheme the path is `GetHistoryAfterMarket/` (Assumed from scheme — the page shows no verbatim example URL). U-22 REST-path half: PARTIAL.
- Same responses (`HistoryOHLCResult`/`HistoryTickResult`).

## 6. GetHistoryGreeks (WS + REST)

WS: `{"MessageType":"GetHistoryGreeks","Exchange":"NFO","InstrumentIdentifier":"BANKNIFTY18SEP2451000CE","Periodicity":"MINUTE","Period":"1","isShortIdentifier":"true","Max":10,"From":0,"To":0}` → `HistoryGreeksResult` = Request-echo + `Result[]` rows `{Token,Timestamp,IV…DTR}` newest-first.

Doc page facts (RUNNER-DOC 2026-07-14, …/greeks-api/gethistorygreeks-2/): **Periodicity ∈ {MINUTE, TICK} only** (Period applies to MINUTE/TICK); From/To epoch-SECONDS; Max default 0 = all; field list verbatim `Token, Timestamp, IV, Delta, Theta, Vega, Gamma, IVVwap, Vanna, Charm, Speed, Zomma, Color, Volga, Veta, ThetaGammaRatio, ThetaVegaRatio, DTR`. Verbatim sample response (trimmed):

```json
{"Request":{"Exchange":"NFO","InstrumentIdentifier":"NIFTY28NOV2426050CE","IsShortIdentifier":true,"From":0,"To":0,"Max":5,"UserTag":null,"MessageType":"GetHistoryGreeks"},
"Result":[{"Token":"48093","Timestamp":1731999380,"IV":0.22,"Delta":0.01,"Theta":-0.77,"Vega":0.66,"Gamma":0.0,"IVVwap":0.22,"Vanna":0.0,"Charm":-0.0,"Speed":0.0,"Zomma":0.0,"Color":-0.0,"Volga":0.2,"Veta":-0.23,"ThetaGammaRatio":-38028.19,"ThetaVegaRatio":-1.17,"DTR":-0.01}],
"MessageType":"HistoryGreeksResult"}
```

Rows newest-first (sample Timestamps 1731999380 → 1731998888 descending); `Token` is a **string**; `Timestamp` epoch-seconds.

REST QUIRKS (verbatim from gfdl-rest@1.0.3): the SDK posts to endpoint path **`GetHistory/`** (NOT `GetHistoryGreeks/`) with only exchange+instrumentIdentifier+optional isShortIdentifier/max/from/to — and the README's sample response is **XML** `<OptionGreeksHistory>` with `<Value Token=… IV=… … Timestamp="09-02-2024 11:34:33"/>` attributes (DD-MM-YYYY human timestamps). **2026-07-14 update (U-22):** the live nav carries a dedicated REST GetHistoryGreeks doc page (`…/greeks-api-global-datafeeds-apis/gethistorygreeks-returns-historical-greeks-data/`, not fetched this run) — GDF documents a real, separate REST GetHistoryGreeks function, so the SDK's `GetHistory/` path reads as an SDK bug rather than server routing. Verify the exact REST path + response format (JSON vs the SDK-seen XML) live.

## 7. Known official-SDK bugs (do not replicate)

| Bug | Where |
|---|---|
| `nonTraded` sent as query param `max` | gfdl-rest GetExchangeSnapshot.py |
| GetHistoryGreeks posts to `GetHistory/` path, XML response | gfdl-rest GetHistoryGreeks.py |
| GetStrikePrices drops OptionType from the JSON despite accepting the arg | wsgfdl-py |
| SubscribeSnapshotGreeks maps identifier → `Product` field | wsgfdl-py |

## 8. Fitness note (tickvault cross-verify class)

~340 SIDs × 1 GetHistory 1m call/day fits trivially inside even the 1800/hr quota tier (tiers 1800/3600/7200 doc-verbatim since 2026-07-14 — RUNNER-DOC, …/faqs/faqs/); From/To are clean UTC epochs; 1m retention ceiling (3 months doc / 44 trading days observed) far exceeds a daily job. Caveat: GDF candles are vendor-built from the 1s feed (Assumed) — a cross-verify tests "our capture vs GDF capture", NOT "us vs exchange" (12 §3). Live-feed purity: GDF history may NEVER be written into `ticks`/`candles_*` (13 §6).

## What this prevents

- Quoting one history-depth number (three layers; per-key GetLimitation decides).
- Time-ordering bugs (rows newest-first) and dialect bugs (s vs ms, PascalCase vs ALL-CAPS, 3 envelope forms).
- Un-adjusted/adjusted confusion (AdjustSplits defaults TRUE on 1.3.5).
- Copying the 4 known SDK bugs into a Rust client.
