# GDF 10 — Streaming "Push Type" API (StreamAllSymbols / StreamAllSnapshots)

> **Source:** https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/streaming_api/stream-all-symbols/ · …/streaming_api/ (landing) · …/advanced_streaming_api/ (hosts SubscribeOptionChain) · …/introduction/type-of-apis-available/
> **Fetched:** 2026-07-13 · **Evidence tier:** **LIVE-DOC (the stream-all-symbols page — operator-pasted from a live browser 2026-07-13; preserved at the coordinator scratchpad `gdf-live-streamallsymbols-paste.md`)** + SEARCH (landing/one-liners). The 2026 docs sidebar (LIVE-DOC nav) confirms the Streaming API as a live current top-level section.

---

## 1. Product model (LIVE-DOC, Verified)

| Fact | Detail |
|---|---|
| What it is | "Push Type API": after connecting + authenticating, the server streams realtime data of **ALL symbols of ALL exchanges enabled on the API key** — NO per-symbol subscribe calls at all |
| Entitlement | Verbatim: "No additional function is required to be enabled for this functionality Only the Authentication need to be done. It will send Realtime data of all symbols of those exchanges which are enabled for that API key." → **key-level (per-exchange) entitlement; the firehose auto-starts after auth** |
| Auth | IDENTICAL to the standard WS API: `{"MessageType":"Authenticate","Password":accessKey}` → `{"Complete":true,"Message":"Welcome!","MessageType":"AuthenticateResult"}` |
| Semantics | Same field SET as `RealtimeResult` (03 §4) — incl. `Close` = prev-day close, epoch-seconds wording identical to the SubscribeRealtime page — but a **DIFFERENT wire dialect** (§2) |
| Sibling | `StreamAllSnapshots` — snapshot bars of all symbols; **wire shape still uncaptured** (U-1 residual) |

## 2. WIRE FORMAT — Batch envelope with ABBREVIATED keys (a THIRD dialect!)

Ticks arrive wrapped in a batch object, rows keyed by SHORT names (NOT the PascalCase `RealtimeResult` shape — a distinct serde struct is required):

```json
{"T":"Batch","data":[
{"E":"NFO","I":"OPTSTK_SBIN_28OCT2025_CE_860","LTT":1759831039,"STT":1759831040,"ATP":22.22,"BP":20.15,"BQ":15000,"C":25.95,"H":26.5,"L":19.25,"LTP":20.15,"LTQ":0,"O":26.45,"OI":1998000,"QL":750,"SP":20.25,"SQ":750,"TTQ":3099000,"VAL":68859780,"PO":false,"PC":-5.8,"PCP":-22.35,"OIC":232500,"T":"RT"},
{"E":"NFO","I":"OPTSTK_OFSS_28OCT2025_CE_9200","LTT":1759831039,"STT":1759831040,"ATP":305.12,"BP":378,"BQ":75,"C":249.55,"H":383,"L":250,"LTP":377.5,"LTQ":0,"O":251.35,"OI":19950,"QL":75,"SP":380.5,"SQ":75,"TTQ":199050,"VAL":60734136,"PO":false,"PC":127.95,"PCP":51.27,"OIC":-3375,"T":"RT"},
{"E":"NFO","I":"OPTIDX_MIDCPNIFTY_28OCT2025_PE_12300","LTT":1759831039,"STT":1759831040,"ATP":20.46,"BP":19.55,"BQ":140,"C":27.6,"H":26.25,"L":16.75,"LTP":20.25,"LTQ":140,"O":25.85,"OI":206500,"QL":140,"SP":20.4,"SQ":700,"TTQ":574140,"VAL":11746904.4,"PO":false,"PC":-7.35,"PCP":-26.63,"OIC":-25900,"T":"RT"}
]}
```

(LIVE-DOC verbatim rows; the doc-page sample carried 17 array elements — 14 elided above, a mix of OPTSTK/OPTIDX incl. quoted-only rows; page note: "++ multiple line of traded symbols in batch wise format like above".)

### Abbreviated-key legend (verbatim from the page) ↔ long-name ↔ type

| Abbrev | Long name (03 §4 semantics) | Type |
|---|---|---|
| `T` (outer) | envelope discriminator `"Batch"` | string |
| `T` (row) | MessageType `"RT"` (realtime row) | string — ⚠ same key name at both levels |
| `E` / `I` | Exchange / InstrumentIdentifier (LONG format observed) | string |
| `LTT` / `STT` | LastTradeTime / ServerTime | int, epoch seconds |
| `LTP` / `LTQ` | LastTradePrice / LastTradeQty | f64 / int |
| `ATP` | AverageTradedPrice | f64 |
| `O` / `H` / `L` | Open / High / Low | f64 |
| `C` | Close (**prev-day close** — ARITH re-verified: SBIN row PC −5.8 = LTP 20.15 − C 25.95) | f64 |
| `BP`/`BQ` · `SP`/`SQ` | BuyPrice/BuyQty · SellPrice/SellQty (L1) | f64/int |
| `TTQ` / `VAL` | TotalQtyTraded / Value | int / f64 |
| `OI` / `OIC` | OpenInterest / OpenInterestChange | int / int (signed) |
| `QL` | QuotationLot | number |
| `PO` | PreOpen | bool |
| `PC` / `PCP` | PriceChange / PriceChangePercentage | f64 |

### Observed behaviors (from the LIVE-DOC sample)

- `STT − LTT = 1` second in every sample row (server stamp one second after exchange stamp). LTT 1759831039 = 2025-10-07 15:27:19 IST (ARITH — in-session, true-UTC reading holds in this dialect too).
- **Quoted-only rows stream too**: full-universe delivery includes untraded strikes — rows with `ATP/H/L/O/TTQ/VAL` all 0 and `LTP` 0 (or carrying a previous value). A consumer MUST tolerate zero-OHLC rows and not treat them as corrupt.
- `LTQ: 0` on traded rows (quote-refresh seconds), matching the standard-feed behavior (03 §3b).
- Number formatting drifts int-vs-float per row (`"BP":378` vs `"BP":20.15`) — parse all prices as f64.

## 3. Adjacent mechanism (unchanged)

Whole-exchange PULL exists without the Streaming API: `GetExchangeSnapshot` (04 §5) + the server-side `GetExchangeSnapshotAfterMarket` name. `StreamAllSnapshots` is presumably the push variant — wire shape **Unknown** (U-1 residual).

## 4. What is STILL not known (U-1 residual — see 99-UNKNOWNS)

| Gap | Why it matters |
|---|---|
| Dedicated endpoint/port vs the standard WS host:port | connection architecture |
| Batch emission cadence + batch size bounds (rows per Batch frame, frames/sec) | consumer sizing |
| Bandwidth for a full NFO universe (tens of thousands of L1 rows/sec-class?) | host + network sizing |
| Do request-response functions (GetLimitation, GetInstruments, …) work on the SAME streaming connection? | boot design |
| `StreamAllSnapshots` wire shape | bar firehose option |
| Pricing/cost of enabling exchanges for streaming (key-level entitlement = the pricing axis) | purchase decision |

## What this prevents

- Decoding the firehose with the `RealtimeResult` struct (it is a different, abbreviated-key Batch dialect — needs its own serde type; note the two-level `T` key).
- Treating zero-OHLC untraded-strike rows as data corruption.
- Assuming per-symbol subscription control exists on this API (it is all-or-nothing per exchange on the key).
