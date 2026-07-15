# GDF 11 — Limits, Errors & Diagnostics (GetLimitation / GetServerInfo / messages)

> **Source:** ~~GetServerInfo/GetLimitation WS pages (slugs unresolved)~~ **slugs RESOLVED 2026-07-14:** …/websockets-api-documentation/function-getlimitation-2/ · …/websockets-api-documentation/function-getserverinfo-2/ · …/rest-api-documentation/function-getlimitation/ · …/rest-api-documentation/function-getserverinfo/ · https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/faqs/faqs/ · …/documentation-support/diagnostic-api-responses/ · …/websockets-api-documentation/how-to-connect-using-websockets-api/ · https://docs.globaldatafeeds.in/diagnostic-api-responses-925705m0
> **Fetched:** 2026-07-13 · **Evidence tier:** CLIENT-LIB-SOURCE(wsgfdl-py@1.3.5, gfdl-rest@1.0.3 READMEs) + GITHUB-SAMPLE(dhelm-2018 — a REAL key's LimitationResult) + SEARCH. Verbatim JSON from official artifacts. **2026-07-14: all source pages machine-fetched verbatim — RUNNER-DOC tier; see reconciliation below.**

---

## 2026-07-14 RUNNER-DOC reconciliation

The GDF doc corpus was machine-fetched verbatim on 2026-07-14 (GitHub Actions `docs-fetch.yml`, runs 29316509037 + 29317477759, branch `docs-fetch/gdf-2026-07-14` @ 7d354a8d). Every claim in this file was compared against the live pages:

| Verdict | Claims |
|---|---|
| **CONFIRMED (tier upgraded to RUNNER-DOC)** | GetLimitation request + `LimitationResult` schema (GeneralParams / AllowedExchanges / AllowedFunctions / HistoryLimitation blocks, MaxTicks/MaxIntraday/MaxEOD) — the official WS doc sample is now quoted in §1; the ExpirationDate seconds(WS)-vs-milliseconds(REST) trap — BOTH sides now doc-proven (WS sample `1485468000`, REST sample `1485468000000`, same instant); the AllowedFunctions era/account-variance (the WS doc sample lists 9 functions, no SubscribeSnapshot); GetServerInfo purpose + `ServerInfoResult` shape; the 1800/3600/7200 plan-tier hourly quotas; the "Reached instrument limitation." symbol-cap message; the one-session literal |
| **CORRECTED / REFINED** | §1: the REST GetLimitation JSON is **FLAT** (no `GeneralParams` wrapper, no `MessageType`) — dialect note added; §2: the REST GetServerInfo JSON has **no `MessageType`** either; §5: unsubscribe frames ALSO consume the hourly quota (FAQ 100+100+100 example); hourly counters reset at the top of each clock hour |
| **NEW facts added** | the COMPLETE verbatim 22-row text/plain diagnostic table (§3) — closes U-10 — incl. `Key Expired.`, `IP address not allowed.`, `Duplicated Request Not Allowed.`, `Function not enabled.` (closes U-13), monthly-cap messages, `RealtimeSubscription disabled for delayed data.`; FD-portal extra diagnostics (demo-key + date-range messages); `AllowVMRunningResult`/`AllowServerOSRunningResult` unsolicited frames; REST GetLimitation/GetServerInfo params (`accessKey`/`xml`/`format=CSV`) + XML/CSV dialects; GetHistory vs GetHistoryAfterMarket never co-enabled on one key (FAQ) |
| **U-row impacts** | U-10 CLOSED (diagnostic list; JSON-failure-`Message` wording residual → probe) · U-11 PARTIALLY CLOSED (text/plain envelope; old-vs-new delivery → probe) · U-13 CLOSED (`Function not enabled.` text/plain) · U-14 still OPEN (no Unsubscribe-ack evidence anywhere in the corpus) · U-9 schema DOC-confirmed, per-key live values still probe-only · U-18 CLOSED at doc level (see 02 §8) |

Citations use `RUNNER-DOC (2026-07-14, <live URL>)` inline below.

## 1. GetLimitation — THE per-key entitlement oracle

```json
{"MessageType":"GetLimitation"}
```

Request CONFIRMED 2026-07-14 — the official WS page sends exactly `{ MessageType: "GetLimitation" }` and describes it as "Returns user account information (e.g. which functions are allowed, Exchanges allowed, symbol limit, etc.) … Returns details about user account (what is allowed / disallowed)" — RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/function-getlimitation-2/).

Response `MessageType:"LimitationResult"` — verbatim (dhelm-2018, a REAL production key; joined):

```json
{"GeneralParams":{"AllowedBandwidthPerHour":-1.0,"AllowedCallsPerHour":7200,"AllowedCallsPerMonth":5356800,"AllowedBandwidthPerMonth":-1.0,"ExpirationDate":1538159399,"Enabled":true},
"AllowedExchanges":[{"AllowedInstruments":200,"DataDelay":0,"ExchangeName":"CDS"},{"AllowedInstruments":200,"DataDelay":0,"ExchangeName":"MCX"},{"AllowedInstruments":200,"DataDelay":0,"ExchangeName":"NFO"},{"AllowedInstruments":160,"DataDelay":0,"ExchangeName":"NSE"},{"AllowedInstruments":40,"DataDelay":0,"ExchangeName":"NSE_IDX"}],
"AllowedFunctions":[{"FunctionName":"GetExchangeMessages","IsEnabled":false},{"FunctionName":"GetHistory","IsEnabled":true},{"FunctionName":"GetLastQuote","IsEnabled":false},{"FunctionName":"GetLastQuoteArray","IsEnabled":false},{"FunctionName":"GetLastQuoteArrayShort","IsEnabled":false},{"FunctionName":"GetLastQuoteShort","IsEnabled":false},{"FunctionName":"GetMarketMessages","IsEnabled":false},{"FunctionName":"GetSnapshot","IsEnabled":true},{"FunctionName":"SubscribeRealtime","IsEnabled":false},{"FunctionName":"SubscribeSnapshot","IsEnabled":false}],
"HistoryLimitation":{"TickEnabled":true,"DayEnabled":true,"WeekEnabled":true,"MonthEnabled":true,"MaxEOD":100000,"MaxIntraday":44,"Hour_1Enabled":true,"Hour_2Enabled":true,"Hour_3Enabled":true,"Hour_4Enabled":true,"Hour_6Enabled":true,"Hour_8Enabled":true,"Hour_12Enabled":true,"Minute_1Enabled":true,"Minute_2Enabled":true,"Minute_3Enabled":true,"Minute_4Enabled":true,"Minute_5Enabled":true,"Minute_6Enabled":true,"Minute_10Enabled":true,"Minute_12Enabled":true,"Minute_15Enabled":true,"Minute_20Enabled":true,"Minute_30Enabled":true,"MaxTicks":2},
"MessageType":"LimitationResult"}
```

### Official WS doc sample (RUNNER-DOC, 2026-07-14, …/websockets-api-documentation/function-getlimitation-2/) — added 2026-07-14

Same block structure as the dhelm-2018 real key, independently confirming the schema. Values verbatim, whitespace joined; note the live page itself carries two doc-site typos NOT reproduced below — a stray `},<` after the GeneralParams block and a trailing comma before the closing brace (cosmetic page defects, not wire format):

```json
{
"GeneralParams":{"AllowedBandwidthPerHour":-1.0,"AllowedCallsPerHour":-1,"AllowedCallsPerMonth":-1,"AllowedBandwidthPerMonth":-1.0,"ExpirationDate":1485468000,"Enabled":true},
"AllowedExchanges":[{"AllowedInstruments":-1,"DataDelay":0,"ExchangeName":"CDS"},{"AllowedInstruments":-1,"DataDelay":0,"ExchangeName":"MCX"},{"AllowedInstruments":-1,"DataDelay":0,"ExchangeName":"NFO"},{"AllowedInstruments":-1,"DataDelay":0,"ExchangeName":"NSE"},{"AllowedInstruments":-1,"DataDelay":0,"ExchangeName":"NSE_IDX"}],
"AllowedFunctions":[{"FunctionName":"GetExchangeMessages","IsEnabled":false},{"FunctionName":"GetHistory","IsEnabled":true},{"FunctionName":"GetLastQuote","IsEnabled":false},{"FunctionName":"GetLastQuoteArray","IsEnabled":false},{"FunctionName":"GetLastQuoteArrayShort","IsEnabled":false},{"FunctionName":"GetLastQuoteShort","IsEnabled":false},{"FunctionName":"GetMarketMessages","IsEnabled":false},{"FunctionName":"GetSnapshot","IsEnabled":true},{"FunctionName":"SubscribeRealtime","IsEnabled":false}],
"HistoryLimitation":{"TickEnabled":true,"DayEnabled":true,"WeekEnabled":false,"MonthEnabled":false,"MaxEOD":100000,"MaxIntraday":44,"Hour_1Enabled":false,"Hour_2Enabled":false,"Hour_3Enabled":false,"Hour_4Enabled":false,"Hour_6Enabled":false,"Hour_8Enabled":false,"Hour_12Enabled":false,"Minute_1Enabled":true,"Minute_2Enabled":false,"Minute_3Enabled":false,"Minute_4Enabled":false,"Minute_5Enabled":false,"Minute_6Enabled":false,"Minute_10Enabled":false,"Minute_12Enabled":false,"Minute_15Enabled":false,"Minute_20Enabled":false,"Minute_30Enabled":false,"MaxTicks":5},
"MessageType":"LimitationResult"
}
```

Notable vs the 2018 real key: this doc-sample account lists only **9** AllowedFunctions (no `SubscribeSnapshot`, no `GetLastQuoteArrayShort` reorderings) — further RUNNER-DOC confirmation that the function LIST is account/era-dependent; iterate whatever arrives. `MaxIntraday:44` matches the 2018 key; `MaxTicks:5` here vs `2` there (per-key).

### LimitationResult schema

| Block / field | Type | Meaning |
|---|---|---|
| `GeneralParams.AllowedCallsPerHour` / `AllowedCallsPerMonth` | int | request quotas; **−1 = unlimited** (2026-07-14 honesty note: no doc page states "−1 = unlimited" literally; the official WS/REST samples use −1 pervasively on unrestricted sample accounts, and the diagnostic messages describe quotas as "configured value" — the −1 reading remains an SDK-era inference, Assumed) |
| `GeneralParams.AllowedBandwidthPerHour` / `…PerMonth` | f64 | bandwidth quotas; −1.0 = unlimited (same honesty note) |
| `GeneralParams.ExpirationDate` | int | key expiry — **⚠ dialect trap, BOTH sides now doc-proven (RUNNER-DOC 2026-07-14):** epoch SECONDS on WS (`1485468000` in …/websockets-api-documentation/function-getlimitation-2/) vs epoch MILLISECONDS on REST (`1485468000000` — the SAME instant, 2017-01-27 — in …/rest-api-documentation/function-getlimitation/); the REST XML dialect even renders it as a date string |
| `GeneralParams.Enabled` | bool | key master switch |
| `AllowedExchanges[]` | array | per-exchange `AllowedInstruments` (symbol cap; −1 = unlimited) + `DataDelay` (seconds; 0 = realtime, >0 = delayed key — the FAQ's DelayedSnapshot section confirms per-exchange delay configuration on the key, RUNNER-DOC 2026-07-14, …/faqs/faqs/) + `ExchangeName` |
| `AllowedFunctions[]` | array | per-function `IsEnabled` flags. ⚠ the flag LIST is account/era-dependent (the 2018 real key lists SubscribeSnapshot; a 2022 README sample AND the official WS doc sample omit it) — NEVER hardcode the set; iterate whatever arrives. NEW 2026-07-14 (FAQ, RUNNER-DOC): `GetHistory` and `GetHistoryAfterMarket` are "**not enabled together for the same API key**" — if one is enabled the other is disabled; expect exactly one of the pair `IsEnabled:true` |
| `HistoryLimitation` | object | per-periodicity enable booleans (`Minute_1Enabled` … `Hour_12Enabled`, `TickEnabled`, `DayEnabled`, `WeekEnabled`, `MonthEnabled`) + depth caps: `MaxTicks` (days of tick history; observed 2/5/7 across sample keys), `MaxIntraday` (days; observed 44), `MaxEOD` (records; observed 100000) |

REST `GetLimitation/` variant carries EXTRA blocks (CLIENT-LIB-SOURCE gfdl-rest README): `AllowedExchanges[].UsedInstrumentsInThisSession`, `HistoryLimitation.OnlyAfterMarket:false`, and `SubscribeSnapshotLimitation` / `GetSnapshotLimitation` / `GetExchangeSnapshotLimitation` (`Day`/`Minute_x`/`Hour_1` flags) / `GetExchangeSnapshotInstrumentTypeLimitation` blocks. Its `AllowedFunctions` names two server-side-only capabilities with NO SDK wrapper: **`GetExchangeSnapshotAfterMarket`** and **`GetLastQuoteOptionChainWithGreeks`** (wire shapes **Unknown**, U-20).

### REST GetLimitation dialect (RUNNER-DOC, 2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/rest-api-documentation/function-getlimitation/) — added 2026-07-14

- **Params:** `accessKey` (required) · `xml` `[true]/[false]` default false · `format=CSV` ("Please make sure not to pass xml parameter … when format=CSV is sent"). Example: `http://endpoint:port/GetLimitation/?accessKey=0a0b0c&xml=true`.
- **⚠ The REST JSON is FLAT** — `AllowedBandwidthPerHour` / `AllowedCallsPerHour` / … / `ExpirationDate` / `Enabled` sit at TOP LEVEL (no `GeneralParams` wrapper) and there is **no `MessageType` field**. Doc sample (curly quotes normalized): `{"AllowedBandwidthPerHour":-1,"AllowedCallsPerHour":-1,"AllowedCallsPerMonth":-1,"AllowedBandwidthPerMonth":-1,"ExpirationDate":1485468000000,"Enabled":true,"AllowedExchanges":[…],"AllowedFunctions":[…],"HistoryLimitation":{…}}`. A shared Rust deserializer for WS + REST is therefore WRONG by construction — two shapes.
- XML dialect root: `<LimitationInfoResult>` (note: NOT `LimitationResult`), with `ExpirationDate` rendered as a date STRING.
- The doc's own CSV header confirms the extra top-level blocks the gfdl-rest README hinted at: `…,ExpirationDate,HistoryLimitation,SubscribeSnapshotLimitation,GetExchangeSnapshotLimitation,GetExchangeSnapshotInstrumentTypeLimitation,GetSnapshotLimitation` (the CSV body leaks .NET type names for the nested lists — the CSV format is unusable for this function; use JSON).
- The Fundamental-Data docs-portal exposes the same helper over **HTTPS** at `https://test.lisuns.com:4532/GetLimitation?accessKey=…&format=Json` (RUNNER-DOC, 2026-07-14, https://docs.globaldatafeeds.in/getlimitation-15575606e0 — note `format=Json` casing there; FD test server, not the market-data endpoint).

## 2. GetServerInfo

```json
{"MessageType":"GetServerInfo"}
```
→ `{"ServerID":"444A-C7","MessageType":"ServerInfoResult"}` (REST sample `'8040-A7'`). "Returns the server endpoint where user is connected" (verbatim one-liner).

**2026-07-14 RUNNER-DOC confirmation + dialect notes:**

- Purpose, verbatim from the official WS page: "Returns information about server where connection is made … **Server EndPoint where enduser is connected (useful while debugging issues)**" — RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/function-getserverinfo-2/). Official WS sample response: `{"ServerID":"2152-24","MessageType":"ServerInfoResult"}` — confirms the `ServerInfoResult` envelope and the `NNNN-NN`-style opaque ServerID.
- **REST dialect (RUNNER-DOC, 2026-07-14, …/rest-api-documentation/function-getserverinfo/):** JSON is `{"ServerID":"2152-24"}` — **no `MessageType` field** (same flat-REST pattern as GetLimitation). Params: `xml` true/false, `format=CSV` (CSV sample value `EF89-18`). XML root `<ServerInfoResult>`.
- FD docs-portal helper twin: `GET https://test.lisuns.com:4532/GetServerInfo?accessKey=…&format=Json` (RUNNER-DOC, 2026-07-14, https://docs.globaldatafeeds.in/getserverinfo-15575607e0).

## 3. Error / status literals — EVERY captured one, verbatim

> **U-10 CLOSED 2026-07-14.** The official Diagnostic API Responses page is now machine-fetched verbatim — RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/documentation-support/diagnostic-api-responses/). Framing statement, verbatim: "At times, server sends diagnostic responses instead of requested data. Please find below list of possible informational messages (**returns as text/plain**) by our server. … Your application should be designed to handle these responses which are returned as text/plain (instead of JSON / XML)."

### 3.1 The complete official text/plain diagnostic table (verbatim literals + condensed doc descriptions)

| API Response (verbatim literal) | Meaning (doc description, condensed) |
|---|---|
| `Data unavailable.` | server-side issue — retry or contact developer@globaldatafeeds.in with request + key |
| `Welcome!` | result of successful authentication (also appears inside the JSON `AuthenticateResult`) |
| `Authentication request received. Try request data in next moment.` | REST-only readiness handoff — data not ready; "send same request again in loop – till you receive requested data" |
| `Access Denied. Key not found.` | key not found at server |
| `Access Denied. Key blocked.` | key blocked — contact vendor |
| `Access Denied. Key unknown or empty.` | key not found at server |
| `Key Expired.` | key expired |
| `Duplicated Request Not Allowed.` | "The same authentication request is sent more than once on the same session, which the server doesn't allow" — see 02 §2 correction |
| `IP address not allowed.` | key is IP-restricted and the request came from a different IP — **keys CAN be IP-bound (NEW fact; ask sales whether ours will be)** |
| `Data for requested exchange is disabled.` | key not authorised for that Exchange value |
| `Reached instrument limitation.` | per-key symbol cap exceeded — "To use new symbols, please reduce no. of used symbols" |
| `Function not enabled.` | the request used an API function not enabled for the key — **this is the permission-denied shape; U-13 CLOSED** |
| `RealtimeSubscription disabled for delayed data.` | delayed-data key attempted SubscribeRealtime |
| `Bandwidth per hour is limited.` | hourly bandwidth quota hit — "The counter is reset every hour (e.g. 10AM, 11AM, 12PM, etc.)" — **clock-hour reset (NEW)** |
| `Calls per hour are limited.` | hourly request quota hit — same clock-hour reset |
| `Calls limitation is reached.` | MONTHLY request quota hit |
| `Bandwidth limitation is reached.` | MONTHLY bandwidth quota hit |
| `Access Denied. Key Invalid.` | key not found at server |
| `Access Denied. Key already in use by other session.` | "Key is used in more than 1 session. … If any new sessions are created, previous sessions are invalidated with this message" — text/plain envelope now proven; old-vs-new delivery still probe (U-11 residual; 02 §4) |
| `Selected periodicity or period disabled.` | wrong/unentitled `Periodicity`/`Period` value |
| `More than 1 match found. Use exact long format identifier instead.` | ambiguous symbol — use the long-format identifier |
| `Data unavailable. Send request after market close to get same day's data.` | after-market-data subscription queried intraday |

FD docs-portal variant adds these EXTRA literals (RUNNER-DOC, 2026-07-14, https://docs.globaldatafeeds.in/diagnostic-api-responses-925705m0 — Fundamental-Data surface, same text/plain contract): `Instrument is not allowed for Demo key.` · `Date is out of allowed range.` (+ `…of 1 day.` / `…of 5 days.` / `…of 30 days.`) · `From - To is out of allowed range of 10 years.` · `Filters cant be empty.`

### 3.2 JSON / envelope literals

| Literal | Channel | Meaning | Tier |
|---|---|---|---|
| `{"Complete":true,"Message":"Welcome!","MessageType":"AuthenticateResult"}` | WS JSON | auth success | ~~CLIENT-LIB-SOURCE~~ **RUNNER-DOC (2026-07-14, …/websockets-api-documentation/authenticate/)** |
| `AuthenticateResult` with `Complete:false` | WS JSON | auth failure — the failure WORDINGS are the §3.1 key diagnostics; whether they ride inside the JSON `Message` field vs arrive as text/plain frames: probe residual of U-10 | CLIENT-LIB-SOURCE |
| `{"AllowVMRunning":false,"MessageType":"AllowVMRunningResult"}` | WS JSON, unsolicited post-connect | undocumented licensing/environment flag (NEW 2026-07-14) — tolerate + route, never panic | **RUNNER-DOC (2026-07-14, …/api-code-samples/websockets-nodejs/)** |
| `{"AllowServerOSRunning":false,"MessageType":"AllowServerOSRunningResult"}` | WS JSON, unsolicited post-connect | same class (also in the Python sample transcript) | **RUNNER-DOC (2026-07-14, …/api-code-samples/websockets-python/)** |
| `{"MessageType":"Echo"}` | WS JSON, server-push | liveness heartbeat (02 §6) — payload has NO other fields | **RUNNER-DOC (2026-07-14, official sample transcripts)** |
| `Normal Market Open` / `Normal Market Close` | `MarketMessagesItemResult.MarketType` | session state events (§4) | GITHUB-SAMPLE(dhelm-2018) |

NOT-IN-EVIDENCE (verified absence across the full 2026-07-14 corpus — do not invent): numeric error codes; ~~a rate-limit-exceeded payload shape~~ (now captured: the four §3.1 quota literals); ~~a permission-denied JSON shape for disabled functions (U-13)~~ (closed: `Function not enabled.`, text/plain); an Unsubscribe acknowledgment (U-14 — still open). GetLimitation remains the PREVENTIVE mechanism — check `IsEnabled`/caps up front; the diagnostic table is the REACTIVE classifier for the client's text-frame channel.

Client-side SDK literals (useful to distinguish SDK vs server text in captured logs; NOT server messages): `'Key Authenticated!!!'`, `'Performing Authentication'`, `'Not connected. Trying again. Please wait...'`, `"Exchange is mandatory."`, `"InstrumentIdentifier / Symbol is mandatory."`.

## 4. Market/session events — GetMarketMessages / GetExchangeMessages

```json
{"MessageType":"GetMarketMessages","Exchange":"NFO"}
{"MessageType":"GetExchangeMessages","Exchange":"NFO"}
```

Verbatim responses (dhelm-2018):

```json
{"Request":{"Exchange":"NFO","MessageType":"GetMarketMessages"},"Result":[{"ServerTime":1536314399,"SessionID":0,"MarketType":"Normal Market Close","MessageType":"MarketMessagesItemResult"}],"MessageType":"MarketMessagesResult"}

{"Request":{"Exchange":"NFO","MessageType":"GetExchangeMessages"},"Result":[{"ServerTime":1391822398,"Identifier":"Market","Message":"Members are requested to note that  ...","MessageType":"ExchangeMessagesItemResult"}],"MessageType":"ExchangeMessagesResult"}
```

These are the documented session open/close signal surface (pull-type; no push variant observed).

## 5. Quota consumption rules

| Rule | Detail | Tier |
|---|---|---|
| Hourly call quota | plan-sized: "this limit could be 1800, 3600, or 7200 requests per hour" (verbatim); exhaust it → "wait until the next hour for the request count to reset" | ~~SEARCH / Assumed-current~~ **RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/faqs/faqs/)** |
| Quota reset boundary | hourly counters reset at the top of each CLOCK hour ("e.g. 10AM, 11AM, 12PM, etc.") — not a rolling 60-min window | **RUNNER-DOC (2026-07-14, …/documentation-support/diagnostic-api-responses/)** — NEW 2026-07-14 |
| **Re-subscribes consume quota** | reconnect re-subscription of 100 symbols = 100 requests against the quota | SEARCH (docs statement) |
| **Unsubscribes ALSO consume quota** | FAQ verbatim: rotating 100 symbols = "total 300 requests i.e. 100 (original) + 100 (unsubscription) + 100 (new subscription)" — symbol churn costs 2× the naive count | **RUNNER-DOC (2026-07-14, …/faqs/faqs/)** — NEW 2026-07-14 |
| Monthly quotas exist | `Calls limitation is reached.` / `Bandwidth limitation is reached.` diagnostics = monthly counters (matches `AllowedCallsPerMonth`/`AllowedBandwidthPerMonth`) | **RUNNER-DOC (2026-07-14, …/diagnostic-api-responses/)** |
| Symbol caps | "The Symbol limit is set per API key based on the user's requirements. If the user exceeds this symbol limit, they will receive the message: 'Reached instrument limitation.'" (FAQ verbatim; 2018 real key: NSE 160 + NSE_IDX 40 + 200 each F&O/CDS/MCX) | Verified (sample) + **RUNNER-DOC (2026-07-14, …/faqs/faqs/)** |
| Push cadence | fixed 1/sec/symbol server-side; no inbound msg/sec limit documented | Verified / absence |
| Function availability | per-account (`IsEnabled` false → function refused; ~~refusal frame shape Unknown, U-13~~ refusal = `Function not enabled.` text/plain diagnostic — U-13 CLOSED 2026-07-14, §3.1) | Verified + RUNNER-DOC |
| History caps | MaxTicks/MaxIntraday/MaxEOD per key (08 §3) | Verified |
| Key IP binding | keys can be restricted to specific IPs (`IP address not allowed.` diagnostic) — NEW 2026-07-14; relevant to the EIP-bound prod box + any Mac-dev use of the same key | **RUNNER-DOC (2026-07-14, …/diagnostic-api-responses/)** |

## What this prevents

- Hardcoding the AllowedFunctions set (era/account-dependent list).
- Blind retry storms against disabled functions (query GetLimitation first; the API has no NUMERIC error-code taxonomy — but since 2026-07-14 the §3.1 verbatim string table IS the retry classifier: quota strings → wait for the clock hour/month, key strings → operator escalation, `Duplicated Request Not Allowed.` → reconnect-not-retry).
- Treating −1 quotas as errors (they mean unlimited — Assumed reading; see §1 honesty note).
- Missing the ExpirationDate dialect trap (seconds on WS, ms on REST — both doc-proven 2026-07-14).
- Deserializing REST GetLimitation/GetServerInfo with the WS structs (REST is FLAT, no `MessageType` — 2026-07-14).
- Burning double quota on symbol rotation (unsubscribe frames also count — 2026-07-14).
