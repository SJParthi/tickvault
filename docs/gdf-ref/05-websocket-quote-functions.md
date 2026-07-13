# GDF 05 — Quote Pull Functions (GetLastQuote family, arrays, movers)

> **Source:** https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/function-getlastquote-2/ · …/rest-api-documentation/function-getlastquotearray/ · REST twins per 09
> **Fetched:** 2026-07-13 · **Evidence tier:** CLIENT-LIB-SOURCE(wsgfdl-py@1.3.5 :: lastquote*.py, topgainerslosers.py, volumeshockers.py + README) + GITHUB-SAMPLE(js-2020, dhelm-2018) + SEARCH. Verbatim JSON from official artifacts.

All functions here are PULL — "send the request every time latest data is needed" (verbatim GFDL comment). They share the tick schema of 03 §4 (`Close` = prev-day close throughout this family).

---

## 1. GetLastQuote / GetLastQuoteShort / GetLastQuoteShortWithClose (WS + REST)

| Variant | Fields | Result MessageType |
|---|---|---|
| GetLastQuote | full tick schema (03 §4) | `LastQuoteResult` |
| GetLastQuoteShort | `Exchange, InstrumentIdentifier, LastTradeTime, BuyPrice, LastTradePrice, SellPrice` | `LastQuoteShortResult` |
| GetLastQuoteShortWithClose | Short + `Close` ("Close of Previous Day" — verbatim) | `LastQuoteShortWithCloseResult` |

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

REST: `GetLastQuote/?accessKey=..&exchange=..&instrumentIdentifier=..[&isShortIdentifier=true]` (paths `GetLastQuoteShort/`, `GetLastQuoteShortWithClose/`); ALL-CAPS response keys, epoch-ms times (09). Param-casing quirk: single-quote REST sends `isShortIdentifier`, the Short/WithClose+array variants send `isShortIdentifiers` (verbatim from SDK).

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

Quirk to expect in decoders: `GetLastQuoteOptionChain` responses ALSO arrive as `LastQuoteArrayResult` (07 §1) — the array result type is shared across functions.

## 3. GetTopGainersLosers (WS + REST) / SubscribeTopGainersLosers (WS push)

```json
{"MessageType":"GetTopGainersLosers","Exchange":"NFO","Count":"5"}
{"MessageType":"SubscribeTopGainersLosers","Exchange":"NFO","Count":"5","Unsubscribe":"false"}
```

- Optional `Series` filter. `Count` sent as a string by the SDK.
- Get → rows of `LastQuoteResult` (SDK filters `LastQuoteArrayResult`). Subscribe → `MessageType:"RealtimeGainersLosersResult"` = Request-echo (`{"Exchange":"NFO","Count":5,"Unsubscribe":false,…}`) + `Result[]` of LastQuoteResult rows, gainers first then losers (Count=5 → 10 rows).
- REST: `GetTopGainersLosers/?exchange&count`.

## 4. GetVolumeShockers (WS only; new in wsgfdl-py 1.3.5)

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
- Misreading `Close` as today's close anywhere in this family (it is PREV-DAY close).
- A decoder keyed on "one MessageType per function" (option-chain reuses `LastQuoteArrayResult`).
