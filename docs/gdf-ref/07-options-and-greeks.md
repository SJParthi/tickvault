# GDF 07 — Option Chain & Greeks APIs

> **Source:** https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/greeks-api/subscriberealtimegreeks/ · …/greeks-api/getlastquoteoptiongreeks/ · …/greeks-api/getlastquotearrayoptiongreeks/ · …/greeks-api/getlastquoteoptiongreekschain/ · …/greeks-api/gethistorygreeks-2/ · …/greeks-api/subscribesnapshotgreeks/ · …/optionchain-api/subscribeoptionchain/ (also …/advanced_streaming_api/subscribeoptionchain/ — byte-identical alias) · …/rest-api-documentation/getlastquoteoptionchain/
> **Fetched:** 2026-07-13 · **Evidence tier:** CLIENT-LIB-SOURCE(wsgfdl-py@1.3.5 + GFDLWS@0.0.4 :: *optionchain*.py, *greeks*.py + READMEs) + GITHUB-SAMPLE(js-2020/2021 Greeks additions) + SEARCH. Verbatim JSON from official artifacts.
> **2026-07-14 upgrade:** the greeks + option-chain pages in the Source line were machine-fetched verbatim (GitHub runner, docs-fetch/gdf-2026-07-14 @ 7d354a8d) — claims marked **RUNNER-DOC (2026-07-14, \<url\>)** are official-docs tier.

Key facts up front: (a) Greeks realtime/quote functions address instruments by numeric **`Token`** (GetInstruments `TokenNumber`), NOT InstrumentIdentifier — snapshot/history greeks address by **`InstrumentIdentifier`**; (b) `Depth` in chain functions = **strike COUNT around the given strike (1/5/10/15/20 — DOC 2026-07-14), NOT order-book depth**; (c) scientific-notation floats appear on the Greeks wire; (d) SubscribeRealtimeGreeks cadence is DOC-pinned: "returns market data **every second**".

---

## 2026-07-14 RUNNER-DOC reconciliation (machine-fetched official docs)

| Verdict | Claim | Evidence |
|---|---|---|
| CONFIRMED | SubscribeRealtimeGreeks: Token-keyed request; the full 18-field greeks schema (IV…DTR) listed verbatim in "What is returned"; scientific-notation floats in the sample; cadence "returns market data every second (detailed)" — the ~1s-class assumption is now DOC-pinned for the REALTIME greeks push | RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/greeks-api/subscriberealtimegreeks/) |
| DRIFT-UPDATED | The greeks push `MessageType`: the CURRENT doc sample (Token 74241, Timestamp 1777886925) stamps **`RealtimeGreeksResult`** (with a doc typo — a stray trailing `""`); the 2022-era sample (Token 70222) echoed the REQUEST name. Decoder keeps accepting BOTH | same page |
| NEW | Greeks methodology FAQ (verbatim): "We are receiving these values directly from NSE"; IV model = **Black Scholes**; "underlying, is taken as Futures or Synthetic futures (in case of expiries where future is not available). Thus Spot Price and Interest rate as a parameter gets removed" | same page (FAQ repeated on all quote-greeks pages) |
| CONFIRMED | SubscribeOptionChain: `Product`/`Expiry`/`StrikePrice`/`Depth` mandatory, `OptionType` optional (absent → both), `Unsubscribe` optional default false; `Depth` = "To received count value up and down from StrikePrice" (verbatim) with values **1, 5, 10, 15, 20** — resolves the earlier 1/5/10/20-vs-1/5/10/15/20 ambiguity in favor of the 5-value list | RUNNER-DOC (2026-07-14, …/optionchain-api/subscribeoptionchain/) |
| CONFIRMED | The `advanced_streaming_api/subscribeoptionchain/` page is a byte-identical ALIAS of the `optionchain-api/` page (content md5 match, 2026-07-14 corpus) | both URLs, same body |
| CONFIRMED | GetLastQuoteOptionChain (REST): `product` mandatory; `expiry`/`optionType`/`strikePrice` optional (absent → all); `xml` + **`format=CSV`** output options (never pass `xml` with `format=CSV`); returned-field list additionally names **`TotalBuyQty`,`TotalSellQty`** | RUNNER-DOC (2026-07-14, …/rest-api-documentation/getlastquoteoptionchain/) |
| CONFIRMED | GetLastQuoteOptionGreeks → `LastQuoteOptionGreeksResult`; GetLastQuoteArrayOptionGreeks → outer `LastQuoteArrayOptionGreeksResult` with inner `LastQuoteOptionGreeksResult` rows, **max 25** ("max 25 in single call", verbatim) | RUNNER-DOC (2026-07-14, …/greeks-api/getlastquoteoptiongreeks/ + …/getlastquotearrayoptiongreeks/) |
| DRIFT-REFUTED | "`detailedInfo` is a REST-only extra param" — the WS GetLastQuoteOptionGreek page lists `detailedInfo` (default false) as a supported WS parameter | …/greeks-api/getlastquoteoptiongreeks/ |
| CONFIRMED | GetLastQuoteOptionGreeksChain: `Product` mandatory; `Expiry`/`OptionType`/`StrikePrice` optional (absent → all active); merged row = full LastQuote fields + all 18 greeks (field list verbatim); response sample = `OptionGreeksChainWithQuoteResponse_JSON.zip` (name matches the `OptionGreeksChainWithQuoteResult` MessageType) | RUNNER-DOC (2026-07-14, …/greeks-api/getlastquoteoptiongreekschain/) |
| DRIFT-UPDATED | SubscribeSnapshotGreeks: the DOC wire param is **`InstrumentIdentifier`** (+ `isShortIdentifiers:"true"`), NOT `Product` — the wsgfdl-py builder's `Product` mapping is an SDK deviation from the documented wire shape. Response sample echoes the identifier **as subscribed (short form)** — corrects the earlier "long form" note | RUNNER-DOC (2026-07-14, …/greeks-api/subscribesnapshotgreeks/) |
| CONFIRMED | GetHistoryGreeks (WS): `InstrumentIdentifier`-keyed (+`isShortIdentifier`), `Periodicity` **MINUTE/TICK only**, `Period` default 1, `From`/`To` epoch secs, `Max` default 0 (= all), `userTag`; → `HistoryGreeksResult`, rows Token-keyed, values ~2-dp rounded | RUNNER-DOC (2026-07-14, …/greeks-api/gethistorygreeks-2/) |
| Unknown (U-20 still open) | No 2026-07-14 page documents a request named `GetLastQuoteOptionChainWithGreeks` — the name remains a LimitationResult capability alias; exact wire shape (if distinct) still Unknown | fetch-log |
| Unknown (unchanged) | SubscribeOptionChain/GreeksChain full response envelopes are ZIP downloads on the doc pages (not inline) — `RealtimeOptionChainResult` / chain-pull-as-`LastQuoteArrayResult` claims stay CLIENT-LIB tier. `GetSnapshotGreeks` + `SubscribeOptionChainGreeks` doc pages were not in the crawl | doc pages + fetch-log |

## 1. GetLastQuoteOptionChain (WS + REST) — pull, whole chain

```json
{"MessageType":"GetLastQuoteOptionChain","Exchange":"NFO","Product":"NIFTY"}
```

Optional (verbatim comments): `Expiry` ("23JAN2020" DDMMMYYYY — absent → all active expiries), `OptionType` (`CE`/`PE` — absent → both), `StrikePrice` (number — absent → all strikes). ⚠ the wsgfdl-py builder emits lowercase `optionType`/`strikePrice` while the JS sample uses `OptionType`/`StrikePrice` — server case tolerance Assumed (U-16). **Response arrives as `LastQuoteArrayResult`** (the SDK filters that type — a chain-specific MessageType does NOT exist for this pull; CLIENT-LIB tier — the DOC page's sample is a ZIP download). REST: `GetLastQuoteOptionChain/?exchange&product[&expiry][&optionType][&strikePrice][&xml][&format=CSV]`.

**RUNNER-DOC (2026-07-14, …/rest-api-documentation/getlastquoteoptionchain/):** `product` mandatory; `expiry`/`optionType`/`strikePrice` optional, absent → all active (the optional-semantics above upgraded to DOC tier); output = JSON (default) / `xml=true` / `format=CSV` ("make sure not to pass xml parameter … when format=CSV is sent" — verbatim). Returned fields (verbatim list): Exchange, InstrumentIdentifier, LastTradeTime, ServerTime, AverageTradedPrice (VWAP), Close (previous day's close), Open, High, Low, LastTradePrice, LastTradeQty, TotalQtyTraded, BuyPrice/BuyQty (bid), SellPrice/SellQty (ask), OpenInterest, QuotationLot, Value (turnover), PreOpen, PriceChange, PriceChangePercentage, OpenInterestChange, **TotalBuyQty, TotalSellQty**. ⚠ the DOC writes the expiry format as "DDMMYYYY format, like 23JAN2020" — a recurring doc typo; the examples are DDMMMYYYY.

## 2. SubscribeOptionChain / SubscribeOptionChainGreeks (WS push)

```json
{"MessageType":"SubscribeOptionChain","Exchange":"NFO","Product":"NIFTY","Expiry":"26SEP2024","StrikePrice":25100,"Depth":1,"Unsubscribe":"false","OptionType":"CE"}
{"MessageType":"SubscribeOptionChainGreeks","Exchange":"NFO","Unsubscribe":"false","Product":"NIFTY","Expiry":"26SEP2024","StrikePrice":25100,"OptionType":"CE","Depth":1}
```

- Mandatory: `Product`, `Expiry`, `StrikePrice`, `Depth`; `OptionType` optional (absent → both), `Unsubscribe` optional default false — **RUNNER-DOC (2026-07-14, …/optionchain-api/subscribeoptionchain/)** param table now confirms the SDK arg-validation exactly.
- `Depth` — RUNNER-DOC (2026-07-14) verbatim: values "1, 5, 10, 15, 20", "Mandatory parameter. To received count value up and down from 'StrikePrice'" — strikes-around-the-given-strike count. The 5-value list is the DOC-pinned one (the older 4-value snippet is superseded).
- DOC returned-field list (verbatim, LastQuote-class per row): Exchange, InstrumentIdentifier, LastTradeTime, ServerTime, AverageTradedPrice, BuyPrice, BuyQty, Open, High, Low, Close, LastTradePrice, LastTradeQty, OpenInterest (listed twice — doc typo), QuotationLot, SellPrice, SellQty, TotalQtyTraded, Value, PreOpen, PriceChange, PriceChangePercentage, OpenInterestChange, MessageType. Full response sample is a ZIP download (`SubscribeOptionChain_Response(1).zip`) — envelope claims below stay CLIENT-LIB tier.
- ⚠ recurring DOC typo: Expiry described as "DDMMYYYY format, like 23JAN2020" / sample `Expiry:"17OCT2024"` — actual format is DDMMMYYYY.
- `SubscribeOptionChain` response `MessageType:"RealtimeOptionChainResult"` = envelope `{"Request":{…echo…},"Result":[<LastQuoteResult per CE/PE strike>],"MessageType":"RealtimeOptionChainResult"}` (verbatim NIFTY 26SEP2024 sample: Depth:1 → 6 rows around 25100).
- `SubscribeOptionChainGreeks` response `MessageType:"RealtimeOptionChainGreeksResult"` — Request-echo + `Result[]` where each row = FULL LastQuote fields **+ all 18 Greek fields merged** (inner rows carry `MessageType:null`).
- Docs host SubscribeOptionChain under BOTH `optionchain-api/` and `advanced_streaming_api/` sections — same function (2026-07-14: byte-identical page bodies, md5-verified in the corpus).
- Push cadence for CHAIN subscriptions: still not doc-pinned — treat as ~1s-class, verify live. (Contrast: SubscribeRealtimeGreeks cadence IS doc-pinned at every second — §3.)

## 3. SubscribeRealtimeGreeks (WS push, Token-keyed, EVERY SECOND)

**RUNNER-DOC (2026-07-14, …/greeks-api/subscriberealtimegreeks/):** "Subscribe to Realtime Greek values of single Token, **returns market data every second** (detailed)" — the push cadence is DOC-pinned at 1/sec. Params: `Exchange` + `Token` ("Token numbers of instruments … you can find [in GetInstruments]").

```json
{"MessageType":"SubscribeRealtimeGreeks","Exchange":"NFO","Unsubscribe":"false","Token":"70222"}
```

**Methodology FAQ (RUNNER-DOC 2026-07-14, verbatim — repeated on every quote-greeks page):** "We are receiving these values directly from NSE." IV model = "Black Scholes Model"; interest rate: "Black Scholes Model, underlying, is taken as Futures or Synthetic futures( in case of expiries where future is not available). Thus Spot Price and Interest rate as a parameter gets removed".

Data push (verbatim 2022-era sample — ⚠ this older sample echoes the REQUEST name as MessageType; the CURRENT doc sample — Token 74241, Timestamp 1777886925, RUNNER-DOC 2026-07-14 — stamps `"MessageType":"RealtimeGreeksResult""` (sic, stray trailing quote = doc typo), matching the SDK's filter. Decoder must accept BOTH forms):

```json
{"Exchange":"NFO","Token":"70222","Timestamp":1660292501,"IV":0.11267127841711044,"Delta":0.2549774944782257,"Theta":-6.706413269042969,"Vega":7.45473575592041,"Gamma":0.0012275002663955092,"IVVwap":0.1129087582230568,"Vanna":2.9635884761810303,"Charm":-0.026660971343517303,"Speed":3.0227099614421604E-06,"Zomma":-0.006058624479919672,"Color":-5.450446406030096E-05,"Volga":4533.580078125,"Veta":132.6668243408203,"ThetaGammaRatio":-5463.4716796875,"ThetaVegaRatio":-0.8996178507804871,"DTR":-0.038019951432943344,"MessageType":"SubscribeRealtimeGreeks"}
```

### The 18-field Greeks schema

| Field | Type | Meaning |
|---|---|---|
| `IV` | f64 | implied volatility (fraction, e.g. 0.1127) |
| `Delta` / `Theta` / `Vega` / `Gamma` | f64 | first-order greeks |
| `IVVwap` | f64 | VWAP-based IV |
| `Vanna` / `Charm` / `Speed` / `Zomma` / `Color` / `Volga` / `Veta` | f64 | higher-order greeks |
| `ThetaGammaRatio` / `ThetaVegaRatio` / `DTR` | f64 | ratio metrics |
| `Timestamp` | integer | epoch seconds (UTC-base) |
| `Exchange`, `Token` | string | Token = GetInstruments `TokenNumber` |

⚠ Scientific-notation floats (`3.02E-06`) on the wire — parser must accept. The 18-field list (Exchange, Token, Timestamp, IV, Delta, Theta, Vega, Gamma, IVVwap, Vanna, Charm, Speed, Zomma, Color, Volga, Veta, ThetaGammaRatio, ThetaVegaRatio, DTR) is DOC-verbatim on the 2026-07-14 pages ("What is returned?") — schema CONFIRMED at RUNNER-DOC tier.

## 4. Greeks quote pulls (WS + REST)

```json
{"MessageType":"GetLastQuoteOptionGreeks","Exchange":"NFO","Token":"52534"}
{"MessageType":"GetLastQuoteArrayOptionGreeks","Exchange":"NFO","Tokens":[{"Value":"39489"},{"Value":"39487"}]}
{"MessageType":"GetLastQuoteOptionGreeksChain","Exchange":"NFO","Product":"NIFTY"}
```

- Results: `LastQuoteOptionGreeksResult` / `LastQuoteArrayOptionGreeksResult` (max 25 tokens — DOC verbatim "max 25 in single call", RUNNER-DOC 2026-07-14) / `OptionGreeksChainWithQuoteResult` (distinct MessageType; the server-side capability name in LimitationResult is **`GetLastQuoteOptionChainWithGreeks`** — a naming alias for the same function; the 2026-07-14 chain page's response zip is named `OptionGreeksChainWithQuoteResponse_JSON.zip`, matching).
- **RUNNER-DOC (2026-07-14, …/getlastquoteoptiongreeks/):** single-token result verbatim (Token 74241 → `"MessageType":"LastQuoteOptionGreeksResult"`); the WS page ALSO lists `detailedInfo` (default false) as a supported parameter — the earlier "REST-only extra param" note is REFUTED (2026-07-14). Array result (…/getlastquotearrayoptiongreeks/): inner rows each stamped `LastQuoteOptionGreeksResult`, outer envelope `LastQuoteArrayOptionGreeksResult`. ⚠ DOC bugs on that page: the sample REQUEST's MessageType reads `GetLastQuoteOptionGreeks` (copy-paste from the singular page — the function is `GetLastQuoteArrayOptionGreeks`) and the sample response JSON is missing its opening brace.
- GreeksChain: `Product` mandatory; optionals `Expiry` (DDMMMYYYY — DOC again typos "DDMMYYYY"), `OptionType`, `StrikePrice` — absent → all active (**RUNNER-DOC 2026-07-14, …/getlastquoteoptiongreekschain/**). Merged-row field list DOC-verbatim: full LastQuote fields (incl. Close = previous day's close, PreOpen, PriceChange, PriceChangePercentage, OpenInterestChange) + Timestamp + all 18 greeks.
- REST: `GetLastQuoteOptionGreeks/?exchange&tokens[&detailedInfo]`, `GetLastQuoteArrayOptionGreeks/?exchange&tokens[&detailedInfo]` (tokens `57660+57661+…`).
- The exact wire shape of a `GetLastQuoteOptionChainWithGreeks`-named request (if different): **Unknown** (U-20 — no 2026-07-14 page carries that request name).

## 5. Bar-cadence Greeks (new in wsgfdl-py 1.3.5)

```json
{"MessageType":"SubscribeSnapshotGreeks","Exchange":"NFO","Unsubscribe":"false","Product":"NIFTY02DEC2526000CE","Periodicity":"MINUTE","Period":"1"}
{"MessageType":"GetSnapshotGreeks","Exchange":"NFO","Periodicity":"MINUTE","Period":1,"isShortIdentifiers":"true","InstrumentIdentifiers":[{"Value":"NIFTY02DEC2526000CE"}]}
```

- ⚠ SDK QUIRK — now DOC-adjudicated (2026-07-14): the official page's request uses **`InstrumentIdentifier`** (+ `isShortIdentifiers:"true"`) as the wire field — `{"MessageType":"SubscribeSnapshotGreeks","Exchange":"NFO","Periodicity":"Minute","Period":1,"Unsubscribe":false,"InstrumentIdentifier":"NIFTY26MAY2624100CE","isShortIdentifiers":"true"}` (RUNNER-DOC 2026-07-14, …/greeks-api/subscribesnapshotgreeks/). The wsgfdl-py builder's mapping of the identifier into a **`Product`** JSON field is therefore an SDK DEVIATION from the documented shape — a native client should send `InstrumentIdentifier`; whether the server also tolerates `Product` is untested.
- DOC params: `Periodicity` = `MINUTE` (only listed value; sample sends `"Minute"` mixed-case), `Period` = 1, `Unsubscribe` optional default false. Long or short identifiers accepted (page comment shows both `OPTIDX_NIFTY_30SEP2025_CE_25000` and `NIFTY30SEP2525000CE`).
- Responses: `RealtimeSnapshotGreeksResult` — DOC sample (2026-07-14, verbatim) echoes the identifier **as subscribed** (`"InstrumentIdentifier":"NIFTY26MAY2624100CE"` — short form; the earlier "long form" note came from a long-form-subscribed sample — echo follows the subscription, Assumed): `{"Exchange":"NFO","InstrumentIdentifier":"NIFTY26MAY2624100CE","Periodicity":"MINUTE","Period":1,"LastTradeTime":1777887420,"Token":"72183","IV":0.179…,…,"MessageType":"RealtimeSnapshotGreeksResult"}`. ⚠ that sample carries `"Color":8.616986633569468E06` — a malformed exponent (no sign; almost certainly a doc typo for `E-06`): tolerate-and-flag class. / `SnapshotGreeksResult` (`Result[]` rows: `InstrumentIdentifier(short), Exchange, LastTradeTime, TokenNumber, IV…DTR`; max 25 — CLIENT-LIB tier; the GetSnapshotGreeks doc page was not in the 2026-07-14 crawl).

## 6. GetHistoryGreeks — see 08 §6 (WS JSON + the REST XML quirk)

**RUNNER-DOC (2026-07-14, …/greeks-api/gethistorygreeks-2/) — the WS contract, now DOC tier:** params `Exchange`, `InstrumentIdentifier` (NOT Token; `isShortIdentifier` default false enables short form), `Periodicity` ∈ {`MINUTE`, `TICK`} only, `Period` (default 1, MINUTE/TICK only), `From`/`To` (epoch secs, optional), `Max` (default 0 = all available), `userTag` (echoed back). Verbatim exchange:

```json
{"MessageType":"GetHistoryGreeks","Exchange":"NFO","InstrumentIdentifier":"NIFTY28NOV2426050CE","isShortIdentifier":"TRUE","Max":5}
```
```json
{"Request":{"Exchange":"NFO","InstrumentIdentifier":"NIFTY28NOV2426050CE","IsShortIdentifier":true,"From":0,"To":0,"Max":5,"UserTag":null,"MessageType":"GetHistoryGreeks"},
 "Result":[{"Token":"48093","Timestamp":1731999380,"IV":0.22,"Delta":0.01,"Theta":-0.77,"Vega":0.66,"Gamma":0.0,"IVVwap":0.22,"Vanna":0.0,"Charm":-0.0,"Speed":0.0,"Zomma":0.0,"Color":-0.0,"Volga":0.2,"Veta":-0.23,"ThetaGammaRatio":-38028.19,"ThetaVegaRatio":-1.17,"DTR":-0.01},…],
 "MessageType":"HistoryGreeksResult"}
```

Notes: request sent `isShortIdentifier` camelCase + string `"TRUE"`; the echo renders `IsShortIdentifier:true` Pascal + boolean (case/type-normalizing server). Result rows are **Token-keyed** (identifier in, token out) with ~2-dp-rounded values (vs full-precision realtime greeks) and negative zeros (`-0.0`) on the wire. REST-side XML quirk unchanged — see 08 §6.

## What this prevents

- Reading `Depth` as order-book depth (it is a strike count — GDF has NO book depth anywhere).
- Subscribing REALTIME/quote greeks by InstrumentIdentifier (Token-keyed; resolve via GetInstruments first) — while snapshot/history greeks are the reverse (InstrumentIdentifier-keyed; Token only in the response).
- A decoder that drops greeks frames because MessageType echoes the request name (or, per the 2026-07-14 sample, stamps `RealtimeGreeksResult` — accept both).
- f32 parsing of greeks (scientific notation + tiny magnitudes require f64) — and a strict float parser that dies on doc/wire oddities like the `8.61…E06` unsigned exponent (2026-07-14 sample).
- Sending the wsgfdl `Product` field for SubscribeSnapshotGreeks in a native client (the documented wire field is `InstrumentIdentifier` — 2026-07-14).
