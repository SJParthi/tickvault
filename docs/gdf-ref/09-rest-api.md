# GDF 09 — REST API

> **Source:** https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/rest-api-documentation/how-to-connect-using-rest-api/ · …/rest-api-documentation/handling-special-characters/ · per-function REST pages (inventory in §3)
> **Fetched:** 2026-07-13 · **Evidence tier:** SEARCH (connect page) + CLIENT-LIB-SOURCE(gfdl-rest@1.0.3 :: base_api.py + 27 endpoint classes + README). Verbatim samples from the official REST package.

---

## 2026-07-14 RUNNER-DOC reconciliation (machine-fetched live docs, GitHub runner @ 7d354a8d)

The REST connect page, special-characters page, per-function REST pages, FAQ, and the entire
separate docs.globaldatafeeds.in Fundamental-Data portal (123 pages incl. its llms.txt machine
index) were fetched verbatim 2026-07-14. Upgrades to **RUNNER-DOC (2026-07-14, \<url\>)**;
corrections integrated in place below, dated.

| # | Verdict | Fact | Source |
|---|---|---|---|
| 1 | CONFIRMED (tier↑) | GET-only, doc-verbatim: "Our RESTful API accepts and responds to only HTTP GET requests. HTTP POST request or any other type of request WILL NOT work"; syntax `http://endpoint:port/function_name/?accesskey=apikey`; `&xml=true` and `&format=CSV` switch examples verbatim; plain-text diagnostic bodies | RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/rest-api-documentation/how-to-connect-using-rest-api/) |
| 2 | CONFIRMED (U-16 PARTIAL) | Casing is mixed INSIDE the official docs themselves: connect page `accesskey=`, every per-function param table `accessKey`, the REST GetHistory example URL even sends `IsShortIdentifier=`. Case-insensitive server handling now strongly implied; still live-verify | connect page + function pages |
| 3 | NEW | The connect page's official "List of REST API Functions" (25 rows) — matches §3 and additionally omits GetHistoryAfterMarket / GetTopGainersLosers / GetVolumeShockers / GetHolidays / GetHistoryGreeks even though dedicated REST doc pages exist for each in the same nav: the connect-page list is NOT exhaustive | connect page + nav |
| 4 | **DRIFT-CORRECTED** | §3 GetHistory row said "from/to epoch-ms on REST" — REFUTED: the REST GetHistory doc sends `from=1716269700&to=1718861700` (epoch SECONDS, worked examples "no. of seconds since Epoch"). Request-side REST timestamps are SECONDS | RUNNER-DOC (2026-07-14, …/rest-api-documentation/function-gethistory/) |
| 5 | **DRIFT-CORRECTED** | §2 "epoch ms, ALWAYS trailing 000" — the GetLastQuoteArray doc's own JSON sample carries `"LASTTRADETIME": 1415114727418` (real sub-second ms, no 000). "Always trailing 000" downgraded to "usually" | RUNNER-DOC (2026-07-14, …/rest-api-documentation/function-getlastquotearray/) |
| 6 | **DISCREPANCY (live-verify)** | REST response timestamp unit: doc PROSE on GetHistory/GetSnapshot/GetLastQuoteArray says "In JSON Response … no. of SECONDS since Epoch", yet the SAME pages' JSON/CSV samples show 13-digit ms (`1434710918000`, `1713851880000`, `1415114727418`). Samples + SDK dumps agree on ms; parse both defensively | function-getsnapshot / function-getlastquotearray / function-gethistory pages |
| 7 | CONFIRMED + verbatim | GetSnapshot REST: `periodicity ∈ {MINUTE, HOUR}` default MINUTE; ≤25 identifiers; returns `INSTRUMENTIDENTIFIER, EXCHANGE, LASTTRADETIME, TRADEDQTY, OPENINTEREST, OPEN, HIGH, LOW, CLOSE`; verbatim JSON/XML/CSV samples — XML envelope `<SnapshotArray>` with HUMAN timestamps `LastTradeTime="6-19-2015 1:48:38 PM"` (M-D-YYYY 12-h — a FOURTH timestamp dialect, XML-only) | RUNNER-DOC (2026-07-14, …/rest-api-documentation/function-getsnapshot/) |
| 8 | CONFIRMED + extended | GetLastQuoteArray REST: ≤25, `+`-joined `instrumentIdentifiers`, plural `isShortIdentifiers`; return list now doc-verbatim incl. **PriceChange, PriceChangePercentage, OpenInterestChange** (present in the CSV sample; absent from the older JSON sample); XML envelope `<RealtimeArray>` (human timestamps) | function-getlastquotearray page |
| 9 | CONFIRMED + extended | GetLastQuoteOptionChain REST: params exchange+product mandatory, expiry/optionType/strikePrice optional ("If absent, result is sent for all active Expiries/OptionTypes/Strike Prices"); return list adds **TotalBuyQty, TotalSellQty**; downloadable JSON/XML/CSV sample zips | RUNNER-DOC (2026-07-14, …/rest-api-documentation/getlastquoteoptionchain/) |
| 10 | NEW function | **GetHolidays** (REST `GetHolidays/`, WS twin page exists): params accessKey, `exchange` (optional), `Year` (mandatory, e.g. 2025), xml. Response `{"Value":[{"Year":2025,"Date":"01-26-2025","Description":"Republic Day","Exchanges":"MCX;NSE;NSE_IDX;NFO;CDS;BSE;BSE_IDX;BSE_DEBT;BFO;NCX"}]}` — note **MM-DD-YYYY** date strings and an exchange list including `NCX`; the page's example-URL href mistakenly links GetMarketMessages (official doc copy-paste bug) | RUNNER-DOC (2026-07-14, …/rest-api-documentation/getholidays-2/) |
| 11 | CONFIRMED (U-22 PARTIAL) | GetHistoryAfterMarket REST doc page EXISTS (`…/rest-api-documentation/gethistoryaftermarket-2/`, params = REST GetHistory's); path per the universal scheme = `GetHistoryAfterMarket/` (no verbatim URL on the page). A dedicated REST GetHistoryGreeks page also exists in nav (unfetched this run) — the SDK's `GetHistory/`-path post reads as an SDK bug | 08 §5/§6 + nav |
| 12 | CONFIRMED (tier↑) | URL-encoding rule + verbatim table now incl. `NIFTY BANK → NIFTY%20BANK` and `MCDOWELL-N → MCDOWELL%2DN` (hyphen encoded too); page states it "is applicable only to users of REST API" (WS sends as-is) | RUNNER-DOC (2026-07-14, …/rest-api-documentation/handling-special-characters/) |
| 13 | CONFIRMED (tier↑) | Quota tiers doc-verbatim: "this limit could be 1800, 3600, or 7200 requests per hour"; hourly reset; subscribe+unsubscribe+resubscribe each consume quota (100+100+100 worked example); symbol-limit breach message "Reached instrument limitation" | RUNNER-DOC (2026-07-14, …/faqs/faqs/) |
| 14 | NEW (U-2-adjacent) | TLS sightings: the REST GetHistory doc example uses `https://endpoint:port/…`; the Fundamental-Data portal's live examples use `https://test.lisuns.com:4532/…`; the FAQ telnet guide names WS test endpoint `test.lisuns.com` port `4575` ("some test endpoints are closed over weekends and market holidays"). Main connect page still `http://` — TLS on the per-account market-data endpoint stays a live/sales question | function-gethistory + portal pages + FAQ |
| 15 | NEW | Delayed-API REST lane confirmed: GetHistory (Delayed) / GetSnapshot (Delayed) / GetExchangeSnapshot (Delayed) each have dedicated REST doc pages with the standard param tables | RUNNER-DOC (2026-07-14, …/delayed-api-rest-api-documentation/*) |
| 16 | REWRITTEN | §6 (Fundamental-Data portal) rewritten below from the 2026-07-14 crawl of docs.globaldatafeeds.in (llms.txt + ~122 pages) | see §6 |

---

## 1. Transport & auth

| Rule | Detail |
|---|---|
| Base URL | per-account `http://endpoint:port/` (assigned on purchase; one SDK README also shows `https://endpoint:port` — TLS availability **Unknown**, U-2. 2026-07-14: the REST GetHistory doc example ALSO uses `https://endpoint:port`, and the FD portal's live examples are `https://test.lisuns.com:4532` — see reconciliation row 14) |
| Method | **HTTP GET only** — doc verbatim (RUNNER-DOC 2026-07-14): "accepts and responds to only HTTP GET requests. HTTP POST request or any other type of request WILL NOT work". All params in the query string; no bodies, no headers |
| Auth | query param **`accessKey=<API_KEY>`** on every call. ⚠ casing: the connect page shows `accesskey=` (lowercase) while every per-function param table shows `accessKey` and one example URL sends `IsShortIdentifier=` — the official docs mix casing themselves (RUNNER-DOC 2026-07-14); assume case-insensitive, verify live (U-16). Sessionless — REST is the parallel-pull path beside the 1-session WS |
| Path scheme | `http://endpoint:port/<FunctionName>/` — path segment = EXACT function name + trailing slash. Example (doc verbatim): `http://endpoint:port/GetExchanges/?accesskey=apikey` |
| Format switches | JSON default; `&xml=true` → XML (`Content-Type: application/xml|text/xml`); `&format=CSV` → CSV (`text/csv`). The SDK dispatches purely on response Content-Type |
| Diagnostics | text/plain diagnostic bodies may replace JSON/XML (expired/invalid key etc.) — handle non-JSON bodies (11 §3) |

## 2. Response dialect (differs from WS!)

| Aspect | REST | WS |
|---|---|---|
| Key casing | **ALL-CAPS** (`LASTTRADEPRICE`, `INSTRUMENTIDENTIFIER`) | PascalCase |
| Timestamps (RESPONSE) | epoch **MILLISECONDS**, usually trailing `000` (second resolution) — e.g. `"LASTTRADETIME": 1724219587000`. Corrected 2026-07-14: not ALWAYS `000` — the GetLastQuoteArray doc sample carries `1415114727418`; and doc PROSE claims "seconds" while every doc sample shows ms (parse both defensively, live-verify). REST **XML** responses use a 4th dialect: HUMAN `M-D-YYYY h:mm:ss AM/PM` attribute strings | epoch SECONDS |
| Timestamps (REQUEST) | `from`/`to` query params are epoch **SECONDS** (RUNNER-DOC 2026-07-14, REST GetHistory example `from=1716269700`) | epoch SECONDS |
| Envelopes | single-quote → bare object; arrays → bare array; reference → `{"EXCHANGES":[…]}` / `{"INSTRUMENTS":[…]}` / `{"EXPIRYDATES":[…]}`; history → `{"USERTAG":…,"OHLC":[…]|"TICK":[…]}` | MessageType-tagged objects |
| Symbols in arrays | joined with `+` in one query param: `instrumentIdentifiers=NIFTY-I+BANKNIFTY-I+FINNIFTY-I` | `[{"Value":…},…]` |

Verbatim REST rows (gfdl-rest@1.0.3 README): `{'CLOSE': 24757.3, 'HIGH': 24794, 'LASTTRADETIME': 1724243400000, …}` (daily bar — 18:00 IST vendor stamp, 04 §3) and `{'LASTTRADETIME': 1724219587000, … 'SERVERTIME': 1724219587000}` (quote).

## 3. Per-function paths (all GET, all + `accessKey`)

| Path | Params (beyond accessKey) | Notes |
|---|---|---|
| `GetExchanges/` | — | `{"EXCHANGES":["BFO","BSE","BSE_DEBT","BSE_IDX","CDS","MCX","NFO","NSE","NSE_IDX"]}` verbatim |
| `GetInstrumentTypes/` | `exchange` | `{"INSTRUMENTTYPES":[…]}` |
| `GetProducts/` | `exchange[&instrumentType]` | `{"PRODUCTS":[…]}` |
| `GetExpiryDates/` | `exchange[&product][&instrumentType]` | `{"EXPIRYDATES":[…]}` DDMMMYYYY upper |
| `GetOptionTypes/` | `exchange[&instrumentType][&product][&expiry]` | sample also shows `'FF','XX'` |
| `GetStrikePrices/` | `exchange[&instrumentType][&product][&expiry][&optionType]` | `{"STRIKEPRICES":[…]}` incl. `'0'` |
| `GetInstruments/` | `exchange[&product][&instrumentType][&optionType][&expiry][&StrikePrice][&onlyActive][&detailedInfo]` | ALL-CAPS row keys; TokenNumber/ISIN presence governed by detailedInfo (Assumed) |
| `GetInstrumentsOnSearch/` | `exchange&search[+ same optionals]` | max 20 rows |
| `GetLastQuote/` | `exchange&instrumentIdentifier[&isShortIdentifier]` | ⚠ singular `isShortIdentifier` here; `isShortIdentifiers` (plural) on Short/WithClose + array variants — verbatim SDK quirk |
| `GetLastQuoteShort/`, `GetLastQuoteShortWithClose/` | same + plural flag | |
| `GetLastQuoteArray/`, `…ArrayShort/`, `…ArrayShortWithClose/` | `exchange&instrumentIdentifiers` (`+`-joined, ≤25) `[&isShortIdentifiers]` | |
| `GetSnapshot/` | `exchange&periodicity&period&instrumentIdentifiers[&isShortIdentifiers]` | echoes LONG identifier even for short requests; doc (2026-07-14): periodicity ∈ {MINUTE, HOUR}, default MINUTE |
| `GetExchangeSnapshot/` | `exchange&periodicity&period[&instrumentType][&from][&to]` | ⚠ SDK BUG: `nonTraded` value sent as param `max` |
| `GetHistory/` | `exchange&instrumentIdentifier&periodicity&period[&isShortIdentifier][&max][&from][&to][&userTag][&AdjustSplits]` | **from/to = epoch SECONDS** (corrected 2026-07-14 — doc example `from=1716269700`; the old "epoch-ms on REST" note was response-side only); response `{"USERTAG":…,"OHLC"|"TICK":[…]}` |
| `GetHistoryAfterMarket/` | same params as `GetHistory/` | dedicated REST doc page confirmed 2026-07-14 (`…/rest-api-documentation/gethistoryaftermarket-2/`); path from the universal scheme (no verbatim URL on page) — U-22 PARTIAL |
| `GetHistoryGreeks` | ⚠ SDK posts to `GetHistory/` path; response = XML `<OptionGreeksHistory>` with `DD-MM-YYYY HH:MM:SS` timestamps | 08 §6; 2026-07-14: dedicated REST doc page exists in nav (unfetched) — SDK path reads as a bug |
| `GetLastQuoteOptionChain/` | `exchange&product[&expiry][&optionType][&strikePrice]` | 2026-07-14: optional params absent → "all active" expiries/types/strikes; returns add `TotalBuyQty`/`TotalSellQty` |
| `GetHolidays/` | `[exchange]&Year` (Year mandatory, exchange optional) | NEW 2026-07-14; `{"Value":[{"Year":…,"Date":"01-26-2025",…,"Exchanges":"MCX;NSE;…;NCX"}]}` — MM-DD-YYYY dates |
| `GetLastQuoteOptionGreeks/`, `GetLastQuoteArrayOptionGreeks/` | `exchange&tokens[&detailedInfo]` (tokens `+`-joined) | `detailedInfo` REST-only |
| `GetTopGainersLosers/` | `exchange&count` | |
| `GetServerInfo/` | — | sample `'8040-A7'` |
| `GetLimitation/` | — | REST variant carries EXTRA blocks vs WS (11 §2) |
| `GetMarketMessages/`, `GetExchangeMessages/` | `exchange` | |
| Delayed lane (2026-07-14) | `GetHistory` / `GetSnapshot` / `GetExchangeSnapshot` (Delayed) | dedicated REST doc pages under `…/delayed-api-rest-api-documentation/`; standard param tables |
| Gainers/Shockers/Greeks lanes (2026-07-14) | `GetTopGainersLosers`, `GetVolumeShockers`, `GetLastQuoteOptionGreeks`, `GetLastQuoteArrayOptionGreeks`, `GetLastQuoteOptionGreeksChain` | dedicated REST doc pages exist in the live nav (greeks under `…/greeks-api-global-datafeeds-apis/`); the chain-greeks function was missing from this table pre-2026-07-14 |

## 4. URL-encoding rule (REST-only)

Identifiers with special characters (`L&T`, `M&M`) or spaces (`NIFTY 50`) MUST be URL-encoded in REST query strings (`%26`, `%20`/`+`). Contrast: WS sends special characters AS-IS with no encoding — the doc page itself says the rule "is applicable only to users of REST API" (RUNNER-DOC 2026-07-14). Doc-verbatim table adds `NIFTY BANK → NIFTY%20BANK` and `MCDOWELL-N → MCDOWELL%2DN` — the HYPHEN is encoded too, so encode conservatively (RFC-3986 unreserved-set only).

## 5. Quotas

Request quota is per API key **per hour** — FAQ tiers **1,800 / 3,600 / 7,200 calls/hour** (**RUNNER-DOC since 2026-07-14**, …/faqs/faqs/ — verbatim "this limit could be 1800, 3600, or 7200 requests per hour"; hourly reset, wait out the remainder of the hour when exhausted). Observed real key (2018): `AllowedCallsPerHour: 7200`, `AllowedCallsPerMonth: 5356800`. Bandwidth quotas exist as LimitationResult fields (−1 = unlimited). WS re-subscribes also consume quota (02 §3; FAQ worked example 2026-07-14: subscribe 100 + unsubscribe 100 + subscribe new 100 = 300 requests). Symbol-limit breach message: "Reached instrument limitation". Per-second throttle: none documented (**Unknown** — behave conservatively).

## 6. Separate portal: docs.globaldatafeeds.in (Fundamental Data API) — REWRITTEN 2026-07-14 (full crawl)

The portal was fully machine-fetched 2026-07-14 (apidog-hosted; machine index
`https://docs.globaldatafeeds.in/llms.txt` HTTP 200 — 123 pages captured; `llms-full.txt` is 404).
It is a **separate product** ("Fundamental Data API" — the main site links it as "FD Doc /
Samples"; a third portal `newsdocs.globaldatafeeds.in` exists for Stock Market News APIs,
uncrawled).

**Transport & auth** (RUNNER-DOC 2026-07-14, https://docs.globaldatafeeds.in/authentication-request-response-923682m0):
HTTP **GET** with query params; auth = `AccessKey` query param on every request ("User can get
the accessKey … by contacting our Sales Team and making the payment for the data subscription"
— entitlement is paid/sales-gated; bundled-vs-separate from the market-data key: still a sales
question, U-3 class). Response `format` = `Json | Xml | Csv | CsvContent` (CsvContent → CSV
file); plain-text diagnostic responses possible — "send the request again – till they receive
actual data". Date params use a `dTFormat=String|Epoch` switch (schema `DateTimeFormat`, values
`"Epoch"`/`"String"`); date inputs in the live examples are epoch-seconds at IST-midnight
boundaries. Live example base URL throughout: **`https://test.lisuns.com:4532/`** (HTTPS, with a
trial accessKey GUID in the examples) — e.g.
`https://test.lisuns.com:4532/GetVolatality?accessKey=…&exchange=NSE&instrumentIdentifier=RELIANCE&date=1740594600&dTformat=string&format=Json`.

**Function catalog** (from llms.txt, https://docs.globaldatafeeds.in/llms.txt — ~50 endpoints + 61 schemas):

| Category | Functions |
|---|---|
| Corporate Data (WS API) | How-to-connect page + `SubscribeCorporateAnnouncements`, `SubscribeFinancialResults` (a WS push lane on this portal too — endpoint+port+API-key model) |
| Corporate Announcements / Actions | `GetCorporateAnnouncementsCategories`, `GetCorporateAnnouncements`, `GetCorporateActionsCategories`, `GetCorporateActions` |
| Financial Results | `GetResultsCalendar`, `GetFinancialResultsItems`, `GetFinancialResults`, `GetFinancialRatios` |
| Sectoral Classification | `GetSectoralClassification`, `GetSectors`, `GetMei`, `GetIndustries`, `GetBasicIndustries` |
| Shareholding / MCap / Reports / Company | `GetShpItems`, `GetSHP`, `GetSHPAdvanced`, `GetScripMCap`, `GetExchangeMCap`, `GetAnnualReports`, `GetCompanyData` |
| Deals / Volumes / FII-DII | `GetBulkDeals`, `GetBlockDeals`, `GetDeliveryVolumes`, `GetFIIDII` |
| Other Data APIs | Index Constituents, `GetEODStats`, **`GetBhavCopyCM`**, **`GetBhavCopyFO`** (exchange-official EOD bhavcopy — schemas `QeBhavCopyCM(Result)`/`QeBhavCopyFO(Result)`; FO fields incl. FinInstrmId, ISIN, SttlmPric), `GetFuturesAndOptions`, `GetIndexDetail`, `GetCircuitFilterDetails`, `GetStatisticsGroupAbCompanies`, `GetTop5GainersLosers`, `GetIndexHighlights`, `GetTotalTradeHighlights`, `GetScripGroupTradeHighlights`, `GetTurnoverDetailsOfTop15ScripsofAgroup` |
| EOD Statistics | `GetSeriesChange`, `GetBannedSecurities`, `GetDeliverable`, `GetVolatality` (sic), `GetStatsMCap`, `GetNewHL`, `GetCircuitBreakers` |
| Helper APIs | `GetServerInfo`, `GetLimitation` ("which functions are allowed, Exchanges allowed, etc."), `GetInstruments` |
| Reference | Glossary, Diagnostic API Responses, Release Notes, FAQ, 61 named Schemas (`AnnualReportItem` … `VotingFactsResponse`, incl. `DateTimeFormat`, `ResponseFormat`, `GainersLosersRequest`, the `Qe*`/`Qe*Result` EOD families) |

Notable schema/URL anchors: `GetBhavCopyFO` = https://docs.globaldatafeeds.in/getbhavcopyfo-15575592e0 ·
`GetEODStats` = https://docs.globaldatafeeds.in/geteodstats-16005369e0 ·
`GetLimitation` = https://docs.globaldatafeeds.in/getlimitation-15575606e0 ·
schemas index under `…/-59992NNd0` slugs (per llms.txt). Do NOT conflate this portal's
`GetServerInfo`/`GetLimitation`/`GetInstruments` with the market-data API's same-named
functions — different product, different endpoint, different key.

## What this prevents

- POSTing (GET-only API), header-auth attempts, and body-JSON attempts.
- Parsing REST epoch-ms as seconds (1000× time error) or expecting PascalCase keys.
- Reproducing the gfdl-rest `nonTraded→max` and GetHistoryGreeks-path bugs.
- Unencoded `NIFTY 50` / `M&M` breaking REST query strings.
