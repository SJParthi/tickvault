# GDF 10 — Streaming "Push Type" API (StreamAllSymbols / StreamAllSnapshots)

> **Source:** https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/streaming_api/stream-all-symbols/ · …/streaming_api/streamallsnapshots/ · …/streaming_api/ (landing) · …/advanced_streaming_api/ (hosts SubscribeOptionChain) · …/introduction/type-of-apis-available/
> **Fetched:** 2026-07-13 (operator paste) + **2026-07-14 (machine-fetched, docs-fetch.yml runner @ 7d354a8d)** · **Evidence tier:** **RUNNER-DOC (2026-07-14 — stream-all-symbols FULL page + the never-before-captured streamallsnapshots page + the streaming_api landing; equals LIVE-DOC)** + **LIVE-DOC (the stream-all-symbols page — operator-pasted from a live browser 2026-07-13; preserved at the coordinator scratchpad `gdf-live-streamallsymbols-paste.md`)** + SEARCH (landing/one-liners). The 2026 docs sidebar (LIVE-DOC nav + RUNNER-DOC nav) confirms the Streaming API as a live current top-level section with exactly two children: StreamAllSymbols + StreamAllSnapshots.

---

## 2026-07-14 RUNNER-DOC reconciliation (docs-fetch runner, branch docs-fetch/gdf-2026-07-14 @ 7d354a8d)

**LIVE-DOC cross-check verdict (§2): IDENTICAL.** The runner fetch of the StreamAllSymbols page (RUNNER-DOC 2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/streaming_api/stream-all-symbols/, sha256 `16b7bd7f…`) matches the 2026-07-13 operator paste: same key-level-entitlement wording ("No additional function is required … Only the Authentication need to be done"), same Authenticate request/response, same Batch sample — the runner captured the FULL 17-row array (the paste had quoted 3 of 17); the 3 previously-quoted rows (SBIN CE 860, OFSS CE 9200, MIDCPNIFTY PE 12300) are byte-identical, and the 14 previously-elided rows match the pack's description (a mix of OPTSTK/OPTIDX incl. quoted-only rows). The abbreviated-key legend ("Expansion on returns") is verbatim-identical. ZERO divergence.

| # | Verdict | Claim | Evidence |
|---|---|---|---|
| 1 | CONFIRMED (tier now RUNNER-DOC) | §1 entire product model: firehose auto-starts after auth; key-level per-exchange entitlement; Authenticate identical to standard WS | stream-all-symbols page, verbatim |
| 2 | CONFIRMED (RUNNER-DOC) | §1 "Same field SET as RealtimeResult": the page carries the SAME 23-field long-name "What is returned?" list + the same epoch-seconds wording (incl. the "ServerTradeTime" typo) as the SubscribeRealtime page — field-set parity is now page-stated, not inferred | same page |
| 3 | CONFIRMED (RUNNER-DOC) | §2 Batch envelope + abbreviated-key dialect + the full legend; `STT − LTT ∈ {0, 1}` s (the full 17-row sample adds rows with `STT == LTT`, e.g. NIFTYNXT50 CE 61900 — the pack's "= 1 in every sample row" was an artifact of the 3-row subset; correction folded into §2 below) | same page |
| 4 | NEW (RUNNER-DOC) | Untraded-row artifacts in the newly captured rows: `PC = −C` with `PCP = −100` on LTP=0 rows; `LTP` seeded from prev-close with `PC/PCP = 0` on other never-traded rows; nonzero `OIC` on `OI = 0` rows — folded into §2 observed behaviors | same page, rows quoted in §2b |
| 5 | **NEW — U-1 residual CLOSED: StreamAllSnapshots wire format** | The streamallsnapshots page (never captured before) documents the full request/response: Authenticate-only, key-level entitlement **including Exchange + Periodicity + Period**, and a PascalCase `RealtimeSnapshotCollection` envelope — a FOURTH dialect, NOT the short-key Batch shape. Documented verbatim in the new §2c | https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/streaming_api/streamallsnapshots/, sha256 `6e7083cc…` |
| 6 | U-1 residuals still OPEN | Endpoint/port identity (neither streaming page names a host), batch emission cadence, batch-size bounds, full-universe bandwidth, whether request-response functions work on the same streaming connection, and streaming pricing — NONE stated on either page; live probe / sales question | both streaming pages |
| 7 | CONFIRMED (RUNNER-DOC) | §3 sidebar/nav: the Streaming API section contains exactly StreamAllSymbols + StreamAllSnapshots (no third function) | streaming_api landing + every page's nav |

---

## 1. Product model (LIVE-DOC, Verified)

| Fact | Detail |
|---|---|
| What it is | "Push Type API": after connecting + authenticating, the server streams realtime data of **ALL symbols of ALL exchanges enabled on the API key** — NO per-symbol subscribe calls at all |
| Entitlement | Verbatim: "No additional function is required to be enabled for this functionality Only the Authentication need to be done. It will send Realtime data of all symbols of those exchanges which are enabled for that API key." → **key-level (per-exchange) entitlement; the firehose auto-starts after auth** |
| Auth | IDENTICAL to the standard WS API: `{"MessageType":"Authenticate","Password":accessKey}` → `{"Complete":true,"Message":"Welcome!","MessageType":"AuthenticateResult"}` |
| Semantics | Same field SET as `RealtimeResult` (03 §4) — incl. `Close` = prev-day close, epoch-seconds wording identical to the SubscribeRealtime page — but a **DIFFERENT wire dialect** (§2) |
| Sibling | `StreamAllSnapshots` — snapshot bars of all symbols; **wire shape CAPTURED 2026-07-14 (RUNNER-DOC)** — see §2c |

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

### Observed behaviors (from the LIVE-DOC sample; extended by the RUNNER-DOC full 17-row capture 2026-07-14)

- `STT − LTT ∈ {0, 1}` seconds across the full 17-row sample (**corrected 2026-07-14** — the earlier "= 1 in every sample row" reflected only the 3-row paste subset; rows with `LTT == STT == 1759831040` exist, e.g. `OPTIDX_NIFTYNXT50_28OCT2025_CE_61900`). LTT 1759831039 = 2025-10-07 15:27:19 IST (ARITH — in-session, true-UTC reading holds in this dialect too).
- **Quoted-only rows stream too**: full-universe delivery includes untraded strikes — rows with `ATP/H/L/O/TTQ/VAL` all 0 and `LTP` 0 (or carrying a previous value). A consumer MUST tolerate zero-OHLC rows and not treat them as corrupt.
- **Untraded-row derived-field artifacts (RUNNER-DOC 2026-07-14, §2b rows):** on `LTP: 0` rows the server still computes `PC = LTP − C` → `"PC":-6923.3,"PCP":-100` (a nonsense −100% "change"); on other never-traded rows `LTP` is SEEDED from prev-close (`LTP == C`, `PC/PCP = 0`, e.g. BANKNIFTY PE 60700); and `OIC` can be nonzero while `OI` is 0 (e.g. NIFTY 04NOV CE 24650: `OI:0, OIC:-525`). A consumer must not trust `PC/PCP/OIC` as trade signals on zero-volume rows.
- `LTQ: 0` on traded rows (quote-refresh seconds), matching the standard-feed behavior (03 §3b).
- Number formatting drifts int-vs-float per row (`"BP":378` vs `"BP":20.15`) — parse all prices as f64.

### §2b. Additional verbatim rows (RUNNER-DOC 2026-07-14 — the previously-elided artifact rows)

```json
{"E":"NFO","I":"OPTIDX_NIFTYNXT50_28OCT2025_CE_61900","LTT":1759831040,"STT":1759831040,"ATP":0,"BP":6224.4,"BQ":575,"C":6923.3,"H":0,"L":0,"LTP":0,"LTQ":0,"O":0,"OI":0,"QL":25,"SP":7893.2,"SQ":50,"TTQ":0,"VAL":0,"PO":false,"PC":-6923.3,"PCP":-100,"OIC":0,"T":"RT"}
{"E":"NFO","I":"OPTIDX_NIFTY_04NOV2025_CE_24650","LTT":1759831039,"STT":1759831040,"ATP":0,"BP":302.4,"BQ":75,"C":723.8,"H":0,"L":0,"LTP":723.8,"LTQ":0,"O":0,"OI":0,"QL":75,"SP":692.35,"SQ":75,"TTQ":0,"VAL":0,"PO":false,"PC":0,"PCP":0,"OIC":-525,"T":"RT"}
{"E":"NFO","I":"OPTIDX_BANKNIFTY_28OCT2025_PE_60700","LTT":1759831040,"STT":1759831040,"ATP":0,"BP":4209.75,"BQ":35,"C":5186.7,"H":0,"L":0,"LTP":5186.7,"LTQ":0,"O":0,"OI":0,"QL":35,"SP":4468.3,"SQ":350,"TTQ":0,"VAL":0,"PO":false,"PC":0,"PCP":0,"OIC":-140,"T":"RT"}
```

(3 of the 14 newly captured rows — the artifact exemplars cited above; the full 17-row sample is in the corpus file, sha256 `16b7bd7f…`.)

### §2c. StreamAllSnapshots — the bar firehose (RUNNER-DOC 2026-07-14, first-ever capture; closes the U-1 "StreamAllSnapshots format" residual)

Verbatim page semantics (https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/streaming_api/streamallsnapshots/):

> "StreamAllSnapshots – No additional function is required to be enabled for this functionality. It will send Realtime Snapshot data of all symbols **with respective to the Exchange, Periodicity and Period**, which are enabled for the API key."

- **Authenticate-only**, exactly like StreamAllSymbols — same `{"MessageType":"Authenticate","Password":accessKey}` → `{"Complete":true,"Message":"Welcome!","MessageType":"AuthenticateResult"}`.
- **Key-level entitlement now includes Periodicity + Period** (not just exchanges) — the streamed bar timeframe is configured ON THE KEY, not per request (Verified verbatim above). Which periodicities are purchasable is a sales question (U-1 pricing residual).
- Fields returned (page list, verbatim): "Exchange, InstrumentIdentifier (Symbol), Periodicity, Period, LastTradeTime, TradedQuantity, OpenInterest, Open, High, Low, Close (previous Day's Close)." ⚠ TWO page-prose issues, both resolved by the sample: the JSON field is **`TradedQty`** (prose says "TradedQuantity"), and the "(previous Day's Close)" gloss appears copy-pasted from the realtime pages — the sample's `Close` sits INSIDE the bar's O/H/L range (CRUDEOIL O 5934 / H 5936 / L 5934 / C 5935), i.e. **BAR close per the Snapshot-family dual semantics (README claim 14)**. Treat the sample as authoritative; live-probe to confirm (page-internal contradiction, flagged).

**Wire format — a FOURTH dialect** (PascalCase envelope, NOT the §2 short-key Batch shape; a third distinct serde type is required):

```json
{"Exchange":"MCX","Periodicity":"MINUTE","Period":1,"Result":[{"InstrumentIdentifier":"FUTCOM_CRUDEOIL_19NOV2024__0",
"LastTradeTime":1729683720,
"TradedQty":61,
"OpenInterest":14564,
"Open":5934.0,
"High":5936.0,
"Low":5934.0,
"Close":5935.0},
{"InstrumentIdentifier":"OPTFUT_NATURALGAS_24OCT2024_CE_190",
"LastTradeTime":1729683720,
"TradedQty":141,
"OpenInterest":10085,
"Open":4.95,
"High":5.05,
"Low":4.9,
"Close":5.05}],
"MessageType":"RealtimeSnapshotCollection"}
```

(Verbatim, elided rows marked "++ multiple line of traded symbols" on the page.)

| Property | Verdict |
|---|---|
| Envelope discriminator | `MessageType: "RealtimeSnapshotCollection"` — a NEW decoder discriminant (add to the tagged-enum set, 03 §5) |
| `Exchange` / `Periodicity` / `Period` | ENVELOPE-level (once per collection), NOT per row — one collection per exchange per bar interval (sample: `"MCX"`, `"MINUTE"`, `1`) |
| Row fields | `InstrumentIdentifier` (LONG format), `LastTradeTime`, `TradedQty`, `OpenInterest`, `Open/High/Low/Close` — NO `ServerTime`, NO bid/ask, NO `Value`, NO `PreOpen` per row |
| `LastTradeTime` | epoch SECONDS (same boilerplate wording as the realtime pages, same "ServerTradeTime" typo); sample `1729683720` = 2024-10-23 17:12:00 IST — MCX in-session, and **minute-aligned** (`% 60 == 0`), consistent with MINUTE/1 bar stamps; BOTH sample rows carry the same aligned stamp. Whether the stamp is bar-open or bar-close time is NOT stated (Unknown — live probe) |
| `TradedQty` | reads as per-BAR traded quantity (Snapshot-family semantics); NOT the session cumulative `TotalQtyTraded` — Assumed from the field-name family, verify live |
| "traded symbols" | the page's elision note says "multiple line of **traded** symbols" — suggests only symbols with bar activity appear per collection (Assumed from wording; contrast §2's quoted-only rows on StreamAllSymbols) |

## 3. Adjacent mechanism (unchanged)

Whole-exchange PULL exists without the Streaming API: `GetExchangeSnapshot` (04 §5) + the server-side `GetExchangeSnapshotAfterMarket` name. `StreamAllSnapshots` IS the push variant — wire shape captured 2026-07-14 (RUNNER-DOC), see §2c.

## 4. What is STILL not known (U-1 residual — see 99-UNKNOWNS; re-checked against the RUNNER-DOC 2026-07-14 corpus — neither streaming page states any of these)

| Gap | Why it matters |
|---|---|
| Dedicated endpoint/port vs the standard WS host:port | connection architecture |
| Batch emission cadence + batch size bounds (rows per Batch frame, frames/sec) | consumer sizing |
| Bandwidth for a full NFO universe (tens of thousands of L1 rows/sec-class?) | host + network sizing |
| Do request-response functions (GetLimitation, GetInstruments, …) work on the SAME streaming connection? | boot design |
| ~~`StreamAllSnapshots` wire shape~~ **CLOSED 2026-07-14 (RUNNER-DOC — §2c)**; residuals: bar-stamp open-vs-close, `TradedQty` per-bar semantics, collection cadence | bar firehose option |
| Pricing/cost of enabling exchanges (+ Periodicity/Period for StreamAllSnapshots) for streaming (key-level entitlement = the pricing axis) | purchase decision |

## What this prevents

- Decoding the firehose with the `RealtimeResult` struct (it is a different, abbreviated-key Batch dialect — needs its own serde type; note the two-level `T` key).
- Decoding `StreamAllSnapshots` with EITHER of those structs (it is a THIRD wire shape — PascalCase `RealtimeSnapshotCollection` envelope with envelope-level Exchange/Periodicity/Period; §2c).
- Treating zero-OHLC untraded-strike rows as data corruption — or trusting `PC/PCP/OIC` on zero-volume rows (server-computed artifacts, §2 + §2b).
- Trusting the StreamAllSnapshots page's "Close (previous Day's Close)" prose gloss — the sample proves BAR close (§2c; README claim 14 dual semantics).
- Assuming per-symbol subscription control exists on this API (it is all-or-nothing per exchange on the key).
