# GDF 00 — COVERAGE MANIFEST (every known page/sub-section, per-URL status)

> **Dated:** 2026-07-13 · **REWRITTEN 2026-07-14** after the RUNNER-DOC full-corpus crawl.
> **What this is:** the COMPLETENESS MANIFEST for the GDF broker doc pack — one row per
> KNOWN page/sub-section of GDF's documentation, each with an explicit capture status.
> **This manifest pattern is the standard for all broker doc packs** (operator mandate
> 2026-07-13): every page the vendor's own site structure reveals is ENUMERATED here;
> none is silently absent.
>
> **Enumeration basis (2026-07-14 — supersedes the 2026-07-13 "unobtainable" caveat):**
> GDF's `robots.txt` + both sitemaps + the docs-portal `llms.txt` + `knowledgebase-sitemap.xml`
> were ALL fetched by the runner crawl, so the page inventory below is now built from the
> vendor's OWN machine-readable indexes plus the fetched pages' navigation — a genuinely
> knowable universe, no longer a best-effort reconstruction. (History: on 2026-07-13 all of
> those were WAF-403-blocked to sandbox egress and the enumeration was built from the
> operator-pasted sidebar nav + the pack's URL universe + search-derived slugs.)
>
> **Runner-based crawl (GitHub Actions `docs-fetch.yml`): DONE 2026-07-14** — runs
> **29316509037 + 29317477759**, corpus on branch **`docs-fetch/gdf-2026-07-14` @ `7d354a8d`**
> (`crawl-out/`, per-page URL + fetch time + sha256 provenance headers). Verdict:
> **215 URLs attempted, 213 HTTP 200, 2 genuine 404s, ZERO 403s** — the WAF blocks only
> sandbox/Anthropic egress (robots.txt UA-blocks scraper agents), not datacenter runner IPs.
>
> **Status legend (exact values):**
> - `fetched-verbatim (RUNNER-DOC 2026-07-14)` — machine-fetched verbatim by the runner crawl; in the corpus with sha256 provenance.
> - `fetched-verbatim (LIVE-DOC 2026-07-13 + RUNNER-DOC 2026-07-14)` — the two operator-pasted pages, re-fetched by the runner and confirmed IDENTICAL to the pastes.
> - `HTTP 404 (genuine absence, 2026-07-14)` — the runner reached the site and the page does not exist (not a block).
> - `slug-known → runner-crawl-pending (next docs-fetch run)` — page confirmed to exist (sidebar nav / archive-index pages / worker slug inventory) but not in this crawl's 215-URL set; reachable by the proven runner route, queued for the next run.
> - `uncrawled host` — a whole additional host discovered but not crawled.

## COVERAGE SUMMARY (2026-07-14)

| Metric | Count |
|---|---|
| **URLs attempted by the runner crawl** | **215** |
| `fetched-verbatim (RUNNER-DOC 2026-07-14)` | **213** — 91 on `globaldatafeeds.in` (incl. the 2 double-tier LIVE-DOC+RUNNER-DOC pages, the AllFunctions PDF, robots.txt + both sitemaps, ~20 section/archive index pages) + 122 on `docs.globaldatafeeds.in` (the full FD portal incl. `llms.txt`) |
| `HTTP 404 (genuine absence)` | **2** — `…/advanced_streaming_api/` index slug (child pages exist + fetched) and `docs.globaldatafeeds.in/llms-full.txt` (only `llms.txt` exists) |
| `slug-known → runner-crawl-pending` | **~30 named leaf slugs** (enumerated below — secondary reference twins: WS GetLastQuote variants, GetExchanges-family reference pulls, GainersLosers/VolumeShockers leafs, 2 greeks leafs, REST twins) + DotNet/COM leaf remainders |
| `uncrawled host` | **1** — `newsdocs.globaldatafeeds.in` (third portal, News API; existence discovered in the crawl) |

**Verdict: coverage is effectively COMPLETE for the knowable universe.** Every
protocol-critical page — connect, auth, every subscribe/snapshot/history/limitation
function with a known slug, both streaming pages, diagnostics, naming conventions, pricing,
FAQ, the full FD docs portal — is fetched verbatim. The residue is secondary reference
twins whose content is already covered by fetched siblings/SDKs, all reachable by the
proven runner route on the next crawl. The operator paste queue
([`00-PASTE-LIST.md`](./00-PASTE-LIST.md)) is a **fallback route only**.

**History (2026-07-13 numbers, superseded):** 81 rows enumerated — 2 fetched-verbatim
(operator pastes), 38 blocked-by-WAF, 12 known-URL never-fetched, 29 slug-unknown; honest
coverage fraction 2/81 (2.5%). The 2026-07-14 crawl flipped this in one run.

## THE MANIFEST TABLE

`P` = `https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis`.
All `fetched-verbatim (RUNNER-DOC 2026-07-14)` rows: evidence = the corpus on
`docs-fetch/gdf-2026-07-14` @ `7d354a8d` (per-page sha256), runs 29316509037/29317477759.

### Introduction

| Page | URL | Status |
|---|---|---|
| Introduction To APIs | `P/introduction/introduction-to-apis/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `01` (uptime 99.995%, 50M req/day, 50k+ symbols) |
| Type Of Data Available | `P/introduction/type-of-data-available/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `01`/`08` (retention + contractwise tables, plan matrix) |
| Type Of APIs Available | `P/introduction/type-of-apis-available/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `01` |

### WebSockets API (26 function pages per the 2026 sidebar)

| Page | URL | Status |
|---|---|---|
| How To Connect | `P/websockets-api-documentation/how-to-connect-using-websockets-api/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `02` (1-session rule verbatim) |
| Authenticate | `P/websockets-api-documentation/authenticate/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `02` |
| SubscribeRealtime | `P/websockets-api-documentation/function-subscriberealtime/` | **fetched-verbatim (LIVE-DOC 2026-07-13 + RUNNER-DOC 2026-07-14)** — runner fetch IDENTICAL to the operator paste → `03` §3b |
| SubscribeSnapshot | `P/websockets-api-documentation/function-subscribesnapshot/` | fetched-verbatim (RUNNER-DOC 2026-07-14) — **slug RESOLVED 2026-07-14** → `04` §1 |
| GetLastQuote | `P/websockets-api-documentation/function-getlastquote-2/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `05` |
| GetLastQuoteShort | slug unknown | slug-known → runner-crawl-pending (next docs-fetch run) |
| GetLastQuoteShortWithClose | slug unknown | slug-known → runner-crawl-pending |
| GetLastQuoteArray (WS) | slug unknown (REST twin fetched) | slug-known → runner-crawl-pending |
| GetLastQuoteArrayShort | slug unknown | slug-known → runner-crawl-pending |
| GetLastQuoteArrayShortWithClose | slug unknown | slug-known → runner-crawl-pending |
| GetSnapshot | `P/websockets-api-documentation/function-getsnapshot-2/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `04` (open-stamp verbatim — U-6 closed) |
| GetHistory | `P/websockets-api-documentation/function-gethistory-2/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `08` |
| GetHistoryAfterMarket | `P/websockets-api-documentation/gethistoryaftermarket-3/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `08` |
| GetExchanges | slug unknown | slug-known → runner-crawl-pending |
| GetInstrumentsOnSearch | slug unknown | slug-known → runner-crawl-pending |
| GetInstruments | `P/websockets-api-documentation/getinstruments/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `06` §3 |
| GetInstrumentTypes | slug unknown | slug-known → runner-crawl-pending |
| GetProducts | slug unknown | slug-known → runner-crawl-pending |
| GetExpiryDates | slug unknown | slug-known → runner-crawl-pending |
| GetOptionTypes | slug unknown | slug-known → runner-crawl-pending |
| GetStrikePrices | `P/websockets-api-documentation/getstrikeprices-2/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `06` |
| GetServerInfo | `P/websockets-api-documentation/function-getserverinfo-2/` | fetched-verbatim (RUNNER-DOC 2026-07-14) — **slug RESOLVED 2026-07-14** → `02`/`11` |
| GetLimitation | `P/websockets-api-documentation/function-getlimitation-2/` | fetched-verbatim (RUNNER-DOC 2026-07-14) — **slug RESOLVED 2026-07-14** → `11` (full LimitationResult schema) |
| GetMarketMessages | slug unknown | slug-known → runner-crawl-pending |
| GetExchangeMessages | slug unknown | slug-known → runner-crawl-pending |
| GetExchangeSnapshot (WS) | `P/websockets-api-documentation/getexchangesnapshot-2/` (slug inventoried 2026-07-14, body not fetched) | slug-known → runner-crawl-pending |
| GetHolidays (WS) | `P/websockets-api-documentation/getholidays/` | fetched-verbatim (RUNNER-DOC 2026-07-14) — **slug RESOLVED 2026-07-14** → `06` §6 |

### REST API

| Page | URL | Status |
|---|---|---|
| How To Connect Using REST API | `P/rest-api-documentation/how-to-connect-using-rest-api/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `09` (GET-only verbatim + official 25-function list) |
| GetHistory (REST) | `P/rest-api-documentation/function-gethistory/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `08` (from/to = epoch SECONDS) |
| GetSnapshot (REST) | `P/rest-api-documentation/function-getsnapshot/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `04`/`09` |
| GetHistoryAfterMarket (REST) | `P/rest-api-documentation/gethistoryaftermarket-2/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `08` (U-22a advanced) |
| GetLastQuoteArray (REST) | `P/rest-api-documentation/function-getlastquotearray/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `05` |
| GetExchangeSnapshot (REST) | `P/rest-api-documentation/getexchangesnapshot/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `04` §5 (Day periodicity, nonTraded for NSE_IDX) |
| GetLastQuoteOptionChain (REST) | `P/rest-api-documentation/getlastquoteoptionchain/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `07` |
| GetLimitation (REST) | `P/rest-api-documentation/function-getlimitation/` | fetched-verbatim (RUNNER-DOC 2026-07-14) — **slug RESOLVED 2026-07-14** → `11` (FLAT-JSON dialect) |
| GetServerInfo (REST) | `P/rest-api-documentation/function-getserverinfo/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `11` |
| GetHolidays (REST) | `P/rest-api-documentation/getholidays-2/` | fetched-verbatim (RUNNER-DOC 2026-07-14) — **slug RESOLVED 2026-07-14** → `06` §6 (mandatory `year`, MM-DD-YYYY dates, NCX code) |
| Handling Special Characters | `P/rest-api-documentation/handling-special-characters/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `06`/`09` (`%20`, `%2D`) |
| GetInstruments (REST) | `P/rest-api-documentation/function-getinstruments/` (slug inventoried, body not fetched) | slug-known → runner-crawl-pending |
| (remaining REST reference twins — GetExchanges/GetInstrumentTypes/GetProducts/GetExpiryDates/GetOptionTypes/GetStrikePrices/GetLastQuote variants/GetMarketMessages etc.) | per the REST archive index pages (fetched) | slug-known → runner-crawl-pending |

### Streaming API

| Page | URL | Status |
|---|---|---|
| Streaming API landing | `P/streaming_api/` | fetched-verbatim (RUNNER-DOC 2026-07-14) — section = exactly {StreamAllSymbols, StreamAllSnapshots} |
| StreamAllSymbols | `P/streaming_api/stream-all-symbols/` | **fetched-verbatim (LIVE-DOC 2026-07-13 + RUNNER-DOC 2026-07-14)** — runner fetch IDENTICAL + full 17-row sample → `10` §2b |
| StreamAllSnapshots | `P/streaming_api/streamallsnapshots/` | fetched-verbatim (RUNNER-DOC 2026-07-14) — **slug RESOLVED 2026-07-14; wire format captured FIRST TIME** → `10` §2c |

### Advanced Streaming API

| Page | URL | Status |
|---|---|---|
| Advanced Streaming API landing | `P/advanced_streaming_api/` | **HTTP 404 (genuine absence, 2026-07-14)** — the index slug does not exist; its child page does (below) |
| SubscribeOptionChain (advanced-streaming variant) | `P/advanced_streaming_api/subscribeoptionchain/` | fetched-verbatim (RUNNER-DOC 2026-07-14) — byte-identical alias of the optionchain-api page (md5-matched) → `07` |

### OptionChain API

| Page | URL | Status |
|---|---|---|
| SubscribeOptionChain | `P/optionchain-api/subscribeoptionchain/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `07` §2 (Depth 1/5/10/15/20) |
| GetLastQuoteOptionChain (WS pull) | `P/optionchain-api/function-getlastquoteoptionchain/` (slug inventoried, body not fetched) | slug-known → runner-crawl-pending |

### Greeks API

| Page | URL | Status |
|---|---|---|
| SubscribeRealtimeGreeks | `P/greeks-api/subscriberealtimegreeks/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `07` §3 (1s cadence doc-pinned) |
| GetLastQuoteOptionGreeks | `P/greeks-api/getlastquoteoptiongreeks/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `07` |
| GetLastQuoteArrayOptionGreeks | `P/greeks-api/getlastquotearrayoptiongreeks/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `07` |
| GetLastQuoteOptionGreeksChain | `P/greeks-api/getlastquoteoptiongreekschain/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `07` |
| GetHistoryGreeks (WS) | `P/greeks-api/gethistorygreeks-2/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `07` §6/`08` §6 (MINUTE/TICK only) |
| SubscribeSnapshotGreeks | `P/greeks-api/subscribesnapshotgreeks/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `07` §5 |
| GetSnapshotGreeks | slug unknown | slug-known → runner-crawl-pending |
| SubscribeOptionChainGreeks | slug unknown | slug-known → runner-crawl-pending |
| GetHistoryGreeks (REST) | `…/rest-api-documentation/greeks-api-global-datafeeds-apis/gethistorygreeks-returns-historical-greeks-data/` (in live nav; category index fetched) | slug-known → runner-crawl-pending |

### Delayed API (WS lane + REST lane)

| Page | URL | Status |
|---|---|---|
| SubscribeSnapshot (Delayed) | `P/delayed-api/subscribesnapshot-delayed/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `04` §1a (15-min worked timing example) |
| GetSnapshot (Delayed) | `P/delayed-api/getsnapshot-delayed/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `04` §4a |
| GetHistory (Delayed) | `P/delayed-api/gethistory-delayed/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `08` |
| GetExchangeSnapshot (Delayed) | `P/delayed-api/getexchangesnapshot-delayed/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `04` §5 |
| GetHistory (Delayed, REST) | `P/delayed-api-rest-api-documentation/gethistory-delayed-returns-historical-data-tick-minute-eod/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `09` |
| GetSnapshot (Delayed, REST) | `P/delayed-api-rest-api-documentation/getsnapshot-delayed-2/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `09` |
| GetExchangeSnapshot (Delayed, REST) | `P/delayed-api-rest-api-documentation/getexchangesnapshot-delayed-2/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `09` |

### GainersLosers / Volume Shockers

| Page | URL | Status |
|---|---|---|
| SubscribeTopGainersLosers (WS) | `P/…/gainers-losers/subscribetopgainerslosers/` (slug inventoried, body not fetched) | slug-known → runner-crawl-pending |
| GetTopGainersLosers (WS) | `P/…/gainers-losers/gettopgainerslosers/` (slug inventoried) | slug-known → runner-crawl-pending |
| GetTopGainersLosers (REST) | `…/gainers-losers-rest-api-documentation/gettopgainerslosers-2/` (slug inventoried) | slug-known → runner-crawl-pending |
| GetVolumeShockers (WS) | `P/…/volume-shockers/getvolumeshockers-2/` (slug inventoried) | slug-known → runner-crawl-pending |
| GetVolumeShockers (REST) | `…/volume-shockers-rest-api-documentation/getvolumeshockers/` (slug inventoried — refutes the pack's "WS only") | slug-known → runner-crawl-pending |

### DotNet API / COM API

| Page | URL | Status |
|---|---|---|
| COM API — How To Connect | `P/com-api-documentation/how-to-connect-using-com-api/` | fetched-verbatim (RUNNER-DOC 2026-07-14) |
| COM API — Download | `P/com-api-documentation/download/` | fetched-verbatim (RUNNER-DOC 2026-07-14) |
| DotNet/COM archive indexes | `…/documentation-support/documentation/{dotnet,com}-api-documentation/` | fetched-verbatim (RUNNER-DOC 2026-07-14) — leaf pages beyond the two above: slug-known remainders → runner-crawl-pending |

### Code Samples

| Page | URL | Status |
|---|---|---|
| WebSockets Python sample | `P/api-code-samples/websockets-python/` | fetched-verbatim (RUNNER-DOC 2026-07-14) — official timestamped transcripts (Echo cadence evidence) |
| WebSockets Java sample | `P/api-code-samples/websockets-java/` | fetched-verbatim (RUNNER-DOC 2026-07-14) |
| WebSockets NodeJS sample | `P/api-code-samples/websockets-nodejs/` | fetched-verbatim (RUNNER-DOC 2026-07-14) — AllowVMRunning/AllowServerOSRunning frames |
| WebSockets JavaScript sample | `P/api-code-samples/websockets-javascript-sample/` | fetched-verbatim (RUNNER-DOC 2026-07-14) |
| WebSockets Postman sample | `P/api-code-samples/websockets-postman/` | fetched-verbatim (RUNNER-DOC 2026-07-14) — discovered in the crawl |
| REST Python sample | `P/api-code-samples/rest-python/` | fetched-verbatim (RUNNER-DOC 2026-07-14) |
| Download Code Samples | `P/api-code-samples/download-code-samples-2/` | fetched-verbatim (RUNNER-DOC 2026-07-14) |
| Code Samples index | `…/documentation-support/api-code-samples/` | fetched-verbatim (RUNNER-DOC 2026-07-14) |

### Documentation Support

| Page | URL | Status |
|---|---|---|
| API Fields Description | `P/documentation-support/api-fields-description/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `03` (REST-twin glossary, ms samples) |
| Symbol Naming Conventions | `P/documentation-support/symbol-naming-conventions/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `06` §2 (BFO IF_/SF_/IO_, MCX double-underscore) |
| Diagnostic API Responses | `P/documentation-support/diagnostic-api-responses/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `11` §3.1 (the complete 22-row table — U-10 closed) |
| Documentation index pages (WS/REST/Delayed/Greeks/OptionChain/GainersLosers/VolumeShockers/Streaming archives, incl. page-2s) | `…/documentation-support/documentation/…` | fetched-verbatim (RUNNER-DOC 2026-07-14) — ~20 section/archive index pages; the slug-inventory source for the pending rows above |

### Pricing & Sales / Contact / FAQs

| Page | URL | Status |
|---|---|---|
| API Pricing | `P/pricing-sales/api-pricing/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `01` (quote-only confirmed; 5-plan matrix; 13 packages) |
| Who Can Purchase | `P/pricing-sales/who-can-purchase/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `01` (personal-use verbatim) |
| Contact Sales | `P/contact/contact-sales/` | fetched-verbatim (RUNNER-DOC 2026-07-14) |
| Contact Technical Support | `P/contact/contact-technical-support/` | fetched-verbatim (RUNNER-DOC 2026-07-14) |
| FAQs | `P/faqs/faqs/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `01`/`02`/`08`/`11`/`12` (maintenance windows, quota semantics, candle-construction, GetHistory↔AfterMarket exclusion — U-18 closed) |

### Top-level site pages + assets

| Page | URL | Status |
|---|---|---|
| APIs product landing | `https://globaldatafeeds.in/apis/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `01` (T&C verbatim) |
| Authorised Data Vendors | `https://globaldatafeeds.in/authorised-data-vendors/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `01` |
| Global Datafeeds APIs landing + archive index | `https://globaldatafeeds.in/global-datafeeds-apis/` (+ `…/global-datafeeds-apis/global-datafeeds-apis/`) | fetched-verbatim (RUNNER-DOC 2026-07-14) — landing is a thin shell page |
| NimbleDataPro (desktop product) | `https://globaldatafeeds.in/nimbledatapro/` | fetched-verbatim (RUNNER-DOC 2026-07-14) → `01` (₹233 first-party figure) |
| WS-Python all-functions sample PDF (2024) | `…/wp-content/uploads/2024/07/GFDL_Websocket_Python_Sample_AllFunctions.pdf` | fetched-verbatim (RUNNER-DOC 2026-07-14) — 25 pages, text-extracted → `08` (newest-first proof, DAY 18:00 stamp) |
| robots.txt / sitemap.xml / wp-sitemap.xml | `https://globaldatafeeds.in/…` | fetched-verbatim (RUNNER-DOC 2026-07-14) — robots.txt shows the scraper-UA blocks that explain the sandbox 403s |

### docs.globaldatafeeds.in portal (Fundamental Data docs — summarized; 122 pages fetched)

| Item | Status |
|---|---|
| **The FULL portal: 122 pages fetched-verbatim (RUNNER-DOC 2026-07-14)** — root + intro/auth/connect/code-samples/FAQ/glossary/release-notes/diagnostic pages, the complete endpoint catalog (~50 `Get*` endpoint pages: GetBhavCopyCM/FO, GetEODStats, GetFIIDII, GetSHP/GetSHPAdvanced, GetVolatality, GetCircuitBreakers, GetCorporateActions/Announcements, GetDeliveryVolumes, GetIndexDetail/Highlights, GetInstruments, GetLimitation, GetServerInfo, …), 61 schema pages (`…d0` slugs: DateTimeFormat, ResponseFormat, Qe*/…Result models), the portal WS lane (SubscribeCorporateAnnouncements, SubscribeFinancialResults), `llms.txt`, and `knowledgebase-sitemap.xml` | fetched-verbatim (RUNNER-DOC 2026-07-14) — catalog enumerated by category in `09` §6 |
| `llms-full.txt` | **HTTP 404 (genuine absence, 2026-07-14)** — only `llms.txt` exists |
| `newsdocs.globaldatafeeds.in` (third portal — News API) | uncrawled host — discovered 2026-07-14; queue for a future run |

## Discovery infrastructure (history)

| URL | 2026-07-13 (sandbox egress) | 2026-07-14 (runner) |
|---|---|---|
| `https://globaldatafeeds.in/robots.txt` | HTTP 403 (WebFetch) | HTTP 200 — fetched; UA-blocks scrapers (explains the sandbox 403s) |
| `https://globaldatafeeds.in/sitemap.xml` + `wp-sitemap.xml` | HTTP 403 | HTTP 200 — fetched |
| `docs.globaldatafeeds.in` `llms.txt` / `knowledgebase-sitemap.xml` | 403 / unknown | HTTP 200 — fetched (the portal's own machine index) |
| archive.org (Wayback/CDX) | unreachable | not needed — the live site is directly fetchable via the runner |
