# GDF 00 — OPERATOR BROWSER-PASTE LIST (the definitive capture queue)

> **⚠ 2026-07-14 — DEMOTED TO FALLBACK ROUTE.** The runner route succeeded: the GitHub
> Actions workflow `docs-fetch.yml` (runs 29316509037 + 29317477759) fetched **215 pages,
> 213 HTTP 200 + 2 genuine 404s, ZERO 403s** — the WAF blocks only sandbox/Anthropic
> egress, NOT datacenter runner IPs. Every Tier-1/Tier-2 page below (and nearly everything
> else) is now `fetched-verbatim (RUNNER-DOC 2026-07-14)` — see
> [`00-COVERAGE-MANIFEST.md`](./00-COVERAGE-MANIFEST.md) for the per-URL truth. **This
> paste list is retained ONLY as a FALLBACK capture route** (for future refreshes if the
> runner route ever breaks); the per-row statuses below are the 2026-07-13 historical
> snapshot and are deliberately NOT updated.

> **Dated:** 2026-07-13 · **Purpose:** every URL below is WAF-403-blocked to ALL automated
> fetchers — re-verified 2026-07-13, **42/42 requests returned HTTP 403** across BOTH hosts
> (`globaldatafeeds.in` AND `docs.globaldatafeeds.in`) via WebFetch, which DOES reach the site
> (sandbox curl is proxy-denied before reaching it, so it gives no WAF signal). archive.org
> (Wayback/CDX) is unreachable from the research environment entirely, so no ARCHIVE-tier
> fallback exists. **The ONLY way this content enters the pack is the operator's browser.**
> **How:** open each URL in a normal browser, select-all, copy, paste the page back into a
> Claude session. Pasted content becomes **LIVE-DOC (operator-pasted)** tier — the pack's
> highest tier — exactly like the two pages already captured this way on 2026-07-13
> (SubscribeRealtime → `03` §3b; StreamAllSymbols → `10`).
> **Order:** walk down the list. Tier 1 first — those pages close client-blocking unknowns.
> Slug-unknown rows carry sidebar navigation instructions instead of a URL.

Prefix `P` = `https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis`

Capture-state legend: **NEVER CAPTURED** (no page content in the pack — slug-unknown or
search-snippet-URL-only) · **SEARCH-tier only** (substantive search-engine extraction exists,
not byte-verbatim) · **LIVE-DOC 2026-07-13** (already operator-pasted).

## Tier 1 — protocol-critical (paste these FIRST)

| # | URL / how to find it | Documents | Closes | Capture state |
|---|---|---|---|---|
| 1 | `P/websockets-api-documentation/authenticate/` | Authenticate handshake (request frame, AuthenticateResult, failure Messages) | U-10 (partial — auth failure wordings) | SEARCH-tier only |
| 2 | `P/websockets-api-documentation/function-gethistory-2/` | GetHistory (WS) — periodicities, params, row shape | U-7/U-8 (doc side) | SEARCH-tier only |
| 3 | slug unknown — open any WebSockets API page, use the sidebar → **GetLimitation** | GetLimitation — the per-key entitlement oracle (full response schema) | U-9 (doc side; live values still need the trial-key probe) | NEVER CAPTURED |
| 4 | slug unknown — WebSockets API sidebar → **GetServerInfo** | GetServerInfo (WS) — server/endpoint info function | — | NEVER CAPTURED |
| 5 | `P/streaming_api/` (landing) + sidebar → **StreamAllSnapshots** (slug unknown) | Streaming API landing + StreamAllSnapshots wire format | U-1 residual (StreamAllSnapshots shape, endpoint/cadence wording if stated) | NEVER CAPTURED (landing: SEARCH-tier only) |
| 6 | `P/websockets-api-documentation/how-to-connect-using-websockets-api/` | WS connect — `ws://endpoint:port` model | U-2 (only if the page states TLS availability) | SEARCH-tier only |
| 7 | `P/documentation-support/diagnostic-api-responses/` | The complete plain-text diagnostic message list | U-10 | SEARCH-tier only |
| 8 | `P/websockets-api-documentation/getinstruments/` | GetInstruments — instrument-master row fields (TokenNumber, ISIN, QuotationLot) | U-21 (doc side — TokenNumber semantics) | SEARCH-tier only |
| 9 | `P/websockets-api-documentation/function-getsnapshot-2/` | GetSnapshot (WS) — bar snapshot request/response | U-6 (partial — bar-stamp wording) | SEARCH-tier only |
| 10 | slug unknown — WebSockets API sidebar → **SubscribeSnapshot** | SubscribeSnapshot — streamed bar closes | U-6 (doc side; live probe still confirms) | NEVER CAPTURED |
| 11 | slug unknown — top-level 2026 sidebar section → **GetHolidays** (confirmed present in BOTH the WS and REST function lists via search-snippet nav text) | GetHolidays — 2026 section absent from every mined SDK/sample | U-24 (GetHolidays item) | NEVER CAPTURED |
| 12 | `P/documentation-support/symbol-naming-conventions/` | InstrumentIdentifier grammar (all 3 formats) | — | SEARCH-tier only |
| 13 | `P/documentation-support/api-fields-description/` | Field-name descriptions across the response families | U-15 (partial — field semantics) | SEARCH-tier only |
| 14 | `P/websockets-api-documentation/function-getlastquote-2/` | GetLastQuote (WS) | — | SEARCH-tier only |
| 15 | `P/websockets-api-documentation/getstrikeprices-2/` | GetStrikePrices (WS) | — | SEARCH-tier only |
| 16 | `P/websockets-api-documentation/gethistoryaftermarket-3/` | GetHistoryAfterMarket (WS) | U-22 (WS-side context) | SEARCH-tier only |

## Tier 2 — REST pages

| # | URL | Documents | Closes | Capture state |
|---|---|---|---|---|
| 17 | `P/rest-api-documentation/how-to-connect-using-rest-api/` | REST connect (GET-only, `accessKey` param, format switches) | U-2 (only if TLS/`https` stated) | SEARCH-tier only |
| 18 | `P/rest-api-documentation/function-gethistory/` | GetHistory (REST dialect) | U-22 (partial) | SEARCH-tier only |
| 19 | `P/rest-api-documentation/function-getsnapshot/` | GetSnapshot (REST) — NEW slug found 2026-07-13, was not in the pack's URL universe | — | NEVER CAPTURED (search-snippet URL only) |
| 20 | `P/rest-api-documentation/getexchangesnapshot/` | GetExchangeSnapshot (REST) — whole-exchange pull | — | SEARCH-tier only |
| 21 | `P/rest-api-documentation/function-getlastquotearray/` | GetLastQuoteArray (REST) | — | SEARCH-tier only |
| 22 | `P/rest-api-documentation/gethistoryaftermarket-2/` | GetHistoryAfterMarket (REST) — the missing REST path | U-22 | SEARCH-tier only |
| 23 | `P/rest-api-documentation/getlastquoteoptionchain/` | GetLastQuoteOptionChain (REST) | — | SEARCH-tier only |
| 24 | `P/rest-api-documentation/handling-special-characters/` | URL-encoding of special chars (`NIFTY 50`, `M&M`) | — | SEARCH-tier only |

## Tier 3 — Greeks / OptionChain / Delayed pages

| # | URL | Documents | Closes | Capture state |
|---|---|---|---|---|
| 25 | `P/greeks-api/subscriberealtimegreeks/` | SubscribeRealtimeGreeks | — | SEARCH-tier only |
| 26 | `P/greeks-api/getlastquoteoptiongreeks/` | GetLastQuoteOptionGreeks | — | SEARCH-tier only |
| 27 | `P/greeks-api/getlastquotearrayoptiongreeks/` | GetLastQuoteArrayOptionGreeks | — | SEARCH-tier only |
| 28 | `P/greeks-api/getlastquoteoptiongreekschain/` | GetLastQuoteOptionGreeksChain | U-20 (partial — the GetLastQuoteOptionChainWithGreeks alias question) | SEARCH-tier only |
| 29 | `P/greeks-api/gethistorygreeks-2/` | GetHistoryGreeks | U-22 (partial — XML/DD-MM-YYYY question) | SEARCH-tier only |
| 30 | `P/optionchain-api/subscribeoptionchain/` | SubscribeOptionChain (OptionChain API section) | U-20 (context) | SEARCH-tier only |
| 31 | `P/advanced_streaming_api/` + `P/advanced_streaming_api/subscribeoptionchain/` | advanced_streaming_api section landing + its SubscribeOptionChain variant | U-1/U-20 (context) | SEARCH-tier only |
| 32 | `P/delayed-api/getexchangesnapshot-delayed/` | GetExchangeSnapshot (Delayed API) | U-24 (Delayed-API item) | SEARCH-tier only |

## Tier 4 — refresh-only (already LIVE-DOC; re-paste ONLY if you suspect the page changed)

| # | URL | Documents | Closes | Capture state |
|---|---|---|---|---|
| 33 | `P/websockets-api-documentation/function-subscriberealtime/` | SubscribeRealtime (in `03` §3b, + the 2026 sidebar nav capture in README) | — | **LIVE-DOC 2026-07-13** |
| 34 | `P/streaming_api/stream-all-symbols/` | StreamAllSymbols Batch wire format (in `10`) | — | **LIVE-DOC 2026-07-13** |

## Tier 5 — commercial / entitlement

| # | URL | Documents | Closes | Capture state |
|---|---|---|---|---|
| 35 | `P/pricing-sales/api-pricing/` | API pricing page (quote-only per SEARCH tier — verify) | U-3 (partial — pricing is likely still quote-only; sales call remains) | SEARCH-tier only |
| 36 | `P/pricing-sales/who-can-purchase/` | Licensing / personal-use tier | U-5 (partial — non-display wording, if any) | SEARCH-tier only |
| 37 | `P/faqs/faqs/` | FAQ — quotas (1800/3600/7200/hr), maintenance windows, history depth | U-18 (maintenance windows), U-3 (trial-duration wording, if stated) | SEARCH-tier only |
| 38 | `P/introduction/type-of-data-available/` | Data types — the 1-second L1 feed claim | — | SEARCH-tier only |
| 39 | `P/introduction/type-of-apis-available/` | API product inventory (incl. Streaming API existence) | — | SEARCH-tier only |
| 40 | `P/introduction/introduction-to-apis/` | Intro-to-APIs page — NEW slug found 2026-07-13 | — | NEVER CAPTURED (search-snippet URL only) |
| 41 | `https://globaldatafeeds.in/apis/` | API product landing (transports: WS/REST/.NET/COM/FIX) | — | SEARCH-tier only |
| 42 | `https://globaldatafeeds.in/authorised-data-vendors/` | Vendor authorization status (NSE/BSE/MCX/NCDEX) | — | SEARCH-tier only |
| 43 | `https://globaldatafeeds.in/nimbledatapro/` | NimbleDataPro desktop product (pricing anchor, NOT the API) | — | SEARCH-tier only |

## Tier 6 — bonus assets (download / share, not just paste)

| # | URL | Documents | Closes | Capture state |
|---|---|---|---|---|
| 44 | `https://globaldatafeeds.in/wp-content/uploads/2024/07/GFDL_Websocket_Python_Sample_AllFunctions.pdf` | WS-Python all-functions sample PDF (2024) — DOWNLOAD it and share the file; likely covers the slug-unknown functions (GetLimitation/GetServerInfo/SubscribeSnapshot request shapes) | U-24 (the PDF item); potentially U-9/U-6 doc sides | NEVER CAPTURED (search-snippet URL only) |
| 45 | `P/api-code-samples/websockets-python/`, `…/websockets-java/`, `…/websockets-nodejs/`, `…/websockets-javascript-sample/`, `…/rest-python/`, `…/download-code-samples-2/` | Official code-sample pages — NEW slugs found 2026-07-13 | — | NEVER CAPTURED (search-snippet URLs only) |
| 46 | `https://globaldatafeeds.in/global-datafeeds-apis/documentation-support/documentation/websockets-api-documentation/` + `…/rest-api-documentation/` + `…/documentation-support/api-code-samples/` | Documentation index pages — NEW slugs found 2026-07-13; useful for discovering the slug-unknown leaf pages (rows 3/4/5/10/11) | — | NEVER CAPTURED (search-snippet URLs only) |
| 47 | `https://docs.globaldatafeeds.in/` + `https://docs.globaldatafeeds.in/getserverinfo-15575607e0` | The separate Fundamental Data docs portal (slug pattern `docs.globaldatafeeds.in/<function>-<10-hex>`) — GetEODStats, GetBhavCopy*, GetServerInfo | — | NEVER CAPTURED (403 on root AND deep page) |

## How to paste back

1. **One page per paste.** Open the URL, select-all, copy, paste into a Claude session.
2. **Include the URL at the top of the paste** (so the capture is provenance-stamped).
3. **Don't reformat, trim, or summarize** — the raw page text (nav sidebar included) is
   wanted verbatim; the sidebar itself is evidence (it resolved GetHolidays' existence).
4. For slug-unknown rows (3/4/5/10/11): open any nearby known page, click through the
   sidebar to the target, and paste BOTH the final URL and the page.
5. For the PDF (row 44): download and attach/share the file rather than pasting.
