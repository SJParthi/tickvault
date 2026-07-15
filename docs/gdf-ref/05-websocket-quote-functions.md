# GDF 05 — Quote Pull Functions (GetLastQuote family, arrays, movers)

> **Source:** https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/function-getlastquote-2/ · …/rest-api-documentation/function-getlastquotearray/ · REST twins per 09
> **Fetched:** 2026-07-13 (pack) + 2026-07-14 (machine crawl) · **Evidence tier:** **RUNNER-DOC (2026-07-14, URLs above)** for everything cited in the reconciliation section below; CLIENT-LIB-SOURCE(wsgfdl-py@1.3.5 :: lastquote*.py, topgainerslosers.py, volumeshockers.py + README) + GITHUB-SAMPLE(js-2020, dhelm-2018) for SDK-only claims. Verbatim JSON from official artifacts.

All functions here are PULL — "send the request every time latest data is needed" (verbatim GFDL comment). They share the tick schema of 03 §4 (`Close` = prev-day close throughout this family).

---

## 2026-07-14 RUNNER-DOC reconciliation (machine-fetched verbatim corpus, docs-fetch/gdf-2026-07-14 @ 7d354a8d)

| Claim | Verdict | Evidence |
|---|---|---|
| `Close` = previous day's close in this family | **CONFIRMED, doc-glossed verbatim**: "**Close** (previous Day's Close)" in the "What is returned?" list of both GetLastQuote (WS) and GetLastQuoteArray (REST) | RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/function-getlastquote-2/ + …/rest-api-documentation/function-getlastquotearray/) |
| LastQuoteResult field set | **NEW fields (current era)** — the live WS GetLastQuote page documents AND returns `PriceChange`, `PriceChangePercentage`, `OpenInterestChange`, `TotalBuyQty`, `TotalSellQty` on top of the 2018-era set; `TotalBuyQty`/`TotalSellQty` arrive as FLOATS (`0.0`) in the doc sample. All five must be `Option`-al (2018-era rows lack them) | WS GetLastQuote page (URL above) |
| `isShortIdentifier` (singular) on single-quote; `isShortIdentifiers` (plural) on array | **CONFIRMED** — doc param tables use exactly those casings | both pages |
| `LastTradeTime`/`ServerTime` epoch SECONDS on WS | **CONFIRMED** (doc-verbatim epoch-seconds gloss) | WS GetLastQuote page |
| REST times epoch-ms | **CONFIRMED with a doc-internal contradiction** — the REST GetLastQuoteArray doc TEXT says "expressed as no. of seconds" but its own JSON sample shows `"LASTTRADETIME": 1415114727418` and the CSV row `1713851904000` (both 13-digit ms). Observed wire = ms (matches 09) | REST GetLastQuoteArray page |
| REST array response envelope | **NEW fact** — REST returns a **BARE ALL-CAPS JSON array** (`[ {…}, {…} ]`, NO `Result`/`MessageType` envelope); XML root `RealtimeArray`; CSV via `format=CSV`; `xml=[true]/[false]` param | REST GetLastQuoteArray page |
| 25-symbol array cap; `+`-joined REST identifiers; URL-encoding for specials | **CONFIRMED** — "max. limit is 25 instruments per single request"; example `instrumentIdentifiers=A+B+C`; special-character page linked | REST GetLastQuoteArray page |
| GetVolumeShockers "WS only" | **DRIFT (qualified)** — the site nav carries a REST page (`…/volume-shockers-rest-api-documentation/getvolumeshockers/`) alongside the WS one (`…/volume-shockers/getvolumeshockers-2/`); neither page body was fetched in this crawl, so the wire shapes stay SDK-tier, but "WS only" is wrong at the surface level | sidebar of every fetched page |
| GainersLosers / VolumeShockers page slugs | **NEW (slugs only)** — WS: `…/gainers-losers/subscribetopgainerslosers/`, `…/gainers-losers/gettopgainerslosers/`, `…/volume-shockers/getvolumeshockers-2/`; REST: `…/gainers-losers-rest-api-documentation/gettopgainerslosers-2/`, `…/volume-shockers-rest-api-documentation/getvolumeshockers/`. Page bodies NOT captured (not in the fetch list) — §3/§4 content remains CLIENT-LIB-SOURCE tier | sidebars (2026-07-14) |
| GetLastQuote variant page slugs | **NEW (slugs only)** — WS: `getlastquoteshort`, `getlastquoteshortwithclose`, `function-getlastquotearray-2`, `getlastquotearrayshort`, `getlastquotearrayshortwithclose`; REST: `function-getlastquote`, `function-getlastquoteshort`, `function-getlastquoteshortwithclose`, `function-getlastquotearrayshort`, `function-getlastquotearrayshortwithclose` (all under the obvious parents; bodies not fetched) | sidebars (2026-07-14) |
| `GetLastQuoteOptionChainWithGreeks` (U-20) | **STILL OPEN** — zero mentions across the 215-page corpus (grep 2026-07-14); remains an SDK-observed name with UNKNOWN wire shape | corpus-wide grep |

## 1. GetLastQuote / GetLastQuoteShort / GetLastQuoteShortWithClose (WS + REST)

| Variant | Fields | Result MessageType |
|---|---|---|
| GetLastQuote | full tick schema (03 §4) + current-era extras `PriceChange`, `PriceChangePercentage`, `OpenInterestChange`, `TotalBuyQty`, `TotalSellQty` (RUNNER-DOC 2026-07-14 — see doc field list + sample below; all `Option`-al for 2018-era rows) | `LastQuoteResult` |
| GetLastQuoteShort | `Exchange, InstrumentIdentifier, LastTradeTime, BuyPrice, LastTradePrice, SellPrice` | `LastQuoteShortResult` |
| GetLastQuoteShortWithClose | Short + `Close` ("Close of Previous Day" — verbatim) | `LastQuoteShortWithCloseResult` |

Doc "What is returned?" gloss list (RUNNER-DOC 2026-07-14, verbatim glosses): `AverageTradedPrice` (VWAP), **`Close` (previous Day's Close)**, `BuyPrice` (Bid), `BuyQty` (Bid Size), `SellPrice` (Ask), `SellQty` (Sell Size), `QuotationLot` (Lot Size), `Value` (Turnover), `PreOpen` (if PreOpen), `PriceChange` / `PriceChangePercentage` (change vs previous trading day's Close), `OpenInterestChange` (Change in Open Interest compared to previous trading day's Close), `TotalBuyQty`, `TotalSellQty`. `LastTradeTime`/`ServerTime` = epoch SECONDS (WS).

Requests (verbatim; `isShortIdentifier:"true"` mandatory with contractwise symbols):

```json
{"MessageType":"GetLastQuote","Exchange":"NFO","isShortIdentifier":"false","InstrumentIdentifier":"NIFTY-I"}
{"MessageType":"GetLastQuoteShort","Exchange":"NFO","InstrumentIdentifier":"NIFTY-I"}
{"MessageType":"GetLastQuoteShortWithClose","Exchange":"NFO","InstrumentIdentifier":"NIFTY20JULFUT","isShortIdentifier":"true"}
```

Response (verbatim, dhelm-2018 — `LastQuoteResult`, pre-2022 field set):

```json
{"Exchange":"NSE","InstrumentIdentifier":"SBIN","LastTradeTime":1508320899,"ServerTime":1508320899,"AverageTradedPrice":243.81,"BuyPrice":243.2,"BuyQty":36000,"Close":244.75,"High":244.6,"Low":243.0,"LastTradePrice":243.2,"LastTradeQty":0,"Open":244.35,"OpenInterest":50052000,"SellPrice":243.3,"SellQty":24000,"TotalQtyTraded":12864000,"Value":563571840.0,"PreOpen":false,"MessageType":"LastQuoteResult"}
```

Doc-verbatim CURRENT-era response (RUNNER-DOC 2026-07-14, WS GetLastQuote page — note the five trailing extras and that `TotalBuyQty`/`TotalSellQty` are FLOATS):

```json
{"Exchange":"NSE","InstrumentIdentifier":"SBIN","LastTradeTime":1777890555,"ServerTime":1777890556,"AverageTradedPrice":1076.63,"BuyPrice":1068.4,"BuyQty":4158,"Close":1068.4,"High":1089.1,"Low":1063.6,"LastTradePrice":1068.4,"LastTradeQty":0,"Open":1063.6,"OpenInterest":0,"QuotationLot":1.0,"SellPrice":0.0,"SellQty":0,"TotalQtyTraded":11693211,"Value":12589261758.93,"PreOpen":false,"PriceChange":-0.05,"PriceChangePercentage":-0.0,"OpenInterestChange":0,"TotalBuyQty":0.0,"TotalSellQty":0.0,"MessageType":"LastQuoteResult"}
```

REST: `GetLastQuote/?accessKey=..&exchange=..&instrumentIdentifier=..[&isShortIdentifier=true]` (paths `GetLastQuoteShort/`, `GetLastQuoteShortWithClose/`); ALL-CAPS response keys, epoch-ms times (09). Param-casing quirk: single-quote REST sends `isShortIdentifier`, the Short/WithClose+array variants send `isShortIdentifiers` (verbatim from SDK; the WS doc param table confirms the same singular/plural split — RUNNER-DOC 2026-07-14).

## 2. GetLastQuoteArray / …Short / …ShortWithClose (WS + REST) — max 25 symbols

Requests (verbatim; symbol list = objects with `Value`):

```json
{"MessageType":"GetLastQuoteArray","Exchange":"NFO","isShortIdentifiers":"false","InstrumentIdentifiers":[{"Value":"NIFTY-I"},{"Value":"BANKNIFTY-I"}]}
{"MessageType":"GetLastQuoteArrayShort","Exchange":"NFO","InstrumentIdentifiers":[{"Value":"NIFTY20JULFUT"},{"Value":"BANKNIFTY20JULFUT"}],"isShortIdentifiers":"true"}
{"MessageType":"GetLastQuoteArrayShortWithClose","Exchange":"NFO","InstrumentIdentifiers":[{"Value":"NIFTY20JULFUT"},{"Value":"BANKNIFTY20JULFUT"}],"isShortIdentifiers":"true"}
```

Response (verbatim, dhelm-2018 — inner rows are full `LastQuoteResult` objects):

```json
{"Result":[
{"Exchange":"NSE","InstrumentIdentifier":"SBIN","LastTradeTime":1536316145,"ServerTime":1536316145,"AverageTradedPrice":291.74,"BuyPrice":0.0,"BuyQty":0,"Close":291.65,"High":295.9,"Low":289.45,"LastTradePrice":291.65,"LastTradeQty":0,"Open":295.9,"OpenInterest":0,"QuotationLot":1.0,"SellPrice":291.65,"SellQty":30531,"TotalQtyTraded":23716678,"Value":6919103639.72,"PreOpen":false,"MessageType":"LastQuoteResult"},
{"Exchange":"NSE","InstrumentIdentifier":"INFY","LastTradeTime":1536316142,"ServerTime":1536316142,"AverageTradedPrice":730.95,"BuyPrice":732.8,"BuyQty":3128,"Close":732.8,"High":735.15,"Low":723.8,"LastTradePrice":732.8,"LastTradeQty":0,"Open":734.35,"OpenInterest":0,"QuotationLot":1.0,"SellPrice":0.0,"SellQty":0,"TotalQtyTraded":6510605,"Value":4758926724.75,"PreOpen":false,"MessageType":"LastQuoteResult"}],
"MessageType":"LastQuoteArrayResult"}
```

Variant result types: `LastQuoteArrayShortResult`, `LastQuoteArrayShortWithCloseResult`. More than 25 symbols → multiple requests. REST joins symbols with `+`: `instrumentIdentifiers=NIFTY-I+BANKNIFTY-I+FINNIFTY-I` (URL-encode specials — 09 §4).

### 2a. REST GetLastQuoteArray specifics (RUNNER-DOC 2026-07-14, …/rest-api-documentation/function-getlastquotearray/)

- Params: `accessKey` (required), `exchange`, `instrumentIdentifiers` (max 25, `+`-joined, URL-encode specials like `BAJAJ-AUTO`/`M&M`/`NIFTY 50`), `xml=[true]/[false]` default false, `format=CSV` (never combined with `xml`), `isShortIdentifiers=[true]/[false]` default false.
- **JSON response = BARE ALL-CAPS array, NO envelope** (contrast the WS `{"Result":[…],"MessageType":"LastQuoteArrayResult"}` shape — a REST decoder must not expect `Result`): `[{ "AVERAGETRADEDPRICE": 123.45, …, "LASTTRADETIME": 1415114727418, …, "SERVERTIME": 1415114727418, … }, …]`.
- **Times: the doc TEXT says "no. of seconds since Epoch" but its own JSON sample (`1415114727418`) and CSV row (`1713851904000`) are 13-digit epoch-MILLISECONDS** — doc-internal contradiction; observed wire = ms (consistent with 09's REST convention). XML output (`<RealtimeArray><Value …/></RealtimeArray>`) carries human-readable datetimes (`"11-4-2014 5:20:31 PM"`).
- The CSV header includes the current-era extras: `…,PriceChange,PriceChangePercentage,OpenInterestChange` (sample NIFTY-I row: `23.8,0.11,341450`); the REST "What is returned?" list carries those three but NOT `TotalBuyQty`/`TotalSellQty` (which the WS GetLastQuote page does list) — treat the pair as WS-observed, REST-presence Unknown.

Quirk to expect in decoders: `GetLastQuoteOptionChain` responses ALSO arrive as `LastQuoteArrayResult` (07 §1) — the array result type is shared across functions.

## 3. GetTopGainersLosers (WS + REST) / SubscribeTopGainersLosers (WS push)

```json
{"MessageType":"GetTopGainersLosers","Exchange":"NFO","Count":"5"}
{"MessageType":"SubscribeTopGainersLosers","Exchange":"NFO","Count":"5","Unsubscribe":"false"}
```

- Optional `Series` filter. `Count` sent as a string by the SDK.
- Get → rows of `LastQuoteResult` (SDK filters `LastQuoteArrayResult`). Subscribe → `MessageType:"RealtimeGainersLosersResult"` = Request-echo (`{"Exchange":"NFO","Count":5,"Unsubscribe":false,…}`) + `Result[]` of LastQuoteResult rows, gainers first then losers (Count=5 → 10 rows).
- REST: `GetTopGainersLosers/?exchange&count`.
- Official pages exist but were NOT in the 2026-07-14 crawl (slugs from live sidebars: WS `…/gainers-losers/subscribetopgainerslosers/` + `…/gainers-losers/gettopgainerslosers/`; REST `…/gainers-losers-rest-api-documentation/gettopgainerslosers-2/`) — this section stays CLIENT-LIB-SOURCE tier.

## 4. GetVolumeShockers (new in wsgfdl-py 1.3.5 — "WS only" corrected 2026-07-14: a REST page ALSO exists)

**2026-07-14 correction:** the live site nav lists BOTH a WS page (`…/volume-shockers/getvolumeshockers-2/`) AND a REST page (`…/volume-shockers-rest-api-documentation/getvolumeshockers/`) — so the earlier "WS only" claim (derived from the SDK surface) is wrong at the API-surface level. Neither page body was captured in the 2026-07-14 crawl; the wire shapes below remain CLIENT-LIB-SOURCE tier (WS-observed).

```json
{"MessageType":"GetVolumeShockers","Exchange":"NSE","Count":"5"}
```

Response `MessageType:"LastRealtimeVolumeShockersResult"` — Request-echo (`{"Exchange":"NSE","Count":5,"Series":null,"Delay":0,…}`) + `Result[]` rows:

| Field | Meaning |
|---|---|
| `InstrumentIdentifier`, `Description` | symbol + name |
| `LastTradeTime`, `LastTradePrice` | as usual |
| `TTQ` / `TTQ1Week` / `TTQChangeTimes` | today's total traded qty vs 5-day SMA baseline (definition: TTQ > SMA(TTQ, 5-day)) |
| `PriceChange`, `PriceChangePercentage` | vs prev close |

## 5. Envelope patterns (decoder note)

Three response envelope shapes recur across the whole WS API:
1. **Bare object** — single-quote/tick pushes (`RealtimeResult`, `LastQuoteResult`, …).
2. **`{"Result":[…],"MessageType":…}`** — arrays (Exchanges, Snapshot, LastQuoteArray…).
3. **`{"Request":{…echo…},"Result":[…],"MessageType":…}`** — reference + chain + history functions (2018 mirror shows GetHistory in this form too — 08 §4).

A decoder must accept all three; inner rows may carry their own `MessageType` (sometimes `null` on merged Greeks rows).

## What this prevents

- Missing the 25-symbol array cap (silent truncation risk if assumed unlimited).
- Misreading `Close` as today's close anywhere in this family (it is PREV-DAY close — doc-glossed verbatim 2026-07-14).
- A decoder keyed on "one MessageType per function" (option-chain reuses `LastQuoteArrayResult`).
- A REST decoder expecting the WS `{"Result":[…]}` envelope (REST arrays are BARE ALL-CAPS arrays — RUNNER-DOC 2026-07-14).
- Trusting the REST doc's "epoch seconds" TEXT over its own ms samples (13-digit values; a seconds-parser would land in year ~46813).
- Parsing `TotalBuyQty`/`TotalSellQty` as integers (doc sample shows floats) or requiring the five current-era extras on 2018-era rows.
