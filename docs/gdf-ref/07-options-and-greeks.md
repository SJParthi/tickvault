# GDF 07 — Option Chain & Greeks APIs

> **Source:** https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/greeks-api/subscriberealtimegreeks/ · …/greeks-api/getlastquoteoptiongreeks/ · …/greeks-api/getlastquotearrayoptiongreeks/ · …/greeks-api/getlastquoteoptiongreekschain/ · …/greeks-api/gethistorygreeks-2/ · …/optionchain-api/subscribeoptionchain/ (also …/advanced_streaming_api/subscribeoptionchain/) · …/rest-api-documentation/getlastquoteoptionchain/
> **Fetched:** 2026-07-13 · **Evidence tier:** CLIENT-LIB-SOURCE(wsgfdl-py@1.3.5 + GFDLWS@0.0.4 :: *optionchain*.py, *greeks*.py + READMEs) + GITHUB-SAMPLE(js-2020/2021 Greeks additions) + SEARCH. Verbatim JSON from official artifacts.

Key facts up front: (a) Greeks realtime/quote functions address instruments by numeric **`Token`** (GetInstruments `TokenNumber`), NOT InstrumentIdentifier; (b) `Depth` in chain functions = **strike COUNT around ATM (1/5/10/20), NOT order-book depth**; (c) scientific-notation floats appear on the Greeks wire.

---

## 1. GetLastQuoteOptionChain (WS + REST) — pull, whole chain

```json
{"MessageType":"GetLastQuoteOptionChain","Exchange":"NFO","Product":"NIFTY"}
```

Optional (verbatim comments): `Expiry` ("23JAN2020" DDMMMYYYY — absent → all active expiries), `OptionType` (`CE`/`PE` — absent → both), `StrikePrice` (number — absent → all strikes). ⚠ the wsgfdl-py builder emits lowercase `optionType`/`strikePrice` while the JS sample uses `OptionType`/`StrikePrice` — server case tolerance Assumed (U-16). **Response arrives as `LastQuoteArrayResult`** (the SDK filters that type — a chain-specific MessageType does NOT exist for this pull). REST: `GetLastQuoteOptionChain/?exchange&product[&expiry][&optionType][&strikePrice]`.

## 2. SubscribeOptionChain / SubscribeOptionChainGreeks (WS push)

```json
{"MessageType":"SubscribeOptionChain","Exchange":"NFO","Product":"NIFTY","Expiry":"26SEP2024","StrikePrice":25100,"Depth":1,"Unsubscribe":"false","OptionType":"CE"}
{"MessageType":"SubscribeOptionChainGreeks","Exchange":"NFO","Unsubscribe":"false","Product":"NIFTY","Expiry":"26SEP2024","StrikePrice":25100,"OptionType":"CE","Depth":1}
```

- Mandatory per SDK arg-validation: `Product`, `Expiry`, `StrikePrice`, `Depth`; `OptionType` optional.
- `Depth` (verbatim Parameter table): "when Depth given as 1,5,10,20 … response of Both CE/PE after given strike price above and below" — strikes-around-the-given-strike count. (One doc snippet lists `1,5,10,15,20`.)
- `SubscribeOptionChain` response `MessageType:"RealtimeOptionChainResult"` = envelope `{"Request":{…echo…},"Result":[<LastQuoteResult per CE/PE strike>],"MessageType":"RealtimeOptionChainResult"}` (verbatim NIFTY 26SEP2024 sample: Depth:1 → 6 rows around 25100).
- `SubscribeOptionChainGreeks` response `MessageType:"RealtimeOptionChainGreeksResult"` — Request-echo + `Result[]` where each row = FULL LastQuote fields **+ all 18 Greek fields merged** (inner rows carry `MessageType:null`).
- Docs host SubscribeOptionChain under BOTH `optionchain-api/` and `advanced_streaming_api/` sections — same function.
- Push cadence for chain subscriptions ("as updates become available" vs strict 1s): not pinned by any artifact — treat as ~1s-class, verify live.

## 3. SubscribeRealtimeGreeks (WS push, Token-keyed)

```json
{"MessageType":"SubscribeRealtimeGreeks","Exchange":"NFO","Unsubscribe":"false","Token":"70222"}
```

Data push (verbatim — ⚠ the wire echoes the REQUEST name as MessageType; the SDK's own filter expects `RealtimeGreeksResult` — decoder must accept BOTH):

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

⚠ Scientific-notation floats (`3.02E-06`) on the wire — parser must accept.

## 4. Greeks quote pulls (WS + REST)

```json
{"MessageType":"GetLastQuoteOptionGreeks","Exchange":"NFO","Token":"52534"}
{"MessageType":"GetLastQuoteArrayOptionGreeks","Exchange":"NFO","Tokens":[{"Value":"39489"},{"Value":"39487"}]}
{"MessageType":"GetLastQuoteOptionGreeksChain","Exchange":"NFO","Product":"NIFTY"}
```

- Results: `LastQuoteOptionGreeksResult` / `LastQuoteArrayOptionGreeksResult` (max 25 tokens) / `OptionGreeksChainWithQuoteResult` (distinct MessageType; the server-side capability name in LimitationResult is **`GetLastQuoteOptionChainWithGreeks`** — a naming alias for the same function).
- GreeksChain optionals: `Expiry` (DDMMMYYYY), `OptionType`, `StrikePrice` — absent → all.
- REST: `GetLastQuoteOptionGreeks/?exchange&tokens[&detailedInfo]`, `GetLastQuoteArrayOptionGreeks/?exchange&tokens[&detailedInfo]` (tokens `57660+57661+…`; `detailedInfo` is a REST-only extra param).
- The exact wire shape of a `GetLastQuoteOptionChainWithGreeks`-named request (if different): **Unknown** (U-20).

## 5. Bar-cadence Greeks (new in wsgfdl-py 1.3.5)

```json
{"MessageType":"SubscribeSnapshotGreeks","Exchange":"NFO","Unsubscribe":"false","Product":"NIFTY02DEC2526000CE","Periodicity":"MINUTE","Period":"1"}
{"MessageType":"GetSnapshotGreeks","Exchange":"NFO","Periodicity":"MINUTE","Period":1,"isShortIdentifiers":"true","InstrumentIdentifiers":[{"Value":"NIFTY02DEC2526000CE"}]}
```

- ⚠ SDK QUIRK: `SubscribeSnapshotGreeks` maps the instrument-identifier argument into the **`Product`** JSON field (verbatim from the builder) — likely an SDK naming quirk; the README example passes a short identifier there.
- Responses: `RealtimeSnapshotGreeksResult` (per-bar greeks: `Exchange, InstrumentIdentifier(long form), Periodicity, Period, LastTradeTime, Token, IV…DTR`) / `SnapshotGreeksResult` (`Result[]` rows: `InstrumentIdentifier(short), Exchange, LastTradeTime, TokenNumber, IV…DTR`; max 25).

## 6. GetHistoryGreeks — see 08 §6 (WS JSON + the REST XML quirk)

## What this prevents

- Reading `Depth` as order-book depth (it is a strike count — GDF has NO book depth anywhere).
- Subscribing greeks by InstrumentIdentifier (Token-keyed; resolve via GetInstruments first).
- A decoder that drops greeks frames because MessageType echoes the request name.
- f32 parsing of greeks (scientific notation + tiny magnitudes require f64).
