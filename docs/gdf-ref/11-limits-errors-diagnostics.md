# GDF 11 — Limits, Errors & Diagnostics (GetLimitation / GetServerInfo / messages)

> **Source:** GetServerInfo/GetLimitation WS pages (slugs unresolved) · https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/faqs/faqs/ · …/documentation-support/diagnostic-api-responses/ · …/websockets-api-documentation/how-to-connect-using-websockets-api/
> **Fetched:** 2026-07-13 · **Evidence tier:** CLIENT-LIB-SOURCE(wsgfdl-py@1.3.5, gfdl-rest@1.0.3 READMEs) + GITHUB-SAMPLE(dhelm-2018 — a REAL key's LimitationResult) + SEARCH. Verbatim JSON from official artifacts.

---

## 1. GetLimitation — THE per-key entitlement oracle

```json
{"MessageType":"GetLimitation"}
```

Response `MessageType:"LimitationResult"` — verbatim (dhelm-2018, a REAL production key; joined):

```json
{"GeneralParams":{"AllowedBandwidthPerHour":-1.0,"AllowedCallsPerHour":7200,"AllowedCallsPerMonth":5356800,"AllowedBandwidthPerMonth":-1.0,"ExpirationDate":1538159399,"Enabled":true},
"AllowedExchanges":[{"AllowedInstruments":200,"DataDelay":0,"ExchangeName":"CDS"},{"AllowedInstruments":200,"DataDelay":0,"ExchangeName":"MCX"},{"AllowedInstruments":200,"DataDelay":0,"ExchangeName":"NFO"},{"AllowedInstruments":160,"DataDelay":0,"ExchangeName":"NSE"},{"AllowedInstruments":40,"DataDelay":0,"ExchangeName":"NSE_IDX"}],
"AllowedFunctions":[{"FunctionName":"GetExchangeMessages","IsEnabled":false},{"FunctionName":"GetHistory","IsEnabled":true},{"FunctionName":"GetLastQuote","IsEnabled":false},{"FunctionName":"GetLastQuoteArray","IsEnabled":false},{"FunctionName":"GetLastQuoteArrayShort","IsEnabled":false},{"FunctionName":"GetLastQuoteShort","IsEnabled":false},{"FunctionName":"GetMarketMessages","IsEnabled":false},{"FunctionName":"GetSnapshot","IsEnabled":true},{"FunctionName":"SubscribeRealtime","IsEnabled":false},{"FunctionName":"SubscribeSnapshot","IsEnabled":false}],
"HistoryLimitation":{"TickEnabled":true,"DayEnabled":true,"WeekEnabled":true,"MonthEnabled":true,"MaxEOD":100000,"MaxIntraday":44,"Hour_1Enabled":true,"Hour_2Enabled":true,"Hour_3Enabled":true,"Hour_4Enabled":true,"Hour_6Enabled":true,"Hour_8Enabled":true,"Hour_12Enabled":true,"Minute_1Enabled":true,"Minute_2Enabled":true,"Minute_3Enabled":true,"Minute_4Enabled":true,"Minute_5Enabled":true,"Minute_6Enabled":true,"Minute_10Enabled":true,"Minute_12Enabled":true,"Minute_15Enabled":true,"Minute_20Enabled":true,"Minute_30Enabled":true,"MaxTicks":2},
"MessageType":"LimitationResult"}
```

### LimitationResult schema

| Block / field | Type | Meaning |
|---|---|---|
| `GeneralParams.AllowedCallsPerHour` / `AllowedCallsPerMonth` | int | request quotas; **−1 = unlimited** |
| `GeneralParams.AllowedBandwidthPerHour` / `…PerMonth` | f64 | bandwidth quotas; −1.0 = unlimited |
| `GeneralParams.ExpirationDate` | int | key expiry — epoch SECONDS in the WS/2018 sample; ⚠ epoch-ms in the REST variant |
| `GeneralParams.Enabled` | bool | key master switch |
| `AllowedExchanges[]` | array | per-exchange `AllowedInstruments` (symbol cap; −1 = unlimited) + `DataDelay` (seconds; 0 = realtime, >0 = delayed key) + `ExchangeName` |
| `AllowedFunctions[]` | array | per-function `IsEnabled` flags. ⚠ the flag LIST is account/era-dependent (the 2018 real key lists SubscribeSnapshot; a 2022 README sample omits it) — NEVER hardcode the set; iterate whatever arrives |
| `HistoryLimitation` | object | per-periodicity enable booleans (`Minute_1Enabled` … `Hour_12Enabled`, `TickEnabled`, `DayEnabled`, `WeekEnabled`, `MonthEnabled`) + depth caps: `MaxTicks` (days of tick history; observed 2/5/7 across sample keys), `MaxIntraday` (days; observed 44), `MaxEOD` (records; observed 100000) |

REST `GetLimitation/` variant carries EXTRA blocks (CLIENT-LIB-SOURCE gfdl-rest README): `AllowedExchanges[].UsedInstrumentsInThisSession`, `HistoryLimitation.OnlyAfterMarket:false`, and `SubscribeSnapshotLimitation` / `GetSnapshotLimitation` / `GetExchangeSnapshotLimitation` (`Day`/`Minute_x`/`Hour_1` flags) / `GetExchangeSnapshotInstrumentTypeLimitation` blocks. Its `AllowedFunctions` names two server-side-only capabilities with NO SDK wrapper: **`GetExchangeSnapshotAfterMarket`** and **`GetLastQuoteOptionChainWithGreeks`** (wire shapes **Unknown**, U-20).

## 2. GetServerInfo

```json
{"MessageType":"GetServerInfo"}
```
→ `{"ServerID":"444A-C7","MessageType":"ServerInfoResult"}` (REST sample `'8040-A7'`). "Returns the server endpoint where user is connected" (verbatim one-liner).

## 3. Error / status literals — EVERY captured one, verbatim

| Literal | Channel | Meaning | Tier |
|---|---|---|---|
| `{"Complete":true,"Message":"Welcome!","MessageType":"AuthenticateResult"}` | WS JSON | auth success | CLIENT-LIB-SOURCE |
| `AuthenticateResult` with `Complete:false` | WS JSON | auth failure — failure `Message` text NOT captured (**Unknown**, U-10) | CLIENT-LIB-SOURCE |
| `Access Denied. Key already in use by other session.` | server response (envelope shape unknown — U-11) | new connection invalidated the PREVIOUS session (02 §4) | SEARCH ×2 |
| `Reached instrument limitation` | text/plain diagnostic | per-key symbol cap exceeded | SEARCH ×2 |
| Expired / invalid API-key diagnostics | **text/plain** frames ("server sends diagnostic responses instead of requested data … returns as text/plain") — exact wordings NOT captured (**Unknown**, U-10) | key problems | SEARCH |
| `Normal Market Open` / `Normal Market Close` | `MarketMessagesItemResult.MarketType` | session state events (§4) | GITHUB-SAMPLE(dhelm-2018) |

NOT-IN-EVIDENCE (verified absence — do not invent): numeric error codes; a rate-limit-exceeded payload shape; a permission-denied JSON shape for disabled functions (U-13); an Unsubscribe acknowledgment (U-14). GetLimitation is the PREVENTIVE mechanism — check `IsEnabled`/caps up front rather than probing for error frames.

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
| Hourly call quota | plan-sized: 1,800 / 3,600 / 7,200 calls/hour (FAQ) | SEARCH / Assumed-current |
| **Re-subscribes consume quota** | reconnect re-subscription of 100 symbols = 100 requests against the quota | SEARCH (docs statement) |
| Symbol caps | per exchange per key (`AllowedInstruments`; 2018 real key: NSE 160 + NSE_IDX 40 + 200 each F&O/CDS/MCX); exceed → "Reached instrument limitation" text frame | Verified (sample) |
| Push cadence | fixed 1/sec/symbol server-side; no inbound msg/sec limit documented | Verified / absence |
| Function availability | per-account (`IsEnabled` false → function refused; refusal frame shape Unknown, U-13) | Verified |
| History caps | MaxTicks/MaxIntraday/MaxEOD per key (08 §3) | Verified |

## What this prevents

- Hardcoding the AllowedFunctions set (era/account-dependent list).
- Blind retry storms against disabled functions (query GetLimitation first; the API has no documented error-code taxonomy to key retries on).
- Treating −1 quotas as errors (they mean unlimited).
- Missing the ExpirationDate dialect trap (seconds on WS-era sample, ms on REST).
