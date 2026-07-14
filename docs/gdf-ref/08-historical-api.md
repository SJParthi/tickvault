# GDF 08 — Historical API (GetHistory / GetHistoryAfterMarket / GetHistoryGreeks)

> **Source:** https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/function-gethistory-2/ · …/rest-api-documentation/function-gethistory/ · …/websockets-api-documentation/gethistoryaftermarket-3/ · …/rest-api-documentation/gethistoryaftermarket-2/ · …/greeks-api/gethistorygreeks-2/ · …/faqs/faqs/
> **Fetched:** 2026-07-13 · **Evidence tier:** CLIENT-LIB-SOURCE(wsgfdl-py@1.3.5 :: history*.py; gfdl-rest@1.0.3 :: GetHistory*.py + READMEs) + GITHUB-SAMPLE(js-2020, dhelm-2018) + SEARCH + ARITH. Verbatim JSON from official artifacts.

---

## 1. GetHistory — WS request (three wrapper forms, verbatim)

```json
{"MessageType":"GetHistory","Exchange":"NFO","InstrumentIdentifier":"NIFTY-I","Periodicity":"MINUTE","Period":1,"From":1658115000,"To":1658138400,"Max":10,"UserTag":"x","isShortIdentifier":"false"}
```

| Form | Fields | Notes |
|---|---|---|
| by period | From + To (epoch SECONDS) | `From:0` → "entire possible history" (verbatim); omit `To` or send a future time → till latest |
| by count | `Max:<N>` (no From/To) | SDK defaults: Max=10, Periodicity `MINUTE`, Period 1; `Max:0` = no cap |
| adjusted (1.3.5 NEW) | adds `"AdjustSplits":<bool>` between To and Max | Parameter-table default **"true"** — split-adjusted rows by default |

- `Periodicity` (verbatim): `Tick, Minute, Hour, Day, Week, Month`. `Period` (verbatim): `1,2,3,4,5,10,12,15,20,30`.
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

Consecutive official TICK rows are 1–2 s apart (…008, …010, …012, …013 in the gfdl-rest dump) — consistent with the tick history being the archived 1s L1 stream, NOT sub-second trades. Whether tick history == exactly the live 1s stream: **Unknown** (U-7).

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
| (i) Doc-page ceiling | tick **1 calendar week (~7d)**; 1–4 min **3 months**; 5,6,10,12 min 4.5 months; 15,20,30 min + hourly 6 months; Day/Week/Month **since Jan 2010** | SEARCH (GetHistory + type-of-data pages) |
| (ii) Plan tier (ProPlus/reseller figures) | tick ~2 days; 1-min ~60 calendar days; EOD from 2010 | SEARCH (FAQ via 2026-07-03 eval) — Assumed |
| (iii) **Per-key truth: `GetLimitation`** | observed `MaxTicks` **2** (2018 real key) / **5** (ws-gfdl README) / **7** (gfdl-rest README); `MaxIntraday: 44` (≈44 trading days ≈ 60 calendar days — explains the "60d" tier); `MaxEOD: 100000`; plus per-periodicity `Minute_xEnabled`/`Hour_xEnabled` booleans | CLIENT-LIB-SOURCE + GITHUB-SAMPLE(dhelm-2018) |

**GetLimitation is the ONLY authoritative per-account answer** (11 §1). Actual purchased-plan values: **Unknown** until a key exists (U-9).

## 4. Timestamp dialects (load-bearing)

- WS `From`/`To`/`LastTradeTime`: epoch **SECONDS**, standard UTC epoch. Doc verbatim: "'1658138400' (18-07-2022 15:30:00) … expressed as no. of seconds since Epoch time (i.e. 1st January 1970)" — 1658138400 UTC = 10:00:00 UTC = 15:30:00 IST exactly (ARITH). Second worked example: 1567655100 = 09:15:00 IST. **NOT the Dhan IST-shifted convention.**
- REST: epoch **MILLISECONDS** with trailing `000`. Any 13-digit `ServerTime` seen in doc snippets is the REST dialect — never import into WS parsing.
- REST GetHistoryGreeks responds in XML with HUMAN timestamps `DD-MM-YYYY HH:MM:SS` (§6).

## 5. GetHistoryAfterMarket

```json
{"MessageType":"GetHistoryAfterMarket","Exchange":"NFO","InstrumentIdentifier":"NIFTY-I","Periodicity":"MINUTE","Period":1,"From":0,"To":0,"Max":10,"UserTag":"","isShortIdentifier":"false"}
```

- Official purpose: history "till previous working day"; current day's data only after market close; cheaper API cost for back-testing. NOT enabled together with GetHistory on one key (Assumed — SEARCH).
- ⚠ The official SDK module opens its OWN WS connection (endpoint+apikey args) for this function — hints GDF may serve it from a separate endpoint/key class (Assumed).
- REST: README documents the function ("requests same as History") but gfdl-rest has NO GetHistoryAfterMarket class — the REST path is **Unknown** (presumably `GetHistoryAfterMarket/`; U-22).
- Same responses (`HistoryOHLCResult`/`HistoryTickResult`).

## 6. GetHistoryGreeks (WS + REST)

WS: `{"MessageType":"GetHistoryGreeks","Exchange":"NFO","InstrumentIdentifier":"BANKNIFTY18SEP2451000CE","Periodicity":"MINUTE","Period":"1","isShortIdentifier":"true","Max":10,"From":0,"To":0}` → `HistoryGreeksResult` = Request-echo + `Result[]` rows `{Token,Timestamp,IV…DTR}` newest-first.

REST QUIRKS (verbatim from gfdl-rest@1.0.3): the SDK posts to endpoint path **`GetHistory/`** (NOT `GetHistoryGreeks/`) with only exchange+instrumentIdentifier+optional isShortIdentifier/max/from/to — and the README's sample response is **XML** `<OptionGreeksHistory>` with `<Value Token=… IV=… … Timestamp="09-02-2024 11:34:33"/>` attributes (DD-MM-YYYY human timestamps). Whether that path is a bug or server-side routing: verify live.

## 7. Known official-SDK bugs (do not replicate)

| Bug | Where |
|---|---|
| `nonTraded` sent as query param `max` | gfdl-rest GetExchangeSnapshot.py |
| GetHistoryGreeks posts to `GetHistory/` path, XML response | gfdl-rest GetHistoryGreeks.py |
| GetStrikePrices drops OptionType from the JSON despite accepting the arg | wsgfdl-py |
| SubscribeSnapshotGreeks maps identifier → `Product` field | wsgfdl-py |

## 8. Fitness note (tickvault cross-verify class)

~340 SIDs × 1 GetHistory 1m call/day fits trivially inside even the 1800/hr quota tier; From/To are clean UTC epochs; 1m retention ceiling (3 months doc / 44 trading days observed) far exceeds a daily job. Caveat: GDF candles are vendor-built from the 1s feed (Assumed) — a cross-verify tests "our capture vs GDF capture", NOT "us vs exchange" (12 §3). Live-feed purity: GDF history may NEVER be written into `ticks`/`candles_*` (13 §6).

## What this prevents

- Quoting one history-depth number (three layers; per-key GetLimitation decides).
- Time-ordering bugs (rows newest-first) and dialect bugs (s vs ms, PascalCase vs ALL-CAPS, 3 envelope forms).
- Un-adjusted/adjusted confusion (AdjustSplits defaults TRUE on 1.3.5).
- Copying the 4 known SDK bugs into a Rust client.
