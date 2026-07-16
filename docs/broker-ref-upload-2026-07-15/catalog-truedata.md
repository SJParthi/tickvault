# TrueData — Complete Provider Data Catalog

> Synthesized 2026-07-16 from TWO read-only research passes:
> **[DOCS]** = doc-archive notes (`notes/truedata-docs.md`, built from
> `broker-docs/docs/broker-ref-upload-2026-07-15/truedata/` — archive files `00-INDEX.md` …
> `13-official-announcements-telegram.md`, each citing its live URL) and
> **[WEB]** = live-web notes (`notes/truedata-web.md` — PyPI pages + official sample-code repo
> fetched OK; truedata.in / feedback.truedata.in pages 403-blocked, quoted via Google-indexed
> snippets = NEAR-verbatim, re-confirm from a residential IP).
> Every row carries an Evidence cell (verbatim quote + source). Anything not provable is marked
> **UNVERIFIED (live probe needed)** with the exact probe to run.
> Archive caveat (00-INDEX.md, verbatim): the raw-protocol PDF ("TrueData Market Data API
> Documentation v2.1/v2.2") is email-distributed and gated — "exact raw JSON key names for
> subscribe requests are only in that PDF."

---

## ⚡ HEADLINE VERDICT — Push WS granularity

**Verdict: CONFLATED ~1-second L1 snapshot stream, NOT true tick-by-tick.**
**Scope caveat: this is an NSE-F&O-ANCHORED verdict** — the structural evidence (rows 2–4
below) is NSE, and row 2 is F&O-SEGMENT-specific (NSE F&O Snapshot MDR, NSE co-lo TBT rule,
marketcalls' NSE 1-sec statement). NSE EQ/indices are INFERRED (row 3 only excludes TBT —
intermediate non-colo broadcast products are not ruled out), and MCX/BSE are inferred as the
same vendor-feed class; per-segment **UNVERIFIED** (row 10 + probe #1). BSE is also
the "L1" exception — its depth is L2 top-5 (see Category 2). Delivery model: per-update
(analyst reading of the SDK event wording + `tick_seq` — [WEB] notes §HEADLINE, **not a vendor
statement**), so >1 msg/sec/symbol is possible, but NOT every exchange trade. Marketing says
"tick-by-tick"; the feature grid + independent evidence + NSE's own vendor-feed structure say
1-sec conflated snapshot. No per-plan or per-segment rate statement exists anywhere in the
corpus — rate per plan/segment **UNVERIFIED — live probe: count msgs/sec on a liquid NIFTY
future + option during open, check whether distinct trades between snapshots appear, and
inspect `tick_seq` gaps.**

| # | Evidence line (verbatim) | Direction | Source |
|---|---|---|---|
| 1 | "**Streaming Live Data API — L1 data @1sec frequency with single best bid/ask details**" | 1-sec conflated | truedata.in/products/marketdataapi (via 01-overview.md [DOCS] + web snippet [WEB]) — the ONLY explicit frequency number in the whole corpus |
| 2 | "MARKET FEED Futures and Options (FO) **(Realtime Snapshot)** Version: 1.2" | NSE **F&O** vendor snapshot product EXISTS (title evidence only — F&O-segment-specific); which NSE product TrueData actually consumes, and the NSE-EQ/indices vendor-feed class, are INFERRED — covered by probe #1 | nsearchives.nseindia.com/.../Snapshot_MDR_RT_FAO_V1.2.pdf [WEB] |
| 3 | "Tick-by-tick data is available only at NSE co-lo servers. The TBT data is not available at TAP Server or through DotEx for further broadcast." | true TBT not redistributable to ANY vendor | nseindia.com/static/market-data/real-time-data-subscription (snippet) [WEB] |
| 4 | "Almost all data vendors … get the same legacy netted consolidated 1-second snapshot data feed … (it is not true tick data but consolidated snapshot data)" — article names "Globaldatafeeds, **Truedata**, Esignal" | independent 1-sec verdict naming TrueData — **undated third-party snippet (the vendor-quality article class dates to the mid-2010s): corroborating, not current-state proof; weight accordingly and re-confirm the article date from a residential IP** | marketcalls.in/softwares/data-vendors-and-quality-of-datafeeds.html (snippet, UNKNOWN DATE) [WEB] |
| 5 | "The Tick by Tick Feed is provided by NSE and consists of each and every order or a change in the order" — presented as a SEPARATE NSE product class distinct from the L1/L2 TrueData distributes | TrueData's own blog concedes TBT is a different product | truedata.in/blog/levels-real-time-data-nse-bse-mcx (snippet) [WEB] |
| 6 | "Live tick-by-tick data with ultra-low latency"; "**Full-Precision Ticks** — every price movement captured" | marketing counter-claim (contradicts #1) | truedata.in/market-data-apis (01-overview.md) [DOCS] |
| 7 | "Whenever the trade happens or bid-ask changes, event will be called…" | SDK event-per-update wording (of what TrueData sends) | official .NET README (10-dotnet-sdk.md) [DOCS] |
| 8 | Tick field 15 = "Tick Sequence No"; sequence numbers exist "to track and confirm no ticks are lost" | proves lossless delivery OF WHAT IS SENT, not every-trade delivery | 03-realtime-websocket.md §5 [DOCS] + WS-doc snippet [WEB] |
| 9 | "15:30:00 to 15:30:59 > bar 15:30 (… there can be max 1 tick or even no tick in the last second …)" | inconclusive (market fact, not feed cap) | bar-formation KB, 03-realtime-websocket.md §6 [DOCS] |
| 10 | Per-segment conflation statement: NONE anywhere (NSE EQ vs F&O vs MCX vs BSE vs CDS) | **UNVERIFIED per segment** — probe each | [DOCS] headline item 8 |
| 11 | Tick timestamp precision (ms vs whole seconds): no doc states it | **UNVERIFIED** — probe: check sub-second digits on live `timestamp`/LTT | [DOCS] item 9, [WEB] §1 |

---

## Category 1 — Streaming tick/quote message (field-by-field)

Official 19-field wire order (KB verbatim: "Below are the fields included in Real-Time Tick
Streaming with TrueData Market API (WebSocket):" — 03-realtime-websocket.md §5 [DOCS];
identical list in feedback.truedata.in KB "Fields included in Real-Time Tick & 1 Min. Bar
Streaming" [WEB]).

| # | Wire field | Type/precision (as documented) | Meaning | Evidence |
|---|---|---|---|---|
| 1 | Symbol ID | numeric id | TrueData numeric symbol id | "Format >> Symbol ID, Date-Time (Timestamp), LTP, LTQ, ATP …" — 03 §5 [DOCS] |
| 2 | Date-Time (Timestamp) | **precision ms-vs-s UNVERIFIED (probe: sub-second digits on live tick)** | tick timestamp | same KB line [DOCS]; "No doc line found stating ms vs sec resolution" [WEB] §1 |
| 3 | LTP | price (float; truedata-ws 4.0.7 fixed a "32-bit float" precision bug) | last traded price | KB line [DOCS]; 08-python-sdk release note [DOCS] |
| 4 | LTQ | qty | last traded quantity | KB line [DOCS] |
| 5 | ATP | price | "Average Traded Price – for the day" (verbatim) | KB line [DOCS] |
| 6 | TTQ | qty (cumulative day) | "Total Traded Qty – Volume for the day" (verbatim) | KB line [DOCS] |
| 7 | Open | price | day open — **rides on EVERY tick** | KB line [DOCS]; .NET README: "Timestamp of Symbol, LTP, Volume, Open, High, Low, Close and OI" (10-dotnet-sdk.md) [DOCS] |
| 8 | High | price | day high (every tick) | same |
| 9 | Low | price | day low (every tick) | same |
| 10 | Prev Close | price | previous day close (no intraday "Close" field — LTP serves) | KB line [DOCS] |
| 11 | OI | int | open interest | KB line [DOCS] |
| 12 | Prev Open Int Close | int | previous day OI close | KB line [DOCS] |
| 13 | Day's Turnover | number | day turnover | KB line [DOCS] |
| 14 | Special Tag | enum `"OHL"`, `"H"`, `"L"`, `""` | tick made a new day Open/High/Low | KB line [DOCS] |
| 15 | Tick Sequence No | int | sequence number ("to track and confirm no ticks are lost" — WS doc snippet [WEB]) | KB line [DOCS] |
| 16 | Bid | price | best bid, inline in tick (L1) | KB line [DOCS] |
| 17 | Bid Qty | qty | best bid qty | KB line [DOCS] |
| 18 | Ask | price | best ask | KB line [DOCS] |
| 19 | Ask Qty | qty | best ask qty | KB line [DOCS] |

**SDK-computed extras** (Python `truedata` dataclass, verbatim from PyPI [WEB] + 07-python-sdk
[DOCS]): `change, change_perc, oi_change, oi_change_perc` — "computed by the library". "If
Bid/Ask is not enabled for the account, zeros are appended … 'to keep the structure intact'"
(truedata-ws 5.0.2 note, 03 §5 [DOCS]). Python tick objects expose `tick_type` (`1` = trade
packet vs bid-ask-only packet) and `.to_dict()`.

| Aspect | Value | Evidence |
|---|---|---|
| Wire encoding | JSON, **LZ4-compressed by default** (SDKs decompress transparently) | "all websocket messages are compressed for performance improvements by default" — Python truedata v7.0.0 note, 02-authentication-connection.md §7 [DOCS] |
| Message types on wire | `touchline, tick, greeks, bidask, bidaskL2 (BSE only), bar (1min and 5min), marketstatus, heartbeat` (Node event set, verbatim) + .NET markers `"TrueData"` (welcome), `"trade"`, `"bidask"`, `"HeartBeat"` | 03-realtime-websocket.md §3 [DOCS] |
| Touchline | snapshot on subscribe (+ auto post-market): "last updates / settlement updates / snap quotes … during market hours all of the fields of the touchline data already form part of the real time market data feed"; populates ~1 s after subscribe | 03 §8 [DOCS]. Exact touchline field list **UNVERIFIED** (doc page 403) [WEB] |
| Heartbeat | "Heartbeats arrive **every 5 seconds**" | 03 §3 [DOCS] |
| Market status | open/close messages stream as they happen (`marketstatus` event) | 03 §3 [DOCS] |
| Raw JSON key names | **UNVERIFIED — only in the gated v2.1/v2.2 PDF** (probe: capture raw frames live). Closest evidence: .NET `SnapData` model (SymbolId, Timestamp, LTP, Volume, Open, High, Low, PrevClose…) | 00-INDEX.md + 10-dotnet-sdk.md [DOCS] |
| WS auth | credentials in URL query: `wss://push.truedata.in:8082?user=<USER_ID>&password=<PASSWORD>` | 02 §2 [DOCS] |
| Full Market Feed | entire-exchange firehose on port 9084, subscription-gated: "Realtime Exchange Snapshot API — get live data of entire exchange segment in single call" | 03 §11 [DOCS]; sample code `live_port = 9084, full_feed = True` (github.com/Nahas-N/Truedata-sample-code, fetched) [WEB] |

## Category 2 — Market depth

| Aspect | Value | Evidence |
|---|---|---|
| NSE & MCX depth | **Level 1 ONLY — single best bid/ask** | "The data is Level 1 from NSE & MCX which has Best Bid Ask (Default) as part of the feed" — getting-started KB (01-overview.md) [DOCS]; "as an Authorized Data Feed Vendor of NSE and MCX, they provide only Level 1 Data" (truedata.in snippet) [WEB] |
| BSE depth | **Level 2 = top 5 levels**, incl. per-level trade count | "Market Depth API (Exclusive to BSE) \| Top 5 bid and ask levels, quantities, and traded volumes" (01-overview.md) [DOCS]; tuples `[(price_1, qty_1, no_of_trades_1), …]` length 5 both sides; Node event `bidaskL2` "only for BSE exchange"; BSEFO L2 added truedata-ws 5.0.1 (03 §7) [DOCS] |
| Delivery | STREAMING `bidask` message ("Whenever the trade happens or bid-ask changes, event will be called" — .NET README [DOCS]) AND inlined in tick fields 16–19 | 10-dotnet-sdk.md + 03 §5/§7 [DOCS] |
| Bidask message fields | `["timestamp","symbol_id","symbol","bid","ask","total_bid","total_ask"]` (`total_*` added truedata-ws 5.0.10 for L2); L1 tuple `[(price, qty)]` length 1 | 03 §7 [DOCS]; PyPI (fetched) [WEB] |
| Opt-out | "Bid/Ask: NSE & MCX L1 best bid/ask is part of the feed by default; **you can opt out** … to reduce feed volume"; "ability to enable or disable bid-ask headers" | 03 §4 [DOCS]; WS-doc snippet [WEB] |
| 20-level / full book | **NOT offered** per docs — blog describes L3 ("Level 3 provides market depth data up to 20 best bid and ask prices") as an exchange product class only; no API surface anywhere | truedata.in blog snippet [WEB]; "No 20-level or full-order-book depth exists anywhere in the docs" [DOCS] |
| Depth update frequency | no number stated → falls under the headline "@1sec L1" ambiguity — **UNVERIFIED (probe: bidask msgs/sec on a liquid symbol)** | [DOCS] cat-2 |

## Category 3 — Greeks / IV

| Aspect | Value | Evidence |
|---|---|---|
| Delivery | separate per-option-symbol WS message stream (NOT in ticks), **opt-in add-on** | "If subscribed to NSE options + greeks add-on, greek messages stream per option symbol" — 03 §9 [DOCS]; PyPI: "If user is subscribed to Nse options and also opt for option greek then whenever new greek data arrived" the callback fires [WEB] |
| Fields (verbatim) | `["timestamp","symbol_id","symbol","iv","delta","theta","gamma","vega","rho"]` | 03 §9 [DOCS] + PyPI (fetched) [WEB] |
| Enablement | "Option Greek streaming — Needs backend enablement"; "Requires NSE options + greeks opt-in"; `greek=True` flag (default false) | 07-python-sdk [DOCS]; PyPI [WEB] |
| Computed by whom | NOT STATED in any doc — [WEB] infers vendor-computed ("NSE does not broadcast greeks") but **UNVERIFIED** | 03 §9 [DOCS]; [WEB] §3 |
| Update cadence | NOT STATED — **UNVERIFIED (probe: greek msgs/sec per option symbol)** | both notes |
| "16+ matrices" | "Realtime Option Greeks API — IV, Delta, Theta, Vega, Gamma & 16+ matrices in a single call" — list never enumerated — **UNVERIFIED** | truedata.in/market-data-apis (01-overview.md) [DOCS] |
| Historical greeks | REST analytics `get_history_greeks(symbol, expiry, strike, series="CE", ltp=bool)` — historical or latest greeks for one contract | 05-analytics-optionchain-greeks.md §3 [DOCS] |
| Packaging | ".NET README: 'Analytics and Greeks — ADDON: Top Gainers / Losers, Industry Gainer / Losers, Options Greeks'" | 10-dotnet-sdk.md [DOCS] |

## Category 4 — Option chain

| Aspect | Value | Evidence |
|---|---|---|
| Mechanisms | TWO: streaming chain (WS-fed, SDK-assembled DataFrame) + one-shot REST snapshot | 05 §1/§3 [DOCS] |
| Streaming API | `start_option_chain(symbol, expiry, chain_length=10, bid_ask=False, greek=False)` — "chain_length: number of strikes pulled around the future price, default 10" | 05 §1 [DOCS]; PyPI (fetched, verbatim): "chain_length : number of strike need to pull with respect to future prices. default value is 10 (int)" [WEB] — the earlier web-note wording ("configurable strike range … default 10 strikes") was a paraphrase, corrected 2026-07-16 against the live page |
| Chain columns (verbatim) | `symbols, strike, type, ltp, ltt, ltq, volume, price_change, price_change_perc, oi, prev_oi, oi_change, oi_change_perc, bid, bid_qty, ask, ask_qty, iv, delta, theta, gamma, vega, rho` | 05 §1 [DOCS] |
| Update model | chain object updates continuously; poll `get_option_chain()` "callable anywhere, any frequency"; official sample polls every 5 s; "Chains work for tick and 1-min bar streaming subscriptions, and also pre/post market via touchline data" | 05 §1 [DOCS]; sample code (fetched) polls every 5 s with SBIN(20)/NIFTY(20)/BANKNIFTY(100)/CRUDEOIL(20) strikes [WEB] |
| REST snapshot | `TD_analytics.get_option_chain(symbol="NIFTY", expiry=d(2023,12,28), greeks=True)` — "One-shot full option chain (REST, not streaming)" | 05 §3 [DOCS] |
| Underlyings proven | NIFTY, BANKNIFTY, SBIN, CRUDEOIL (MCX); SENSEX/BANKEX (BSE — legacy `bse_option=True`); currency chains since truedata-ws 4.0.8 | 05 §1 [DOCS] |
| Full chain vs windowed | streaming chain is WINDOWED by `chain_length`; "entire option chain … in a single call" is the marketing claim — exact max chain_length / full-chain behavior **UNVERIFIED (probe: request chain_length beyond strike count)** | 01-overview.md + 05 §1 [DOCS] |
| Wire mechanism | dedicated chain messages vs SDK-side assembly from subscribed legs: NOT STATED — **UNVERIFIED** ([WEB] infers client-side assembly from live leg streams) | [DOCS] cat-4; [WEB] §4 |

## Category 5 — Segments covered

| Segment | Status | Evidence |
|---|---|---|
| NSE Equity | ✅ | "Real-Time Data and Historical Data via our Market Data API for NSE Equity, NSE Indices, NSE Futures & Options, NSE Currency Derivatives and MCX" — getting-started KB (01-overview.md) [DOCS] |
| NSE Indices (incl. **INDIA VIX**) | ✅ | same KB; "Use exchange spot-index names with spaces (verbatim): `NIFTY 50, NIFTY BANK, INDIA VIX, NIFTY IT`" — 06-symbols-master.md §1 [DOCS] (exact VIX symbol string flagged for master-download confirm in [WEB] §5) |
| NSE F&O | ✅ | same KB [DOCS] |
| NSE CDS (currency) | ✅ live | "NSE Currency Derivatives Segment (NSE CDS) Real time feed is now live over Websocket. Historical Data also enabled via REST." — Telegram msg 97 (13-official-announcements) [DOCS]; symbols `USDINR-I, EURINR-I, JPYINR210430FUT, …` |
| NSE interest-rate derivatives | masters exist | WS-API master lists `NSE_INTEREST_CONTINUOUS_FUTURES / NSE_INTEREST_OPTIONS` — 06 §2 [DOCS] |
| BSE EQ / Indices / F&O | ✅ (signup segment list; L2 feed; `_BSE` suffix; BSEFO L2 in truedata-ws 5.0.1) | "NSE EQ, NSE Indices, NSE F&O, NSE CDS, BSE EQ, BSE Indices, BSE F&O, MCX" — signup form (01-overview.md) [DOCS]; "NSE (Stocks, Indices, F&O & Currency), BSE (Stocks, Indices & F&O) and MCX" (truedata.in/price snippet) [WEB] |
| MCX (futures + options) | ✅ | same lists; MCX masters #12–14 (06 §2) [DOCS] |
| BSE debt / G-sec / mutual funds | Velocity (retail terminal) lists only — **WS-API availability UNVERIFIED** | 06 §3 [DOCS] |
| NCDEX / GIFT / SGX | no evidence | [WEB] §5 |
| Per-account gating | "Subscriptions streamlined at our backend … Please ensure that you are using the correct segments and ports assigned to you. Others may not work." | Telegram msg 129 (13) [DOCS] |

## Category 6 — Bars / candles

| Aspect | Value | Evidence |
|---|---|---|
| Streaming bar intervals | **1-min and 5-min only** (feed combos: ticks / 1min / 5min / tick+1min / tick+5min / tick+1min+5min / 1min+5min — all backend entitlements; ticks default) | 03 §4 [DOCS]; PyPI: "Live Data (Streaming 1 min bars) - Needs to be enabled" [WEB] |
| 1-min bar wire fields (verbatim) | "Format >> Symbol ID, Date-Time (Timestamp), bar_open, bar_high, bar_low, bar_close (LTP), bar volume, OI" | official KB, 03 §6 [DOCS] |
| Python bar dataclass (verbatim) | `["symbol","symbol_id","day_open","day_high","day_low","prev_day_close","prev_day_oi","oi","ttq","timestamp","open","high","low","close","volume","change","change_perc","oi_change","oi_change_perc"]` | 03 §6 / 07 [DOCS]; PyPI (fetched) [WEB] |
| Bar formation rule | "For data coming from: 09:15:00 to 09:15:59 > bar 09:15 … 15:30:00 to 15:30:59 > bar 15:30 (but the market closes at 15:30:00, so there can be max 1 tick or even no tick in the last second resulting in no 15:30 bar)"; single-tick bar ⇒ O=H=L=C ("OHLC == LTP at 3:30 PM" FAQ) | 03 §6 + 11-limits-errors-faq.md §4 [DOCS] |
| Bar delivery latency | completed bar for "all subscribed symbols simultaneously within 1 second of bar completion" — official recommendation for 200+ symbol latest-bar needs ("at the end of every minute you get the last bar for all 200+ symbols in a streaming fashion and without calling the server") | 03 §6 + 11 §6 FAQ 2 [DOCS] |
| REST bar intervals (verbatim enum) | `'1min','2min','3min','5min','10min','15min','30min','60min','EOD'`; Python adds `week/WEEK, month/MONTH` (truedata-ws 4.3.2, aggregated from EOD) + `tick` | 04-history-rest-api.md §3 [DOCS]; PyPI: "tick, 1 min, 2 mins, 3 mins, 5 mins, 10 mins, 15 mins, 30 mins, 60 mins, eod, week, month" [WEB] |
| Snapshot getters | REST `getLTP(symbol, bidask=1)`; `getLastNBars(symbol, nbars=200, interval='1min' OR 'EOD')`; `getLastNTicks(symbol, nticks=2000)` | 04 §2 [DOCS] |
| Compliance cap | "Only exchange-approved intervals may be offered: **Tick, 1-Minute, 2-Minute, 5-Minute, 15-Minute (Delayed), and End-of-Day (EOD)**" | 01-overview.md [DOCS] |

## Category 7 — Historical data

| Aspect | Value | Evidence |
|---|---|---|
| Tick history depth | "**Tick data**: last **5 trading days**" | 04 §5 / 11 §3 [DOCS]; KB snippet "Tick Data: Last 5 Trading Days" [WEB] |
| Intraday bar depth | "**Bar data (1/2/3/5/10/15/30/60 min)**: last **6 months**" | same |
| EOD depth | "**Daily (EOD) bars**: **10+ years**" | same |
| Extended history | "We are soon going to be enabling Extended History, which would be available as an Add-on" / "Extended History is being offered as an Add-on option" | 11 §3 [DOCS]; KB snippet [WEB] |
| Bar response shape | `timestamp, open, high, low, close, volume, oi` (sandbox table `Sr \| Timestamp \| LTP \| O \| H \| L \| C \| V \| OI`); JSON keys lowercase (`ltp`, `c`, `timestamp`) | 04 §4 [DOCS] |
| Tick response shape | `timestamp, ltp, volume/ltq (+ bid, bidqty, ask, askqty when bidask=1; + delivery volume when requested)`; sample: `get_historic_data(..., bar_size='ticks', bidask=True, delivery=True)` | 04 §4 [DOCS]; sample code (fetched) [WEB] |
| Historical bid/ask | "Historical Bid-Ask requires subscription (not active by default)" | 08-python-sdk-truedata-ws-legacy.md [DOCS] |
| EOD delivery volume | supported ("DelVolume" checkbox; `delivery=True`) | 04 §4 [DOCS] |
| Multi-symbol | ONE symbol per request: "Historical data needs to be requested and can be delivered 1 by 1. Currently, there is no method to send you the history of multiple symbols at the same time." | 11 §6 FAQ 1 [DOCS] |
| Time formats | `YYMMDDTHH:mm:ss` from/to, or `duration` (`'5D'/'3W'/'2M'/'1Y'`) | 04 §3 [DOCS] |
| Expired contracts | **NOT MENTIONED anywhere — UNVERIFIED (probe: query an expired option by contract symbol against history REST / ask sales)**; only continuity mechanism documented = continuous futures `SYMBOL-I/-II/-III` + "Continuous Futures & Adjusted Series — seamless long-term data continuity" | 04/01-overview.md [DOCS]; no evidence either way [WEB] §7 |
| Bhavcopy | `get_bhavcopy(segment)` for `'FO','EQ','MCX','BSEFO','BSEEQ'`; error text: "No complete bhavcopy found for requested date. Last available for 2021-03-19 16:46:00." | 04 §7 [DOCS]; sample code [WEB] |
| Market Replay | full-market-feed replay post-market at `replay.truedata.in:8082`, "no additional cost; for trial + paid users", "All code should work as is without any changes." | 03 §10 [DOCS] |
| Transport | History via WebSocket TERMINATED 30 Apr 2021 — REST-only since; REST auth = OAuth2 password grant `POST https://auth.truedata.in/token` → Bearer on `https://history.truedata.in/...` | 04 header + Telegram msg 96 [DOCS]; 02 §3 [DOCS] |

## Category 8 — Reference data

| Aspect | Value | Evidence |
|---|---|---|
| Symbol masters | 17 WS-API plain-text lists at `https://www.truedata.in/downloads/symbol_lists/…` (NSE spot index, all NSE EQ, NSE continuous/contract futures, NSE indices/all options, interest & currency futures/options, MCX contract/continuous futures + options, BSE indices options, BSE index, all BSE EQ); programmatic: .NET `TDMaster` "To download latest master", maps `symbol ↔ symbolid` | 06-symbols-master.md §2/§5 [DOCS] |
| Symbol formats | options `<ticker><expiry:YYMMDD><strike><CE\|PE>` (weekly & monthly SAME format, e.g. `BANKNIFTY21031834500PE`); indices with spaces (`NIFTY 50`, `INDIA VIX`); continuous futures `SYMBOL-I/-II/-III`; contract futures `NIFTY19OCTFUT`; NSE equities plain ticker; "BSE equities carry the `_BSE` suffix" (`RELIANCE_BSE`) | 06 §1 [DOCS] |
| Corporate actions | REST `getCorpAction(symbol)`; .NET `GetCorporateActions("AARTIIND", true)` | 04 §2 + 10 [DOCS] |
| Symbol renames | `getSymbolNameChange()` | 04 §2 [DOCS] |
| 52-week high/low | .NET `Get52WeekHighLow("RELIANCE")` | 10 [DOCS] |
| Gainers/losers | REST `getTopGainers/getTopLosers/getTopVolumeGainers(topsegment ∈ NSEFUT, NSEEQ, NSEOPT, CDS, MCX; top=50)`; analytics add-on `get_oi_gainer_losers`, `get_index_gainer_losers` (e.g. `index_name="NIFTY 500"`), `get_industry_gainer_losers` | 04 §2 + 05 §3 [DOCS]; sample code `get_gainers/get_losers("NSEEQ", topn=25)` [WEB] |
| News / fundamentals | separate paid product "Corporate Data API (TrueWealth)": "real-time corporate announcements, AI-powered disclosure analysis, financial results & ratio insights, shareholding patterns, corporate actions, company news feeds; fundamental & technical screeners 'coming soon'" — "no public endpoint docs; sales-gated" — **endpoint surface UNVERIFIED** | 05 §4 + 00-INDEX.md gap 4 [DOCS] |
| Mutual Fund API | "contact via ticket … no public docs" — **UNVERIFIED** | 12-velocity-external-api.md [DOCS] |

## Category 9 — Plan tiers / included vs paid extra

| Aspect | Value | Evidence |
|---|---|---|
| Public price list | none on docs; "Pricing depends on exchange selection (NSE/BSE/MCX), number of symbols/instruments, and type of data (Streaming, Charting, Corporate)." | 01-overview.md [DOCS] |
| Feed tiers (web) | "Rs. 799 per month for 1-minute data (per additional segment) and Rs. 1,499 per month for Tick data (per additional segment)" — tick vs 1-min sold as SEPARATE tiers, priced per segment. **CAVEAT: `/price` is assessed by the doc archive as Velocity RETAIL pricing, not the API** ("Velocity retail pricing; API pricing is quote-based (per 01)" — SOURCES.md; 01-overview.md: "No public price list for APIs"), so the API tier structure is INFERRED from a retail page — confirm with sales | truedata.in/price snippet [WEB] — full grid **UNVERIFIED** (page 403); SOURCES.md [DOCS] |
| Reseller pricing datapoint | "Real Time Tick Streaming API is Rs.1599 + GST and Real Time + Historical Data API is Rs. 2599 + GST, with 200 symbols provided in the default subscription." — **of UNKNOWN DATE** (undated Google-indexed snippet; may be stale) | fyers.in/notice-board/revision-in-truedata-subscription-pricing/ snippet [WEB] |
| Trial | "Up to 10 days free trial; a paid trial of another 10 days possible." | 01-overview.md onboarding [DOCS] |
| Default-ON | "Live data (Streaming Ticks) — **enabled by default**"; "Bid/Ask: NSE & MCX L1 best bid/ask is part of the feed by default" | 03 §4 [DOCS] |
| Backend enablement (entitlements) | 1-min bars, 5-min bars, all tick+bar combos, "Option Greek streaming — needs backend enablement"; "Data not enabled by default may require exchange approvals depending on exchange/segment"; "Historical Bid-Ask and greeks/bar streams are account entitlements — request activation via apisupport@truedata.in" | 07 + 11 §7 [DOCS] |
| Explicit ADD-ONs | "Analytics and Greeks — ADDON — Top Gainers / Losers, Industry Gainer / Losers, Options Greeks" (.NET README); Extended History "planned as a paid Add-on" | 05/10 + 11 §3 [DOCS] |
| Free for all | "Full Market Feed Replay is available to all active (paid & trial) subscribers post market hours at no extra cost." | 01 onboarding step 8 [DOCS] |
| Full Market Feed (port 9084) | "subject to subscription: 'Realtime Exchange Snapshot API — get live data of entire exchange segment in single call'" — tier/pricing **UNVERIFIED** | 03 §11 [DOCS] |
| Per-plan symbol limits | account-gated — 200 default on FYERS-resold plan; Velocity terminal plans "Standard (50 symbols) and Optima (200 symbols)"; max purchasable count **UNVERIFIED (probe/sales query)** | 00-INDEX.md gap 5 [DOCS]; [WEB] §9 |

## Category 10 — Limits

| Limit | Value | Evidence |
|---|---|---|
| Concurrent connections | **1 login instance per user**: "Connection Control >> You can Connect only from 1 place at a time (1 login instance)" | 11 §2 (official verbatim) [DOCS]; KB snippet [WEB] |
| Stuck-session recovery | force logout `https://api.truedata.in/logoutRequest?user=xxx&password=yyy&port=8082`, "wait ~5 minutes, then log in again" | 02 §4 / 11 §4 [DOCS] |
| Symbols per connection | "Symbols Control >> addSymbol / removeSymbol / getMarketStatus / logout - all combined - As per your Plan's symbols limit" — numeric limit account-gated, **UNVERIFIED** | 11 §2 [DOCS] |
| Live-stream message rate | NO limit once subscribed: "For the Real-Time API, there is no such limit. Symbols once added would continue to flow in a streaming manner." | 11 §2 [DOCS] |
| Historical REST — tick requests | 5/sec · 300/min · 18000/hour | 04 §6 / 11 §1 (official) [DOCS]; KB snippet [WEB] |
| Historical REST — bar requests | 10/sec · 600/min · 18000/hour | same |
| History request shape | 1 symbol per request (no multi-symbol call); Python lib "handles rate limiting gracefully" since truedata-ws 4.2.3 | 11 §6 FAQ 1 + 04 §6 [DOCS] |
| Ports | production RT **8082 or 8084** ("Default is 8082. 8084 only if specified to you"), migration 8083, sandbox 8086, pushbeta 8088, full feed 9084, replay `replay.truedata.in:8082`; "All ports other than the default port provided to you need to be enabled by us at the backend." | 02 §1 / Telegram msg 123 [DOCS] |
| Maintenance windows | trading weekdays 07:30–08:00; Sat & Sun 07:30–10:30 | 02 §6 / 11 §5 [DOCS] |
| Uptime claim | "99.995% uptime through redundant data centres" (marketing) | 01-overview.md [DOCS] |
| Known SDK pitfall | `websocket-client==0.58.0` broke RT connections; pin 0.57.0. Callbacks run on the WS thread — keep light; poll loops need `time.sleep(0.05)`. SDK auto-reconnect re-subscribes symbols; `truedata` 6.0.4 removed auto-reconnect after MANUAL disconnect. | 02 §1/§5 + 03 §13 [DOCS] |

---

## Context comparison — TrueData vs Dhan WS vs Groww

| Provider | Push granularity | Timestamp precision | Payload character | Evidence |
|---|---|---|---|---|
| **TrueData** | Conflated ~1-sec L1 snapshot updates, sequenced (`tick_seq`), NOT every trade — **rate per segment UNVERIFIED, live probe needed** | ms-vs-s **UNVERIFIED** | RICH per update: LTP+LTQ+ATP+day volume+day OHLC+prev close+OI+prev OI+turnover+special tag+seq+L1 bid/ask, all in one message; greeks/chain/1m+5m bars as extra streams | this catalog, headline table |
| Dhan WS (retired here 2026-07-13) | ~2–4 conflated updates/sec/SID (sampled stream, not full tape) | whole-second LTT (IST epoch secs; "Dhan's whole-second LTT quantization") | binary fixed-offset packets; Quote 50B carries day OHLC; prev-close routing quirks; OI as separate code-5 packet | repo: `websocket-connection-scope-lock.md` §E ("Dhan WS is a ~2–4 ticks/sec SAMPLED stream vs their full-tape candle API"); `dhan/live-market-feed.md` |
| Groww WS (retired here 2026-07-15) | ms-timestamped LTP ticks, price-only | milliseconds (`tsInMillis`) | `{ltp, tsInMillis}` only — no volume/OI/depth on the LTP stream | repo: `groww-second-feed-scope-2026-06-19.md` §1 ("Groww's tick timestamps carry **millisecond** precision"), §36.2 ("Groww LTP carries only `{ltp, tsInMillis}` — no volume/OI") |

**Net:** TrueData sits between the two — far richer per-message payload than Groww (full day
state + OI + L1 book on every update) and the SAME conflated-snapshot class as Dhan (both are
vendors of the same NSE broadcast); the RELATIVE update rate is **unknown until the live probe**
— TrueData's own "@1sec" figure, taken literally, would be FEWER updates/sec than Dhan's
measured ~2–4 conflated updates/sec/SID (scope-lock §E), so TrueData's advantage in evidence is
payload richness + `tick_seq` sequencing, NOT rate. It is NOT a true-TBT upgrade over either —
no Indian redistributable vendor feed is (NSE TBT is co-lo-only). The decisive unknowns before
any adoption decision: measured updates/sec/symbol and timestamp precision (both live-probe
items).

## Consolidated UNVERIFIED list (exact probes)

| # | Unknown | Probe |
|---|---|---|
| 1 | Tick-by-tick vs 1-sec conflation, PER SEGMENT — **incl. NSE INDICES** (the retained universe is NIFTY/BANKNIFTY/SENSEX/INDIA VIX) | count msgs/sec on liquid NSE F&O, NSE EQ, MCX FUT, BSE EQ **and spot indices (`NIFTY 50`, `INDIA VIX`)** during open; check `tick_seq` gaps; check if distinct trades between snapshots appear; for indices, document index tick field semantics (no trades — what do LTQ/ATP/OI/turnover carry on an index-value update, and at what cadence?) |
| 2 | Tick timestamp precision (ms vs s) | inspect sub-second digits on live `timestamp`/LTT |
| 3 | Raw JSON keys (subscribe + tick/bar messages) | capture raw WS frames (LZ4-decompress) OR obtain the gated v2.1/v2.2 PDF from apisupport@truedata.in |
| 4 | Bid/ask + greeks stream cadence | msgs/sec per stream per symbol |
| 5 | Greeks vendor-computed vs relayed; the "16+ matrices" list | ask apisupport / inspect greek values vs own BS computation |
| 6 | Full-chain (all strikes) vs `chain_length` window; chain wire mechanism | request chain_length > strike count; sniff whether dedicated chain messages exist |
| 7 | Expired-contract historical availability | query an expired option contract symbol on history REST |
| 8 | Per-plan symbol limits (numeric); Full Market Feed tier/pricing; add-on pricing; full price grid | sales query + re-pull truedata.in/price from residential IP |
| 9 | WS-API availability of BSE debt/G-sec/MF instruments | check WS-API masters vs Velocity lists; ask support |
| 10 | Touchline exact field list; INDIA VIX exact symbol string; max symbol tier | subscribe live + download `NSE_SPOT_INDEX` master |
| 11 | Corporate Data API (TrueWealth) endpoints | sales-gated; request docs |
