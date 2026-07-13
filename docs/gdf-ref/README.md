# Global Data Feeds (GDF / GFDL) — API Reference Pack

> **Vendor:** Global Financial Datafeeds LLP (globaldatafeeds.in), Nagpur/Nashik, India — NSE/BSE/MCX/NCDEX authorized realtime **L1** data vendor.
> **Fetched:** 2026-07-13 (all research performed this date)
> **Purpose:** research-only reference pack so a future implementation session needs ZERO re-research. Companion integration PROPOSAL in `13-integration-proposal.md` — NO code exists; any code PR requires operator dated-quote authorization per `.claude/rules/project/websocket-connection-scope-lock.md`.
> **Single source of open questions:** `99-UNKNOWNS.md` — every genuinely-undetermined fact lives there with the exact question to ask GDF support or live-probe. Inline `**Unknown**` flags cross-reference it as `U-<n>`.

---

## ⚠ ACCESS CONSTRAINT — read before trusting any tier label

**globaldatafeeds.in was NOT directly fetchable from the research environment** (egress proxy returned 403 for the site, for web.archive.org, and for every relay). Therefore almost nothing in this pack is labeled LIVE-DOC and nothing is labeled ARCHIVED. **Exception (2026-07-13, post-initial-draft): one page — the SubscribeRealtime function page — was pasted verbatim by the OPERATOR from a live browser and is labeled LIVE-DOC (operator-pasted); see `03` §3b.** All other doc-page content was recovered two ways:

1. **Search-engine extraction** of the live pages (substantive, URL-cited, NOT byte-verbatim) → labeled **SEARCH**.
2. **Official GFDL artifacts read as raw source**: four official PyPI packages (`Author-email: developer@globaldatafeeds.in`) and GFDL's official JavaScript sample + 2018 third-party client docs preserved verbatim in GitHub mirrors → labeled **CLIENT-LIB-SOURCE(pkg@ver)** / **GITHUB-SAMPLE**.

All verbatim JSON in this pack comes from tier-2 artifacts (plus the operator paste) and IS byte-exact. All SEARCH-tier prose should be re-verified against the live pages before contracting.

## Evidence-tier legend (header block on every section)

| Tier | Meaning |
|---|---|
| `LIVE-DOC` | live globaldatafeeds.in page content — unattainable by direct fetch; **used ONLY for operator-pasted live pages (2026-07-13: the SubscribeRealtime page, `03` §3b)** |
| `ARCHIVED` | web.archive.org capture — **UNUSED (unattainable)** |
| `SEARCH` | search-engine extraction of the named live page URL, 2026-07-13; substantive but not byte-verbatim |
| `CLIENT-LIB-SOURCE(pkg@ver)` | read from an extracted OFFICIAL GFDL PyPI sdist: `wsgfdl-py@1.3.5` (2025-11-28, newest WS lib), `GFDLWS@0.0.4`, `ws-gfdl@1.1.0` (2022), `gfdl-rest@1.0.3`. Includes each package's official README with real captured server responses |
| `GITHUB-SAMPLE(js-2020)` | GFDL's official JavaScript "NimbleWebStream" sample ("updated 29th June 2020" + 2021 Greeks additions), mirrored byte-verbatim at github.com/rohittrank/trade `application/views/websocket_view.html` — carries `//GFDL :` authored comments |
| `GITHUB-SAMPLE(dhelm-2018)` | github.com/kncsolutions/dhelm-gfeed-python-client `docs/index.rst` (2018) — reproduces verbatim GDF server response JSON incl. a real key's LimitationResult |
| `ARITH` | arithmetic verification performed during the 2026-07-13 reconciliation pass (epoch decodes, count checks) |

Confidence labels: **Verified** (multiple consistent sources or official artifact) / **Assumed** (single source or inferred) / **Unknown** (→ `99-UNKNOWNS.md`).

## File index

| File | Contents |
|---|---|
| `01-product-and-entitlement.md` | vendor status, segments matrix, product tiers, pricing evidence, licensing, trial |
| `02-authentication-and-connection.md` | endpoint model, Authenticate handshake, session exclusivity (new-connection-wins), Echo heartbeat, reconnect contract, maintenance windows, frame sizes |
| `03-websocket-realtime-feed.md` | SubscribeRealtime/Unsubscribe, 1/sec/symbol cadence, COMPLETE RealtimeResult field table, PreOpen, OI, no depth |
| `04-websocket-snapshot-bars.md` | SubscribeSnapshot streamed bars, GetSnapshot, bar-stamp convention, no-trade-bar gaps, daily-stamp anomaly |
| `05-websocket-quote-functions.md` | GetLastQuote family, arrays, GetSnapshot(Greeks) request+response schemas |
| `06-instruments-and-identity.md` | reference functions, full identifier grammar, GetInstruments field table, mapping to tickvault identities |
| `07-options-and-greeks.md` | option chain functions (Depth = strike count), Greeks subscribe/quote/history, 18-field schema |
| `08-historical-api.md` | GetHistory WS+REST dialects, periodicities, 3-layer depth model, ordering, AdjustSplits, SDK bugs |
| `09-rest-api.md` | base URL, GET-only, accessKey, paths, format switches, envelopes, quotas |
| `10-streaming-push-api.md` | StreamAllSymbols/StreamAllSnapshots "Push Type API" — what is and is NOT known |
| `11-limits-errors-diagnostics.md` | GetLimitation full schema, every captured error/status literal, quota rules |
| `12-data-quality-assessment.md` | timestamps, conflation, candle provenance, latency, comparison vs Dhan-WS and Groww-WS |
| `13-integration-proposal.md` | **PROPOSAL** — feed-#3 integration map, 7-PR series, binding rule-file constraints |
| `99-UNKNOWNS.md` | the single consolidated open-questions table (U-1 … U-24) |

## Reconciled master claims table (44 load-bearing facts)

Adjudicated 2026-07-13 across 6 parallel research passes + raw artifacts. Conflicts between researchers were resolved by arithmetic + artifact grep; the verdicts below are the pack's authority.

| # | Claim | Label |
|---|---|---|
| 1 | GFDL = NSE/BSE/MCX(2010)/NCDEX authorized realtime **L1** vendor; Nagpur/Nashik LLP | Verified |
| 2 | Transports: WebSocket, REST, .NET, COM, FIX; WS is JSON-only | Verified |
| 3 | Exchange codes: NSE, NSE_IDX, NFO, CDS, MCX (+ BSE, BFO, BSE_IDX, BSE_DEBT on REST GetExchanges) | Verified |
| 4 | WS endpoint `ws://endpoint:port`, per-account host+port issued on purchase; none hardcoded anywhere | Verified |
| 5 | No wss:// anywhere; plaintext ws + API key in cleartext frame | Verified (absence) / **Unknown** (availability, U-2) |
| 6 | Auth = single JSON frame `{"MessageType":"Authenticate","Password":"<API_KEY>"}` → `AuthenticateResult{Complete:true,Message:"Welcome!"}`; no headers/query auth | Verified |
| 7 | 1 active WS session per key; a NEW connection INVALIDATES the previous one ("Access Denied. Key already in use by other session.") | Verified |
| 8 | Server sends plain-TEXT diagnostic frames (expired/invalid key, "Reached instrument limitation") — parser must tolerate non-JSON | Verified |
| 9 | Full diagnostic message list NOT captured | **Unknown** (U-10) |
| 10 | Heartbeat: server→client `MessageType:"Echo"` every few seconds; no client pong in any sample | Verified |
| 11 | Reconnect contract: full re-auth + re-subscribe every symbol; no resume/sequence protocol | Verified |
| 12 | SubscribeRealtime: 1 request/symbol, no batch form; pushes ~1/sec/symbol full-image L1 tick | Verified |
| 13 | Tick payload: LTP/LTQ/ATP, best bid/ask+qty, OHL, `Close`=PREV-DAY close, Volume, Value, OI+OIChange inline, QuotationLot, PreOpen flag, PriceChange(%), LastTradeTime+ServerTime | Verified |
| 14 | `Close` means prev-day close in quote/realtime family but BAR close in Snapshot/History rows — context-dependent | Verified |
| 15 | Timestamps: WS epoch-seconds true UTC; REST epoch-ms (`…000`); no sub-second precision anywhere | Verified (ARITH ×2) |
| 16 | 2018-era ticks lack PriceChange/OIChange fields — schema grew; decoder fields must be Option-al | Verified |
| 17 | Scientific-notation floats on the wire (Greeks) | Verified |
| 18 | SubscribeSnapshot = per-symbol streamed bar closes; Periodicity Minute/Hour (docs also list Day); Period 1,2,5,10,15,30 (Minute only); pushed when bar completes | Verified |
| 19 | Bar labels minute-aligned; open-time labeling **Assumed** (doc snippet supports it; live-probe U-6) | Assumed |
| 20 | Daily-bar LASTTRADETIME = 18:00 IST (2024 REST series) / 06:00 IST (2018) — vendor processing stamp; date label only, never a time key | Verified (ARITH) |
| 21 | GetHistory: Tick/Minute/Hour/Day/Week/Month; Period 1,2,3,4,5,10,12,15,20,30; From=0→full history; rows newest-first; AdjustSplits param (default true, 1.3.5) | Verified |
| 22 | History depth = 3 layers: doc ceiling (tick 7d / 1m 3mo / EOD 2010) vs plan tier (tick 2d / 1m 60d) vs per-key GetLimitation (MaxTicks 2/5/7 seen, MaxIntraday 44, MaxEOD 100000) — GetLimitation is authoritative | Verified (mechanism) / Assumed (values) |
| 23 | GetLimitation returns full entitlement matrix (functions on/off, per-exchange symbol caps + DataDelay, call/bandwidth quotas, history caps); −1 = unlimited | Verified |
| 24 | Real observed plan values: 7200 calls/hr, 5.36M/month, 200 sym/exchange (NSE 160 + NSE_IDX 40) on a 2018 key; FAQ tiers 1800/3600/7200/hr | Verified (samples) / Assumed (current tiers) |
| 25 | Max 25 symbols per Array/GetSnapshot call; 20 rows GetInstrumentsOnSearch; 5 snapshots per GetExchangeSnapshot call | Verified |
| 26 | Huge single frames (full instrument master) — official SDK lifts frame cap to 2^50; client must not cap at defaults | Verified |
| 27 | Request booleans are STRINGS (`"Unsubscribe":"false"`); request-key casing inconsistent across functions/libs — treat server as case-tolerant (verify live, U-16) | Verified (string bools) / Assumed (case tolerance) |
| 28 | Identity: string InstrumentIdentifier in 3 formats (continuous -I/-II/-III; long TYPE_PRODUCT_EXPIRY_OPT_STRIKE incl. FUTCOM double-underscore; contractwise short = TradeSymbol); indices = display names with spaces ("NIFTY 50") on NSE_IDX | Verified |
| 29 | GetInstruments row carries `TokenNumber` (exchange numeric token, string) + ISIN (equities) + QuotationLot + Series + price bands + 52wk | Verified |
| 30 | Greeks realtime/last-quote APIs key on **Token**, not identifier; 18 greek fields incl. Vanna/Charm/Zomma/Volga/DTR; historical greeks exist | Verified |
| 31 | L1 only — no depth function on WS/REST; OptionChain `Depth` = strike-count around ATM | Verified |
| 32 | Streaming API (StreamAllSymbols/StreamAllSnapshots) exists as separate doc'd product; wire shapes/entitlement/endpoint | Verified (existence) / **Unknown** (everything else, U-1) |
| 33 | Whole-exchange pull exists TODAY without Streaming API: GetExchangeSnapshot (+ server-side GetExchangeSnapshotAfterMarket) | Verified |
| 34 | Delayed API = same functions with backend-configured per-exchange delay; same response fields | Verified |
| 35 | REST: GET-only, `accessKey` query param, path = function name + `/`; JSON default, `xml=true`, `format=CSV`; ALL-CAPS keys; known SDK bugs | Verified |
| 36 | API pricing: unpublished, quote-only (sales@globaldatafeeds.in; +91-77210 80002); free trial all APIs, duration unpublished; personal-use-only tier for individuals | Verified |
| 37 | Desktop pricing anchors (NOT the API): ProPlus NSE-only ≈ ₹2,775/mo; all-exchange ≈ ₹9,340/mo; reseller ProPlus 225-sym ₹3,199 (+MCX ₹3,229); Pro2 ≈ ₹70k/yr (2019) | Assumed (3rd-party, mixed dates) |
| 38 | Usage: single-subscriber charting/analysis; redistribution banned; no gaming/virtual-trading; NSE non-display (algo) entitlement UNSTATED | Verified (stated parts) / **Unknown** (non-display, U-5) |
| 39 | Maintenance windows 02:00–02:30 & 08:00–08:45 IST (collides with tickvault 08:30–08:45 boot window) | Assumed (single source, U-18) |
| 40 | Candles are vendor-built from the 1s feed (no exchange-official claim); exchange-official EOD via separate GetBhavCopyCM/FO | Assumed (vendor-built) / Verified (bhavcopy APIs exist) |
| 41 | No independent latency/reliability measurements exist | Unknown (U-19) |
| 42 | GetHistoryAfterMarket: separate function (prev-working-day history, cheaper; not co-enabled with GetHistory on one key); SDK opens its own WS connection for it | Verified (existence+SDK behavior) / Assumed (key-class semantics) |
| 43 | Pre-open ticks delivered in-band with `PreOpen:true` flag | Verified (flag) / **Unknown** (pre-open OHLC semantics, U-15) |
| 44 | WS product internal name = "NimbleWebStream" (JS sample title); no product named "NimbleTBT" exists in any artifact; the only TBT hint is the FIX API tier (nature unknown, U-4) | Verified |

## Refuted claims (do NOT resurrect)

| Refuted claim | Truth |
|---|---|
| "EOD/daily bars stamped 09:15 IST session open" | ARITH-refuted: 2024 REST daily series uniformly 18:00:00 IST; 2018 sample 06:00 IST. Vendor processing stamp — date label only |
| "GFDL provides millisecond timestamps" (third-party roundup) | Contradicted by every official wire sample: WS = whole seconds; REST ms values always end `000` |
| "SubscribeSnapshot bar-open stamp is Verified" | Downgraded to Assumed — the decisive arithmetic example was wrong (1669265340 = 10:19:00 IST, not 11:39); doc snippet supports open-stamp but live-probe required (U-6) |
| "StreamAllSymbols/StreamAllSnapshots don't exist" | They exist as a separate docs-only Streaming API product; no official client implements them |
| "GDF offers native exchange TBT with zero aggregation" (stratzy blog) | Puffery: GDF's own docs say 1-second L1; NSE's vendor broadcast is itself ~1s snapshots |

## Merged function catalog (41 functions + Authenticate + Echo)

See per-topic files for full request/response detail. Presence key: JS = official JS sample (27 data functions, 2020+2021); W1/W2/W3 = ws-gfdl 1.1.0 / GFDLWS 0.0.4 / wsgfdl-py 1.3.5; R = gfdl-rest 1.0.3.

| Function | Transport | In | File |
|---|---|---|---|
| Authenticate | WS | JS W1 W2 W3 | 02 |
| SubscribeRealtime | WS | JS W1 W2 W3 | 03 |
| SubscribeSnapshot | WS | JS W1 W2 W3 | 04 |
| SubscribeRealtimeGreeks | WS | W1 W2 W3 | 07 |
| SubscribeOptionChain / SubscribeOptionChainGreeks | WS | W2 W3 | 07 |
| SubscribeSnapshotGreeks | WS | W3 | 07 |
| SubscribeTopGainersLosers | WS | W2 W3 | 05 |
| GetLastQuote / Short / ShortWithClose | WS+REST | JS W1 W2 W3 R | 05 |
| GetLastQuoteArray / Short / ShortWithClose | WS+REST | JS W1 W2 W3 R | 05 |
| GetSnapshot / GetSnapshotGreeks | WS+REST / WS | JS W1 W2 W3 R / W3 | 04, 05, 07 |
| GetHistory / GetHistoryAfterMarket / GetHistoryGreeks | WS+REST | JS W1 W2 W3 R (varies) | 08 |
| GetLastQuoteOptionChain / OptionGreeks / ArrayOptionGreeks / OptionGreeksChain | WS+REST | JS W1 W2 W3 R | 07 |
| GetExchangeSnapshot | WS+REST | JS W1 W2 W3 R | 04 |
| GetExchanges / GetInstruments / GetInstrumentsOnSearch / GetInstrumentTypes / GetProducts / GetExpiryDates / GetOptionTypes / GetStrikePrices | WS+REST | JS W1 W2 W3 R | 06 |
| GetServerInfo / GetLimitation | WS+REST | JS W1 W2 W3 R | 11 |
| GetMarketMessages / GetExchangeMessages | WS+REST | JS W1 W2 W3 R | 11 |
| GetTopGainersLosers / GetVolumeShockers | WS+REST / WS | W2 W3 R / W3 | 05 |
| Echo (server-push heartbeat) | WS | JS (comment) | 02 |
| GetExchangeSnapshotAfterMarket / GetLastQuoteOptionChainWithGreeks | server-side names (LimitationResult) | — | 11, 07 |
| **StreamAllSymbols / StreamAllSnapshots** | Streaming API (docs only; wire unknown) | — | 10 |
| Delayed twins (SubscribeSnapshot/GetSnapshot/GetHistory/GetExchangeSnapshot Delayed) | Delayed API | — | 01, 04 |

## The CURRENT (2026) live docs tree — authoritative function inventory

> **Source:** sidebar navigation of the live docs, captured in the 2026-07-13 operator paste of the SubscribeRealtime page · **Tier:** LIVE-DOC (operator-pasted).

Top-level API sections in the 2026 docs: **WebSockets API, Streaming API, OptionChain API, Greeks API, Delayed API, GainersLosers, Volume Shockers, GetHolidays** — i.e. Greeks/OptionChain/Delayed/GainersLosers/VolumeShockers are SEPARATE top-level sections in 2026 (this pack's Greeks coverage in `07` remains CLIENT-LIB-SOURCE-tier and matches), the **Streaming API is confirmed as a live current section** (10), and **`GetHolidays` is a 2026 section NOT present in any mined SDK/sample — wire shape uncaptured** (see `99-UNKNOWNS.md` U-24).

WebSockets API functions listed in the 2026 sidebar (26): How To Connect, Authenticate, SubscribeRealtime, SubscribeSnapshot, GetLastQuote, GetLastQuoteShort, GetLastQuoteShortWithClose, GetLastQuoteArray, GetLastQuoteArrayShort, GetLastQuoteArrayShortWithClose, GetSnapshot, GetHistory, GetHistoryAfterMarket, GetExchanges, GetInstrumentsOnSearch, GetInstruments, GetInstrumentTypes, GetProducts, GetExpiryDates, GetOptionTypes, GetStrikePrices, GetServerInfo, GetLimitation, GetMarketMessages, GetExchangeMessages, GetExchangeSnapshot — all covered by this pack; GetHistoryAfterMarket + GetMarketMessages/GetExchangeMessages existence in the CURRENT docs is thereby LIVE-DOC-confirmed. Remaining sidebar sections: REST API, DotNet API, COM API, Code Samples, API Fields Description, Symbol Naming Conventions, Diagnostic API Responses, Pricing & Sales, Contact, FAQs.

## What this pack prevents

- Building a client against hallucinated endpoints (there are NONE public — host:port is per-account).
- Importing REST epoch-milliseconds into WS parsing (dialects differ; both second-resolution).
- Treating GDF as a tick-by-tick or millisecond feed (it is a 1s-conflated L1 feed — see 12).
- Killing your own production session with a debug connect (new-connection-wins — see 02).
- Panicking on plain-text diagnostic frames or petabyte-class instrument-master frames.
- Presenting desktop-product INR prices as API prices (API pricing is quote-only).
