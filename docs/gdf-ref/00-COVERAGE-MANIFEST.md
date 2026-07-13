# GDF 00 — COVERAGE MANIFEST (every known page/sub-section, per-URL status)

> **Dated:** 2026-07-13 · **What this is:** the COMPLETENESS MANIFEST for the GDF broker
> doc pack — one row per KNOWN page/sub-section of GDF's documentation, each with an
> explicit capture status. **This manifest pattern is now the standard for all broker doc
> packs** (operator mandate 2026-07-13): every page the vendor's own site structure
> reveals is ENUMERATED here; none is silently absent.
>
> **Enumeration basis (what "every known page" means — honest knowability caveat):**
> GDF's `robots.txt` and both sitemaps are WAF-403-blocked (42/42-request sweep,
> 2026-07-13 — recorded in the README "2026-07-13 freshness sweep" section and the
> [`00-PASTE-LIST.md`](./00-PASTE-LIST.md) header), so a machine-complete page inventory
> is UNOBTAINABLE. The enumeration below is therefore built from the three knowable
> sources, and is as complete as they allow:
> 1. **GDF's OWN sidebar navigation**, captured LIVE-DOC in the 2026-07-13 operator paste
>    of the SubscribeRealtime page (README "The CURRENT (2026) live docs tree") — the
>    authoritative section list + the full 26-entry WebSockets function list.
> 2. **The pack's URL universe** — every GDF URL cited anywhere in `docs/gdf-ref/`
>    (= the 47-row [`00-PASTE-LIST.md`](./00-PASTE-LIST.md), the capture-queue ordering
>    this manifest cross-references via `paste #N`).
> 3. **SDK/search-derived slugs** from the 2026-07-13 crawl attempt (WebSearch result
>    URLs — search-index tier, page content never fetched).
>
> Sections whose per-page slugs are unknown (REST leaf list, DotNet/COM, GainersLosers,
> Volume Shockers, Contact, section remainders) are LISTED AS SUCH — never dropped.
>
> **Status legend (exact values):**
> - `fetched-verbatim (LIVE-DOC 2026-07-13)` — operator browser-pasted, byte-verbatim in the pack. The ONLY two such pages are SubscribeRealtime (`03` §3b) and StreamAllSymbols (`10`).
> - `blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending` — URL attempted in the 2026-07-13 sweep, HTTP 403 via WebFetch (which does reach the site); a blocked page is ALSO paste-pending.
> - `operator-paste-pending` — URL known (search-snippet evidence only), fetch NEVER attempted (discovered post-crawl / crawl request budget); the operator's browser is the only route.
> - `slug-unknown → paste-pending (navigate sidebar)` — page/section confirmed to exist by the captured nav, exact URL unknown; operator navigates the live sidebar to it.

## COVERAGE SUMMARY (computed from the table below)

| Metric | Count |
|---|---|
| **Rows enumerated (pages + sections)** | **81** |
| `fetched-verbatim (LIVE-DOC 2026-07-13)` — operator pastes | **2** |
| `blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending` | **38** (36 doc pages + 2 docs-portal pages) |
| `operator-paste-pending` (URL known from search snippet, never fetched) | **12** |
| `slug-unknown → paste-pending` | **29** (19 NAMED pages: 17 WebSockets functions + StreamAllSnapshots + GetHolidays; 10 section-level/remainder rows, each representing an UNKNOWN number of pages) |

**Honest coverage fraction: 2/81 known rows live-verified (2.5%)** — and because 10 rows
are section-level remainders of unknown size, the true page denominator is ≥ 81; the
fraction is an upper bound. This is expected and fine: the WAF makes the operator's
browser the only capture route, and this manifest exists precisely so the gap is
ENUMERATED, not hidden.

**Runner-based crawl attempt (GitHub Actions `docs-fetch.yml`): PENDING** — the workflow
does NOT exist yet as of 2026-07-13. When it lands, this manifest will be updated with
per-URL runner results (a GitHub-runner egress may or may not share the WebFetch 403
verdict — unverified until run).

**Capture queue ordering:** work [`00-PASTE-LIST.md`](./00-PASTE-LIST.md) top-down
(Tier 1 = protocol-critical first). `paste #N` in the evidence column = that file's row N.
17 rows below (the non-protocol-critical WS sidebar functions, sections like DotNet/COM/
Contact, and section remainders) are NOT in the paste queue — they are enumerated here for
completeness and can be pasted opportunistically.

## THE MANIFEST TABLE

`P` = `https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis`.
403 evidence for every `blocked-by-WAF` row = the 2026-07-13 sweep (42/42 requests HTTP
403 via WebFetch across both hosts; per-URL table in the crawl agent transcript, verdict
recorded in the README freshness-sweep section + the `00-PASTE-LIST.md` header).

### Introduction

| Page | URL | Status | Evidence |
|---|---|---|---|
| Introduction To APIs | `P/introduction/introduction-to-apis/` | operator-paste-pending | search-snippet URL (new slug, 2026-07-13); never fetched · paste #40 |
| Type Of Data Available | `P/introduction/type-of-data-available/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier content in `01` · paste #38 |
| Type Of APIs Available | `P/introduction/type-of-apis-available/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier content in `01` · paste #39 |

### WebSockets API (26 function pages — the FULL list per the LIVE-DOC 2026 sidebar capture)

| Page | URL | Status | Evidence |
|---|---|---|---|
| How To Connect | `P/websockets-api-documentation/how-to-connect-using-websockets-api/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `02` · paste #6 |
| Authenticate | `P/websockets-api-documentation/authenticate/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `02` · paste #1 |
| SubscribeRealtime | `P/websockets-api-documentation/function-subscriberealtime/` | **fetched-verbatim (LIVE-DOC 2026-07-13)** | operator paste → `03` §3b (+ the sidebar nav capture in README); also 403 to automated fetch · paste #33 (refresh-only) |
| SubscribeSnapshot | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar entry (README nav capture) · paste #10 |
| GetLastQuote | `P/websockets-api-documentation/function-getlastquote-2/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `05` · paste #14 |
| GetLastQuoteShort | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar entry (README nav capture); not in paste queue |
| GetLastQuoteShortWithClose | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar entry; not in paste queue |
| GetLastQuoteArray | slug unknown — sidebar nav only (WS dialect; the REST twin's slug IS known, below) | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar entry; not in paste queue |
| GetLastQuoteArrayShort | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar entry; not in paste queue |
| GetLastQuoteArrayShortWithClose | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar entry; not in paste queue |
| GetSnapshot | `P/websockets-api-documentation/function-getsnapshot-2/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `04` · paste #9 |
| GetHistory | `P/websockets-api-documentation/function-gethistory-2/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `08` · paste #2 |
| GetHistoryAfterMarket | `P/websockets-api-documentation/gethistoryaftermarket-3/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `08` · paste #16 |
| GetExchanges | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar entry; not in paste queue |
| GetInstrumentsOnSearch | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar entry; not in paste queue |
| GetInstruments | `P/websockets-api-documentation/getinstruments/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `06` · paste #8 |
| GetInstrumentTypes | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar entry; not in paste queue |
| GetProducts | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar entry; not in paste queue |
| GetExpiryDates | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar entry; not in paste queue |
| GetOptionTypes | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar entry; not in paste queue |
| GetStrikePrices | `P/websockets-api-documentation/getstrikeprices-2/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `06` · paste #15 |
| GetServerInfo | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar entry · paste #4 |
| GetLimitation | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar entry · paste #3 (Tier 1 — entitlement oracle) |
| GetMarketMessages | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar entry; not in paste queue |
| GetExchangeMessages | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar entry; not in paste queue |
| GetExchangeSnapshot (WS dialect) | slug unknown — sidebar nav only (REST twin's slug IS known, below) | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar entry; not in paste queue |

### REST API

> The REST section's full leaf-page list was NOT LIVE-DOC-captured (the 2026 sidebar
> capture enumerated only the WebSockets function list). Known slugs below; the remainder
> row covers whatever else the section holds (a mirror of the WS function list is
> plausible — GetHolidays is snippet-confirmed in BOTH lists — but unverified).

| Page | URL | Status | Evidence |
|---|---|---|---|
| How To Connect Using REST API | `P/rest-api-documentation/how-to-connect-using-rest-api/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `09` · paste #17 |
| GetHistory (REST) | `P/rest-api-documentation/function-gethistory/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `08` · paste #18 |
| GetSnapshot (REST) | `P/rest-api-documentation/function-getsnapshot/` | operator-paste-pending | search-snippet URL (NEW slug 2026-07-13); never fetched · paste #19 |
| GetHistoryAfterMarket (REST) | `P/rest-api-documentation/gethistoryaftermarket-2/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `08` · paste #22 |
| GetLastQuoteArray (REST) | `P/rest-api-documentation/function-getlastquotearray/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `05` · paste #21 |
| GetExchangeSnapshot (REST) | `P/rest-api-documentation/getexchangesnapshot/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `04` · paste #20 |
| GetLastQuoteOptionChain (REST) | `P/rest-api-documentation/getlastquoteoptionchain/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `07` · paste #23 |
| Handling Special Characters | `P/rest-api-documentation/handling-special-characters/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `09` · paste #24 |
| (remaining REST leaf pages — count unknown) | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | REST API section confirmed in LIVE-DOC sidebar; leaf list uncaptured |

### Streaming API

| Page | URL | Status | Evidence |
|---|---|---|---|
| Streaming API landing | `P/streaming_api/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `10` · paste #5 |
| StreamAllSymbols | `P/streaming_api/stream-all-symbols/` | **fetched-verbatim (LIVE-DOC 2026-07-13)** | operator paste → `10` (Batch wire format); also 403 to automated fetch · paste #34 (refresh-only) |
| StreamAllSnapshots | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | section confirmed (docs + LIVE-DOC Streaming API section) · paste #5 |

### Advanced Streaming API

| Page | URL | Status | Evidence |
|---|---|---|---|
| Advanced Streaming API landing | `P/advanced_streaming_api/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `10` · paste #31 |
| SubscribeOptionChain (advanced-streaming variant) | `P/advanced_streaming_api/subscribeoptionchain/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `07`/`10` · paste #31 |

### OptionChain API

| Page | URL | Status | Evidence |
|---|---|---|---|
| SubscribeOptionChain | `P/optionchain-api/subscribeoptionchain/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `07` · paste #30 |
| (remaining OptionChain API leaf pages — count unknown) | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | OptionChain API is a LIVE-DOC top-level section; leaf list uncaptured |

### Greeks API

| Page | URL | Status | Evidence |
|---|---|---|---|
| SubscribeRealtimeGreeks | `P/greeks-api/subscriberealtimegreeks/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `07` · paste #25 |
| GetLastQuoteOptionGreeks | `P/greeks-api/getlastquoteoptiongreeks/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `07` · paste #26 |
| GetLastQuoteArrayOptionGreeks | `P/greeks-api/getlastquotearrayoptiongreeks/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `07` · paste #27 |
| GetLastQuoteOptionGreeksChain | `P/greeks-api/getlastquoteoptiongreekschain/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `07` · paste #28 |
| GetHistoryGreeks | `P/greeks-api/gethistorygreeks-2/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `07`/`08` · paste #29 |
| (remaining Greeks API leaf pages — count unknown; e.g. SubscribeOptionChainGreeks / GetSnapshotGreeks exist as functions in the SDKs) | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | Greeks API is a LIVE-DOC top-level section; leaf list uncaptured |

### Delayed API

| Page | URL | Status | Evidence |
|---|---|---|---|
| GetExchangeSnapshot (Delayed) | `P/delayed-api/getexchangesnapshot-delayed/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `01`/`04` · paste #32 |
| (remaining Delayed API leaf pages — count unknown; delayed twins of SubscribeSnapshot/GetSnapshot/GetHistory exist per `01`/`04`) | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | Delayed API is a LIVE-DOC top-level section; leaf list uncaptured |

### GainersLosers / Volume Shockers / GetHolidays (LIVE-DOC top-level sections)

| Page | URL | Status | Evidence |
|---|---|---|---|
| GainersLosers section | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC top-level section (README nav capture); leaf slugs unknown |
| Volume Shockers section | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC top-level section; leaf slugs unknown |
| GetHolidays | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC top-level section; ALSO snippet-confirmed in both WS + REST function lists · paste #11 (U-24) |

### DotNet API / COM API (LIVE-DOC sidebar sections)

| Page | URL | Status | Evidence |
|---|---|---|---|
| DotNet API section | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar section; leaf slugs unknown; not in paste queue |
| COM API section | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar section; leaf slugs unknown; not in paste queue |

### Code Samples

| Page | URL | Status | Evidence |
|---|---|---|---|
| WebSockets Python sample | `P/api-code-samples/websockets-python/` | operator-paste-pending | search-snippet URL (NEW 2026-07-13); never fetched · paste #45 |
| WebSockets Java sample | `P/api-code-samples/websockets-java/` | operator-paste-pending | search-snippet URL; never fetched · paste #45 |
| WebSockets NodeJS sample | `P/api-code-samples/websockets-nodejs/` | operator-paste-pending | search-snippet URL; never fetched · paste #45 |
| WebSockets JavaScript sample | `P/api-code-samples/websockets-javascript-sample/` | operator-paste-pending | search-snippet URL; never fetched · paste #45 |
| REST Python sample | `P/api-code-samples/rest-python/` | operator-paste-pending | search-snippet URL; never fetched · paste #45 |
| Download Code Samples | `P/api-code-samples/download-code-samples-2/` | operator-paste-pending | search-snippet URL; never fetched · paste #45 |
| API Code Samples index | `https://globaldatafeeds.in/global-datafeeds-apis/documentation-support/api-code-samples/` | operator-paste-pending | search-snippet URL (NEW 2026-07-13); never fetched · paste #46 |

### Documentation Support

| Page | URL | Status | Evidence |
|---|---|---|---|
| API Fields Description | `P/documentation-support/api-fields-description/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier · paste #13 |
| Symbol Naming Conventions | `P/documentation-support/symbol-naming-conventions/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `06` · paste #12 |
| Diagnostic API Responses | `P/documentation-support/diagnostic-api-responses/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `11` · paste #7 (U-10) |
| Documentation index — WebSockets API | `https://globaldatafeeds.in/global-datafeeds-apis/documentation-support/documentation/websockets-api-documentation/` | operator-paste-pending | search-snippet URL (NEW 2026-07-13); never fetched · paste #46 — useful for discovering slug-unknown leaf pages |
| Documentation index — REST API | `https://globaldatafeeds.in/global-datafeeds-apis/documentation-support/documentation/rest-api-documentation/` | operator-paste-pending | search-snippet URL (NEW 2026-07-13); never fetched · paste #46 |

### Pricing & Sales

| Page | URL | Status | Evidence |
|---|---|---|---|
| API Pricing | `P/pricing-sales/api-pricing/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `01` · paste #35 |
| Who Can Purchase | `P/pricing-sales/who-can-purchase/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `01` · paste #36 |

### Contact

| Page | URL | Status | Evidence |
|---|---|---|---|
| Contact section | slug unknown — sidebar nav only | slug-unknown → paste-pending (navigate sidebar) | LIVE-DOC sidebar section; not in paste queue |

### FAQs

| Page | URL | Status | Evidence |
|---|---|---|---|
| FAQs | `P/faqs/faqs/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `01`/`11` · paste #37 (U-18/U-3) |

### Top-level site pages + assets

| Page | URL | Status | Evidence |
|---|---|---|---|
| APIs product landing | `https://globaldatafeeds.in/apis/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `01` · paste #41 |
| Authorised Data Vendors | `https://globaldatafeeds.in/authorised-data-vendors/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `01` · paste #42 |
| NimbleDataPro (desktop product) | `https://globaldatafeeds.in/nimbledatapro/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | SEARCH-tier in `01` · paste #43 |
| WS-Python all-functions sample PDF (2024) | `https://globaldatafeeds.in/wp-content/uploads/2024/07/GFDL_Websocket_Python_Sample_AllFunctions.pdf` | operator-paste-pending | search-snippet URL; fetch NOT attempted (crawl request budget) · paste #44 — DOWNLOAD + share the file, don't paste |

### docs.globaldatafeeds.in portal (separate Fundamental Data docs)

| Page | URL | Status | Evidence |
|---|---|---|---|
| Portal root | `https://docs.globaldatafeeds.in/` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | 403 on root · paste #47 |
| GetServerInfo (portal) | `https://docs.globaldatafeeds.in/getserverinfo-15575607e0` | blocked-by-WAF (HTTP 403, 2026-07-13) → paste-pending | 403 on the known deep page · paste #47 |
| (remaining portal function pages — GetEODStats, GetBhavCopyCM, GetBhavCopyFO, …; slug pattern `<function>-<10-hex>`, exact slugs unknown) | slug unknown — pattern only | slug-unknown → paste-pending (navigate sidebar) | pattern evidence from the getserverinfo slug · paste #47 |

## Discovery infrastructure attempted (not doc pages — evidence only, not counted above)

| URL | Result (2026-07-13) |
|---|---|
| `https://globaldatafeeds.in/robots.txt` | HTTP 403 (WebFetch) — no sitemap pointers obtainable |
| `https://globaldatafeeds.in/sitemap.xml` | HTTP 403 |
| `https://globaldatafeeds.in/wp-sitemap.xml` | HTTP 403 |
| archive.org (Wayback availability API + CDX) | unreachable from the research environment entirely — no ARCHIVE-tier fallback, no slug discovery via CDX |
