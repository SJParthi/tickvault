# GDF 09 — REST API

> **Source:** https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/rest-api-documentation/how-to-connect-using-rest-api/ · …/rest-api-documentation/handling-special-characters/ · per-function REST pages (inventory in §3)
> **Fetched:** 2026-07-13 · **Evidence tier:** SEARCH (connect page) + CLIENT-LIB-SOURCE(gfdl-rest@1.0.3 :: base_api.py + 27 endpoint classes + README). Verbatim samples from the official REST package.

---

## 1. Transport & auth

| Rule | Detail |
|---|---|
| Base URL | per-account `http://endpoint:port/` (assigned on purchase; one SDK README also shows `https://endpoint:port` — TLS availability **Unknown**, U-2) |
| Method | **HTTP GET only** — "POST or other request types will NOT work" (doc verbatim class). All params in the query string; no bodies, no headers |
| Auth | query param **`accessKey=<API_KEY>`** on every call. ⚠ casing: the live docs page shows `accesskey=` (lowercase), the official SDK sends `accessKey=` — assume case-insensitive, verify live (U-16). Sessionless — REST is the parallel-pull path beside the 1-session WS |
| Path scheme | `http://endpoint:port/<FunctionName>/` — path segment = EXACT function name + trailing slash. Example (doc verbatim): `http://endpoint:port/GetExchanges/?accesskey=apikey` |
| Format switches | JSON default; `&xml=true` → XML (`Content-Type: application/xml|text/xml`); `&format=CSV` → CSV (`text/csv`). The SDK dispatches purely on response Content-Type |
| Diagnostics | text/plain diagnostic bodies may replace JSON/XML (expired/invalid key etc.) — handle non-JSON bodies (11 §3) |

## 2. Response dialect (differs from WS!)

| Aspect | REST | WS |
|---|---|---|
| Key casing | **ALL-CAPS** (`LASTTRADEPRICE`, `INSTRUMENTIDENTIFIER`) | PascalCase |
| Timestamps | epoch **MILLISECONDS**, always trailing `000` (second resolution) — e.g. `"LASTTRADETIME": 1724219587000` | epoch SECONDS |
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
| `GetSnapshot/` | `exchange&periodicity&period&instrumentIdentifiers[&isShortIdentifiers]` | echoes LONG identifier even for short requests |
| `GetExchangeSnapshot/` | `exchange&periodicity&period[&instrumentType][&from][&to]` | ⚠ SDK BUG: `nonTraded` value sent as param `max` |
| `GetHistory/` | `exchange&instrumentIdentifier&periodicity&period[&isShortIdentifier][&max][&from][&to][&userTag]` | from/to epoch-ms on REST; `{"USERTAG":…,"OHLC"|"TICK":[…]}` |
| `GetHistoryAfterMarket/` | README-documented; NO SDK class — path **Unknown** (U-22) | |
| `GetHistoryGreeks` | ⚠ SDK posts to `GetHistory/` path; response = XML `<OptionGreeksHistory>` with `DD-MM-YYYY HH:MM:SS` timestamps | 08 §6 |
| `GetLastQuoteOptionChain/` | `exchange&product[&expiry][&optionType][&strikePrice]` | |
| `GetLastQuoteOptionGreeks/`, `GetLastQuoteArrayOptionGreeks/` | `exchange&tokens[&detailedInfo]` (tokens `+`-joined) | `detailedInfo` REST-only |
| `GetTopGainersLosers/` | `exchange&count` | |
| `GetServerInfo/` | — | sample `'8040-A7'` |
| `GetLimitation/` | — | REST variant carries EXTRA blocks vs WS (11 §2) |
| `GetMarketMessages/`, `GetExchangeMessages/` | `exchange` | |

## 4. URL-encoding rule (REST-only)

Identifiers with special characters (`L&T`, `M&M`) or spaces (`NIFTY 50`) MUST be URL-encoded in REST query strings (`%26`, `%20`/`+`). Contrast: WS sends special characters AS-IS with no encoding.

## 5. Quotas

Request quota is per API key **per hour** — FAQ tiers **1,800 / 3,600 / 7,200 calls/hour** (plan-sized; SEARCH, Assumed-current). Observed real key (2018): `AllowedCallsPerHour: 7200`, `AllowedCallsPerMonth: 5356800`. Bandwidth quotas exist as LimitationResult fields (−1 = unlimited). WS re-subscribes also consume quota (02 §3). Per-second throttle: none documented (**Unknown** — behave conservatively).

## 6. Separate portal: docs.globaldatafeeds.in (Fundamental Data API)

A distinct REST portal (separate product) hosting `GetEODStats`, `GetBhavCopyCM`, `GetBhavCopyFO` (exchange-official EOD bhavcopy — fields incl. FinInstrmId, ISIN, StrikePrice, SttlmPric, OI, OiChng, TTQ…), `GetFuturesAndOptions`, `GetServerInfo`. Field lists captured via SEARCH only; auth/entitlement for this portal not established (**Unknown** — bundled-vs-separate is a sales question, U-3 class).

## What this prevents

- POSTing (GET-only API), header-auth attempts, and body-JSON attempts.
- Parsing REST epoch-ms as seconds (1000× time error) or expecting PascalCase keys.
- Reproducing the gfdl-rest `nonTraded→max` and GetHistoryGreeks-path bugs.
- Unencoded `NIFTY 50` / `M&M` breaking REST query strings.
