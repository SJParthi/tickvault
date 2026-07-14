# Global Data Feeds (GDF / GFDL) — API Reference Pack

> **Vendor:** Global Financial Datafeeds LLP (globaldatafeeds.in), Nagpur/Nashik, India — NSE/BSE/MCX/NCDEX authorized realtime **L1** data vendor.
> **Fetched:** 2026-07-13 (initial research) · **RUNNER-DOC full-corpus reconciliation: 2026-07-14** (see the dated section below — the pack is now machine-verified against the vendor's live pages nearly corpus-wide)
> **Purpose:** research-only reference pack so a future implementation session needs ZERO re-research. Companion integration PROPOSAL in `13-integration-proposal.md` — NO code exists; any code PR requires operator dated-quote authorization per `.claude/rules/project/websocket-connection-scope-lock.md`.
> **Single source of open questions:** `99-UNKNOWNS.md` — every genuinely-undetermined fact lives there with the exact question to ask GDF support or live-probe. Inline `**Unknown**` flags cross-reference it as `U-<n>`.

---

## ⚠ ACCESS CONSTRAINT — read before trusting any tier label

> **⚠ SUPERSEDED IN PART 2026-07-14:** the "NOT directly fetchable" verdict below described SANDBOX/Anthropic egress ONLY. A GitHub Actions runner machine-fetched the full 215-page corpus on 2026-07-14 (next section) — the WAF wall is egress-specific, not universal. The paragraph below is retained as the 2026-07-13 historical record; the tier history it explains still applies to content not yet re-verified.

**globaldatafeeds.in was NOT directly fetchable from the research environment** (egress proxy returned 403 for the site, for web.archive.org, and for every relay). Therefore almost nothing in this pack is labeled LIVE-DOC and nothing is labeled ARCHIVED. **Exception (2026-07-13, post-initial-draft): TWO pages — the SubscribeRealtime function page (`03` §3b) and the StreamAllSymbols page (`10`) — were pasted verbatim by the OPERATOR from a live browser and are labeled LIVE-DOC (operator-pasted).** All other doc-page content was recovered two ways:

1. **Search-engine extraction** of the live pages (substantive, URL-cited, NOT byte-verbatim) → labeled **SEARCH**.
2. **Official GFDL artifacts read as raw source**: four official PyPI packages (`Author-email: developer@globaldatafeeds.in`) and GFDL's official JavaScript sample + 2018 third-party client docs preserved verbatim in GitHub mirrors → labeled **CLIENT-LIB-SOURCE(pkg@ver)** / **GITHUB-SAMPLE**.

All verbatim JSON in this pack comes from tier-2 artifacts (plus the operator paste) and IS byte-exact. All SEARCH-tier prose should be re-verified against the live pages before contracting. **2026-07-14 update: the great majority of that re-verification is now DONE — see the reconciliation section below.**

## 2026-07-14 RUNNER-DOC full-corpus reconciliation (dated update)

**The crawl verdict — SUPERSEDES the 2026-07-13 "WAF block is total" verdict.** A GitHub
Actions runner (workflow `docs-fetch.yml`, runs 29316509037 + 29317477759, 2026-07-14)
fetched **215 pages across both hosts: 213 HTTP 200 + 2 genuine 404s, ZERO 403s.** The WAF
blocks only sandbox/Anthropic egress (`robots.txt` UA-blocks scraper agents), NOT
datacenter runner IPs. The two 404s are real absences, not blocks: the
`…/advanced_streaming_api/` index slug (its child pages exist and were fetched) and
`docs.globaldatafeeds.in/llms-full.txt` (only `llms.txt` exists).

**Corpus location:** branch `docs-fetch/gdf-2026-07-14` @ `7d354a8d`, directory
`crawl-out/` — every page stored verbatim with a provenance header (URL + fetch time +
sha256). Evidence tier for everything reconciled from it: **RUNNER-DOC (2026-07-14, url)**
— equal to LIVE-DOC in trust.

**All 5 previously slug-unknown protocol pages resolved AND fetched:** SubscribeSnapshot
(`function-subscribesnapshot`), GetLimitation (`function-getlimitation-2` WS +
`function-getlimitation` REST), GetServerInfo (`function-getserverinfo-2` WS + REST twin),
StreamAllSnapshots (`streaming_api/streamallsnapshots`), GetHolidays
(`websockets-api-documentation/getholidays` WS + `getholidays-2` REST).

**Operator-paste cross-check:** the runner fetches of the two 2026-07-13 LIVE-DOC pastes
(SubscribeRealtime, StreamAllSymbols) are IDENTICAL to the pastes in every element — zero
live-page drift.

**Per-file reconciliation summary (all 12 content files carry dated 2026-07-14 sections):**

| File | One-line verdict |
|---|---|
| 01 | Vendor status/1s-L1/quote-only-pricing/personal-use-T&C upgraded to RUNNER-DOC; maintenance windows + quota semantics + candle-construction FAQ added; GetHolidays enumerates a 10th exchange code `NCX` |
| 02 | Connect/auth/session-exclusivity/Echo confirmed verbatim; NEW: re-sending Authenticate on a live session is server-REJECTED (`Duplicated Request Not Allowed.`); maintenance windows refined to 02:00–02:30 + 08:00–08:30 (+BOD to 08:45) |
| 03 | SubscribeRealtime runner fetch byte-identical to the operator paste (zero drift); REST-twin epoch-ms half now doc-page-evidenced (glossary) |
| 04 | SubscribeSnapshot slug closed + documented; bar OPEN-stamp now DOC-STATED verbatim (U-6 CLOSED); GetExchangeSnapshot gains Day periodicity, mandatory params, and the NSE_IDX `nonTraded="true"` requirement |
| 05 | GetLastQuote family doc-verbatim; `Close` = prev-day close doc-glossed; current-era extras (PriceChange/OpenInterestChange/TotalBuyQty/TotalSellQty) captured |
| 06 | Identifier-grammar corrections (BFO `IF_`/`SF_`/`IO_` + OptionType `FF`; 2-digit short-format years); GetInstruments DOC param table + `PriceQuotationUnit`; NEW §6 GetHolidays (both transports) |
| 07 | Chain `Depth` doc-pinned 1/5/10/15/20; greeks methodology FAQ verbatim (Black-Scholes, values "directly from NSE"); SubscribeRealtimeGreeks cadence doc-pinned every second; SubscribeSnapshotGreeks wire adjudicated |
| 08 | GetHistory doc defaults are TICK + Max=0 (SDK wrapper defaults refuted); per-periodicity Period sets + retention/contractwise-depth tables doc-pinned; GetHistoryAfterMarket doc pages on BOTH transports |
| 09 | REST request From/To = epoch SECONDS (pack's ms claim REFUTED); response prose-vs-sample unit conflict flagged live-verify; docs-portal catalog rewritten from the real crawl (~50 endpoints + 61 schemas, `dTFormat`, portal WS lane) |
| 10 | StreamAllSymbols full 17-row sample folded in (STT−LTT ∈ {0,1}); **StreamAllSnapshots wire captured for the FIRST time** (`RealtimeSnapshotCollection` PascalCase envelope — a 4th dialect) |
| 11 | COMPLETE 22-row verbatim plain-text diagnostic table captured (U-10 CLOSED); REST GetLimitation JSON is FLAT (no MessageType); unsolicited `AllowVMRunningResult`/`AllowServerOSRunningResult` frames documented |
| 12 | Vendor's own candle-construction + "hundreds of trades can occur every second" admissions folded in; maintenance-window/boot-collision op note added; ZERO refutations of the 2026-07-13 quality verdicts |

**U-row scoreboard (detail in `99-UNKNOWNS.md`):**

- **CLOSED (5):** U-6 (bars open-stamped — doc-verbatim), U-10 (complete diagnostic list), U-13 (unentitled-function refusal = `Function not enabled.`), U-18 (maintenance windows verbatim), U-24's GetHolidays item (both transports documented).
- **ADVANCED / PARTIAL (10):** U-1 (StreamAllSnapshots format closed; endpoint/cadence/bandwidth open), U-3, U-7, U-8, U-9, U-11, U-12, U-21, U-22, U-23.
- **STILL OPEN (9):** U-2, U-4, U-5, U-14, U-15, U-16 (narrowed), U-17, U-19, U-20.

**Bottom line (flips the 2026-07-13 verdict):** the pack is now **RUNNER-DOC-verified
nearly corpus-wide** — verbatim machine fetches of the vendor's live pages back the claims
table below; the operator browser-paste list (`00-PASTE-LIST.md`) is demoted to a
**fallback capture route only** (for future refreshes if the runner route ever breaks).
Remaining un-fetched leaf slugs are enumerated in `00-COVERAGE-MANIFEST.md` and are
reachable by the proven runner route on the next crawl.

## 2026-07-13 freshness sweep (dated update — historical; crawl verdict SUPERSEDED 2026-07-14)

A same-day re-verification pass (crawl + PyPI + slug discovery), run after the pack build:

**Crawl verdict — the WAF block is still TOTAL.** *(superseded 2026-07-14: the block is
egress-specific — see the section above)* 42/42 requests across BOTH hosts
(`globaldatafeeds.in` AND `docs.globaldatafeeds.in` — robots.txt, sitemaps, every resolved
doc URL, the docs-portal root and a known deep page) returned HTTP 403 via WebFetch, which
does reach the site (sandbox curl is proxy-denied at CONNECT before reaching it, so it gives
no WAF signal). archive.org (Wayback availability API + CDX) is unreachable from this
environment entirely, so no ARCHIVE-tier fallback was possible. **Zero bytes of page content
were retrieved**; nothing in this pack changed tier as a result of the sweep.

**PyPI verdict — 4/4 official SDKs unchanged since the pack's baseline; zero protocol drift
possible.** Verified via the PyPI JSON API 2026-07-13:

| Package | Baseline = latest | Latest upload date |
|---|---|---|
| `wsgfdl-py` | 1.3.5 | 2025-11-28 |
| `gfdl-rest` | 1.0.3 | 2024-11-11 |
| `GFDLWS` | 0.0.4 | 2024-09-16 |
| `ws-gfdl` | 1.1.0 | 2022-08-12 |

Two additional official-author (`developer@globaldatafeeds.in`) packages were found, neither
carrying protocol content: `GlobalDatafeeds-ws` 0.0.1 (2024-10-14) is a **verified empty
shell** — wheel + sdist both contain zero code modules; `restgfdl-py` is indexed on PyPI but
has **zero retrievable releases** (JSON API returns Not Found). No pack update required.

**New slug leads (tier: SEARCH-SNIPPET only — URLs seen in search results; page content was
NEVER fetched and must not be treated as captured):** `P/rest-api-documentation/function-getsnapshot/`
(REST GetSnapshot — new), `P/introduction/introduction-to-apis/`, six
`P/api-code-samples/…` slugs (websockets-python/-java/-nodejs/-javascript-sample,
rest-python, download-code-samples-2), documentation index slugs under
`…/documentation-support/documentation/`, the docs-portal slug pattern
`docs.globaldatafeeds.in/<function>-<10-hex>` (e.g. `getserverinfo-15575607e0`), and the
asset `https://globaldatafeeds.in/wp-content/uploads/2024/07/GFDL_Websocket_Python_Sample_AllFunctions.pdf`
(a WS-Python all-functions sample PDF). GetHolidays is confirmed present in BOTH the WS and
REST function lists via snippet nav text; its exact slug is still unknown.
*(2026-07-14: all of these — including GetHolidays' slugs and the PDF — were fetched by the
runner crawl.)*

**Operator capture queue:** [`00-PASTE-LIST.md`](./00-PASTE-LIST.md) is the ordered
browser-paste list (Tier 1 = protocol-critical never-captured pages first) — each paste
becomes LIVE-DOC tier, the only route past the WAF. *(2026-07-14: demoted to fallback.)*

**Coverage manifest:** [`00-COVERAGE-MANIFEST.md`](./00-COVERAGE-MANIFEST.md) enumerates
EVERY known page/sub-section of GDF's docs (from the LIVE-DOC sidebar nav capture below +
the pack's URL universe + the search-derived slugs) with a per-URL status — 2/81 rows
fetched-verbatim as of 2026-07-13 *(2026-07-14: rewritten — 213/215 known URLs
fetched-verbatim)*.

## Evidence-tier legend (header block on every section)

| Tier | Meaning |
|---|---|
| `RUNNER-DOC (YYYY-MM-DD)` | verbatim HTML of the named live page, machine-fetched by OUR OWN GitHub Actions runner via `.github/workflows/docs-fetch.yml` (corpus branch `docs-fetch/gdf-2026-07-14` @ `7d354a8d`, per-page sha256 provenance headers). **Equal to LIVE-DOC in trust** — same live-page content, machine-verbatim capture |
| `LIVE-DOC` | live globaldatafeeds.in page content — **used ONLY for operator-pasted live pages (2026-07-13: SubscribeRealtime `03` §3b + StreamAllSymbols `10`)**; the 2026-07-14 runner fetches of both are byte-identical to the pastes |
| `ARCHIVED` | web.archive.org capture — **UNUSED (unattainable)** |
| `SEARCH` | search-engine extraction of the named live page URL, 2026-07-13; substantive but not byte-verbatim |
| `CLIENT-LIB-SOURCE(pkg@ver)` | read from an extracted OFFICIAL GFDL PyPI sdist: `wsgfdl-py@1.3.5` (2025-11-28, newest WS lib), `GFDLWS@0.0.4`, `ws-gfdl@1.1.0` (2022), `gfdl-rest@1.0.3`. Includes each package's official README with real captured server responses |
| `GITHUB-SAMPLE(js-2020)` | GFDL's official JavaScript "NimbleWebStream" sample ("updated 29th June 2020" + 2021 Greeks additions), mirrored byte-verbatim at github.com/rohittrank/trade `application/views/websocket_view.html` — carries `//GFDL :` authored comments |
| `GITHUB-SAMPLE(dhelm-2018)` | github.com/kncsolutions/dhelm-gfeed-python-client `docs/index.rst` (2018) — reproduces verbatim GDF server response JSON incl. a real key's LimitationResult |
| `ARITH` | arithmetic verification performed during the 2026-07-13/2026-07-14 reconciliation passes (epoch decodes, count checks) |

Confidence labels: **Verified** (multiple consistent sources or official artifact) / **Assumed** (single source or inferred) / **Unknown** (→ `99-UNKNOWNS.md`).

## File index

| File | Contents |
|---|---|
| `00-PASTE-LIST.md` | the operator browser-paste capture queue (2026-07-13) — **demoted to FALLBACK route 2026-07-14** (the runner crawl succeeded); kept for future refreshes if the runner route breaks |
| `00-COVERAGE-MANIFEST.md` | the completeness manifest — **rewritten 2026-07-14**: 213/215 known URLs fetched-verbatim (RUNNER-DOC), 2 genuine 404s, remaining leaf slugs enumerated for the next crawl |
| `01-product-and-entitlement.md` | vendor status, segments matrix, product tiers, pricing evidence, licensing, trial |
| `02-authentication-and-connection.md` | endpoint model, Authenticate handshake, session exclusivity (new-connection-wins), Echo heartbeat, reconnect contract, maintenance windows, frame sizes |
| `03-websocket-realtime-feed.md` | SubscribeRealtime/Unsubscribe, 1/sec/symbol cadence, COMPLETE RealtimeResult field table, PreOpen, OI, no depth |
| `04-websocket-snapshot-bars.md` | SubscribeSnapshot streamed bars, GetSnapshot, bar-stamp convention, no-trade-bar gaps, daily-stamp anomaly |
| `05-websocket-quote-functions.md` | GetLastQuote family, arrays, GetSnapshot(Greeks) request+response schemas |
| `06-instruments-and-identity.md` | reference functions, full identifier grammar, GetInstruments field table, GetHolidays, mapping to tickvault identities |
| `07-options-and-greeks.md` | option chain functions (Depth = strike count), Greeks subscribe/quote/history, 18-field schema |
| `08-historical-api.md` | GetHistory WS+REST dialects, periodicities, 3-layer depth model, ordering, AdjustSplits, SDK bugs |
| `09-rest-api.md` | base URL, GET-only, accessKey, paths, format switches, envelopes, quotas |
| `10-streaming-push-api.md` | StreamAllSymbols/StreamAllSnapshots "Push Type API" — both wire formats now captured |
| `11-limits-errors-diagnostics.md` | GetLimitation full schema, the complete verbatim diagnostic table, quota rules |
| `12-data-quality-assessment.md` | timestamps, conflation, candle provenance, latency, comparison vs Dhan-WS and Groww-WS |
| `13-integration-proposal.md` | **PROPOSAL** — feed-#3 integration map, 7-PR series, binding rule-file constraints |
| `99-UNKNOWNS.md` | the single consolidated open-questions table (U-1 … U-24) |

## Reconciled master claims table (44 facts adjudicated 2026-07-13 + 4 added 2026-07-14 = 48)

Adjudicated 2026-07-13 across 6 parallel research passes + raw artifacts; conflicts resolved
by arithmetic + artifact grep. **2026-07-14:** every row cross-checked against the RUNNER-DOC
corpus by 6 reconciliation workers — dated in-place notes below; the verdicts remain the
pack's authority.

| # | Claim | Label |
|---|---|---|
| 1 | GFDL = NSE/BSE/MCX(2010)/NCDEX authorized realtime **L1** vendor; Nagpur/Nashik LLP — 2026-07-14: vendor status + "Established in 2010" verbatim RUNNER-DOC (authorised-data-vendors) | Verified |
| 2 | Transports: WebSocket, REST, .NET, COM, FIX; WS is JSON-only | Verified |
| 3 | Exchange codes: NSE, NSE_IDX, NFO, CDS, MCX (+ BSE, BFO, BSE_IDX, BSE_DEBT on REST GetExchanges) — 2026-07-14: GetHolidays rows enumerate a 10th code `NCX` (NCDEX) in their `Exchanges` field; the entitlement/GetExchanges list may be key-scoped while the holiday calendar is global (01 §2) | Verified |
| 4 | WS endpoint `ws://endpoint:port`, per-account host+port issued on purchase; none hardcoded anywhere — 2026-07-14: RUNNER-DOC (how-to-connect); doc-cited TEST endpoint `ws://test.lisuns.com:4575` (FAQ; closed weekends/holidays) | Verified |
| 5 | No wss:// anywhere; plaintext ws + API key in cleartext frame — 2026-07-14: absence CONFIRMED corpus-wide (zero `wss://` in all 215 RUNNER-DOC pages); nuance: the FD docs-portal test server IS https (`https://test.lisuns.com:4532`) and one REST doc example shows `https://endpoint:port` — U-2 (market-data WS TLS) still open | Verified (absence) / **Unknown** (availability, U-2) |
| 6 | Auth = single JSON frame `{"MessageType":"Authenticate","Password":"<API_KEY>"}` → `AuthenticateResult{Complete:true,Message:"Welcome!"}`; no headers/query auth — 2026-07-14: RUNNER-DOC (authenticate page); NEW: re-sending Authenticate on the SAME session is REJECTED (`Duplicated Request Not Allowed.`) — reconnect-then-authenticate, never re-auth in place | Verified |
| 7 | 1 active WS session per key; a NEW connection INVALIDATES the previous one ("Access Denied. Key already in use by other session.") — 2026-07-14: verbatim RUNNER-DOC (how-to-connect); reject envelope = text/plain diagnostic (U-11 partially closed) | Verified |
| 8 | Server sends plain-TEXT diagnostic frames (expired/invalid key, "Reached instrument limitation") — parser must tolerate non-JSON — 2026-07-14: the COMPLETE 22-message list is now captured verbatim (11 §3.1, diagnostic-api-responses) | Verified |
| 9 | Full diagnostic message list — **captured verbatim 2026-07-14** (22 rows + FD-portal extras, 11 §3.1); U-10 CLOSED (residual probe: does an `AuthenticateResult{Complete:false}` JSON carry these strings, or do failures arrive text/plain only) | Verified (RUNNER-DOC 2026-07-14) |
| 10 | Heartbeat: server→client `MessageType:"Echo"` every few seconds; no client pong in any sample — 2026-07-14: payload RUNNER-DOC (`{"MessageType":"Echo"}` only); ~2–3 s cadence observed in official transcripts; Echo can arrive BEFORE AuthenticateResult | Verified |
| 11 | Reconnect contract: full re-auth + re-subscribe every symbol; no resume/sequence protocol — 2026-07-14: RUNNER-DOC (how-to steps 1–3); NOTE: unsubscribe ALSO consumes hourly quota (FAQ 100+100+100 rotation example) | Verified |
| 12 | SubscribeRealtime: 1 request/symbol, no batch form; pushes ~1/sec/symbol full-image L1 tick — 2026-07-14: RUNNER-DOC re-confirmed (machine fetch identical to the 2026-07-13 paste; api-pricing "pushes market data every second") | Verified |
| 13 | Tick payload: LTP/LTQ/ATP, best bid/ask+qty, OHL, `Close`=PREV-DAY close, Volume, Value, OI+OIChange inline, QuotationLot, PreOpen flag, PriceChange(%), LastTradeTime+ServerTime | Verified |
| 14 | `Close` means prev-day close in quote/realtime family but BAR close in Snapshot/History rows — context-dependent — 2026-07-14: doc-glossed verbatim on GetLastQuote + GetLastQuoteArray ("Close (previous Day's Close)"); the StreamAllSnapshots page is a live example of the trap: its prose glosses `Close` as prev-day close while its own bar sample proves BAR close (10 §2c) | Verified |
| 15 | Timestamps: WS epoch-seconds true UTC; REST epoch-ms (`…000`); no sub-second precision anywhere — 2026-07-14 refinements: WS-seconds RUNNER-DOC; REST-ms half doc-page-evidenced (glossary 13-digit samples) BUT (a) REST REQUEST From/To = epoch SECONDS (doc-verbatim), (b) "always `…000`" softened to "usually" (real ms `1415114727418` seen), (c) REST prose SAYS seconds while the same pages' samples show ms — live-verify (U-23 residual), (d) REST XML = a 4th HUMAN-datetime dialect | Verified (ARITH ×2 + RUNNER-DOC) |
| 16 | 2018-era ticks lack PriceChange/OIChange fields — schema grew; decoder fields must be Option-al | Verified |
| 17 | Scientific-notation floats on the wire (Greeks) — 2026-07-14: incl. a malformed doc exponent (`8.616986633569468E06`) the decoder should tolerate | Verified |
| 18 | SubscribeSnapshot = per-symbol streamed bar closes; pushed when bar completes — 2026-07-14 correction (RUNNER-DOC function-subscribesnapshot): Periodicity = **MINUTE/HOUR ONLY** (Day is doc-listed on GetExchangeSnapshot, NOT here); the doc Period list is open-ended ("1, 2, 3…") — the SDK's 1,2,5,10,15,30 set is a client-side hint | Verified |
| 19 | Bar labels = bucket-START (open-time) stamp — **DOC-STATED verbatim 2026-07-14** on WS GetSnapshot + GetSnapshot(Delayed) ("timestamp of the returned data will be start timestamp of the period"); SubscribeSnapshot(Delayed) worked example corroborates; U-6 CLOSED — first-live-session sanity check is cheap insurance, not a blocker | Verified (RUNNER-DOC 2026-07-14) |
| 20 | Daily-bar LASTTRADETIME = 18:00 IST (2024 REST series) / 06:00 IST (2018) — vendor processing stamp; date label only, never a time key — 2026-07-14: 18:00 IST re-confirmed on the official AllFunctions PDF DAY sample (ARITH) | Verified (ARITH) |
| 21 | GetHistory: Tick/Minute/Hour/Day/Week/Month; From=0→full history; rows newest-first; AdjustSplits param (default true) — 2026-07-14: params RUNNER-DOC (function-gethistory-2); doc/server defaults are Periodicity=TICK + Max=0 (=all) — the Minute/Max=10 defaults were SDK wrapper behavior; per-periodicity Period sets doc-pinned (Minute 1,2,3,4/5,6,10,12/15,20,30; Hour 1,2,3,4,6,8,12) | Verified |
| 22 | History depth = 3 layers: doc ceiling vs plan tier vs per-key GetLimitation — GetLimitation is authoritative — 2026-07-14: doc ceiling RUNNER-DOC (tick "Every 1 Sec"/1 cal week; 1–4m 3mo; 5–12m 4.5mo; 15–30m + hourly 6mo; D/W/M since 2010; NEW contractwise splits: futures 3 cal months / options 1 cal month contractwise, continuous since 2010); MaxIntraday 44 / MaxEOD 100000 / MaxTicks 5 appear in the OFFICIAL WS doc sample — per-key values still probe (U-9) | Verified (mechanism + doc ceiling) / probe (per-key values) |
| 23 | GetLimitation returns full entitlement matrix (functions on/off, per-exchange symbol caps + DataDelay, call/bandwidth quotas, history caps) — 2026-07-14: schema RUNNER-DOC (function-getlimitation-2); "−1 = unlimited" DOWNGRADED to Assumed-inference (no doc literal states it; samples use −1 pervasively); REST GetLimitation JSON dialect is FLAT (no GeneralParams wrapper, NO MessageType; XML root `LimitationInfoResult`) — a shared WS/REST deserializer is wrong by construction | Verified (schema) / Assumed (−1 semantics) |
| 24 | Real observed plan values: 7200 calls/hr, 5.36M/month, 200 sym/exchange (NSE 160 + NSE_IDX 40) on a 2018 key; FAQ tiers 1800/3600/7200/hr — 2026-07-14: the tiers are now RUNNER-DOC verbatim (FAQ; hourly clock-hour reset "e.g. 10AM, 11AM, 12PM"; subscribe+unsubscribe+resubscribe EACH consume) | Verified |
| 25 | Max 25 symbols per Array/GetSnapshot call; 20 rows GetInstrumentsOnSearch; 5 snapshots per GetExchangeSnapshot call — 2026-07-14: 25-cap + 5-snapshot cap RUNNER-DOC | Verified |
| 26 | Huge single frames (full instrument master) — official SDK lifts frame cap to 2^50; client must not cap at defaults | Verified |
| 27 | Request booleans are STRINGS (`"Unsubscribe":"false"`); request-key casing inconsistent across functions/libs — treat server as case-tolerant (verify live, U-16) — 2026-07-14: the DOCS THEMSELVES mix casings (`Unsubscribe` doc vs `unsubscribe` SDK; `accesskey`/`accessKey`; `isShortIdentifier:"TRUE"` echoed as `IsShortIdentifier:true`) — tolerance strongly implied, still not vendor-stated | Verified (string bools) / Assumed (case tolerance) |
| 28 | Identity: string InstrumentIdentifier in 3 formats (continuous -I/-II/-III; long TYPE_PRODUCT_EXPIRY_OPT_STRIKE incl. FUTCOM double-underscore; contractwise short = TradeSymbol); indices = display names with spaces ("NIFTY 50") on NSE_IDX — 2026-07-14: grammar RUNNER-DOC (symbol-naming-conventions); short-format years are 2-DIGIT (the "4-digit-year" reading was a mis-parse); BSE indices are BARE (`SENSEX`, `BANKEX`); continuous -I/-II/-III also on BFO + NFO stocks | Verified |
| 29 | GetInstruments row carries `TokenNumber` (exchange numeric token, string) + ISIN (equities) + QuotationLot + Series + price bands + 52wk — 2026-07-14: field list + `detailedInfo` gating RUNNER-DOC (getinstruments); adds `PriceQuotationUnit`; "TokenNumber == the exchange's own token" stays Assumed (U-21 partial) | Verified (fields) / Assumed (token identity) |
| 30 | Greeks realtime/last-quote APIs key on **Token**, not identifier; 18 greek fields incl. Vanna/Charm/Zomma/Volga/DTR; historical greeks exist — 2026-07-14: methodology DOC-verbatim (values "directly from NSE"; IV Black-Scholes on futures/SYNTHETIC-futures underlying); SubscribeRealtimeGreeks cadence doc-pinned "every second"; GetHistoryGreeks Periodicity MINUTE/TICK only | Verified |
| 31 | L1 only — no depth function on WS/REST; OptionChain `Depth` = strike-count around ATM — 2026-07-14: L1-only re-confirmed RUNNER-DOC (api-pricing + type-of-data + the /apis/ page); chain `Depth` values doc-pinned **1, 5, 10, 15, 20** | Verified |
| 32 | Streaming API (StreamAllSymbols/StreamAllSnapshots) exists as separate doc'd product; StreamAllSymbols wire LIVE-DOC 2026-07-13 + RUNNER-DOC 2026-07-14 (full 17-row sample; STT−LTT ∈ {0,1}); **StreamAllSnapshots wire RUNNER-DOC-captured 2026-07-14** (`RealtimeSnapshotCollection` PascalCase envelope, key-level Exchange+Periodicity+Period — 10 §2c); endpoint/cadence/bandwidth still **Unknown** (U-1 residual) | Verified (existence + BOTH wire formats) / Unknown (residual) |
| 33 | Whole-exchange pull exists TODAY without Streaming API: GetExchangeSnapshot (+ server-side GetExchangeSnapshotAfterMarket) — 2026-07-14: periodicity = Minute/Hour/**Day**, BOTH periodicity+period MANDATORY, From/To epoch-seconds + 5-snapshot cap, and `nonTraded="true"` REQUIRED for NSE_IDX (indices otherwise absent from exchange snapshots) | Verified |
| 34 | Delayed API = same functions with backend-configured per-exchange delay; same response fields — 2026-07-14: delayed twins RUNNER-DOC (same wire MessageTypes; delay visible only via LastTradeTime vs clock; GetSnapshot (Delayed) "only active during market hours") | Verified |
| 35 | REST: GET-only, `accessKey` query param, path = function name + `/`; JSON default, `xml=true`, `format=CSV`; ALL-CAPS keys; known SDK bugs — 2026-07-14: RUNNER-DOC verbatim (`?accesskey=apikey`; "HTTP POST … WILL NOT work"); docs mix `accesskey`/`accessKey` casing (U-16); JSON arrays are BARE (no Result envelope) | Verified |
| 36 | API pricing: unpublished, quote-only (sales@globaldatafeeds.in; +91-77210 80002); free trial all APIs, duration unpublished; personal-use-only tier for individuals — 2026-07-14: unpublished-pricing + personal-use halves RUNNER-DOC (api-pricing / who-can-purchase); trial DURATION still unpublished; the phone number remains SEARCH-tier (not on the fetched pages) | Verified |
| 37 | Desktop pricing anchors (NOT the API): ProPlus NSE-only ≈ ₹2,775/mo; all-exchange ≈ ₹9,340/mo; reseller ProPlus 225-sym ₹3,199 (+MCX ₹3,229); Pro2 ≈ ₹70k/yr (2019) — 2026-07-14: the ₹233/mo extra-computer figure is now FIRST-PARTY (nimbledatapro); other anchors remain third-party | Assumed (3rd-party, mixed dates) / Verified (₹233 first-party) |
| 38 | Usage: single-subscriber charting/analysis; redistribution banned; no gaming/virtual-trading; NSE non-display (algo) entitlement UNSTATED — 2026-07-14: stated parts RUNNER-DOC verbatim (/apis/#terms-and-conditions); ZERO "non-display" hits in the 215-page corpus — U-5 unchanged; NEW documented tension: /apis/ + FAQ market algo/HFT use vs the single-subscriber charting/analysis T&C | Verified (stated parts) / **Unknown** (non-display, U-5) |
| 39 | Maintenance windows **02:00–02:30 & 08:00–08:30 IST** (BOD process sometimes to 08:45; symptom "Connection refused"; vendor advice connect ≥08:45) — collides with tickvault's 08:30 IST boot: GDF connect must defer to ≥08:45 | Verified (RUNNER-DOC 2026-07-14, FAQ verbatim — U-18 CLOSED) |
| 40 | Candles are vendor-built from the 1s feed (no exchange-official claim); exchange-official EOD via separate GetBhavCopyCM/FO — 2026-07-14: vendor-built half now Verified (FAQ verbatim: each vendor's own tick aggregation + filtering; Close + day-Volume match the exchange, O/H/L may differ cross-vendor) | Verified |
| 41 | No independent latency/reliability measurements exist | Unknown (U-19) |
| 42 | GetHistoryAfterMarket: separate function (prev-working-day history, cheaper; not co-enabled with GetHistory on one key); SDK opens its own WS connection for it — 2026-07-14: mutual exclusion RUNNER-DOC verbatim (FAQ "not enabled together for the same API key"); doc pages exist on BOTH transports ("till previous working day", current day only after close, "saves API costs") | Verified |
| 43 | Pre-open ticks delivered in-band with `PreOpen:true` flag — 2026-07-14: glossary adds only `PREOPEN` = "If Preopen is active" — pre-open OHLC semantics still open (U-15) | Verified (flag) / **Unknown** (pre-open OHLC semantics, U-15) |
| 44 | WS product internal name = "NimbleWebStream" (JS sample title); no product named "NimbleTBT" exists in any artifact; the only TBT hint is the FIX API tier (nature unknown, U-4) — 2026-07-14: FIX remains marketing-only (zero FIX documentation in the corpus) | Verified |
| 45 | **GetHolidays (both transports, added 2026-07-14):** WS — Exchange optional, From/To epoch-secs (default "holidays from 2010") → `HolidaysResult` with `Date` = epoch-secs; REST — accessKey + MANDATORY `year`, optional exchange/xml/CSV → `Date` = STRING **MM-DD-YYYY**; rows enumerate 10 exchange codes incl. `NCX` (06 §6) | Verified (RUNNER-DOC 2026-07-14) |
| 46 | **BFO identifier grammar (added 2026-07-14):** long-format uses `IF_`/`SF_`/`IO_` prefixes with OptionType `FF` (`IF_BANKEX_28JUN2024_FF_0`, `IO_SENSEX_28JUN2024_PE_71800`) — NOT FUTIDX/OPTIDX (06 §2a) | Verified (RUNNER-DOC 2026-07-14) |
| 47 | **Unsolicited post-auth frames (added 2026-07-14):** `{"AllowVMRunning":false,"MessageType":"AllowVMRunningResult"}` + `AllowServerOSRunningResult` appear in official transcripts; Echo can precede AuthenticateResult; keys can be IP-BOUND (`IP address not allowed.` diagnostic) — decoder must tolerate all three (02/11) | Verified (RUNNER-DOC 2026-07-14) |
| 48 | **Current-era LastQuoteResult extras (added 2026-07-14):** `PriceChange`, `PriceChangePercentage`, `OpenInterestChange` (+ `TotalBuyQty`/`TotalSellQty` as FLOATS on WS; REST presence Unknown) — all Option-al in the decoder (05 §1/§2a) | Verified (RUNNER-DOC 2026-07-14) |

## Refuted claims (do NOT resurrect)

| Refuted claim | Truth |
|---|---|
| "EOD/daily bars stamped 09:15 IST session open" | ARITH-refuted: 2024 REST daily series uniformly 18:00:00 IST; 2018 sample 06:00 IST. Vendor processing stamp — date label only |
| "GFDL provides millisecond timestamps" (third-party roundup) | Contradicted by every official wire sample: WS = whole seconds; REST ms values usually end `000`. Re-confirmed 2026-07-14 across the full corpus |
| "SubscribeSnapshot bar-open stamp is Verified" | Downgraded to Assumed 2026-07-13 (a decisive arithmetic example was wrong) — **RE-UPGRADED to Verified 2026-07-14**: the open-stamp is DOC-STATED verbatim (WS GetSnapshot + Delayed pages); U-6 CLOSED |
| "StreamAllSymbols/StreamAllSnapshots don't exist" | They exist as a separate docs'd Streaming API product — 2026-07-14: BOTH wire formats now captured (10 §2b/§2c) |
| "GDF offers native exchange TBT with zero aggregation" (stratzy blog) | Puffery: GDF's own docs say 1-second L1 — 2026-07-14: first-party-reinforced ("hundreds of trades can occur every second" FAQ admission); NSE's vendor broadcast is itself ~1s snapshots |
| "Re-send Authenticate to recover a session" (SDK-inferred) | Added 2026-07-14: server REJECTS a second Authenticate on the same session (`Duplicated Request Not Allowed.`) — reconnect-then-authenticate |
| "GetHistory REST from/to are epoch-ms" (pack 09 §3, SDK-tier) | Refuted 2026-07-14: REST request from/to are epoch SECONDS (doc-verbatim examples); only RESPONSE timestamps show ms |

## Merged function catalog (42 functions + Authenticate + Echo — GetHolidays added 2026-07-14)

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
| **GetHolidays** | WS+REST — **RUNNER-DOC-documented 2026-07-14** (WS From/To epoch-secs → `HolidaysResult`; REST mandatory `year`, MM-DD-YYYY string dates) | — | 06 |
| GetServerInfo / GetLimitation | WS+REST | JS W1 W2 W3 R | 11 |
| GetMarketMessages / GetExchangeMessages | WS+REST | JS W1 W2 W3 R | 11 |
| GetTopGainersLosers / GetVolumeShockers | WS+REST / WS | W2 W3 R / W3 | 05 — 2026-07-14: a REST GetVolumeShockers page exists in the live nav; "WS only" was wrong at surface level (05 §4) |
| Echo (server-push heartbeat) | WS | JS (comment) | 02 |
| GetExchangeSnapshotAfterMarket / GetLastQuoteOptionChainWithGreeks | server-side names (LimitationResult) — 2026-07-14: ZERO mentions in the 215-page corpus; wire shapes stay Unknown (U-20) | — | 11, 07 |
| **StreamAllSymbols** | Streaming API — wire format LIVE-DOC 2026-07-13 + **RUNNER-DOC 2026-07-14 full 17-row sample** (Batch dialect, abbreviated keys; endpoint/cadence still open) | — | 10 |
| **StreamAllSnapshots** | Streaming API — **wire RUNNER-DOC-captured 2026-07-14** (`RealtimeSnapshotCollection` PascalCase envelope — 10 §2c) | — | 10 |
| Delayed twins (SubscribeSnapshot/GetSnapshot/GetHistory/GetExchangeSnapshot Delayed) | Delayed API — 2026-07-14: dedicated pages RUNNER-DOC on both WS + REST lanes | — | 01, 04 |

## The CURRENT (2026) live docs tree — authoritative function inventory

> **Source:** sidebar navigation of the live docs, captured in the 2026-07-13 operator paste of the SubscribeRealtime page · **Tier:** LIVE-DOC (operator-pasted).
> **2026-07-14:** the tree below is corroborated by the RUNNER-DOC crawl's section/archive pages; GetHolidays' wire shape is now captured (06 §6) — the "wire shape uncaptured" clause is closed; the Streaming API section = exactly {StreamAllSymbols, StreamAllSnapshots} (no third function).

Top-level API sections in the 2026 docs: **WebSockets API, Streaming API, OptionChain API, Greeks API, Delayed API, GainersLosers, Volume Shockers, GetHolidays** — i.e. Greeks/OptionChain/Delayed/GainersLosers/VolumeShockers are SEPARATE top-level sections in 2026 (this pack's Greeks coverage in `07` is RUNNER-DOC-reconciled 2026-07-14), the **Streaming API is confirmed as a live current section** (10), and **`GetHolidays` is now fully documented** (was: "wire shape uncaptured", U-24).

WebSockets API functions listed in the 2026 sidebar (26): How To Connect, Authenticate, SubscribeRealtime, SubscribeSnapshot, GetLastQuote, GetLastQuoteShort, GetLastQuoteShortWithClose, GetLastQuoteArray, GetLastQuoteArrayShort, GetLastQuoteArrayShortWithClose, GetSnapshot, GetHistory, GetHistoryAfterMarket, GetExchanges, GetInstrumentsOnSearch, GetInstruments, GetInstrumentTypes, GetProducts, GetExpiryDates, GetOptionTypes, GetStrikePrices, GetServerInfo, GetLimitation, GetMarketMessages, GetExchangeMessages, GetExchangeSnapshot — all covered by this pack; GetHistoryAfterMarket + GetMarketMessages/GetExchangeMessages existence in the CURRENT docs is thereby LIVE-DOC-confirmed. Remaining sidebar sections: REST API, DotNet API, COM API, Code Samples, API Fields Description, Symbol Naming Conventions, Diagnostic API Responses, Pricing & Sales, Contact, FAQs.

## What this pack prevents

- Building a client against hallucinated endpoints (there are NONE public — host:port is per-account).
- Importing REST epoch-milliseconds into WS parsing (dialects differ; REST requests are seconds, responses ms).
- Treating GDF as a tick-by-tick or millisecond feed (it is a 1s-conflated L1 feed — see 12).
- Killing your own production session with a debug connect (new-connection-wins — see 02), or re-sending Authenticate on a live session (server-rejected — see 02).
- Panicking on plain-text diagnostic frames, unsolicited AllowVMRunning frames, or petabyte-class instrument-master frames.
- Presenting desktop-product INR prices as API prices (API pricing is quote-only).
- Booting the GDF connect inside the vendor's 08:00–08:45 IST BOD window ("Connection refused" — connect ≥08:45).
