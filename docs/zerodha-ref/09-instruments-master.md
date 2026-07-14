# Kite Connect v3 — Instruments Master (full reference)

> **Source:** https://kite.trade/docs/connect/v3/market-quotes/#retrieving-full-market-instruments (primary; search results also surface a sibling page https://kite.trade/docs/connect/v3/market-data-and-instruments/ — same content family)
> **Fetched-Verified:** 2026-07-13 via CLIENT-LIB-SOURCE (pykiteconnect@master `connect.py` + `ticker.py`; gokiteconnect@master `market.go` + `connect.go` + `ticker/ticker.go`), OFFICIAL-MOCK (`kiteconnect-mocks@main` `instruments_all.csv` + `instruments_nse.csv`), SEARCH (kite.trade docs/forum result snippets), ARITH. **The live kite.trade docs page is UNREACHABLE from this sandbox** (egress proxy CONNECT 403 on `kite.trade:443`; web.archive.org and r.jina.ai equally blocked — see `zerodha-probe.md`), so ZERO claims here are byte-verbatim from the docs HTML.
> **Evidence tiers:** see README legend — Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) / SEARCH (snippet-level, not byte-verbatim — sub-Verified; per the pack's hard rule NOTHING search-derived carries a bare Verified prefix) / ARITH / Assumed / Unknown.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. Any TickVault consumption of Kite endpoints requires its own dated operator grant per the pluggable-feed rule files.

---

## 1. Endpoints

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master kiteconnect/connect.py, fetched 2026-07-13):** the SDK `_routes` dict defines exactly two instrument-master routes (root `https://api.kite.trade`, `_default_root_uri` at connect.py:36):

| Route key | Path | Meaning |
|---|---|---|
| `market.instruments.all` | `GET /instruments` | full dump, ALL exchanges |
| `market.instruments` | `GET /instruments/{exchange}` | one exchange only (e.g. `/instruments/NSE`) |

(connect.py:148-149; `instruments(exchange=None)` at connect.py:607-619 dispatches between them.)

**Cross-check — Verified (CLIENT-LIB-SOURCE gokiteconnect@master connect.go, fetched 2026-07-13):** `URIGetInstruments = "/instruments"`, `URIGetInstrumentsExchange = "/instruments/%s"` (connect.go:147-149) — identical paths, consumed by `GetInstruments()` / `GetInstrumentsByExchange(exchange)` in market.go:309-321.

**Related sibling routes on the same path prefix (for orientation only):** `market.historical = /instruments/historical/{instrument_token}/{interval}` (covered in `08-historical-candles.md`) and `market.trigger_range = /instruments/trigger_range/{transaction_type}` (connect.py:151-152; covered in `07-market-quotes.md` §10 — cross-SDK route divergence + the official `trigger_range.json` mock recorded there). The mutual-fund master is a SEPARATE endpoint `GET /mf/instruments` (connect.py:146) with a different CSV schema — do not conflate.

**Headers sent by the SDK on these calls — Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py:938-946):** `X-Kite-Version: 3` (constant `kite_header_version = "3"`, connect.py:41) and, when both are set, `Authorization: token <api_key>:<access_token>`. **Unknown:** whether the server actually REQUIRES the Authorization header for `/instruments` (forum snippets suggest the CSV link is directly downloadable, but that is a search-summary attribution risk — see OPEN QUESTIONS).

## 2. Response format — gzipped CSV, not JSON

**SEARCH (kite.trade docs snippet via web search 2026-07-13, https://kite.trade/docs/connect/v3/market-quotes/):** unlike the rest of the API (JSON envelopes), the instrument list API "returns a gzipped CSV dump of instruments across all exchanges that can be imported into a database". Snippet-level wording, not byte-verbatim.

**Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py:991-992):** the SDK's response handler branches on `Content-Type` — `"csv" in r.headers["content-type"]` → return raw `r.content` bytes (no JSON envelope, no `data` unwrapping). Everything else JSON-shaped goes through the `{status, data, error_type, message}` envelope.

**Assumed — gzip is TRANSPORT-level (`Content-Encoding: gzip`), not a `.gz` file body:** neither pykiteconnect nor gokiteconnect contains any explicit gunzip step; pykiteconnect feeds `r.content` straight into `csv.DictReader` (connect.py:855-879) and gokiteconnect feeds `resp.Body` straight into `gocsv.UnmarshalBytes` (market.go:290-306). Both HTTP stacks transparently decompress `Content-Encoding: gzip`. If the body were an opaque `.gz` file, both official SDKs would be broken — so transport-level compression is the only consistent reading. Confirm on first live fetch.

**Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py:855-879) — client-side type coercion applied per row** (useful as a parser contract):

| Field | Coercion |
|---|---|
| `instrument_token` | `int` |
| `last_price`, `strike`, `tick_size` | `float` |
| `lot_size` | `int` |
| `expiry` | parsed to a date ONLY when the string is exactly 10 chars (`YYYY-MM-DD`); empty string otherwise left as-is |

Everything else stays a string.

## 3. CSV columns — all 12, with meanings

**Verified (OFFICIAL-MOCK instruments_all.csv + instruments_nse.csv, kiteconnect-mocks@main c7a8123, fetched 2026-07-13):** header row, in order:

```
instrument_token,exchange_token,tradingsymbol,name,last_price,expiry,strike,tick_size,lot_size,instrument_type,segment,exchange
```

**Cross-check — Verified (CLIENT-LIB-SOURCE gokiteconnect market.go:77-90):** the `Instrument` struct carries exactly these 12 `csv:` tags: `instrument_token, exchange_token, tradingsymbol, name, last_price, expiry, strike, tick_size, lot_size, instrument_type, segment, exchange`.

| # | Column | Type (post-coercion) | Meaning | Tier |
|---|---|---|---|---|
| 1 | `instrument_token` | int (uint32 on the wire) | Kite's numeric instrument identifier; THE key for WebSocket subscription and historical-data calls (`historical_data(instrument_token, …)`, connect.py:664-684) | Verified (CLIENT-LIB-SOURCE + OFFICIAL-MOCK) |
| 2 | `exchange_token` | int | the exchange-assigned token for the instrument; `instrument_token = exchange_token*256 + segment_id` (see §5) | Verified (OFFICIAL-MOCK + ARITH) |
| 3 | `tradingsymbol` | string | exchange trading symbol, incl. series suffixes on NSE cash (`CENTRALBK-BE`, `CDSL-BL`) and derivative names (`BANKNIFTY18JAN23500CE`) | Verified (OFFICIAL-MOCK) |
| 4 | `name` | string | company/instrument name; EMPTY for derivatives in the mock (`BANKNIFTY18JAN23500CE` has blank name) | Verified (OFFICIAL-MOCK) |
| 5 | `last_price` | float | last price AT DUMP-GENERATION TIME — "the dump is generated once everyday and hence last_price is not real time" (SEARCH snippet of the docs page); the mock shows `0.0` for most cash rows | SEARCH (docs snippet) + Verified (OFFICIAL-MOCK, the 0.0 values) |
| 6 | `expiry` | date or empty | expiry date `YYYY-MM-DD` (derivatives); empty for cash | Verified (OFFICIAL-MOCK) |
| 7 | `strike` | float | option strike; `0.0` for non-options | Verified (OFFICIAL-MOCK) |
| 8 | `tick_size` | float | minimum price increment in rupees (`0.05` NSE equity, `0.01` for ETFs/debt rows in the mock) | Verified (OFFICIAL-MOCK) |
| 9 | `lot_size` | int | trading lot (1 for cash equity; `40` for the 2018-era BANKNIFTY options in the mock — historical value, lot sizes change) | Verified (OFFICIAL-MOCK) |
| 10 | `instrument_type` | string | `EQ`, `CE`, `PE` observed in the mock; `FUT` expected for futures (Assumed — no futures row in the 99-row mock sample) | Verified (OFFICIAL-MOCK) / Assumed (FUT) |
| 11 | `segment` | string | observed values in the mock: `NSE`, `BSE`, `NFO-OPT`; `NFO-FUT`, `BFO-OPT/-FUT`, `CDS-…`, `MCX-…`, `INDICES` follow the same `EXCHANGE[-KIND]` pattern (Assumed — not in the mock sample) | Verified (OFFICIAL-MOCK, 3 values) / Assumed (rest) |
| 12 | `exchange` | string | `NSE`, `BSE`, `NFO` observed in the mock; full enum in §4 | Verified (OFFICIAL-MOCK) |

**Column-count wart (honest note):** the mock/gokiteconnect CSV has NO ISIN column — contrast both Dhan (detailed CSV carries `ISIN`) and Groww (`isin` column). Cross-vendor instrument joins by ISIN are therefore NOT possible from the Kite master alone; join by `(exchange, tradingsymbol)` or via a Dhan/Groww-side ISIN→symbol hop instead. (See §10.)

## 4. Exchanges covered

**Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py:80-86) — the SDK's exchange constants (valid values for `/instruments/{exchange}`):**

| Constant | Value | Meaning |
|---|---|---|
| `EXCHANGE_NSE` | `NSE` | NSE cash |
| `EXCHANGE_BSE` | `BSE` | BSE cash |
| `EXCHANGE_NFO` | `NFO` | NSE F&O |
| `EXCHANGE_BFO` | `BFO` | BSE F&O |
| `EXCHANGE_CDS` | `CDS` | NSE currency derivatives |
| `EXCHANGE_BCD` | `BCD` | BSE currency derivatives |
| `EXCHANGE_MCX` | `MCX` | commodity |

**Indices are NOT an "exchange"** — index rows live inside the NSE/BSE dumps with segment `INDICES` (Assumed for the CSV segment string — the WebSocket segment id 9 named `indices` is Verified, see §5; no index row appears in the 99-row mock sample). **Unknown:** whether `GET /instruments/NSE` includes the NSE index rows or they only appear in the full `/instruments` dump — probe on first live fetch.

**Note (cross-SDK wart):** pykiteconnect's WebSocket `EXCHANGE_MAP` additionally lists `mcxsx` (id 8) — MCX-SX, a defunct exchange — and gokiteconnect names id 3 `NseCD` / id 6 `BseCD`. Neither SDK defines an `MCXSX` REST exchange constant. Treat ids 3/6/8 as reserved segment slots, not fetchable exchanges.

## 5. `instrument_token` vs `exchange_token` — the ×256+segment relationship

**Verified (CLIENT-LIB-SOURCE, both SDKs):**
- pykiteconnect `ticker.py:726`: `segment = instrument_token & 0xff  # Retrive segment constant from instrument_token`, against `EXCHANGE_MAP = {"nse":1, "nfo":2, "cds":3, "bse":4, "bfo":5, "bcd":6, "mcx":7, "mcxsx":8, "indices":9}` (ticker.py:359-372).
- gokiteconnect `ticker/ticker.go:657`: `seg = tk & 0xFF`, against segment constants `NseCM=1, NseFO=2, NseCD=3, BseCM=4, BseFO=5, BseCD=6, McxFO=7, McxSX=8, Indices=9` (ticker.go:84-93). `isIndex = seg == Indices` drives index-tick parsing.

**Verified (ARITH over OFFICIAL-MOCK instruments_all.csv, all 99 data rows, 2026-07-13):** every row satisfies `instrument_token == exchange_token*256 + segment_id` with the segment id matching the row's exchange per the maps above — 55/55 NSE rows end in 1, 25/25 NFO rows end in 2, 19/19 BSE rows end in 4, zero mismatches. Examples: `3813889 = 14898×256 + 1` (CENTRALBK-BE, NSE), `12073986 = 47164×256 + 2` (BANKNIFTY18JAN23500CE, NFO), `136483588 = 533139×256 + 4` (FFTF16BGR, BSE).

So: **`instrument_token = (exchange_token << 8) | segment_id`** — the low byte of `instrument_token` is the WebSocket segment id, and `instrument_token >> 8` recovers `exchange_token`. **Unknown:** whether the official docs page states this relationship narratively (docs HTML unreachable); both official SDKs hard-code it and the official mock data satisfies it exhaustively, so it is contract-grade regardless.

**Why the low byte matters downstream (cross-ref `03-websocket` pack file):** the tick parsers divide raw price ints by a SEGMENT-dependent divisor — CDS (3) → 10,000,000; BCD (6) → 10,000; everything else → 100 (ticker.py:729-735) — and `segment == indices (9)` selects the index-packet layout and `tradable = False`.

## 6. Daily refresh + the fetch-once-daily advisory

- **SEARCH (kite.trade docs snippet 2026-07-13):** "the dump is generated once everyday and hence last_price is not real time".
- **SEARCH (kite.trade forum snippets 2026-07-13, e.g. kite.trade/forum/discussion/3059 + /5658):** best practice is to "request it once a day (ideally at around 08:30 AM) and store in a database at your end"; "Exchanges republish these files everyday and there may be expiries (weekly options, monthly futures and options), delistings, symbol/series changes etc." — snippet-level; whether the "~08:30 AM" phrasing is on the docs page itself or only in Zerodha-staff forum answers could not be disambiguated from the blocked sandbox.
- **The task brief's "~8 AM" figure:** NOT sourced as an exact docs number. The only time figure surfaced by search is **~08:30 AM (IST assumed)**. Exact daily publish time = **Unknown** (OPEN QUESTIONS).
- **Assumed: IST** — no timezone was attached in any snippet; Indian-exchange convention.

Operational consequence (same rule class as Dhan): tokens for derivatives churn daily (new contracts list, old expire) and even equity tokens can change on relist/series change — **never hardcode tokens; resolve fresh from the daily CSV**.

## 7. Size / row-count expectations

- **Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py:607-614):** the `instruments()` docstring: "Note that the results could be large, several hundred KBs in size, with tens of thousands of entries in the list." (That is the compressed-transfer/entry-count framing in the official SDK; the docstring is old.)
- **Assumed (order of magnitude, 2026):** the present-day full dump is commonly reported in the ~80k–110k-row range and several MB uncompressed — NOT verified from any fetched source here; treat as unmeasured until the first live fetch (OPEN QUESTIONS).
- **Verified (OFFICIAL-MOCK):** the mock repo's `instruments_all.csv` / `instruments_nse.csv` are 99-row SAMPLES (2017/2018-era data, `last_price` mostly 0.0) — they pin the schema, not the size.
- Per the groww-ref/dhan-ref house rule: the master is a runtime download, **never vendored into git** (Groww's is ~23.5 MB / ~164k rows for scale comparison — `docs/groww-ref/README.md`).

## 8. Index tokens (NIFTY 50 / NIFTY BANK / SENSEX)

**Not officially documented on a docs page we could fetch.** Search-tier only:

- **SEARCH (kite.trade forum snippets 2026-07-13 — forum/discussion/12826, /2825, /11137):** `NSE:NIFTY 50` → `instrument_token 256265`; `NSE:NIFTY BANK` → `260105`; `BSE:SENSEX` → `265`; NIFTY 50 exchange_token `1001`, NIFTY BANK `1016`.
- **ARITH consistency check (2026-07-13):** `256265 = 1001×256 + 9`, `260105 = 1016×256 + 9`, `265 = 1×256 + 9` — all three end in segment id 9 = `indices`, exactly matching the §5 relationship and the forum-quoted exchange tokens. Internally consistent, but still forum-sourced.
- **Binding advisory:** do NOT hardcode these. Resolve at runtime from the daily CSV by `(exchange, tradingsymbol)` — `NSE / NIFTY 50`, `NSE / NIFTY BANK`, `BSE / SENSEX` (tradingsymbol spellings themselves Assumed from forum snippets; confirm against the first live CSV). This mirrors the tickvault Dhan rule ("never hardcode SecurityIds") — even though index tokens have been historically stable, the pack takes zero token-stability risk.

## 9. Operational rules for a consumer (derived, honest)

1. Fetch `GET /instruments` once per trading day pre-open (~08:30 AM IST per §6), store, and serve all lookups from the stored copy. (SEARCH advisory + Verified CLIENT-LIB-SOURCE SDK docstring.)
2. Parse with the §2 coercion contract; tolerate empty `expiry`/`name`, `0.0` strike/last_price.
3. Key instruments by `(exchange, tradingsymbol)` for REST quote calls (`quote("NSE:INFY")` format — connect.py:621-635) and by `instrument_token` for WebSocket + historical. Both keys come from the same CSV row.
4. Derivative tokens are day-scoped state; diff daily and expire the disappeared (the Dhan `instrument_lifecycle` never-delete pattern applies if this ever feeds tickvault — reference only).
5. `last_price` in the CSV is stale by design (§6) — never use it as a live price.

## 10. Compare: Dhan / Groww

| Aspect | Kite (this file) | Dhan (`docs/dhan-ref/09-instrument-master.md`) | Groww (`docs/groww-ref/09-instruments-csv.md`, README) |
|---|---|---|---|
| Delivery | authenticated-SDK API endpoint `GET /instruments` on `api.kite.trade`, gzipped CSV | public static CDN CSVs (`images.dhan.co/api-data/api-scrip-master[-detailed].csv`), no auth | public static CDN CSV (`growwapi-assets.groww.in/instruments/instrument.csv`), no auth |
| Per-exchange subset | yes — `GET /instruments/{exchange}` | yes — REST `/v2/instrument/{exchangeSegment}` (banned as daily-universe fallback in tickvault) | no (single file) |
| Primary key | `instrument_token` (uint32; low byte = segment; recoverable from `exchange_token`) | `SECURITY_ID` (NOT unique alone — composite `(security_id, exchange_segment)` per I-P1-11) | `exchange_token` keyed with `(exchange, segment)` |
| ISIN column | **ABSENT** (12 columns, no ISIN) | present (detailed CSV) | present |
| Underlying linkage for derivatives | none in the CSV (derive from tradingsymbol; name is blank on derivative rows in the mock) | explicit `UNDERLYING_SECURITY_ID` / `UNDERLYING_SYMBOL` columns | explicit `underlying_symbol` / `underlying_exchange_token` columns |
| Freshness fields | `last_price` frozen at daily dump generation | no price column | no price column |
| Refresh cadence | daily, fetch once ~08:30 AM advisory | daily, tickvault fetched at 08:45 IST (historical; Dhan download chain retired 2026-07-13) | daily |

The ISIN absence is the load-bearing difference: any Kite↔Dhan/Groww instrument mapping must go through `(exchange, tradingsymbol)` normalization or an ISIN bridge built from the OTHER vendor's master — the ISIN-primary join used for NTM constituency (`daily-universe-scope-expansion-2026-05-27.md` §31.1) cannot be replicated Kite-side from this CSV alone.

**India-F&O coverage note (NIFTY / BANKNIFTY / SENSEX incl. BSE/BFO):** consolidated per-surface coverage table (master / quote / WS / historical, with per-surface Verified-implied vs Unknown labels) lives in `14-option-chain-composition.md` §5 — read it before any NFO/BFO capacity or coverage decision.

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Does `GET /instruments` require the `Authorization: token api_key:access_token` header, or is it publicly fetchable?** Paste: https://kite.trade/docs/connect/v3/market-quotes/#retrieving-full-market-instruments — matters for pre-auth boot ordering (Dhan/Groww masters are public CDN files; if Kite's needs a live session, the master fetch must sequence AFTER token mint).
- **Exact daily dump generation/publish time (and timezone)** — is "~08:30 AM" on the docs page or only in forum answers, and is it guaranteed? Paste: same URL + https://kite.trade/forum/discussion/3059/do-i-need-to-download-instrument-list-daily-do-you-change-instrument-token-daily — matters for the pre-open fetch scheduling (a fetch before publish silently serves yesterday's dump).
- **Full enumeration of `segment` column values** (`INDICES`, `NFO-FUT`, `BFO-OPT/-FUT`, `CDS-…`, `MCX-…` etc.) and `instrument_type` values beyond `EQ/CE/PE` (`FUT`?) — only `NSE/BSE/NFO-OPT` and `EQ/CE/PE` are mock-verified. Paste: same docs URL (CSV columns table) — matters for fail-closed CSV validation (unknown-enum handling).
- **Are index rows present in the per-exchange dump (`/instruments/NSE`) or only the full dump?** Probe on first live fetch — matters for choosing which endpoint a spot-indices consumer needs.
- **Current full-dump row count + compressed/uncompressed size** — SDK docstring says "several hundred KBs / tens of thousands of entries" (dated); measure live. Matters for download budget + parse-time sizing.
- **Is the ×256+segment token relationship documented as a STABLE contract on the docs page, or SDK-implementation detail?** Paste: https://kite.trade/docs/connect/v3/websocket/#binary-market-data (packet-structure section) — matters for whether a consumer may rely on `instrument_token >> 8 == exchange_token` forever or must treat both columns as opaque.
- **Does the official docs page confirm index tokens (NIFTY 50 = 256265, NIFTY BANK = 260105, SENSEX = 265) anywhere?** Currently forum-sourced only. Paste: https://kite.trade/docs/connect/v3/market-quotes/ — matters only for cross-checking; runtime CSV resolution is mandatory regardless.

## CLAIMS (for README reconciled table)

- Instruments master endpoints are `GET /instruments` (all exchanges) and `GET /instruments/{exchange}` on `https://api.kite.trade` — Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py:148-149 + gokiteconnect connect.go:147-149, fetched 2026-07-13) — https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/connect.py
- The response is a gzipped CSV dump (not the JSON envelope) — SEARCH (docs snippet 2026-07-13); the SDK returns raw bytes when Content-Type contains "csv" — Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py:991–992, the csv content-type branch) — https://kite.trade/docs/connect/v3/market-quotes/
- CSV has exactly 12 columns: `instrument_token, exchange_token, tradingsymbol, name, last_price, expiry, strike, tick_size, lot_size, instrument_type, segment, exchange` — Verified (OFFICIAL-MOCK instruments_all.csv @ kiteconnect-mocks main c7a8123 + CLIENT-LIB-SOURCE gokiteconnect market.go:77-90) — https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/instruments_all.csv
- The Kite CSV carries NO ISIN column (cross-vendor joins need a symbol bridge, unlike Dhan/Groww) — Verified (OFFICIAL-MOCK header + gokiteconnect struct, absence) — same citations
- `instrument_token = exchange_token*256 + segment_id`; low byte = WebSocket segment (nse 1, nfo 2, cds 3, bse 4, bfo 5, bcd 6, mcx 7, mcxsx 8, indices 9) — Verified (CLIENT-LIB-SOURCE pykiteconnect ticker.py:359-372,726 + gokiteconnect ticker.go:84-93,657) + ARITH (99/99 mock rows, zero mismatches)
- Exchange enum for the per-exchange endpoint: NSE, BSE, NFO, BFO, CDS, BCD, MCX — Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py:80-86)
- The dump is generated once every day; `last_price` in it is not real time — SEARCH (docs snippet 2026-07-13) — https://kite.trade/docs/connect/v3/market-quotes/
- Advisory: fetch once daily (~08:30 AM) and store locally; exchanges republish daily with expiries/delistings/symbol changes, so tokens must be re-resolved every day — SEARCH (forum snippets 2026-07-13; exact-source attribution of the 08:30 figure Unknown) — https://kite.trade/forum/discussion/3059/do-i-need-to-download-instrument-list-daily-do-you-change-instrument-token-daily
- SDK parser contract: instrument_token→int, last_price/strike/tick_size→float, lot_size→int, expiry parsed only when exactly 10 chars — Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py:855-879)
- Full-dump scale: "several hundred KBs … tens of thousands of entries" per the SDK docstring (dated); present-day row count Unknown/unmeasured — Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py:607-614) / Unknown (current size)
- Index tokens NIFTY 50 = 256265 (exch_token 1001), NIFTY BANK = 260105 (1016), SENSEX = 265 — SEARCH (kite.trade forum, 2026-07-13) + ARITH (all three satisfy ×256+9/indices); NOT docs-page-verified; runtime CSV resolution mandatory, never hardcode
- Whether `/instruments` requires the Authorization header is Unknown (SDK always sends it when configured; forum suggests direct download) — Unknown — probe/paste per OPEN QUESTIONS
