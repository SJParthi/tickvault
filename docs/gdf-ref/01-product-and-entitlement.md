# GDF 01 — Product & Entitlement

> **Source:** https://globaldatafeeds.in/authorised-data-vendors/ · https://globaldatafeeds.in/apis/ · https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/introduction/type-of-data-available/ · …/introduction/type-of-apis-available/ · …/pricing-sales/api-pricing/ · …/pricing-sales/who-can-purchase/ · …/faqs/faqs/ · https://nsearchives.nseindia.com/web/sites/default/files/inline-files/Authorized_Realtime_Data_Vendors.pdf
> **Fetched:** 2026-07-13 · **Evidence tier:** SEARCH (site 403 from research env — see README access constraint; NOTHING here is LIVE-DOC). Third-party pricing rows individually tiered.
> **⚠ 2026-07-14 UPDATE:** the access constraint is LIFTED — every source page of this file was machine-fetched verbatim (HTTP 200) on 2026-07-14 via the GitHub runner (branch `docs-fetch/gdf-2026-07-14` @ `7d354a8d`). See the "2026-07-14 RUNNER-DOC reconciliation" section below: most SEARCH rows upgrade to **RUNNER-DOC (2026-07-14)**; where a row conflicts, the reconciliation section wins. §§1–6 text is retained as the 2026-07-13 baseline.

---

## 2026-07-14 RUNNER-DOC reconciliation

Corpus: 215 pages fetched 2026-07-14 (213 HTTP 200 of 215 in the two combined runs). Pages reconciled for THIS file: `pricing-sales/api-pricing/`, `pricing-sales/who-can-purchase/`, `faqs/faqs/`, `introduction/type-of-data-available/`, `introduction/type-of-apis-available/`, `introduction/introduction-to-apis/` (NEW page, not in the 2026-07-13 pass), `/authorised-data-vendors/`, `/apis/`, `/nimbledatapro/`.

### Confirmations (SEARCH → RUNNER-DOC tier upgrades)

| §/claim | Verbatim proof | Source (RUNNER-DOC 2026-07-14) |
|---|---|---|
| §1 vendor status + 2010 | "We are authorized realtime data vendor of Indian Stock Exchanges (NSE, NFO, CDS, MCX, BSE, BFO, NCDEX). Established in 2010" | https://globaldatafeeds.in/authorised-data-vendors/ |
| §1 L1 meaning / §2 realtime coverage | "Realtime data updating at 1 second frequency is available for NSE Stocks, NSE Indices, NSE F&O, NSE Currency ,MCX,BSE Stocks ,BSE Indices & BSE F&O. This is L1 data with single best Bid & Ask details." | …/pricing-sales/api-pricing/ + …/introduction/type-of-data-available/ (identical wording) |
| §3 Realtime tier | SubscribeRealtime "pushes market data every second (Bid/Ask/Trade)" | …/pricing-sales/api-pricing/ (Compare APIs table) |
| §3 Snapshot tier periods | Exchange Snapshot "Currently supported periods are Minute (1,2,5,10,15,30), Hour and Day (end-of-day)" | …/pricing-sales/api-pricing/ |
| §3 Delayed tier | FAQ: per-exchange delay configured on the key (worked 15-min example); "there is no difference in the fields returned … for real-time and delayed snapshot data"; delays "(e.g., 1,2, 5,10,15,30 minutes etc.)" | …/faqs/faqs/ |
| §3 Historical / **GetHistoryAfterMarket exclusivity — was Assumed, now VERBATIM** | "GetHistory and GetHistoryAfterMarket are … **not enabled together for the same API key** … If GetHistory is enabled → GetHistoryAfterMarket will be disabled." (The "cheaper" claim stays Assumed — not on any fetched page.) | …/faqs/faqs/ |
| §3 Transports | Type-of-APIs page: WebSockets / DotNet / COM = "Push Type API", RESTful = "Pull Type API"; WS "Sends data only in JSON format. RESTful API sends data in JSON & XML" | …/introduction/type-of-apis-available/ + …/pricing-sales/api-pricing/ (Important Notes) |
| §4 API pricing UNPUBLISHED | "we have flexible pricing policy which is tailored to suit your needs. Do contact us with your requirements" — **ZERO INR figures anywhere on the API pricing page**; "No commitment required, flexible pricing" repeated per API family | …/pricing-sales/api-pricing/ + https://globaldatafeeds.in/apis/ |
| §5 who-can-purchase | "APIs available here are only for Individual Users for Personal Use. For Commercial Use, you will need to sign agreement with respective Exchanges and pay them the requisite fees." | …/pricing-sales/who-can-purchase/ |
| §5 scope / redistribution / gaming (previously cited from the same page family) | /apis/ **Terms & Conditions** verbatim: "information received from Global Datafeeds is terminated at their end" · "As per NSE/SEBI guidelines, we do not provide data for any kind of Gaming, lottery, Virtual trading, Simulation applications" · "Data … is for charting / analysis use of a single subscriber only" · violation ⇒ termination "without any prior notice"; amounts non-refundable | https://globaldatafeeds.in/apis/#terms-and-conditions |
| §5 session enforcement | "WebSockets, DotNet & COM APIs allows only 1 active session with the server with 1 API Key. If you create another session with same key, previous session will be invalidated with response 'Access Denied. Key already in use by other session.'" | …/pricing-sales/api-pricing/ (Important Notes) |
| §6 trial existence | "Take a Free Trial" links on every API family section of /apis/ (WS, REST, DotNet, COM, FIX) — **duration still unpublished** (see U-3 below) | https://globaldatafeeds.in/apis/ |
| §6 contract periods (desktop) | NimbleDataPro: "Available subscription period of 1,3,6,12 months" (VM edition: 12 months only) | https://globaldatafeeds.in/nimbledatapro/ |
| §2 NCDEX marketing-only (U-24 strengthened) | NCDEX appears on the vendor/marketing page but is ABSENT from every API supported-exchanges list on /apis/ ("NSE (Stocks & Indices), NFO, NSE CDS, MCX, BSE (Stocks & Indices), BFO") and from the realtime-coverage sentence | https://globaldatafeeds.in/apis/ vs /authorised-data-vendors/ |

### New facts (first captured 2026-07-14)

1. **Entitlement quota semantics (FAQ — key GetLimitation context):** the Symbol limit is per API key ("Reached instrument limitation" on breach). A **Request-per-hour limit** exists per key — "For example, this limit could be 1800, 3600, or 7200 requests per hour"; it resets on the hour; **subscribe + unsubscribe + re-subscribe EACH consume quota** (100 symbols swapped = 300 requests of an 1800/hr budget). Directly relevant to any reconnect/resubscribe ladder: re-subscriptions burn hourly quota. — RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/faqs/faqs/)
2. **Compare-APIs plan matrix (api-pricing):** five named plans — Realtime + Historical / Option Chain / Realtime / Historical / Exchange Snapshot — with a per-function YES/NO grid: all helper functions (GetExchanges…GetExchangeMessages incl. GetLimitation/GetServerInfo) = YES on every plan; SubscribeRealtime = NO on Historical & Exchange Snapshot plans; SubscribeSnapshot = NO on the Realtime plan; GetHistory = NO on the Realtime plan. ⚠ Page-internal anomaly: the bottom rows show GetLastQuoteOptionChain & GetExchangeSnapshot as YES **only** under "Exchange Snapshot API" and NO under "Option Chain API" — inconsistent with the OptionChain package definition on type-of-data-available (which includes GetLastQuoteOptionChain); treat as a probable column-shift typo, verify per-key via GetLimitation. — RUNNER-DOC (2026-07-14, …/pricing-sales/api-pricing/)
3. **13 named data packages** (type-of-data-available "Functions available in API's Data Packages"): RealTime · Historical · RealTime+Historical · OptionChain · OptionGreeks · OptionChain+OptionGreeks · Ondemand SnapShot · ExchangeSnapShot · GetHistoryAfterMarket · Delayed Snapshot · Streaming API · Special Functions · Helper Functions. StreamAllSymbols/StreamAllSnapshots are listed as "available individually" — i.e. a separately-enabled package, consistent with the U-1 residual (separate entitlement question). — RUNNER-DOC (2026-07-14, …/introduction/type-of-data-available/)
4. **Historical backfill depth is now DOCUMENTED per exchange** (type-of-data-available tables): Tick "Every 1 Sec" × 1 calendar week (all segments); Minute 1–4 × 3 months; Minute 5–12 × 4.5 months; Minute 15–30 + Hour 1–12 × 6 months; Day/Week/Month since 2010 (NSE CM/CDS/MCX, NFO continuous futures), since 2007 (BSE cash), since 2024 (BFO continuous futures); contractwise F&O Day/Week/Month only 1–3 months; options contractwise 1 month. — RUNNER-DOC (2026-07-14, …/introduction/type-of-data-available/)
5. **Scale/uptime marketing (NEW page, introduction-to-apis):** "With an uptime of 99.995%, we handle 50 million data requests daily … since 2010"; "data spans all symbols (currently 50000+)"; "1 second update frequency"; same APIs power 25+ charting platforms. — RUNNER-DOC (2026-07-14, …/introduction/introduction-to-apis/)
6. **First-party INR figure recovered (desktop add-on, NOT the API):** "Users with lower subscription period can avail this facility by paying **Rs.233/- per additional computer per month**" — upgrades the §4 traderskart third-party anchor for this one figure to first-party RUNNER-DOC. No other INR amount appears on any fetched page. — RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/nimbledatapro/)
7. **Sales working hours + address:** order processing & support 8:30am–6:00pm M–F, 9:30am–2:30pm Sat (GMT+5:30), closed Sundays + exchange holidays; Global Financial Datafeeds LLP, 301, 4th Floor, Krishna Enclave, Jehan Circle, Gangapur Road, Nashik – 422005. — RUNNER-DOC (2026-07-14, …/pricing-sales/api-pricing/)
8. **Server maintenance windows** (entitlement-adjacent; full note in file 12): 02:00–02:30 + 08:00–08:30 IST (BOD sometimes till 08:45); server answers "Connection refused"; vendor advice "please connect after 8:45AM". — RUNNER-DOC (2026-07-14, …/faqs/faqs/)
9. **Client-host guidance (FAQ):** vendor explicitly warns AGAINST burstable cloud instances (AWS T2/T3) for market-data workloads; recommends compute/memory-optimized (M4/M5-class). tickvault's r8g.large complies. Test endpoint example in the telnet FAQ: `ws://test.lisuns.com:4575` (plaintext ws://, consistent with U-2; `lisuns.com` = GDF infra domain). — RUNNER-DOC (2026-07-14, …/faqs/faqs/)
10. **Official Python client:** /apis/ links "Python Library" → `pypi.org/project/ws-gfdl/1.1.0` (WS API). — RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/apis/)
11. **FIX API (U-4):** /apis/ verbatim — "API based on the Financial Information eXchange (FIX) Protocol … Ultra-fast & reliable: Delivers tick-by-tick market data with minimal latency." The FIX section is the ONLY family with NO Documentation link, NO supported-exchanges list and NO available-data list; zero FIX documentation pages exist in the 215-page crawl. TBT nature remains marketing-only — **U-4 stays open**; sales contact is the route. — RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/apis/)
12. **Algo-use tension recorded (U-5 context):** /apis/ headline says "Power your algo-trading with our time-tested APIs" and the FAQ says realtime snapshot data is "Commonly used for high-frequency trading (HFT), algorithmic trading" — while the same site's T&C scopes data to "charting / analysis use of a single subscriber only" and NO page in the crawl mentions "non-display" (grep across all 215 pages: zero hits). The personal-tier/non-display question remains UNSTATED — ask sales in writing before live-trading use (U-5). — RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/apis/ + …/faqs/faqs/)

### Drift / refutations

- **None refuted.** No 2026-07-13 claim in this file was contradicted by the 2026-07-14 corpus. One page-internal anomaly (Compare-APIs bottom rows, item 2 above) is flagged as probable vendor typo, not our drift.
- §1 phone contact (Shraddha Lokhande numbers) did not appear on the fetched pages — that row stays SEARCH-tier; `sales@globaldatafeeds.in` is RUNNER-DOC-confirmed.

### U-row status after this pass

| U-row | Verdict | Evidence |
|---|---|---|
| **U-3** (API pricing / trial duration / bundling) | **PARTIAL** — pricing confirmed UNPUBLISHED/quote-only at RUNNER-DOC (a negative closure: no INR API figure exists on the site); plan/bundle axes now documented (Compare-APIs matrix + 13 packages); trial EXISTS on every API family but duration is still unpublished (the docs-portal FAQ lists "Is trial access available?" with no extractable answer). Sales contact remains the route for figures + trial length. | api-pricing, /apis/, type-of-data-available, docs.globaldatafeeds.in/faqs-2131262m0 |
| **U-4** (FIX = true TBT? depth?) | **OPEN** — TBT claim re-confirmed verbatim as marketing; zero FIX docs in the whole crawl; L1-only confirmed everywhere else. Sales route. | /apis/ |
| **U-5** (non-display / algo licensing) | **OPEN** — zero "non-display" hits in 215 pages; T&C "charting / analysis … single subscriber only" now verbatim; algo-trading marketed regardless. MUST be asked in writing. | /apis/#terms-and-conditions, faqs |
| **U-24** (NCDEX via API) | strengthened OPEN-negative — NCDEX on marketing page only, absent from all API exchange lists. | /apis/ vs /authorised-data-vendors/ |

---

## 1. Vendor status

| Fact | Value | Confidence |
|---|---|---|
| Legal entity | Global Financial Datafeeds LLP, Nagpur/Nashik, India | Verified |
| Exchange authorization | Authorized real-time (**L1**) data vendor of NSE (Stocks, Indices, F&O, Currency), MCX (since 2010), BSE (Stocks, Indices, F&O), NCDEX; appears in NSE's Authorized Realtime Data Vendors PDF | Verified (SEARCH ×2 + NSE PDF snippet) |
| What "L1" means here | Single best bid/ask + trade — no order-book depth on the API products (see `03` §7) | Verified |
| Sales contact | sales@globaldatafeeds.in · Ms. Shraddha Lokhande +91-77210 80002 / +91-73506 20002 | Verified |

## 2. Exchange-segment coverage matrix

GDF exchange codes as used by the APIs (`Exchange` request field / GetExchanges values):

| GDF code | Segment | Realtime 1s L1 | In official samples | Notes |
|---|---|---|---|---|
| `NSE` | NSE Cash/Stocks | ✓ | ✓ (JS + SDK) | EQ series bare symbol; other series `.BE`-style suffix |
| `NSE_IDX` | NSE Indices | ✓ | ✓ | identifiers = display names with spaces (`NIFTY 50`) |
| `NFO` | NSE F&O | ✓ | ✓ | FUTIDX/OPTIDX/FUTSTK/OPTSTK |
| `CDS` | NSE Currency derivatives | ✓ | ✓ | FUTCUR/OPTCUR (`USDINR-I`) |
| `MCX` | MCX Commodity | ✓ | ✓ | FUTCOM/OPTFUT (`CRUDEOIL-I`) |
| `BSE` | BSE Stocks | ✓ (per type-of-data page) | REST GetExchanges list only | not in WS sample comments |
| `BFO` | BSE F&O | ✓ (per type-of-data page) | REST GetExchanges list only | |
| `BSE_IDX` | BSE Indices | ✓ (per type-of-data page) | REST GetExchanges list only | |
| `BSE_DEBT` | BSE Debt | ? | REST GetExchanges list only | coverage detail **Unknown** |
| NCDEX | — | marketing only | — | no API exchange code observed; **Unknown** (U-24) |

REST `GetExchanges` verbatim (gfdl-rest@1.0.3 README): `{"EXCHANGES":["BFO","BSE","BSE_DEBT","BSE_IDX","CDS","MCX","NFO","NSE","NSE_IDX"]}` — CLIENT-LIB-SOURCE(gfdl-rest@1.0.3). A given key sees only its entitled subset (GetLimitation `AllowedExchanges`).

> **2026-07-14 (RUNNER-DOC):** GetHolidays response rows enumerate a **10th exchange code — `NCX` (NCDEX)** — in their `Exchanges` field (`MCX;NSE;NSE_IDX;NFO;CDS;BSE;BSE_IDX;BSE_DEBT;BFO;NCX`, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/rest-api-documentation/getholidays-2/): the holiday calendar covers NCX even though NCX appears in no API supported-exchanges/GetExchanges list — the 9-code list above may be key-scoped while the holiday calendar is global (06 §6).

## 3. Product tiers (API families)

| Tier | What it is | Evidence tier |
|---|---|---|
| **Realtime L1 (WebSockets API)** | `SubscribeRealtime` — 1 update/sec/symbol L1 stream, internal product name "NimbleWebStream". THE core product; files 02–07 | SEARCH + GITHUB-SAMPLE(js-2020) |
| **Snapshot** | Streamed/pulled candles: `SubscribeSnapshot`, `GetSnapshot`, `GetExchangeSnapshot` (entire exchange, Minute (1,2,5,10,15,30), Hour, Day) | SEARCH + CLIENT-LIB-SOURCE |
| **Delayed** | Same snapshot/history functions with a per-exchange delay configured at GDF's backend on the key (`DataDelay` in LimitationResult). "Response fields remain the same for realtime as well as delayed snapshot data" | SEARCH |
| **Historical** | `GetHistory` (Tick/Minute/Hour/Day/Week/Month) + `GetHistoryAfterMarket` (through previous working day; cheaper — still Assumed; NOT co-enabled with GetHistory on one key — **RUNNER-DOC 2026-07-14** FAQ verbatim, see reconciliation §) | SEARCH + CLIENT-LIB-SOURCE + RUNNER-DOC(2026-07-14) |
| **Option Chain / Greeks** | `GetLastQuoteOptionChain`, `SubscribeOptionChain(Greeks)`, full Greeks realtime + history (file 07) | CLIENT-LIB-SOURCE |
| **Streaming "Push Type" API** | `StreamAllSymbols` / `StreamAllSnapshots` — separate doc section; wire format **Unknown** (file 10, U-1) | SEARCH (existence only) |
| **FIX API** | "tick-by-tick market data with minimal latency" per /apis/. Whether true exchange TBT or the same 1s feed over FIX: **Unknown** (U-4) | SEARCH |
| **Transports** | WebSockets (JSON-only), REST (GET-only), .NET, COM, FIX | Verified |
| **Fundamental Data API** | separate docs portal docs.globaldatafeeds.in: GetEODStats, GetBhavCopyCM/FO (exchange-official EOD), GetFuturesAndOptions | SEARCH |
| **Desktop feeders** (NOT the API) | NimbleDataPro / ProPlus / PlusLite → AmiBroker, MetaStock, NinjaTrader, MultiCharts, Excel, Sierra Chart | SEARCH |

## 4. Pricing evidence table

**API pricing is UNPUBLISHED — quote-only.** "Flexible pricing policy which is tailored to suit your needs"; contact sales. Plan axes per the api-pricing page: realtime-only / historical-only / realtime+historical; per-exchange selection; symbol count per API key. **No figure below is an API price** — all INR figures are third-party desktop-product anchors of mixed dates:

| Figure | Product (NOT the API) | Source + date | Tier / Confidence |
|---|---|---|---|
| ≈ ₹2,775/mo | NimbleDataProPlus, NSE-only | tradingtuitions.com roundup, undated ~2019–21 | SEARCH / Assumed |
| ≈ ₹9,340/mo | NimbleDataProPlus, all exchanges | same | SEARCH / Assumed |
| ₹3,199 (+₹3,229 MCX) | ProPlus Cash-or-F&O 225 symbols (reseller) | tradersgurukul.com, period unstated (monthly ex-GST plausible) | SEARCH / Assumed |
| ≈ ₹70,000/yr | NimbleDataPro2, NSE+NFO+MCX 200 sym each | tradingqna.com forum, Apr-2019 | SEARCH / Assumed |
| "starts at ₹3,000" + ₹233/mo per extra computer | NimbleDataPro | traderskart snippet (independent anchor); the ₹233/mo add-on figure is now FIRST-PARTY — "Rs.233/- per additional computer per month" on globaldatafeeds.in/nimbledatapro/ | SEARCH / Assumed (₹3,000) · **RUNNER-DOC 2026-07-14** (₹233) |

The ₹2,775–3,199 band could not be independently reproduced in the adversarial pass — treat as single-sourced order-of-magnitude only. **Unknown:** actual API INR pricing + trial duration (U-3).

## 5. Licensing & usage restrictions

| Restriction | Statement | Confidence |
|---|---|---|
| Who can buy | "APIs available from Global Datafeeds are only for **Individual Users for Personal Use**." Commercial use requires signing agreements with the exchanges + paying exchange fees directly | Verified (SEARCH, who-can-purchase page) |
| Scope | Data for "charting / analysis use of a **single subscriber** only" | Verified |
| Redistribution | Banned — data "is terminated at [subscriber's] end" | Verified |
| Gaming / virtual trading | Not supplied for gaming/lottery/virtual-trading/simulation apps (needs direct NSE/SEBI approvals) | Verified |
| **Non-display (algo) use** | NO published statement on whether the personal API tier covers NSE "non-display" (algo-execution) usage — under NSE policy non-display normally attracts separate exchange fees. **Unknown — MUST be asked in writing before any live-trading use** (U-5) | Unknown |
| Session enforcement | 1 active WS session per key (also an anti-key-sharing measure) — see 02 §4 | Verified |
| IP whitelist / KYC | No mention found of IP whitelisting; auth is key-only. KYC specifics unpublished | Assumed (absence) / Unknown |

## 6. Trial & signup

| Item | Value | Confidence |
|---|---|---|
| Trial | Free trial offered for ALL APIs (realtime, historical, snapshot, delayed, EOD, chain, greeks); **duration unpublished** (U-3) | Verified (existence) / Unknown (length) |
| Signup flow (API) | contact sales → quote → GDF issues **endpoint hostname + port + API key** | Verified |
| Contract periods | 1/3/6/12-month periods seen for desktop products; "no commitment" marketing; API terms per quote | Assumed |

## What this prevents

- Quoting a desktop-feeder price as the API price in a purchase decision.
- Assuming algo/non-display use is covered by the personal tier (it is UNSTATED — ask in writing).
- Assuming BSE/NCDEX coverage details from the NSE-centric samples.
