# GDF 03 — WebSocket Realtime Feed (SubscribeRealtime)

> **Source:** https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/function-subscriberealtime/ · …/introduction/type-of-data-available/ · …/documentation-support/api-fields-description/
> **Fetched:** 2026-07-13 (operator paste) + **2026-07-14 (machine-fetched, docs-fetch.yml runner @ 7d354a8d)** · **Evidence tier:** **RUNNER-DOC (2026-07-14 — the function-subscriberealtime + api-fields-description pages, machine-captured verbatim; equals LIVE-DOC)** + **LIVE-DOC (the function-subscriberealtime page — operator-pasted from a live browser 2026-07-13; see §3b)** + CLIENT-LIB-SOURCE(wsgfdl-py@1.3.5 :: gfdlws/realtime.py + README) + GITHUB-SAMPLE(js-2020, dhelm-2018) + SEARCH (other doc-page prose). Verbatim JSON from official artifacts + the live paste (preserved at the coordinator scratchpad `gdf-live-subscriberealtime-paste.md`).

---

## 2026-07-14 RUNNER-DOC reconciliation (docs-fetch runner, branch docs-fetch/gdf-2026-07-14 @ 7d354a8d)

**LIVE-DOC cross-check verdict (§3b): IDENTICAL.** The runner fetch of the SubscribeRealtime page (RUNNER-DOC 2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/function-subscriberealtime/, sha256 `2f0d7f17…`) matches the 2026-07-13 operator paste in every checked element: the same 3-parameter table (`Exchange` / `InstrumentIdentifier` / `Unsubscribe` with the verbatim "_[true]/[false], default = [false]_ | Optional parameter" cell), the same 23-field "What is returned?" list with identical glosses, the same epoch wording INCLUDING the "ServerTradeTime" prose typo, the same sample request `{ MessageType: "SubscribeRealtime", Exchange: "NFO", InstrumentIdentifier: "NIFTY-I" }` (still omitting `Unsubscribe`), and the byte-identical sample response (epoch `1776057749`, `QuotationLot` 65.0, `LastTradeQty` 0, `PriceChange` −326.0). ZERO divergence — §3b stands untouched; every §3b claim is now doubly evidenced (operator paste + machine fetch).

| # | Verdict | Claim | Evidence |
|---|---|---|---|
| 1 | CONFIRMED (tier now RUNNER-DOC) | 1/sec cadence wording: "returns market data every second (Bid/Ask/Trade)" | function-subscriberealtime page, verbatim heading |
| 2 | CONFIRMED (RUNNER-DOC) | Epoch-SECONDS wording (with the "ServerTradeTime" typo intact) + all 23 `RealtimeResult` fields current, incl. `PriceChange`/`PriceChangePercentage`/`OpenInterestChange` | same page |
| 3 | CONFIRMED (RUNNER-DOC) | `Unsubscribe` optional, default false, string form in official samples; supported `Exchange` values NFO/NSE/NSE_IDX/CDS/MCX re-confirmed in the runner-fetched Python sample page comment | same page + …/api-code-samples/websockets-python/ |
| 4 | CONFIRMED (RUNNER-DOC — §4 REST-twin note) | api-fields-description glossary uses ALL-CAPS keys (`LASTTRADEPRICE`, `AVERAGETRADEPRICE`, …) and glosses `CLOSE` = "Previous (Trading Day's) Close", `TOTALQTYTRADED` = "Total Volume of the day", `VALUE` = "Total Turnover", `QUOTATIONLOT` = "Lot Size", `PREOPEN` = "If Preopen is active" | https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/documentation-support/api-fields-description/ |
| 5 | CONFIRMED (RUNNER-DOC — README claim 15's REST-ms half) | The glossary page's `LASTTRADETIME`/`SERVERTIME` **sample values are 13-digit** (`1589796000000` = 1589796000 s = 2020-05-18 15:30:00 IST, `…000` ms-scale). The glossary IS the ALL-CAPS REST-twin field table, so this directly confirms the README claim-15 split: **REST dialect = epoch-MILLISECONDS, WS dialect = epoch-SECONDS** (every WS wire sample is 10-digit: 1536558991 / 1669262572 / 1776057749 / 1759831039 / 1729683720). No claim change — first doc-page (vs sample-derived) evidence for the REST-ms convention | same api-fields-description page |
| 6 | U-14 still OPEN | No unsubscribe ACK is documented ANYWHERE in the 215-page corpus — the SubscribeRealtime page documents only the request param; the official Python samples send `Unsubscribe` fire-and-forget with no response handling. Live probe still required | corpus-wide grep, incl. the Python AllFunctions PDF |
| 7 | U-15 still OPEN | PreOpen semantics: the glossary adds only "If Preopen is active" — no pre-open OHLC/field-population detail anywhere. Live probe | api-fields-description page |
| 8 | U-17 still OPEN | Per-symbol every-second delivery under burst load is a live-behavior question — no doc page can close it | — |

---

## 1. Subscribe / Unsubscribe (verbatim)

One request PER SYMBOL — "If you want to subscribe to data of multiple symbols, you will need to send 1 request each - for each symbol" (verbatim GFDL comment). No batch subscribe exists.

```json
{"MessageType":"SubscribeRealtime","Exchange":"NFO","Unsubscribe":"false","InstrumentIdentifier":"NIFTY-I"}
```

Unsubscribe = the SAME message with `"Unsubscribe":"true"`. Notes:
- `Unsubscribe` is a **string** `"true"/"false"`, not a JSON bool (official SDK). **LIVE-DOC confirmation (2026-07-13 paste; re-confirmed identical by RUNNER-DOC 2026-07-14):** the live page lists `Unsubscribe | [true]/[false], default = [false] | Optional. If [true], instrumentIdentifier is unsubscribed` — optionality Verified for THIS function; cross-function case tolerance still Assumed (U-16). The live sample request omits it: `{ MessageType: "SubscribeRealtime", Exchange: "NFO", InstrumentIdentifier: "NIFTY-I" }`.
- `Exchange` mandatory; supported values per GFDL comment: `NSE, NSE_IDX, NFO, CDS, MCX` (BSE family on entitled keys — see 01 §2).
- `InstrumentIdentifier` accepts all 3 formats (continuous `NIFTY-I`, long `FUTIDX_NIFTY_30JUL2020_XX_0`, contractwise `NIFTY20JULFUT`) + index display names (`NIFTY 50` on `NSE_IDX`) + cash symbols as-is (`BAJAJ-AUTO`, `RELCAPITAL.BE`) — full grammar in 06 §2. Special characters are sent as-is on WS (no encoding).
- There is NO documented subscribe-ACK — the first `RealtimeResult` is the confirmation (Assumed; rejection-frame shape when the function/symbol is not entitled is **Unknown**, U-13).
- After ANY disconnect, every symbol must be re-subscribed (02 §3); re-subscribes consume request quota.
- Per-connection symbol cap = account-governed via `GetLimitation → AllowedExchanges[].AllowedInstruments` (−1 = unlimited); typical commercial values **Unknown** (U-9).

## 2. Push cadence — 1 update per second per symbol (L1, conflated)

- GDF's own docs: "returns market data every second (Bid/Ask/Trade)" — **now LIVE-DOC Verified verbatim** (2026-07-13 operator paste of the function page; previously SEARCH-tier; **re-confirmed byte-identical by RUNNER-DOC 2026-07-14**); type-of-data page: "Realtime data updating at 1 second frequency … This is L1 data with single best Bid & Ask details." (SEARCH, Verified.)
- Each push is a **full-image** L1 tick (complete quote every time — no deltas/incremental encoding).
- This is aligned with NSE's own non-colo vendor broadcast, which is itself ~1s snapshots ("MARKET FEED … Realtime Snapshot" per the NSE vendor spec) — so the 1s cadence is the distribution reality, not GDF-only conflation. "True tick-by-tick data with time-stamp of 1 second" (GDF desktop marketing) is internally contradictory marketing for the same 1s L1. See 12 §2.
- Pushes flow from subscribe until market close. Whether EVERY symbol truly gets an update EVERY second under burst load: **Unknown** (U-17 — live probe).

## 3. RealtimeResult — verbatim sample (2022-era, all fields)

```json
{"Exchange":"NFO","InstrumentIdentifier":"NIFTY-I","LastTradeTime":1669262572,"ServerTime":1669262572,"AverageTradedPrice":18309.69,"BuyPrice":18323.9,"BuyQty":50,"Close":18286.75,"High":18329.35,"Low":18297.15,"LastTradePrice":18325.0,"LastTradeQty":750,"Open":18310.0,"OpenInterest":5657350,"QuotationLot":50.0,"SellPrice":18325.1,"SellQty":100,"TotalQtyTraded":681250,"Value":12473476312.5,"PreOpen":false,"PriceChange":38.25,"PriceChangePercentage":0.21,"OpenInterestChange":-98200,"MessageType":"RealtimeResult"}
```

2018-era sample (dhelm docs) — NOTE: no `PriceChange`/`PriceChangePercentage`/`OpenInterestChange` fields; the schema GREW over time, so a decoder must make tick fields Option-al:

```json
{"Exchange":"NSE","InstrumentIdentifier":"SBIN","LastTradeTime":1536558991,"ServerTime":1536558991,"AverageTradedPrice":291.31,"BuyPrice":290.15,"BuyQty":5441,"Close":291.65,"High":293.25,"Low":289.15,"LastTradePrice":290.2,"LastTradeQty":323,"Open":290.65,"OpenInterest":0,"QuotationLot":0.0,"SellPrice":290.3,"SellQty":551,"TotalQtyTraded":7422459,"Value":2162236531.29,"PreOpen":false,"MessageType":"RealtimeResult"}
```

## 3b. LIVE-DOC sample (2026-07-13 operator paste of the live page) — the pack's first LIVE-DOC evidence (the second is the StreamAllSymbols page, 10). RUNNER-DOC 2026-07-14 re-fetched the same page and found it IDENTICAL (see the dated reconciliation section above)

Live page sample response, verbatim ("Example of returned data in JSON format. This data is returned every second"):

```json
{"Exchange":"NFO","InstrumentIdentifier":"NIFTY-I","LastTradeTime":1776057749,"ServerTime":1776057749,"AverageTradedPrice":23689.38,"BuyPrice":23772.3,"BuyQty":325,"Close":24101.0,"High":23782.0,"Low":23625.0,"LastTradePrice":23775.0,"LastTradeQty":0,"Open":23748.0,"OpenInterest":18782270,"QuotationLot":65.0,"SellPrice":23775.0,"SellQty":975,"TotalQtyTraded":2596880,"Value":61518477134.4,"PreOpen":false,"PriceChange":-326.0,"PriceChangePercentage":-1.35,"OpenInterestChange":205335,"MessageType":"RealtimeResult"}
```

What the live page CONFIRMS (upgrades to LIVE-DOC/Verified):
- 1/sec cadence wording verbatim: "returns market data every second (Bid/Ask/Trade)".
- Epoch wording verbatim: "LastTradeTime, ServerTradeTime : These values are expressed as no. of seconds since Epoch time (i.e. 1st January 1970). Also known as Unix Time." ⚠ page prose TYPO: "ServerTradeTime" — the JSON field is `ServerTime`.
- ARITH on the live sample: 1776057749 = 2026-04-13 10:52:29 IST (05:22:29 UTC) — in-session under the true-UTC reading (third independent confirmation); `PriceChange −326.0 = LastTradePrice 23775.0 − Close 24101.0` ✓ — `Close` = previous day's close, stated verbatim on the page ("Close (previous Day's Close)"), as is `OpenInterestChange` ("vs previous trading day's Close").
- Full 2022-era field list is CURRENT: the page enumerates all 23 fields incl. `PriceChange`, `PriceChangePercentage`, `OpenInterestChange` (the fields absent in 2018) — glosses verbatim: ATP=VWAP, Value=Turnover, QuotationLot=Lot Size, BuyQty=Bid Size.
- `LastTradeQty: 0` appears in a LIVE realtime sample (a quote-refresh second without a fresh trade print) — decoders must accept LTQ=0 on the stream, not only on quote pulls.
- `LastTradeTime == ServerTime` in this sample (zero visible lag at capture).
- NIFTY `QuotationLot` 65.0 in the 2026 sample (50.0 in the 2022 sample; 75.0 in the 2025 master row) — lot sizes CHANGE; never hardcode.

## 4. COMPLETE RealtimeResult field table

Types inferred from JSON literals in official samples (Verified as-shown). REST twin uses ALL-CAPS keys (`LASTTRADEPRICE` etc., see 09) — the api-fields-description glossary is now RUNNER-DOC-captured (2026-07-14) and pins those ALL-CAPS names + glosses verbatim; its 13-digit `LASTTRADETIME`/`SERVERTIME` samples confirm the REST-twin epoch-MILLISECONDS convention (README claim 15) while THIS WS dialect stays epoch-seconds (see the dated reconciliation section).

| Field | Type | Units / meaning | Timezone / precision | Notes |
|---|---|---|---|---|
| `Exchange` | string | GDF exchange code | — | NSE / NSE_IDX / NFO / CDS / MCX (+BSE family) |
| `InstrumentIdentifier` | string | symbol as subscribed | — | echo of the request identifier |
| `LastTradeTime` | integer | exchange last-trade timestamp | **UNIX epoch SECONDS, true UTC base** (ARITH-verified; render in IST for display). Whole-second precision ONLY | see 12 §1; NOT the Dhan IST-shifted convention |
| `ServerTime` | integer | GDF server send timestamp | same epoch-seconds | `ServerTime − LastTradeTime` = per-tick feed-lag probe (1s skew seen in samples) |
| `LastTradePrice` | f64 | LTP, ₹ | 2dp typical | |
| `LastTradeQty` | integer | LTQ, units | — | 0 seen on quote pulls |
| `AverageTradedPrice` | f64 | session VWAP/ATP, ₹ | — | |
| `BuyPrice` / `BuyQty` | f64 / integer | best bid price/qty — **L1 single level ONLY** | — | no depth levels exist |
| `SellPrice` / `SellQty` | f64 / integer | best ask price/qty | — | |
| `Open` / `High` / `Low` | f64 | session day open/high/low, ₹ | — | |
| `Close` | f64 | **PREVIOUS trading day's close** in the realtime/quote family | — | ARITH-verified: `PriceChange = LastTradePrice − Close` (487.2−479.1=8.1). ⚠ dual semantics: in Snapshot/History bar rows `Close` = the BAR close (14 in README claims table) |
| `OpenInterest` | integer | OI, contracts (0 for cash/index) | — | INLINE every second (unlike Dhan's separate code-5 packet) |
| `OpenInterestChange` | integer (signed) | OI change vs prev day | — | absent in 2018 era → `Option` |
| `QuotationLot` | f64 | lot size | — | 1.0 cash, 50.0 NIFTY; 0.0 seen in 2018 sample |
| `TotalQtyTraded` | integer | cumulative day volume, units | — | |
| `Value` | f64 | cumulative traded value, ₹ | — | |
| `PreOpen` | JSON bool | pre-open session flag | — | pre-open indicative ticks delivered in-band, self-labelled; pre-open OHLC semantics **Unknown** (U-15) |
| `PriceChange` | f64 | LTP − prev close, ₹ | — | absent 2018 era → `Option` |
| `PriceChangePercentage` | f64 | % change | — | absent 2018 era → `Option` |
| `MessageType` | string | `"RealtimeResult"` | — | discriminator |

Index ticks (`NSE_IDX`): same shape at the same 1s cadence; volume/OI presumably 0 — field population **Unknown** (U-15).

## 5. Decoder requirements (mechanical)

1. All messages are JSON text frames tagged by `MessageType` — build one tagged-enum decoder over the ~25 observed discriminants + an unknown-type fallback arm + a non-JSON plain-text arm (02 §5). Never panic on unknown.
2. All tick fields beyond the 2018 core set must be `Option`-al (schema grew; may grow again).
3. Request booleans serialize as STRINGS; request-key casing drifts across functions/libs (`Unsubscribe` vs `unsubscribe`, `periodicity` vs `Periodicity`) — server case-tolerance Assumed, verify live (U-16).
4. Scientific-notation floats appear on the wire (Greeks family, e.g. `3.0227099614421604E-06`) — the JSON parser must accept them (serde_json does).
5. No frame-size cap (02 §7); huge frames arrive on reference pulls sharing the tick socket.
6. Greeks stream data frames echo the REQUEST name as `MessageType` (`"SubscribeRealtimeGreeks"`) — see 07 §3.

## 6. What is NOT in this feed

| Absent | Detail |
|---|---|
| Market depth (5/20-level) | NO depth function or fields anywhere on WS/REST — best bid/ask only. `SubscribeOptionChain.Depth` = strikes-around-ATM count, NOT book depth. Depth is hinted only for desktop/FIX lines (**Unknown**, U-4) |
| Millisecond timestamps | whole seconds everywhere (12 §1) |
| Batch subscribe | one frame per symbol |
| Delta/incremental encoding | full-image pushes only |
| Sequence numbers / resume | none; reconnect = resubscribe-all |

## What this prevents

- Bid/ask-depth assumptions (L1 single level; anything else is a different GDF product).
- Treating `Close` in a tick as today's close (it is PREV-DAY close here — and BAR close in bar rows).
- Non-optional decoder fields breaking on era/entitlement variations.
- Expecting more than 1 update/sec/symbol (candles built from this feed carry 1s-sampling High/Low error — see 12).
