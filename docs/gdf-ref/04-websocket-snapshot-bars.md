# GDF 04 — Snapshot Bars (SubscribeSnapshot / GetSnapshot / GetExchangeSnapshot)

> **Source:** https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/function-getsnapshot-2/ · the SubscribeSnapshot page (slug unresolved by search) · …/rest-api-documentation/getexchangesnapshot/ · …/delayed-api/getexchangesnapshot-delayed/
> **Fetched:** 2026-07-13 · **Evidence tier:** CLIENT-LIB-SOURCE(wsgfdl-py@1.3.5 :: realsnapshot.py/snapshot.py/exchangesnapshot.py + README) + GITHUB-SAMPLE(js-2020, dhelm-2018) + SEARCH. Verbatim JSON from official artifacts.

---

## 1. SubscribeSnapshot — per-symbol STREAMED bar closes

Server pushes one bar per symbol "whenever 1 minute completes" (verbatim GFDL comment) — i.e. delivery at bar completion. One request per symbol; re-subscribe after disconnect.

Request (verbatim — note the wsgfdl-py 1.3.5 builder uses lowercase `unsubscribe`, unlike `Unsubscribe` elsewhere; older ws-gfdl omitted it — case tolerance Assumed, U-16):

```json
{"MessageType":"SubscribeSnapshot","Exchange":"NFO","Periodicity":"Minute","Period":1,"InstrumentIdentifier":"NIFTY-I","unsubscribe":"false"}
```

Parameters:
- `Periodicity`: JS-sample GFDL comment says `Minute, Hour` only; the live GetSnapshot/SubscribeSnapshot doc snippet ALSO lists **Day (end-of-day)** — "Currently supported periods are Minute (1,2,5,10,15,30), Hour and Day (end-of-day)" (SEARCH, adversarially re-verified 2026-07-13). Treat Day as supported.
- `Period`: `1,2,5,10,15,30` — for Minute periodicity ONLY (Hour/Day imply period 1).
- No backfill/catch-up flag exists on SubscribeSnapshot (verified absence in every SDK signature) — gap-fill is via GetSnapshot (latest bar) or GetHistory.

Response push (verbatim, `MessageType:"RealtimeSnapshotResult"`):

```json
{"Exchange":"NFO","InstrumentIdentifier":"NIFTY-I","Periodicity":"MINUTE","Period":1,"LastTradeTime":1669265340,"TradedQty":11300,"OpenInterest":5608700,"Open":18351.0,"High":18353.95,"Low":18350.25,"Close":18353.95,"MessageType":"RealtimeSnapshotResult"}
```

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

"If no trades happen during that period, no data is sent by the server" (SEARCH, doc snippet — adversarially confirmed). **Consumers must handle missing bars** — a gap in the stream is NOT a feed failure for an illiquid symbol; conversely a dense index will normally emit every minute.

## 2. Bar-stamp convention — open-stamp **ASSUMED**, live-probe required (U-6)

- Every captured minute-bar `LastTradeTime` is minute-aligned (`% 60 == 0`) — Verified.
- FOR open-stamp: the live doc snippet states "The timestamp of the returned data will be start timestamp of the period. So when data between 9:15:00 to 9:20:00 is returned, it will have timestamp of 9:15:00." (SEARCH tier — not byte-verbatim); plus a 1m history row labeled 1658138400 (= exactly 15:30:00 IST) contains the 15:30:00 closing print with O=H=L=C=LTP — bucket-start labeling.
- AGAINST treating it as settled: an earlier "decisive" arithmetic example was WRONG (1669265340 decodes to 10:19:00 IST, not the claimed 11:39), so the derivation chain has already produced one refuted claim. **Status: Assumed (open-stamp) — live probe = compare one live SubscribeSnapshot push's wall-clock arrival vs its label** (U-6).

## 3. Daily-bar stamp anomaly — 18:00 IST (date label ONLY)

The ENTIRE 10-row daily series in the official gfdl-rest dump is stamped **18:00:00 IST** (`LASTTRADETIME: 1724243400000` = 2024-08-21 18:00:00 IST, ARITH); the 2018 dhelm daily row (1536280200) decodes to **06:00:00 IST** — a second era-variant. Verdict (ARITH-verified, supersedes any earlier "daily stamped 09:15 session open" claim — that was arithmetic-refuted): **daily `LASTTRADETIME` is a vendor EOD-process stamp, era-dependent. Use it as a DATE label only, never a time key.**

## 4. GetSnapshot — pull, latest completed bar, ≤25 symbols

Request (verbatim; GFDL comments: Exchange `NFO, NSE, NSE_IDX, CDS, MCX` mandatory; Periodicity `Minute, Hour`; Period `1,2,3,5,10,15,20,30` — NOTE this pull function's Period list differs from SubscribeSnapshot's; `isShortIdentifiers:"true"` for contractwise symbols):

```json
{"MessageType":"GetSnapshot","Exchange":"NFO","Periodicity":"MINUTE","Period":1,"isShortIdentifiers":"false","InstrumentIdentifiers":[{"Value":"NIFTY-I"},{"Value":"BANKNIFTY-I"}]}
```

Response (verbatim, `MessageType:"SnapshotResult"`; the 2022 README row also carries `TokenNumber`):

```json
{"Result":[
{"InstrumentIdentifier":"BEPL","Exchange":"NSE","LastTradeTime":1536309000,"TradedQty":46846,"OpenInterest":0,"Open":125.65,"High":126.9,"Low":125.5,"Close":126.4},
{"InstrumentIdentifier":"SBIN","Exchange":"NSE","LastTradeTime":1536309000,"TradedQty":5417972,"OpenInterest":0,"Open":291.1,"High":292.8,"Low":289.5,"Close":292.4}],
"MessageType":"SnapshotResult"}
```

2022 single-row variant: `{"InstrumentIdentifier":"FUTIDX_NIFTY_28JUL2022_XX_0","Exchange":"NFO","LastTradeTime":1658204580,"TradedQty":25550,"OpenInterest":11313250,"Open":16288.45,"High":16291.75,"Low":16286.5,"Close":16287.4,"TokenNumber":null}`. REST GetSnapshot echoes back the LONG identifier even when short was requested.

## 5. GetExchangeSnapshot — pull, ENTIRE exchange, max 5 bars/call

"Returns entire Exchange Snapshot in realtime. This function can return maximum 5 snapshots in single call. You will need to use 'From' and 'To' parameters to control the dataset required." (verbatim GFDL comment)

WS request — JS-sample form (verbatim) and SDK form (note the SDK builder uses lowercase `periodicity`/`period` — casing drift, U-16):

```json
{"MessageType":"GetExchangeSnapshot","Exchange":"NFO","Periodicity":"Minute","Period":1}
{"MessageType":"GetExchangeSnapshot","Exchange":"NFO","periodicity":"MINUTE","period":"1","instrumentType":"FUTIDX","From":1567655100,"To":1567655400,"nonTraded":"false"}
```

- `Periodicity` mandatory: `Minute, Hour` (default Minute). `Period` mandatory: `1,2,5,10,15,30` (default 1).
- `InstrumentType` optional (FUTIDX, FUTSTK, OPTIDX, OPTSTK, FUTCOM, FUTCUR, …) — absent → all types.
- `From`/`To` optional, epoch SECONDS (doc example 1567655100 = 2019-09-05 09:15:00 IST — a second independent true-UTC-epoch proof).
- `nonTraded` optional, default `"false"`; `"true"` includes non-traded instruments. ⚠ gfdl-rest 1.0.3 BUG: sends the nonTraded value as query param `max` (09 §5).
- Response `MessageType:"ExchangeSnapshotResult"`. This is the whole-exchange mechanism that exists TODAY without the Streaming API (10). `GetLimitation.AllowedFunctions` also names a server-side **GetExchangeSnapshotAfterMarket** — wire shape **Unknown** (U-20).
- Delayed twin: `GetExchangeSnapshot (Delayed)` — same fields, backend-configured per-exchange delay (01 §3).

## 6. tickvault relevance (bounded note)

`SubscribeSnapshot(Minute,1)` is a native official-1-minute-candle stream (relevant to the `spot_1m_rest` use-case class) and `GetHistory` 1m is its backfill — BUT the bars are vendor-built from GDF's own 1s feed (Assumed — 12 §3), so exact-match parity vs exchange-official or tick-built candles must NOT be expected on High/Low.

## What this prevents

- Treating a missing bar as a dead feed (no-trade bars are silently skipped).
- Keying storage on daily-bar timestamps (18:00/06:00 vendor stamps would corrupt time keys).
- Shipping an open-stamp assumption as fact (label bars only after the U-6 live probe).
- Paging through GetExchangeSnapshot without From/To (5-snapshot cap per call).
