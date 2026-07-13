# GDF 01 — Product & Entitlement

> **Source:** https://globaldatafeeds.in/authorised-data-vendors/ · https://globaldatafeeds.in/apis/ · https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/introduction/type-of-data-available/ · …/introduction/type-of-apis-available/ · …/pricing-sales/api-pricing/ · …/pricing-sales/who-can-purchase/ · …/faqs/faqs/ · https://nsearchives.nseindia.com/web/sites/default/files/inline-files/Authorized_Realtime_Data_Vendors.pdf
> **Fetched:** 2026-07-13 · **Evidence tier:** SEARCH (site 403 from research env — see README access constraint; NOTHING here is LIVE-DOC). Third-party pricing rows individually tiered.

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

## 3. Product tiers (API families)

| Tier | What it is | Evidence tier |
|---|---|---|
| **Realtime L1 (WebSockets API)** | `SubscribeRealtime` — 1 update/sec/symbol L1 stream, internal product name "NimbleWebStream". THE core product; files 02–07 | SEARCH + GITHUB-SAMPLE(js-2020) |
| **Snapshot** | Streamed/pulled candles: `SubscribeSnapshot`, `GetSnapshot`, `GetExchangeSnapshot` (entire exchange, Minute (1,2,5,10,15,30), Hour, Day) | SEARCH + CLIENT-LIB-SOURCE |
| **Delayed** | Same snapshot/history functions with a per-exchange delay configured at GDF's backend on the key (`DataDelay` in LimitationResult). "Response fields remain the same for realtime as well as delayed snapshot data" | SEARCH |
| **Historical** | `GetHistory` (Tick/Minute/Hour/Day/Week/Month) + `GetHistoryAfterMarket` (through previous working day; cheaper; not co-enabled with GetHistory on one key — Assumed) | SEARCH + CLIENT-LIB-SOURCE |
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
| "starts at ₹3,000" + ₹233/mo per extra computer | NimbleDataPro | traderskart snippet (independent anchor) | SEARCH / Assumed |

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
