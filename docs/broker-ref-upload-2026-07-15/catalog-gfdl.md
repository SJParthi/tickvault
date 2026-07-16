# GlobalDataFeeds (GFDL) — COMPLETE Data Catalog

> **Provider:** Global Financial Datafeeds LLP (globaldatafeeds.in) — NSE/BSE/MCX authorized realtime data vendor, est. 2010.
> **Compiled:** 2026-07-16, from the 2026-07-15 doc archive (`scratchpad/broker-docs/docs/broker-ref-upload-2026-07-15/globaldatafeeds/` — filenames cited below record their original globaldatafeeds.in URLs), the in-repo RUNNER-DOC verbatim crawl (`docs/gdf-ref/`, 213/215 live pages, 2026-07-14), and cited WebSearch results.
> **Rule kept:** every row carries verbatim evidence + source. Anything not provable is marked **UNVERIFIED (live probe needed)** with exactly what the probe must check.
> **Note:** GFDL and "GDF" (the repo's `gdf-third-feed-scope-2026-07-13.md` lock) are the SAME vendor; nothing found here contradicts the repo lock — the fresh 2026-07-11 pricing page makes the 1-second verdict explicit and current.

---

## ★ HEADLINE VERDICT — Push WS granularity

**VERDICT: 1-SECOND CONFLATED L1 SNAPSHOT — NOT true tick-by-tick. No documented plan or segment receives FINER than 1/sec — several receive coarser, delayed, or no realtime push at all: the snapshot-only plans (Historical API + Exchange Snapshot API carry SubscribeRealtime = NO in the `25` Compare-APIs matrix), delayed keys ("RealtimeSubscription disabled for delayed data." — `03`/`26`), and NCDEX at "Delayed (5min)" (`02` supported-exchanges table); the only possibly-FINER tier is the undocumented, contact-sales FIX API (UNVERIFIED).**

| # | Verbatim evidence | Source |
|---|---|---|
| 1 | "Realtime data updating at 1 second frequency is available for NSE Stocks, NSE Indices, NSE F&O, NSE Currency ,MCX,BSE Stocks ,BSE Indices & BSE F&O. This is L1 data with single best Bid & Ask details." | `25-pricing-purchase.md` (https://globaldatafeeds.in/…/pricing-sales/api-pricing/, last updated 2026-07-11) — identical sentence on `02-introduction-getting-started.md` (type-of-data-available page) |
| 2 | "SubscribeRealtime : Subscribe to Realtime data, returns market data every second (Bid/Ask/Trade)" and "Example of returned data in JSON format. This data is returned every second" | `05-websocket-subscribe.md` (function-subscriberealtime page, last updated 2026-04-13) |
| 3 | "SubscribeRealtime \| Subscribe to market data, pushes market data every second (Bid/Ask/Trade)" | `04-websocket-connection-auth.md` function list; repeated in `02-introduction-getting-started.md` data-package tables + `25-pricing-purchase.md` Compare-APIs matrix |
| 4 | "We disseminate authentic, accurate and affordable realtime data with low latency having 1 second update frequency" | `02-introduction-getting-started.md` (introduction-to-apis page) |
| 5 | "Streaming Live Data API \| L1 data @1sec frequency with single best bid/ask details" | `01-overview.md` (APIs product page feature matrix) |
| 6 | "Client can subscribe Realtime data and get the data for every second." / "In this client will receive the data with 1 second frequency." | `32-python-library-ws-gfdl.md` (official PyPI `ws-gfdl` README) |
| 7 | "SubscribeRealtime : pushes realtime data every second for the subscribed instrument from server" (×3 occurrences in sample-code comments) | `23a-code-samples-websocket-js-python.md` + `23b` |
| 8 | Greeks stream same cadence: "SubscribeRealtimeGreeks: Subscribe to Realtime Greek values of single Token, returns market data every second (detailed)" | `17-greeks-api-websocket.md` |
| 9 | Conflation vendor-admitted: "During live markets, hundreds of trades can occur every second. Different platforms may: Capture slightly different tick sequences … Aggregate ticks differently" — the exchange tape has intra-second trades; a 1/sec L1 stream necessarily folds them into TotalQtyTraded/Value | `26-faq.md` (https://globaldatafeeds.in/…/faqs/faqs/) |
| 10 | Historical "Tick" = the 1-second sample, not exchange trades: backfill tables read "\| Tick \| Every 1 Sec \| 1 Calendar Week \|" | `02-introduction-getting-started.md` (type-of-data-available) |
| 11 | Live sample: ~one `RealtimeResult` per instrument per second — but NOT strictly one-per-second-with-both-clocks-advancing: in the `23b` MCX NATURALGAS-I transcript (13 messages over 11 ServerTime seconds 1594038362–372) `LastTradeTime` REPEATS across consecutive messages (LTT 1594038365 at ST 365 then again at ST 366; LTT 1594038368 at ST 368 then at ST 369) and `ServerTime` itself repeats (two messages each at ST 1594038366 and ST 1594038370); `TotalQtyTraded` advances in EVERY message (+37/+14 on the repeated-LTT ones — trades printed, stamped to the prior second; STT−LTT ∈ {0,1}s), so no trade-less second is observed IN THE LIQUID 23b MCX TRANSCRIPT (the `23a` transcript DOES carry cross-segment trade-less/skipped-second evidence — see residual (a) below). Interleaved `{"MessageType":"Echo"}` heartbeats; the `05` NIFTY-I sample carries `"LastTradeQty":0` (quote-shaped message — whether trade-less, indeterminable from one frame) | `23b` Java/NodeJS sample transcripts; `23a` multi-segment transcript; `05-websocket-subscribe.md` sample; `06-websocket-quotes.md` GetLastQuote sample |
| 12 | Timestamps whole-second only: "LastTradeTime, ServerTradeTime : These values are expressed as no. of seconds since Epoch time (i.e. 1st January 1970). Also known as Unix Time." — repeated on every WS function page; live samples 10-digit | `05-websocket-subscribe.md` (also 06, 07, 16, 17) |
| 13 | FIX-only tick-by-tick claim: "FIX APIs … Ultra-fast & reliable: Delivers tick-by-tick market data with minimal latency." — no documentation pages exist, no exchange list, contact sales | `docs/gdf-ref/01-product-and-entitlement.md` item 11 + `docs/gdf-ref/12` §5 (RUNNER-DOC 2026-07-14, quoting the live https://globaldatafeeds.in/apis/ page verbatim). NOTE: the archive's `01-overview.md` (line 53) carries only a compiler PARAPHRASE of the same section ("Ultra-fast & reliable tick-by-tick delivery with minimal latency") |
| 14 | Marketing self-contradiction (do NOT trust): "True tick-by-tick data with time-stamp of 1 second" — the same page's bandwidth math proves 1 msg/sec/symbol: "our data updates every second and for all symbols simultaneously … 225*60=13500 messages per minute" (= exactly 1/sec × 225 symbols) | https://globaldatafeeds.in/nimbledatapro/ (RUNNER-DOC via `docs/gdf-ref/12`) |
| 15 | Upstream-reality context: NSE's own non-colo vendor broadcast is ~1s snapshots; true TBT exists only at NSE colo — the 1s cadence is upstream distribution reality, not GFDL-only conflation | nsearchives.nseindia.com Snapshot MDR vendor spec, via `docs/gdf-ref/12` §2 |

**Residual UNVERIFIED on the verdict (live probe):** (a) emit-on-change vs unconditional every-second during no-trade seconds for SubscribeRealtime (the GetSnapshot page says "if no trades happen during that period, no data is sent by the server" — the SubscribeRealtime equivalent is undocumented). **(a) is NOT fully open on the doc corpus — it is per-segment suggestive:** the `23a` transcript shows TRADE-LESS per-second emissions for NSE_IDX ("NIFTY 50" `RealtimeResult` rows at four consecutive ServerTime seconds 1593422508/509/510/511, ALL with `TotalQtyTraded:0` + `LastTradeQty:0` — and `LastTradeTime` advances anyway: for indices LTT is the index-VALUE update time, not a trade time), but SKIPPED seconds for a quiet NSE cash stock (BAJAJ-AUTO: messages at ST …507 → …510 → …511, NO message for seconds 508–509 in a contiguous-looking transcript) and for CDS (USDINR-I, subscribed in the same 14:51:47 batch as the others, emits nothing until ST …510). The `23b` repeated-LTT MCX messages do NOT bear on (a) — their `TotalQtyTraded` advances (+37/+14), so they are trades stamped to the prior second, not trade-less emissions. Doc transcripts may be curated/trimmed, so the live probe is still needed — **expected outcome: indices emit every second regardless of trades; traded instruments emit on activity (quiet seconds skipped)**; (b) whether EVERY subscribed symbol truly gets an update EVERY second under burst load (U-17); (c) whether the FIX tier is real exchange TBT or the same 1s feed over FIX transport (U-4 — sales question in writing).

---

## 1. Streaming tick/quote fields — field-by-field

### 1a. `SubscribeRealtime` → `MessageType:"RealtimeResult"` (the primary realtime push; full-image quote, no deltas)

Doc field list, verbatim (`05-websocket-subscribe.md`, "What is returned ?"): *"Exchange, InstrumentIdentifier (Symbol), LastTradeTime, ServerTime, AverageTradedPrice (VWAP), Close (previous Day's Close), Open (Day's Open), High (Day's High), Low (Day's Low), LastTradePrice, LastTradeQty, TotalQtyTraded, BuyPrice (Bid), BuyQty (Bid Size), SellPrice (Ask), SellQty (Sell Size), OpenInterest, QuotationLot (Lot Size), Value (Turnover), PreOpen (if PreOpen), PriceChange…, PriceChangePercentage…, OpenInterestChange…"*

Verbatim 2026 live-page JSON sample (function-subscriberealtime page, LIVE-DOC 2026-07-13 + byte-identical RUNNER-DOC 2026-07-14):

```json
{"Exchange":"NFO","InstrumentIdentifier":"NIFTY-I","LastTradeTime":1776057749,"ServerTime":1776057749,"AverageTradedPrice":23689.38,"BuyPrice":23772.3,"BuyQty":325,"Close":24101.0,"High":23782.0,"Low":23625.0,"LastTradePrice":23775.0,"LastTradeQty":0,"Open":23748.0,"OpenInterest":18782270,"QuotationLot":65.0,"SellPrice":23775.0,"SellQty":975,"TotalQtyTraded":2596880,"Value":61518477134.4,"PreOpen":false,"PriceChange":-326.0,"PriceChangePercentage":-1.35,"OpenInterestChange":205335,"MessageType":"RealtimeResult"}
```

| Field | Type / precision | Meaning (doc wording) | Evidence |
|---|---|---|---|
| `Exchange` | string | exchange code (e.g. "NFO") | sample above; `03-fields-symbols-diagnostics.md`: `EXCHANGE \| "NFO" \| Exchange` |
| `InstrumentIdentifier` | string | "Symbol" | sample; `03-fields`: `INSTRUMENTIDENTIFIER \| "NIFTY-I" \| Instrument Identifier (Symbol)` |
| `LastTradeTime` | int, **epoch SECONDS, whole-second precision only** | last trade time — ⚠ **per-segment semantics: on NSE_IDX index rows LTT is the index-VALUE update time, not a trade time** (`23a` "NIFTY 50" rows: LTT advances 1593422508→511 every second while `TotalQtyTraded:0` + `LastTradeQty:0`) | "expressed as no. of seconds since Epoch time" (`05-websocket-subscribe.md`, repeated on 06/07/16/17); 10-digit live samples; `23a` NSE_IDX transcript. ⚠ `03-fields` master table shows a 13-digit sample `1589796000000` (doc drift / REST dialect); every function page + WS sample = seconds. **Sub-second precision: NONE documented anywhere — UNVERIFIED (live probe: confirm no ms field on wire; expected absent)** |
| `ServerTime` | int, epoch seconds | server-side stamp; `ServerTime − LastTradeTime` = built-in per-message lag probe | same quote (page typo "ServerTradeTime"); samples show STT−LTT ∈ {0,1}s |
| `AverageTradedPrice` | float | "VWAP" (session) | `03-fields`: `AVERAGETRADEPRICE \| 8905.09 \| Average Traded Price(VWAP)` |
| `BuyPrice` / `BuyQty` | float / int | **single best** bid price/size | `03-fields`: "Bid Price"/"Bid Size"; "This is L1 data with single best Bid & Ask details" (`25-pricing-purchase.md`) |
| `SellPrice` / `SellQty` | float / int | **single best** ask price/size (doc typo "Sell Size") | `03-fields`: "Ask Price"/"Ask Size" |
| `Open` / `High` / `Low` | float | current day's O/H/L | `05-websocket-subscribe.md` field list ("Day's Open/High/Low") |
| `Close` | float | ⚠ **PREVIOUS trading day's close**, not today's | `03-fields`: `CLOSE \| 9137.1 \| Previous (Trading Day's) Close`; `05-…`: "Close (previous Day's Close)". (In Snapshot/History BAR rows `Close` = bar close — context-dependent) |
| `LastTradePrice` | float | LTP | `03-fields`: "Last Traded Price (Close)" |
| `LastTradeQty` | int | last trade qty; **0 on quote-only updates** | sample `"LastTradeQty":0`; `03-fields` parenthetical "(Volume)" |
| `TotalQtyTraded` | int | cumulative day volume | `03-fields`: "Total Quantity Traded (Total Volume of the day)" |
| `Value` | float | cumulative day turnover | `03-fields`: "Total Turnover" |
| `OpenInterest` | int | OI, **inline every second** (unlike Dhan's separate OI packet) | sample; field list |
| `OpenInterestChange` | int | "Change in Open Interest compared to previous trading day's Close" | `05-websocket-subscribe.md` field list |
| `QuotationLot` | float | "Lot Size" (observed 0.0 in some samples) | `03-fields`: `QUOTATIONLOT \| 75 \| Lot Size`; 23a/23b samples |
| `PreOpen` | bool | "If Preopen is active" — pre-open indicative ticks delivered in-band, flagged | `03-fields`; pre-open OHLC semantics **UNVERIFIED (U-15 — probe: field behavior 09:00–09:08 IST)** |
| `PriceChange` / `PriceChangePercentage` | float | vs previous day's close, ₹ / % | `05-websocket-subscribe.md` field list; absent in 2018-era schema → parse Option-al |
| `MessageType` | string | `"RealtimeResult"` discriminator | sample |

**Request/subscription mechanics:** `{MessageType:"SubscribeRealtime", Exchange, InstrumentIdentifier, Unsubscribe(optional, default false)}` — ONE request PER symbol ("If you want to subscribe to data of multiple symbols, you will need to send 1 request each - for each symbol" — `23a`). No resume: "if there is internet disconnection for any reason, you will need to subscribe to all the instruments again" (`23a`).

**NOT in the realtime message (Verified absent):** TotalBuyQty/TotalSellQty (only on GetLastQuote family + REST option chain), any depth beyond best bid/ask, trade-id/sequence number, any sub-second timestamp.

### 1b. `StreamAllSymbols` firehose (separately-sold "Streaming API") — short-key dialect, same field set

`{"T":"Batch","data":[{…,"T":"RT"},…]}` batches across ALL symbols of the key's enabled exchanges; authenticate-only ("No additional function is required to be enabled" — `15-streaming-api.md`). Key expansion, doc-verbatim (`15-streaming-api.md`): *"E - Exchange , I - InstrumentIdentifier ,LTT - LastTradeTime , STT - ServerTime , ATP - AverageTradedPrice ,C- Close ,O- Open ,H- High ,L -Low, LTP- LastTradePrice,LTQ- LastTradeQty ,TTQ- TotalQtyTraded, BP- BuyPrice,BQ-BuyQty,SP- SellPrice ,SQ- SellQty,OI-OpenInterest,QL-QuotationLot,VAL-Value,PO-PreOpen,PC-PriceChange,PCP-PriceChangePercentage,OIC-OpenInterestChange, T- MessageType"*. Untraded strikes stream too (zero-OHLC rows). Batch cadence/endpoint identity/bandwidth: **UNVERIFIED (U-1 — probe: batch sizes + per-symbol cadence under load; whether it is a separate endpoint/entitlement)**. `StreamAllSnapshots` = the bar equivalent (`RealtimeSnapshotCollection`).

### 1c. On-demand pull family (same WS)

| Function | Fields | Evidence |
|---|---|---|
| `GetLastQuote` / `GetLastQuoteArray` (detailed; array max 25) | RealtimeResult set **+ TotalBuyQty, TotalSellQty** (samples show `0.0` — ever-populated **UNVERIFIED (live probe)**) | `06-websocket-quotes.md`: "…OpenInterestChange … ,TotalBuyQty , TotalSellQty"; `_decoded/function-getlastquote-2.html` (MODIFIED 2026-05-04) SBIN sample |
| `GetLastQuoteShort` | `{Exchange, InstrumentIdentifier, LastTradeTime, BuyPrice, LastTradePrice, SellPrice}` | `06-websocket-quotes.md` / `11-rest-quotes.md` |
| `GetLastQuoteShortWithClose` | short + `Close` (prev day's close) | `11-rest-quotes.md`: "…BuyPrice (Bid Price), Close (Previous Trading Day's Close), LastTradePrice, SellPrice (Ask Price)" |

### 1d. Housekeeping frames

| Frame | Evidence |
|---|---|
| Heartbeat `{"MessageType":"Echo"}` interleaved; cadence undocumented — **UNVERIFIED (probe: cadence + behavior if client never responds)** | `23a`/`23b` sample transcripts |
| Post-auth policy flags `{"AllowVMRunning":false,…}` / `{"AllowServerOSRunning":false,…}` — semantics undocumented, **UNVERIFIED (probe/sales: VM & server-OS run policy on our key)** | `23a` sample transcript |
| Diagnostics arrive as **text/plain, NOT JSON**: "Your application should be designed to handle these responses which are returned as text/plain (instead of JSON / XML)." (22-message catalog) | `03-fields-symbols-diagnostics.md` |

---

## 2. Market depth

| Item | Finding | Evidence |
|---|---|---|
| Book depth (5/20/200-level) | **NOT OFFERED — L1 only, single best bid/ask** (absence is the Verified finding: no depth function exists in the complete 29-function WS list or the REST function table) | "This is L1 data with single best Bid & Ask details." (`25-pricing-purchase.md` + `02-introduction-getting-started.md`); "L1 data @1sec frequency with single best bid/ask details" (`01-overview.md`); full function lists in `04-websocket-connection-auth.md` / `10-rest-connection.md` |
| The word "Depth" in GFDL docs | Means STRIKE-window count in the option chain, NOT book depth | `16-optionchain-api.md`: "**Depth** \| *Value like* 1, 5, 10, 15, 20. \| Mandatory parameter. To received count value up and down from 'StrikePrice'" |
| Aggregate order totals | `TotalBuyQty`/`TotalSellQty` on GetLastQuote/REST-chain only; samples 0.0 | `06-websocket-quotes.md`; `_decoded/function-getlastquote-2.html` — **UNVERIFIED whether ever non-zero (live probe)** |
| Depth via FIX tier | Marketing-only tier; no docs | `01-overview.md` — **UNVERIFIED (U-4, sales)** |
| "Market depth" in desktop-product marketing | Refers to desktop feeder products (NimbleDataPro), NOT the API | WebSearch snippet of desktop pages, via `gfdl-web.md` §2 |

---

## 3. Greeks / IV

| Item | Detail | Evidence |
|---|---|---|
| Source of values | **Vendor-relayed from NSE, NOT client-computed**: "We are receiving these values directly from NSE." (FAQ repeated 4×) | `17-greeks-api-websocket.md`; `18-greeks-api-rest.md` |
| Model | "Black Scholes Model"; "underlying, is taken as Futures or Synthetic futures( in case of expiries where future is not available). Thus Spot Price and Interest rate as a parameter gets removed" | `17-greeks-api-websocket.md` FAQ (verbatim) |
| Streamed greeks | `SubscribeRealtimeGreeks` — "Subscribe to Realtime Greek values of single Token, returns market data every second (detailed)" → `RealtimeGreeksResult`; keyed by numeric **Token** (from GetInstruments `TokenNumber`), one token per subscribe | `17-greeks-api-websocket.md` |
| Field set (17 greek metrics) | "Exchange, Token(TokenNumber of Symbol), Timestamp, **IV, Delta, Theta, Vega, Gamma, IVVwap, Vanna, Charm, Speed, Zomma, Color, Volga, Veta, ThetaGammaRatio, ThetaVegaRatio, DTR**" — scientific-notation floats on the wire (e.g. `3.0227099614421604E-06`) | `17-greeks-api-websocket.md` "What is returned ?"; `docs/gdf-ref/07` |
| Timestamp | epoch SECONDS (samples `"Timestamp":1777886925`) | `17-greeks-api-websocket.md` samples |
| IV unit | decimal fraction in 2025 samples (`0.0949…`) but 15.53 / 31.41-class values in older samples — **UNVERIFIED (live probe: fraction vs percent scaling)** | `18-greeks-api-rest.md` samples |
| Snapshot/bar greeks | `SubscribeSnapshotGreeks` (MINUTE periodicity, → `RealtimeSnapshotGreeksResult`); `GetSnapshotGreeks` (max 25 symbols, "works only during market hours", minute-bucket semantics, no-trade minutes omitted) | `17-greeks-api-websocket.md`; `18-greeks-api-rest.md`; `31-release-notes.md` "03rd September 2025 — Introduced SubscribeSnapshotGreeks and GetSnapshotGreeks APIs" |
| Chain greeks | `SubscribeOptionChainGreeks` (strike-window mechanics, quote+greeks combined per contract); `GetLastQuoteOptionGreeksChain` (whole chain); `GetLastQuoteOptionGreeks`/`ArrayOptionGreeks` (max 25 tokens, `tokens=39489+39487` syntax) | `17-greeks-api-websocket.md`; `18-greeks-api-rest.md` |
| Historical greeks | `GetHistoryGreeks` — Periodicity **TICK or MINUTE**, From/To/Max → `HistoryGreeksResult` (TICK sample = per-second rows); NFO-only per release note: "GetHistoryGreeks - fetches the history data of Greeks for NFO exchange." (22 Aug 2024). Retention depth undocumented — **UNVERIFIED (live probe)** | `17-greeks-api-websocket.md`; `18-greeks-api-rest.md`; `31-release-notes.md` |
| Greeks on the quote stream itself | NOT offered — greeks are a separate subscription family and a separately-paid package ("OptionGreeks", "OptionChain+OptionGreeks") | `02-introduction-getting-started.md` package tables; `25-pricing-purchase.md` |

---

## 4. Option chain

| Item | Detail | Evidence |
|---|---|---|
| `SubscribeOptionChain` (WS push) | "Returns data of entire OptionChain for requested underlying" — but the SUBSCRIPTION form is a **strike WINDOW**: params Exchange, Product (mandatory), Expiry (mandatory, DDMMMYYYY e.g. 23JAN2020), OptionType (optional; absent = CE+PE both), **StrikePrice (mandatory) + Depth ∈ {1,5,10,15,20} strikes up & down**, Unsubscribe. Returns full quote field set per contract (doc's own duplicated "OpenInterest, OpenInterest" preserved) → `RealtimeOptionChainResult` | `16-optionchain-api.md` |
| Chain push cadence | NOT stated on-page; realtime family default is 1/sec — **UNVERIFIED (live probe: interval of SubscribeOptionChain / SubscribeRealtimeOptionChain pushes)**. Release note: "SubscribeRealtimeOptionChain – fetches data of Options automatically at periodic intervals." (25 Sep 2024) | `16-optionchain-api.md`; `31-release-notes.md` |
| `GetLastQuoteOptionChain` (WS + REST one-shot) | ENTIRE chain in one call; Expiry optional ("If absent, result is sent for all active expiries"), OptionType/StrikePrice optional (all strikes) → full-chain-all-expiries pull in ONE call. REST variant adds `TotalBuyQty,TotalSellQty` + CSV/XML formats (last updated 2026-05-04) | `16-optionchain-api.md` |
| Heaviness | "If you have requested entire GetHistory, GetInstruments (for NSE / NFO / BFO), GetExchangeSnapshot or GetLastQuoteOptionChain, these functions return thousands of rows" | `24-code-samples-rest.md` |
| Marketing | "Live Option Chain API \| Realtime data of entire option chain of the underlying in single call" | `01-overview.md` |
| Chain + greeks combined | SubscribeOptionChainGreeks / GetLastQuoteOptionGreeksChain (§3) | `17-greeks-api-websocket.md` |
| Expiry/strike discovery | `GetExpiryDates` (weekly NFO expiries listed), `GetStrikePrices`, `GetOptionTypes` (`XX` futures placeholder, `FF`, `CE`, `PE`) | `13-rest-instruments-metadata.md`; `08-websocket-instruments-metadata.md` |

---

## 5. Segments / exchanges covered

Supported-exchanges table, verbatim (`02-introduction-getting-started.md`, last updated 2025-05-16):

| Exchange | API code | Update frequency | Evidence |
|---|---|---|---|
| NSE CM (cash/stocks) | `NSE` | "Realtime (L1)" | table row, verbatim |
| NSE Indices | `NSE_IDX` | "Realtime (L1)" | table row |
| NSE F&O | `NFO` | "Realtime (L1)" | table row |
| NSE Currency Derivatives | `CDS` | "Realtime (L1)" | table row |
| MCX | `MCX` | "Realtime (L1)" | table row |
| BSE CM | `BSE` | "Realtime (L1)" | table row |
| BSE Indices | `BSE_IDX` | "Realtime (L1)" | table row |
| BSE F&O | `BFO` | "Realtime (L1)" | table row |
| NCDEX | `NCX` | **"Delayed (5min)"** | table row; NCDEX absent from the API pricing sentence — realtime API access **UNVERIFIED (U-24)** |
| BSE Debt | `BSE_DEBT` | in the enum only — **UNVERIFIED as a subscribable data segment (live probe: GetInstruments BSE_DEBT)** | GetHolidays sample: `"Exchanges":"MCX;NSE;NSE_IDX;NFO;CDS;BSE;BSE_IDX;BSE_DEBT;BFO;NCX"` (`08-…`/`13-…`) |

Notes (all evidenced):
- Per-key entitlement: "Data for requested exchange is disabled." diagnostic (`03-fields`); GetLimitation `AllowedExchanges[{AllowedInstruments, DataDelay, ExchangeName}]` (`09-…`/`14-…`).
- Index identifiers = display names with whitespace: "Use InstrumentIdenfier value 'NIFTY 50', 'NIFTY BANK', 'NIFTY 100', etc. — Indices Symbols have white space" (`23a`); URL-encode in REST ("NIFTY 50 → NIFTY%2050", `10-rest-connection.md`). Index quote rows carry `BuyPrice/BuyQty/SellPrice/SellQty/TotalQtyTraded/Value/OpenInterest = 0` (`23a` NIFTY 50 sample) — and `LastTradeTime` advances every second despite `TotalQtyTraded:0` (`23a` ST/LTT 1593422508→511): **for indices LTT is the value-update time, not a trade time**; NSE_IDX also appears to emit trade-lessly every second while a quiet NSE stock skips seconds (see verdict residual (a)). GetExchangeSnapshot needs `nonTraded="true"` for NSE_IDX (`07-…`/`12-…`).
- **INDIA VIX: never named anywhere in the corpus.** `GetProducts` on NFO returns `{"Value":"INDIAVIX"}` (VIX derivatives exist as an NFO product — `docs/gdf-ref/06`); the NSE_IDX index-value identifier string is **UNVERIFIED (live probe: GetInstruments NSE_IDX dump / GetLastQuote "INDIA VIX")**.
- Symbol formats: long (`FUTIDX_NIFTY_25AUG2022_XX_0`), short (`NIFTY25AUG22FUT`), continuous (`NIFTY-I/-II/-III` — "It will never expire", `24-…`); BFO long uses `IF_/SF_/IO_` prefixes + `FF` option-type placeholder (`03-fields`); NSE non-EQ series suffix `RELCAPITAL.BE` (`23a`); instrument types `FUTIDX/FUTSTK/OPTIDX/OPTSTK/FUTCUR/OPTCUR/FUTCOM/OPTFUT` (`12-…`, `24-…`).

---

## 6. Bars / candles

| Function | Detail | Evidence |
|---|---|---|
| `SubscribeSnapshot` (WS push of just-closed bars) | Periodicity `["MINUTE"]/["HOUR"]` only; Period `1,2,5,10,15,30 default = 1`. Returns "Exchange, InstrumentIdentifier (Symbol), Periodicity, Period, LastTradeTime, Open, High, Low, Close, TradedQty, OpenInterest" → `RealtimeSnapshotResult`. "pushes market data every minute (and other 'Period' values as chosen by user)" | `05-websocket-subscribe.md`; `02-introduction-getting-started.md`; `19-delayed-api.md` (response shape) |
| Bar timestamp | **bucket-START stamp**: "If user sends request for 1minute snapshot data at 9:20:01, he will get record of trade data between 9:19:00 to 9:20:00 … In both cases, timestamp of the returned data will be start timestamp of the period." | `07-websocket-snapshot-history.md` (GetSnapshot) |
| Zero-trade buckets | OMITTED: "if a user subscribes for snapshot data of any symbol for a period and if no trades happen during that period, no data is sent by the server" | `07-websocket-snapshot-history.md` |
| `GetSnapshot` (pull) | latest bar, **max 25 symbols/call**, MINUTE/HOUR | `07-…`/`12-…` |
| `GetExchangeSnapshot` (pull, whole exchange) | periodicity "Minute, Hour, Day"; period 1,2,5,10,15,30; optional instrumentType filter; From/To ("Maximum 5 snapshots can be requested with single request"); `nonTraded` flag | `07-…`/`12-…`; pricing gloss "snapshot data of entire Exchange after every few minutes" (`25-…`) |
| `SubscribeExchangeSnapshot` (WS push, whole exchange) | added "10th October 2025" | `31-release-notes.md` |
| `StreamAllSnapshots` (firehose bars) | "It will send Realtime Snapshot data of all symbols with respective to the Exchange, Periodicity and Period, which are enabled for the API key." → `RealtimeSnapshotCollection` | `15-streaming-api.md` |
| Native periodicities overall | "Tick, 1-2-3-4-5-10-15-30 minutes, 1-2-4-6 hours, Daily, Weekly, Monthly" (GetLimitation enum adds Minute_6/10/12/20, Hour_3/8/12) | `01-overview.md` matrix; `09-…`/`14-…` GetLimitation |
| Candle-construction honesty | vendor-built from the 1s feed: "each data vendor constructs intraday candles using their own tick aggregation and filtering logic"; "the total traded volume we receive matches the exchange day volume" | `26-faq.md` |

---

## 7. Historical data

| Item | Detail | Evidence |
|---|---|---|
| `GetHistory` (WS + REST) | Periodicity `[TICK]/[MINUTE]/[HOUR]/[DAY]/[WEEK]/[MONTH]`, default TICK; Period numeric; From/To epoch-seconds (`From:0` → "entire possible history"); `Max` (0 = all); `UserTag`; `IsShortIdentifier`; **`AdjustSplits` default true** ("pass [AdjustSplits=false] then data will return with non-adjusted values"). Rows NEWEST-first | `07-websocket-snapshot-history.md`; `12-rest-snapshot-history.md`; `docs/gdf-ref/08` |
| OHLC row fields | "LastTradeTime, QuotationLot (LotSize), TradedQty, OpenInterest, Open, High, Low, Close" → `HistoryOHLCResult` (no per-bar VWAP/turnover) | `07-…`; full sample `23a` |
| TICK row fields | `{LastTradeTime, LastTradePrice, QuotationLot, TradedQty, OpenInterest, BuyPrice, BuyQty, SellPrice, SellQty}` — historical "ticks" carry best bid/ask; rows at 1-second-or-coarser spacing with gaps (the archived 1/sec conflated feed) | `32-python-library-ws-gfdl.md` sample; `19-delayed-api.md` `HistoryTickResult` sample |
| Backfill depth — Tick | "Every 1 Sec — **1 Calendar Week**" — but ONLY for the 6 documented segments (NSE CM, NFO, CDS, BSE CM, BFO, MCX — the page has exactly six backfill tabs); **NO backfill table exists for NSE_IDX / BSE_IDX / NCX — index-segment history depth (the segments tickvault needs for NIFTY/SENSEX index values) is UNVERIFIED (live probe: GetLimitation `HistoryLimitation` + a GetHistory probe on "NIFTY 50")** | `02-introduction-getting-started.md` backfill tables, verbatim (tabs: NSE CM / NFO / CDS / BSE CM / BFO / MCX only) |
| Backfill depth — intraday | Minute 1,2,3,4 → 3 calendar months; Minute 5,6,10,12 → 4.5 months; Minute 15,20,30 → 6 months; Hour 1,2,3,4,6,8,12 → 6 months | same tables |
| Backfill depth — EOD | Day/Week/Month: NSE CM & CDS "Since 2010"; BSE CM "Since 2007"; NFO Futures(Continuous) "Since 2010"; BFO Futures(Continuous) "Since 2024"; MCX Futures(Continuous) "Since 2010" | same tables |
| Contract-wise (expired) EOD | NFO/BFO Futures(Contractwise) 3 calendar months; NFO/BFO **Options(Contractwise) 1 calendar month**; MCX Futures/Options(Contractwise) 1 calendar month | same tables |
| History for EXPIRED identifiers | Masters list them (`OnlyActive=false`: "Function will return all (active + expired) instruments if value equals false" — `08-…`/`13-…`), but whether GetHistory SERVES them is unstated — **UNVERIFIED (live probe: GetHistory on an expired option identifier)**; contract-wise retention bounds it anyway | `08-…`; `13-…` |
| `GetHistoryAfterMarket` | "returns historical data (Tick / Minute / EOD) till previous working day … To receive current day's historical data via this function, you will need to send request after market is closed." **Mutually exclusive with GetHistory per key**: "If GetHistory is enabled → GetHistoryAfterMarket will be disabled" | `07-…`/`12-…`; `26-faq.md` |
| Per-key history caps | GetLimitation `HistoryLimitation{TickEnabled, DayEnabled, WeekEnabled, MonthEnabled, MaxEOD (e.g. 100000), MaxIntraday (e.g. 44 or 5), MaxTicks (e.g. 5/2/7), Hour_1..12Enabled, Minute_1..30Enabled}` (−1 = unlimited) — **MaxIntraday/MaxTicks UNITS undocumented (presumed days) — UNVERIFIED (live probe: GetLimitation on our key day 0 + compare against actual served ranges)** | `09-…`/`14-…` |
| Timestamp dialect | WS = epoch seconds; REST samples mixed 10/13-digit (ms-shaped values USUALLY end in `000`; a real-ms sample `1415114727418` exists — the repo's `docs/gdf-ref/README.md` R#15 softened "always `…000`" to "usually" on exactly this evidence) — **UNVERIFIED (live probe: unit per function/format)** | `11-rest-quotes.md` samples (`1415114727418`, `1391863200`); `06-websocket-quotes.md` samples (`1777888801`) |
| Delayed variants | GetHistory/GetSnapshot/GetExchangeSnapshot exist for delayed keys; delay is per-exchange per-key backend config (1/2/5/10/15/30 min; GetLimitation `DataDelay`); "there is no difference in the fields returned … The only difference is the timing" | `19-delayed-api.md`; `26-faq.md` |
| REST transport | HTTP GET only ("HTTP POST request or any other type of request WILL NOT work"); JSON default, `&xml=true`, `&format=CSV`; text/plain diagnostics can replace JSON; auth warm-up: "Authentication request received. Try request data in next moment." → retry loop | `10-rest-connection.md`; `24-code-samples-rest.md` |

---

## 8. Reference data

| Item | Detail | Evidence |
|---|---|---|
| Instrument master | `GetInstruments` (whole-exchange dump; filters InstrumentType/Product/Expiry/OptionType/StrikePrice/Series/showDummyISIN/showETF/showInterOperable/OnlyActive/detailedInfo) + `GetInstrumentsOnSearch` (max 20 rows) | `08-websocket-instruments-metadata.md`; `13-rest-instruments-metadata.md` (last updated 2025-07-09) |
| Instrument record fields | "Identifier (Symbol), Name (Instrument Type), Expiry (Expiry Date), StrikePrice, Product, OptionType, ProductMonth, TradeSymbol (ShortIdentifier…), QuotationLot (Lot Size), When detailedInfo=true … TokenNumber (Token number of Symbol), LowPriceRange (Lower circuit limit), HighPriceRange (Upper circuit limit), ISIN code, Series, High52Week, Low52Week." Samples also carry PriceQuotationUnit, UnderlyingAsset, UnderlyingAssetExpiry, IndexName, Description, IsActive (XML) | `08-…` GetInstruments "What is returned?", verbatim |
| Metadata helpers | `GetExchanges, GetInstrumentTypes, GetProducts, GetExpiryDates, GetOptionTypes, GetStrikePrices, GetHolidays` (year-mandatory; Date/Description/Exchanges; from 2010) | `08-…`; `13-…` |
| Market/session events | `GetMarketMessages` (session open/close: `{ServerTime, SessionID, MarketType}`) + `GetExchangeMessages` (exchange circulars free-text) | `09-websocket-server-info-messages.md`; `14-…` |
| Downloadable symbol lists | per-segment ZIPs on the site (Symbol List table, updated 06 May 2026) | `02-introduction-getting-started.md` |
| Derived analytics | `GetTopGainersLosers`/`SubscribeTopGainersLosers` (Count ∈ 5/10/15/20, Series filter; full detailed-quote rows) + `GetVolumeShockers` ("TTQ > SMA(TTQ,5)"; returns "InstrumentIdentifier, Description, LastTradeTime, TTQ, TTQ1Week, TTQChangeTimes, LastTradePrice, PriceChange, PriceChangePercentage") | `20-gainers-losers-volume-shockers.md`; `31-release-notes.md` (GetVolumeShockers 11 Sep 2025) |
| Corporate actions / fundamentals | **SEPARATE paid product** — Fundamental Data API at docs.globaldatafeeds.in (~30–50 endpoints: `GetCorporateActions` (EOD; dividends/splits/mergers/buybacks with ex-dates/record-dates/paymentDate), `GetCorporateAnnouncements` ("Continuous (1 Min)") + WS pushes `SubscribeCorporateAnnouncements`/`SubscribeFinancialResults`, `GetFinancialResults`/`GetFinancialRatios` (type = Realtime/EOD/Quarter; "30 Custom Financial Ratios"), `GetSHP` shareholding, `GetBulkDeals/GetBlockDeals`, `GetDeliveryVolumes`, `GetFIIDII`, `GetBhavCopyCM/FO`, `GetIndexDetail`, Index Constituents (offline monthly CSV: NSE 109 indices, BSE 70), circuit filters, banned securities, mcap, EOD stats). History mostly "30 Days" per module; separate accessKey ("User can get the accessKey … by contacting our Sales Team and making the payment") — same-key bundling **UNVERIFIED (sales)**. BSE-first rollout (17-03-25 soft launch), NSE endpoints added later — per-endpoint exchange coverage **UNVERIFIED (probe GetLimitation + GetExchanges on that key)** | `28-fundamental-data-api.md`, `28a`–`28i`, `28z`; `gfdl-fundamentals.md` notes |
| News | SEPARATE product — Stock Market News API at newsdocs.globaldatafeeds.in: `/api/news` (single company), `/api/news/multi` (≤20 identifiers), `/api/general/news`; "Continuous (1 Min)" update, "90 Days" history; fields incl. sentiment + long_summary; **webhook push** ("The server sends an HTTP POST request whenever a new article is published") | `29-news-api.md`; `29a` |
| In the market-data API itself | NO corporate-actions endpoint; only `AdjustSplits` on GetHistory (split-adjusted prices, opt-out) | `12-rest-snapshot-history.md` — absence Verified from function lists |

---

## 9. Plan tiers / packages

| Item | Detail | Evidence |
|---|---|---|
| Pricing | **UNPUBLISHED — quote-only, zero INR figures on the API pricing page**: "API is not one-size-fits-all kind of product … we have flexible pricing policy which is tailored to suit your needs. Do contact us" (sales@globaldatafeeds.in) | `25-pricing-purchase.md` (page updated 2026-07-11) |
| Per-exchange enablement | pay per segment; unpaid → "Data for requested exchange is disabled." | `03-fields` diagnostics; `23a` troubleshooting |
| Named data packages (13) | "RealTime · Historical · RealTime+Historical · OptionChain · OptionGreeks · OptionChain+OptionGreeks · Ondemand SnapShot · ExchangeSnapShot · GetHistoryAfterMarket · Delayed Snapshot · Streaming API · Special Functions · Helper Functions" — greeks = paid extra; Streaming API "Below function available individually" | `02-introduction-getting-started.md` package tables |
| Compare-APIs matrix (5 plans) | Realtime+Historical / OptionChain / Realtime / Historical / ExchangeSnapshot. Key rows: SubscribeRealtime YES only in Realtime+Historical / OptionChain / Realtime; GetHistory NO in Realtime-only; SubscribeSnapshot NO in Realtime plan; helper functions YES in all. ⚠ GetLastQuoteOptionChain + GetExchangeSnapshot show YES ONLY under "Exchange Snapshot API" (and NO under OptionChain) — looks like a vendor column-shift error — **UNVERIFIED (confirm package→function mapping with sales; verify per-key via GetLimitation)** | `25-pricing-purchase.md` matrix, reproduced verbatim |
| Symbol-count tiers | "as per package purchased by you, concurrent access of from 50 to 500 symbols will be made available to you. … you may send request to our server to stop data for any stocks and replace them with new stocks. This change happens in realtime without any manual intervention (Subscribe & Unsubscribe mechanism)" | `02-introduction-getting-started.md` (symbol-availability page) |
| Delayed keys | per-exchange configured delay (1/2/5/10/15/30 min); realtime subscribe on a delayed key → "RealtimeSubscription disabled for delayed data." | `26-faq.md`; `03-fields` |
| Entitlement oracle | `GetLimitation`: GeneralParams{AllowedBandwidthPerHour/Month, AllowedCallsPerHour/Month, ExpirationDate, Enabled}; AllowedExchanges[{AllowedInstruments, DataDelay, ExchangeName}]; AllowedFunctions[{FunctionName, IsEnabled}]; HistoryLimitation{…} (−1 = unlimited is an **Assumed inference** — no doc literal states it; samples use −1 pervasively, per `docs/gdf-ref/README.md` R#23) | `09-…`/`14-…` |
| Licensing | "APIs available here are only for Individual Users for Personal Use. For Commercial Use, you will need to sign agreement with respective Exchanges and pay them the requisite fees." T&C: single-subscriber, no redistribution, no gaming/virtual-trading without NSE & SEBI approval, no refunds | `25-pricing-purchase.md`; `01-overview.md` |
| Non-display (algo) entitlement | **UNSTATED anywhere** (zero "non-display" hits in the 215-page crawl) despite "Power your algo-trading with our time-tested APIs" marketing — **UNVERIFIED (U-5 — ask sales in writing before any order-driving use)** | `docs/gdf-ref/01` reconciliation; https://globaldatafeeds.in/apis/ |
| Free trial | offered per API family ("Take a Free Trial") — duration unpublished, **UNVERIFIED (U-3, sales)** | `gfdl-web.md` §9 (RUNNER-DOC) |
| GetHistory vs GetHistoryAfterMarket | only ONE enabled per key | `26-faq.md` |
| Price anchors (context only, NOT the API) | Desktop NimbleDataProPlus NSE-only ≈ ₹2,775/mo, all-exchange ≈ ₹9,340/mo (tradersgurukul.com, third-party); "Rs.233/- per additional computer per month" (globaldatafeeds.in/nimbledatapro/) — Assumed/single-sourced | `gfdl-web.md` §9 |

---

## 10. Limits

| Limit | Value / behavior | Evidence |
|---|---|---|
| WS sessions | **1 active session per API key**: "WebSockets API allows only 1 active session with the server with 1 API Key. If you create another session with same key, previous session will be invalidated with response 'Access Denied. Key already in use by other session.'" (also DotNet & COM). NEW connection kills the OLD. Multi-session purchasability **UNVERIFIED (sales)** | `04-websocket-connection-auth.md`; `25-pricing-purchase.md` |
| Symbols per key | package-based 50–500 concurrent (per-exchange `AllowedInstruments`; −1 = presumed-unlimited on some keys — Assumed per `docs/gdf-ref/README.md` R#23; observed 2018 key: NSE 160 + NSE_IDX 40 + 200 each NFO/CDS/MCX). Exceed → "Reached instrument limitation." / "To use new symbols, please reduce no. of used symbols." Free-form realtime subscribe/unsubscribe swap | `02-…` symbol-availability; `26-faq.md`; `03-fields`; `docs/gdf-ref` observed key |
| Requests/hour | key-configured: "this limit could be 1800, 3600, or 7200 requests per hour"; resets each clock hour; **subscribe AND unsubscribe EACH consume quota**: "subscribing to 100 symbols … will consume 100 requests. If you then unsubscribe … and subscribe to a different set of 100 symbols, total 300 requests" | `26-faq.md`, verbatim |
| Monthly + bandwidth caps | `AllowedCallsPerMonth` + `AllowedBandwidthPerHour/Month` (the archived `09`/`14` doc samples all show `-1`; the 5,356,800/month figure is an observation from a 2018 key recorded in `docs/gdf-ref/README.md` R#24, NOT a doc sample); diagnostics "Bandwidth per hour is limited." / "Bandwidth limitation is reached." / "Calls per hour are limited." / "Calls limitation is reached." | `09-…`/`14-…`; `03-fields`; `docs/gdf-ref/README.md` R#24 |
| Per-call batch caps | 25 symbols: GetLastQuoteArray*/GetSnapshot/GetSnapshotGreeks/GetLastQuoteArrayOptionGreeks; 20 rows GetInstrumentsOnSearch; 5 snapshots per GetExchangeSnapshot From/To; GainersLosers Count ∈ {5,10,15,20} | `06/07/12/16/17/18/20` function pages |
| History caps | per-key HistoryLimitation (MaxEOD/MaxIntraday/MaxTicks — units undocumented, **UNVERIFIED**) + §7 global backfill windows | `09-…`/`14-…` |
| IP restriction | optional per key: "IP address not allowed. \| The key used is configured to be used with specific Ips" | `03-fields` |
| Maintenance windows | 02:00–02:30 AM + 08:00–08:30 AM IST (BOD sometimes till 08:45) — "Connection refused"; "please connect after 8:45AM" → daily boot/cron must connect after 08:45 IST | `26-faq.md` |
| Transport / TLS | WS: plain `ws://endpoint:port` (per-account endpoint+port issued by GFDL; telnet example `ws://test.lisuns.com:4575`); **no `wss://` anywhere in the corpus — WS API key crosses the network in cleartext; WS TLS availability UNVERIFIED (U-2, sales)**. REST: `http://endpoint:port/function/?accesskey=…` — accessKey in the URL query string on EVERY call (token-in-URL surface REGARDLESS of scheme). ⚠ REST scheme is MIXED in-corpus: 45 `http://endpoint` examples vs SIX first-party `https://endpoint:port` examples on the NEWER pages (`12-rest-snapshot-history.md`:103/109 GetHistory-AdjustSplits; `19-delayed-api.md`:426; `20-gainers-losers-volume-shockers.md`:215/249 incl. GetVolumeShockers, a 2025 function; `13-rest-instruments-metadata.md`:191, detailedInfo page updated 2025-07-09) — documentary evidence TLS may already be available on the REST endpoint (fold into the U-2 probe, expected-likely-yes for REST; the WS side stays cleartext-only in every example) | `26-faq.md`; `04-…`; `10-rest-connection.md`; `12-…`; `13-…`; `19-…`; `20-…` |
| Reconnect | full re-auth + re-subscribe every reconnect; NO resume protocol | `04-…` flow; `23a` sample comment |
| VM/server-OS policy | `AllowVMRunning`/`AllowServerOSRunning` flags pushed post-auth — semantics **UNVERIFIED (sales: can our key run on AWS?)**; FAQ separately advises "Please DO NOT choose instances with burstable CPU types (for example T2, T3 on AWS)" (⚠ tickvault prod is t4g — burstable; vendor sizing guidance, not a protocol limit) | `23a` transcript; `26-faq.md` |
| Uptime marketing | "With an uptime of 99.995%, we handle 50 million data requests daily … since 2010"; "data spans all symbols (currently 50000+)" — marketing, no independent measurements exist (U-19) | introduction-to-apis (RUNNER-DOC) |

---

## Consolidated UNVERIFIED / live-probe queue

| # | Item | What the probe must check |
|---|---|---|
| 1 | Sub-second timestamps | Wire capture of RealtimeResult: confirm NO ms field (expected absent; every doc says epoch seconds) |
| 2 | Emit-on-change vs unconditional 1/sec (per segment) | Watch a liquid + an illiquid symbol across no-trade seconds on SubscribeRealtime. Doc-corpus expectation to confirm (23a transcript): NSE_IDX emits every second even trade-less (NIFTY 50, TTQ=0 at ST 508–511); a quiet NSE stock SKIPS seconds (BAJAJ-AUTO: 507→510) — confirm on live wire (doc samples may be curated) |
| 3 | StreamAllSymbols cadence/entitlement | Batch sizes, per-symbol cadence under load, whether it is a distinct endpoint/entitlement (U-1) |
| 4 | Echo heartbeat | Cadence; consequence of never responding |
| 5 | SubscribeOptionChain / SubscribeRealtimeOptionChain push interval | Timestamp deltas on a subscribed chain window |
| 6 | INDIA VIX identifier under NSE_IDX; BSE_DEBT usability | GetInstruments NSE_IDX + BSE_DEBT dumps; GetLastQuote "INDIA VIX" |
| 7 | History for expired identifiers | GetHistory on an expired option (OnlyActive=false discovery exists) |
| 8 | GetLimitation MaxIntraday/MaxEOD/MaxTicks units | Compare stated caps vs actually served ranges |
| 9 | wss:// (TLS) availability | Ask sales / probe TLS on the issued endpoints (U-2). WS: no `wss://` anywhere in the corpus — cleartext key otherwise. REST: probe `https://` on the issued REST endpoint — expected LIKELY YES (six first-party `https://endpoint:port` examples on the newer pages: `12`:103/109, `19`:426, `20`:215/249, `13`:191) |
| 10 | >1 concurrent session purchasable | Sales question (docs: hard 1-session-per-key) |
| 11 | FIX tier: true exchange TBT? depth? pricing? | Sales question in writing (U-4) |
| 12 | Non-display/algo licensing | Sales question in writing (U-5) |
| 13 | TotalBuyQty/TotalSellQty ever non-zero | Live GetLastQuote sampling |
| 14 | IV unit scaling (fraction vs percent) | Live greeks sample vs NSE option chain |
| 15 | REST timestamp unit per function/format | Live REST GetLastQuote JSON vs CSV |
| 16 | Pre-open field semantics (U-15) | 09:00–09:08 IST session capture |
| 17 | Compare-APIs matrix column integrity | GetLimitation AllowedFunctions on our purchased key |
| 18 | Per-key symbol/request/history values for OUR key | GetLimitation on day 0 |
| 19 | Fundamental/News production hosts, bundling with the market-data key, per-endpoint NSE-vs-BSE coverage | Sales + GetLimitation/GetExchanges on those keys |
| 20 | Per-symbol every-second delivery under burst (U-17) | Full-universe capture during a volatile open |
| 21 | Index-segment (NSE_IDX / BSE_IDX / NCX) history depth | NO backfill table exists for these segments in `02` (only 6 tabs: NSE CM/NFO/CDS/BSE CM/BFO/MCX) — GetLimitation `HistoryLimitation` on our key + a GetHistory TICK/MINUTE/DAY probe on "NIFTY 50" |

---

## Comparison vs incumbent feeds (context row)

| Feed | Push granularity | Timestamp precision | Payload per update | Depth | Notes |
|---|---|---|---|---|---|
| **GFDL (this catalog)** | 1 update/sec/symbol, full-image conflated L1 snapshot ("pushes market data every second (Bid/Ask/Trade)" — `05-…`; "This is L1 data with single best Bid & Ask details" — `25-…`) | epoch SECONDS, whole-second only ("no. of seconds since Epoch time" — `05-…`) | 23-field full quote: LTP/LTQ + best bid/ask + day OHL + prev-close + VWAP + cumulative volume/turnover + OI + OI-change + lot + PreOpen + change/% — OI INLINE every second | NONE (L1 only) | 1 session/key; ws:// cleartext; greeks/chain/history as separate paid packages; FIX tier claims TBT (UNVERIFIED) |
| **Dhan WS** (retired live feed, for context) | ~2–4 conflated updates/sec/symbol (sampled stream; measured delivery lag p99 46s on 2026-07-06 — `websocket-connection-scope-lock.md` §E) | whole-second LTT (IST epoch seconds; ≥1s quantization floor — `live-market-feed.md`) | binary packets: Ticker 16B (LTP+LTT) / Quote 50B (adds ATP, volume, day OHLC, totals) / Full 162B (adds OI + 5-level depth); OI on a separate 12-byte code-5 packet in Quote mode | 5 levels in the Full packet | GFDL is SLOWER cadence than Dhan's 2–4/sec but richer per-message (OI + change fields inline) and its ServerTime enables per-message lag measurement |
| **Groww WS** (retired live feed, for context) | per-LTP-event push, ms-timestamped, price-only (`{ltp, tsInMillis}` — `groww-second-feed-scope-2026-06-19.md` §36.2; measured delivery p99 562 ms — scope-lock §E) | **milliseconds** | LTP + ts only — no volume, no OI, no bid/ask, no day OHLC | NONE | Groww beats GFDL on timestamp precision + latency by ~3 orders of magnitude but carries ~1/10th the field content; GFDL beats Groww on field richness (OI, volume, bid/ask, OHLC) at a strictly coarser 1s cadence |

**Bottom line for tickvault:** GFDL ≈ "a 1 Hz full-image L1 quote feed with OI inline" — coarser than both Dhan (2–4 Hz sampled) and Groww (event-driven ms LTP) in cadence, but the richest per-message field set of the three; nothing in it is tick-by-tick, and nothing is sub-second, on any documented plan.
