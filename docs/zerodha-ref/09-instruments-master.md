# Kite Connect v3 — Instruments Master (full reference)

> **Source:** https://kite.trade/docs/connect/v3/market-quotes/#instruments (the CURRENT nav page, "Market quotes and instruments", footer © 2015–2025) — primary; https://kite.trade/docs/connect/v3/market-data-and-instruments/ (the LEGACY sibling "Market and instruments", footer © 2015–2020 — NOT in the live site nav but still serves HTTP 200 with the same instruments content, minus the newer storage-key Note); https://kite.trade/forum/discussion/3059/do-i-need-to-download-instrument-list-daily-do-you-change-instrument-token-daily (Zerodha-staff answers on daily refresh + token stability).
> **Fetched-Verified:** 2026-07-14 — **live-verbatim via GH-runner 2026-07-14 (UTC timestamps in 00-COVERAGE-MANIFEST.md)**: all three source pages above fetched HTTP 200 by the LIVE-DOC runner (per-URL sha256+timestamp in the crawl fetch-log). Prior pass 2026-07-13 was CLIENT-LIB-SOURCE (pykiteconnect@master `connect.py` + `ticker.py`; gokiteconnect@master `market.go` + `connect.go` + `ticker/ticker.go`), OFFICIAL-MOCK (`kiteconnect-mocks@main` `instruments_all.csv` + `instruments_nse.csv`), SEARCH, ARITH — those citations are retained below as cross-check evidence; the 2026-07-13 "docs HTML unreachable from sandbox" caveat is now OBSOLETE for the claims the live pages cover.
> **Evidence tiers:** see README legend — **Verified (LIVE-DOC runner 2026-07-14)** is the top tier for this pack (claim confirmed against the runner-fetched live page) / Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) / SEARCH (snippet-level, sub-Verified) / ARITH / Assumed / Unknown.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. Any TickVault consumption of Kite endpoints requires its own dated operator grant per the pluggable-feed rule files.

---

## 1. Endpoints

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/market-quotes/ + https://kite.trade/docs/connect/v3/market-data-and-instruments/):** both live pages carry the identical endpoint table:

| Type | Endpoint | Live description (verbatim) |
|---|---|---|
| GET | `/instruments` | "Retrieve the CSV dump of all tradable instruments" |
| GET | `/instruments/:exchange` | "Retrieve the CSV dump of instruments in the particular exchange" |

The live curl example (both pages):

```
curl "https://api.kite.trade/instruments"
    -H "X-Kite-Version: 3" \
    -H "Authorization: token api_key:access_token" \
```

**Cross-check — Verified (CLIENT-LIB-SOURCE pykiteconnect@master kiteconnect/connect.py, fetched 2026-07-13):** the SDK `_routes` dict defines exactly these two instrument-master routes (root `https://api.kite.trade`, `_default_root_uri` at connect.py:36): `market.instruments.all → GET /instruments`, `market.instruments → GET /instruments/{exchange}` (connect.py:148-149; `instruments(exchange=None)` at connect.py:607-619 dispatches between them). **Cross-check — Verified (CLIENT-LIB-SOURCE gokiteconnect@master connect.go:147-149):** `URIGetInstruments = "/instruments"`, `URIGetInstrumentsExchange = "/instruments/%s"` — identical paths, consumed by `GetInstruments()` / `GetInstrumentsByExchange(exchange)` in market.go:309-321.

**Related sibling routes on the same path prefix (for orientation only):** `market.historical = /instruments/historical/{instrument_token}/{interval}` (covered in `08-historical-candles.md`) and `market.trigger_range = /instruments/trigger_range/{transaction_type}` (connect.py:151-152; covered in `07-market-quotes.md` §10 — cross-SDK route divergence + the official `trigger_range.json` mock recorded there). The mutual-fund master is a SEPARATE endpoint `GET /mf/instruments` (connect.py:146) with a different CSV schema — do not conflate.

**Headers:** the LIVE docs curl example prescribes BOTH `X-Kite-Version: 3` AND `Authorization: token api_key:access_token` on `/instruments` (Verified LIVE-DOC runner 2026-07-14, both pages). Matches the SDK behaviour — Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py:938-946, `kite_header_version = "3"` at connect.py:41). **Unknown (narrowed):** whether the server actually ENFORCES the Authorization header on `/instruments` (the docs example includes it, so the documented contract is authenticated; whether an unauthenticated GET is also served remains unprobed — see OPEN QUESTIONS).

## 2. Response format — gzipped CSV, not JSON

**Verified (LIVE-DOC runner 2026-07-14, both docs pages, verbatim):** "Unlike the rest of the calls that return JSON, the instrument list API returns a gzipped CSV dump of instruments across all exchanges that can be imported into a database. The dump is generated once everyday and hence last_price is not real time." (Upgraded from the 2026-07-13 SEARCH snippet — the live page confirms the wording exactly.)

**Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py:991-992):** the SDK's response handler branches on `Content-Type` — `"csv" in r.headers["content-type"]` → return raw `r.content` bytes (no JSON envelope, no `data` unwrapping). Everything else JSON-shaped goes through the `{status, data, error_type, message}` envelope.

**Assumed — gzip is TRANSPORT-level (`Content-Encoding: gzip`), not a `.gz` file body:** the live docs say "gzipped CSV dump" without specifying transport-vs-file; neither pykiteconnect nor gokiteconnect contains any explicit gunzip step; pykiteconnect feeds `r.content` straight into `csv.DictReader` (connect.py:855-879) and gokiteconnect feeds `resp.Body` straight into `gocsv.UnmarshalBytes` (market.go:290-306). Both HTTP stacks transparently decompress `Content-Encoding: gzip`. If the body were an opaque `.gz` file, both official SDKs would be broken — so transport-level compression is the only consistent reading. Confirm on first live fetch.

**Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py:855-879) — client-side type coercion applied per row** (useful as a parser contract):

| Field | Coercion |
|---|---|
| `instrument_token` | `int` |
| `last_price`, `strike`, `tick_size` | `float` |
| `lot_size` | `int` |
| `expiry` | parsed to a date ONLY when the string is exactly 10 chars (`YYYY-MM-DD`); empty string otherwise left as-is |

Everything else stays a string.

**Docs-vs-SDK typing wart (Verified LIVE-DOC runner 2026-07-14):** the live "CSV response columns" table types `instrument_token` and `exchange_token` as `string` (and the current market-quotes page types `last_price`/`strike`/`tick_size` as `float64`, `lot_size` as `int64`; the legacy page says plain `float`/`int`), while the SAME market-quotes page's OHLC/LTP response-attribute tables type `instrument_token` as `uint32`. The SDKs coerce both tokens to int. Treat the CSV cells as decimal integer strings that fit uint32.

## 3. CSV columns — all 12, with meanings

**Verified (LIVE-DOC runner 2026-07-14, both docs pages):** header row, in order, exactly as printed in the live example block:

```
instrument_token, exchange_token, tradingsymbol, name, last_price, expiry, strike, tick_size, lot_size, instrument_type, segment, exchange
```

with live sample rows (verbatim from the docs page):

```
408065,1594,INFY,INFOSYS,0,,,0.05,1,EQ,NSE,NSE
5720322,22345,NIFTY15DECFUT,,78.0,2015-12-31,,0.05,75,FUT,NFO-FUT,NFO
5720578,22346,NIFTY159500CE,,23.0,2015-12-31,9500,0.05,75,CE,NFO-OPT,NFO
645639,SILVER15DECFUT,,7800.0,2015-12-31,,1,1,FUT,MCX,MCX
```

**Docs-sample wart (honest note, Verified LIVE-DOC runner 2026-07-14):** the MCX sample row above has only ELEVEN fields — the `exchange_token` value is missing from the docs page's own example (a docs typo, present verbatim on BOTH live pages). ARITH: `645639 = 2522×256 + 7` (segment 7 = mcx per §5), so the omitted exchange_token is 2522. Real dumps are 12-field (mock + gokiteconnect struct + the other three sample rows all agree).

**Cross-check — Verified (OFFICIAL-MOCK instruments_all.csv + instruments_nse.csv, kiteconnect-mocks@main c7a8123, fetched 2026-07-13; CLIENT-LIB-SOURCE gokiteconnect market.go:77-90):** identical 12 columns / `csv:` tags.

| # | Column | Type (post-coercion) | Meaning (live docs wording where quoted) | Tier |
|---|---|---|---|---|
| 1 | `instrument_token` | int (uint32 on the wire) | "Numerical identifier used for subscribing to live market quotes with the WebSocket API" — also the key for historical-data calls (`historical_data(instrument_token, …)`, connect.py:664-684) | Verified (LIVE-DOC runner 2026-07-14 + CLIENT-LIB-SOURCE + OFFICIAL-MOCK) |
| 2 | `exchange_token` | int | "The numerical identifier issued by the exchange representing the instrument"; `instrument_token = exchange_token*256 + segment_id` (see §5) | Verified (LIVE-DOC runner 2026-07-14 + OFFICIAL-MOCK + ARITH) |
| 3 | `tradingsymbol` | string | "Exchange tradingsymbol of the instrument", incl. series suffixes on NSE cash (`CENTRALBK-BE`, `CDSL-BL` in the mock) and derivative names (`NIFTY15DECFUT`, `BANKNIFTY18JAN23500CE`) | Verified (LIVE-DOC runner 2026-07-14 + OFFICIAL-MOCK) |
| 4 | `name` | string | "Name of the company (for equity instruments)"; EMPTY for derivatives (live sample rows + mock both show blank name on F&O rows) | Verified (LIVE-DOC runner 2026-07-14 + OFFICIAL-MOCK) |
| 5 | `last_price` | float | "Last traded market price" — AT DUMP-GENERATION TIME: "the dump is generated once everyday and hence last_price is not real time" (live docs, verbatim); the live INFY sample row and most mock cash rows show `0` | Verified (LIVE-DOC runner 2026-07-14 + OFFICIAL-MOCK) |
| 6 | `expiry` | date or empty | "Expiry date (for derivatives)" `YYYY-MM-DD`; empty for cash | Verified (LIVE-DOC runner 2026-07-14 + OFFICIAL-MOCK) |
| 7 | `strike` | float | "Strike (for options)"; `0.0`/empty for non-options | Verified (LIVE-DOC runner 2026-07-14 + OFFICIAL-MOCK) |
| 8 | `tick_size` | float | "Value of a single price tick" in rupees (`0.05` NSE equity; `1` on the MCX silver sample; `0.01` for ETFs/debt rows in the mock) | Verified (LIVE-DOC runner 2026-07-14 + OFFICIAL-MOCK) |
| 9 | `lot_size` | int | "Quantity of a single lot" (1 for cash equity; `75` on the 2015-era NIFTY F&O sample rows; lot sizes change over time) | Verified (LIVE-DOC runner 2026-07-14 + OFFICIAL-MOCK) |
| 10 | `instrument_type` | string | live docs enumerate exactly: **"EQ, FUT, CE, PE"** — `FUT` is now docs-confirmed (was Assumed on 2026-07-13; the 99-row mock had no futures row) | Verified (LIVE-DOC runner 2026-07-14) |
| 11 | `segment` | string | "Segment the instrument belongs to". Live-observed values: `NSE`, `NFO-FUT`, `NFO-OPT`, `MCX` (docs sample rows) + `BSE` (mock). `BFO-OPT/-FUT`, `CDS-…`, `INDICES` follow the same `EXCHANGE[-KIND]` pattern (Assumed — not in any fetched sample; docs give no enum) | Verified (LIVE-DOC runner 2026-07-14, 4 values + OFFICIAL-MOCK) / Assumed (rest) |
| 12 | `exchange` | string | "Exchange" — `NSE`, `NFO`, `MCX` in the live sample rows; `BSE` in the mock; full enum in §4 | Verified (LIVE-DOC runner 2026-07-14 + OFFICIAL-MOCK) |

**Column-count wart (honest note):** the CSV has NO ISIN column — contrast both Dhan (detailed CSV carries `ISIN`) and Groww (`isin` column). Cross-vendor instrument joins by ISIN are therefore NOT possible from the Kite master alone; join by `(exchange, tradingsymbol)` or via a Dhan/Groww-side ISIN→symbol hop instead. (See §10. The live docs page's 12-column table confirms the absence.)

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

(The live docs pages do not enumerate the `:exchange` values; the endpoint row just says "instruments in the particular exchange" — SDK constants remain the best source for the enum.)

**Indices are NOT an "exchange"** — index rows live inside the NSE/BSE dumps with segment `INDICES` (Assumed for the CSV segment string — the WebSocket segment id 9 named `indices` is Verified, see §5; no index row appears in the live docs samples or the 99-row mock sample). **Unknown:** whether `GET /instruments/NSE` includes the NSE index rows or they only appear in the full `/instruments` dump — probe on first live fetch.

**Note (cross-SDK wart):** pykiteconnect's WebSocket `EXCHANGE_MAP` additionally lists `mcxsx` (id 8) — MCX-SX, a defunct exchange — and gokiteconnect names id 3 `NseCD` / id 6 `BseCD`. Neither SDK defines an `MCXSX` REST exchange constant. Treat ids 3/6/8 as reserved segment slots, not fetchable exchanges.

## 5. `instrument_token` vs `exchange_token` — the ×256+segment relationship

**Verified (CLIENT-LIB-SOURCE, both SDKs):**
- pykiteconnect `ticker.py:726`: `segment = instrument_token & 0xff  # Retrive segment constant from instrument_token`, against `EXCHANGE_MAP = {"nse":1, "nfo":2, "cds":3, "bse":4, "bfo":5, "bcd":6, "mcx":7, "mcxsx":8, "indices":9}` (ticker.py:359-372).
- gokiteconnect `ticker/ticker.go:657`: `seg = tk & 0xFF`, against segment constants `NseCM=1, NseFO=2, NseCD=3, BseCM=4, BseFO=5, BseCD=6, McxFO=7, McxSX=8, Indices=9` (ticker.go:84-93). `isIndex = seg == Indices` drives index-tick parsing.

**Verified (ARITH over OFFICIAL-MOCK instruments_all.csv, all 99 data rows, 2026-07-13):** every row satisfies `instrument_token == exchange_token*256 + segment_id` with the segment id matching the row's exchange per the maps above — 55/55 NSE rows end in 1, 25/25 NFO rows end in 2, 19/19 BSE rows end in 4, zero mismatches. Examples: `3813889 = 14898×256 + 1` (CENTRALBK-BE, NSE), `12073986 = 47164×256 + 2` (BANKNIFTY18JAN23500CE, NFO), `136483588 = 533139×256 + 4` (FFTF16BGR, BSE).

**ARITH over the LIVE docs sample rows (2026-07-14):** `408065 = 1594×256 + 1` (INFY, NSE), `5720322 = 22345×256 + 2` (NIFTY15DECFUT, NFO), `5720578 = 22346×256 + 2` (NIFTY159500CE, NFO), and the typo'd MCX row's `645639 = 2522×256 + 7` — all consistent.

So: **`instrument_token = (exchange_token << 8) | segment_id`** — the low byte of `instrument_token` is the WebSocket segment id, and `instrument_token >> 8` recovers `exchange_token`. **Resolved (live fetch 2026-07-14): the docs pages do NOT state this relationship narratively anywhere** — it remains SDK-implementation + ARITH contract-grade, not a documented stable contract. Worse for stability assumptions, the current market-quotes page explicitly warns tokens may be REUSED (see §6) — so derive the relationship per-day from the fresh CSV, never cache token math across days for derivatives.

**Why the low byte matters downstream (cross-ref `03-websocket` pack file):** the tick parsers divide raw price ints by a SEGMENT-dependent divisor — CDS (3) → 10,000,000; BCD (6) → 10,000; everything else → 100 (ticker.py:729-735) — and `segment == indices (9)` selects the index-packet layout and `tradable = False`.

## 6. Daily refresh, the 08:30 advisory, and token (in)stability

- **Verified (LIVE-DOC runner 2026-07-14, both docs pages, verbatim Warning admonition):** "The instrument list API returns large amounts of data. It's best to request it once a day (ideally at around 08:30 AM) and store in a database at your end." — **RESOLVED: the ~08:30 AM advisory IS on the docs pages themselves**, not just in forum answers (the 2026-07-13 attribution ambiguity is closed).
- **Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/market-quotes/ ONLY — the newer page; verbatim Note admonition):** "For storage, it is recommended to use a combination of exchange and tradingsymbol as the unique key, not the numeric instrument token. **Exchanges may reuse instrument tokens for different derivative instruments after each expiry.**" (This Note is absent from the legacy market-data-and-instruments page — it is a later docs addition.)
- **Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/forum/discussion/3059/… — Zerodha staff answers, Jan 2018, verbatim):**
  - Kailash: "We recommend you download it once every morning as exchanges republishes these files everyday and there may be expiries (weekly options, monthly futures and options), delistings, symbol/series changes etc."
  - sujith (on whether an EQUITY token can change): "We don't have control over that, it may change not change for a very long time but it is possible that it may change one day. I would suggest making a combination of exchange and tradingsymbol as a unique key. For example, NSE:INFY"
- **Assumed: IST** — no timezone is attached to "08:30 AM" on the live page; Indian-exchange convention.
- **Exact daily publish/generation time remains Unknown** — "ideally at around 08:30 AM" is a client-fetch advisory, not a publish-time guarantee (OPEN QUESTIONS).

Operational consequence (same rule class as Dhan): tokens for derivatives churn daily (new contracts list, old expire, **and per the live docs Note the freed tokens may be REUSED for different derivative instruments**) and even equity tokens can change on relist/series change per staff — **never hardcode tokens; resolve fresh from the daily CSV; key stored instruments by `(exchange, tradingsymbol)` per the docs' own recommendation**.

## 7. Size / row-count expectations

- **Verified (LIVE-DOC runner 2026-07-14):** the docs Warning says only "large amounts of data" — no row count or byte size is published.
- **Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py:607-614):** the `instruments()` docstring: "Note that the results could be large, several hundred KBs in size, with tens of thousands of entries in the list." (That is the compressed-transfer/entry-count framing in the official SDK; the docstring is old.)
- **Assumed (order of magnitude, 2026):** the present-day full dump is commonly reported in the ~80k–110k-row range and several MB uncompressed — NOT verified from any fetched source here; treat as unmeasured until the first live fetch (OPEN QUESTIONS).
- **Verified (OFFICIAL-MOCK):** the mock repo's `instruments_all.csv` / `instruments_nse.csv` are 99-row SAMPLES (2017/2018-era data, `last_price` mostly 0.0) — they pin the schema, not the size.
- Per the groww-ref/dhan-ref house rule: the master is a runtime download, **never vendored into git** (Groww's is ~23.5 MB / ~164k rows for scale comparison — `docs/groww-ref/README.md`).

## 8. Index tokens (NIFTY 50 / NIFTY BANK / SENSEX)

- **Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/market-data-and-instruments/ — the OHLC/LTP quote examples on the docs page itself):** `"NSE:NIFTY 50" → "instrument_token": 256265` and `"BSE:SENSEX" → "instrument_token": 265`. **UPGRADED from forum-sourced to docs-example-sourced** — and the quote-key spellings `NSE:NIFTY 50` / `BSE:SENSEX` are now docs-verbatim too. (Example values on a docs page, not a normative token table — the runtime-resolution advisory below still binds.)
- **SEARCH (kite.trade forum snippets 2026-07-13 — forum/discussion/12826, /2825, /11137):** `NSE:NIFTY BANK` → `260105`; NIFTY 50 exchange_token `1001`, NIFTY BANK `1016` — NIFTY BANK and the exchange_token values remain forum-sourced only (not on the fetched docs pages).
- **ARITH consistency check (2026-07-13):** `256265 = 1001×256 + 9`, `260105 = 1016×256 + 9`, `265 = 1×256 + 9` — all three end in segment id 9 = `indices`, exactly matching the §5 relationship and the forum-quoted exchange tokens.
- **Binding advisory:** do NOT hardcode these. Resolve at runtime from the daily CSV by `(exchange, tradingsymbol)` — `NSE / NIFTY 50`, `NSE / NIFTY BANK`, `BSE / SENSEX` (the first and third spellings docs-example-verified 2026-07-14; `NIFTY BANK` still Assumed from forum snippets). This mirrors the tickvault Dhan rule ("never hardcode SecurityIds") — reinforced by the live docs' own token-reuse warning (§6), even though index tokens have been historically stable.

## 9. Operational rules for a consumer (derived, honest)

1. Fetch `GET /instruments` once per trading day pre-open (~08:30 AM, IST assumed — the docs-page Warning, Verified LIVE-DOC runner 2026-07-14), store, and serve all lookups from the stored copy.
2. Parse with the §2 coercion contract; tolerate empty `expiry`/`name`, `0.0`/empty strike + last_price — and tolerate short rows (the docs' own MCX sample is an 11-field row; real dumps should be 12-field, but validate per-row instead of assuming).
3. Key stored instruments by `(exchange, tradingsymbol)` — this is now the docs' OWN recommendation ("use a combination of exchange and tradingsymbol as the unique key, not the numeric instrument token", Verified LIVE-DOC runner 2026-07-14) — and use `instrument_token` only as the day-scoped subscription/historical key. Both keys come from the same CSV row. REST quote calls use the `exchange:tradingsymbol` format (`quote("NSE:INFY")` — connect.py:621-635).
4. Derivative tokens are day-scoped state; diff daily and expire the disappeared — mandatory here because the docs say freed tokens **may be reused for different derivative instruments after each expiry** (the Dhan `instrument_lifecycle` never-delete pattern applies if this ever feeds tickvault — reference only; a reused token MUST NOT inherit the old contract's history).
5. `last_price` in the CSV is stale by design (§2/§6, docs-verbatim) — never use it as a live price.

## 10. Compare: Dhan / Groww

| Aspect | Kite (this file) | Dhan (`docs/dhan-ref/09-instrument-master.md`) | Groww (`docs/groww-ref/09-instruments-csv.md`, README) |
|---|---|---|---|
| Delivery | authenticated-SDK API endpoint `GET /instruments` on `api.kite.trade`, gzipped CSV (docs curl passes `Authorization`) | public static CDN CSVs (`images.dhan.co/api-data/api-scrip-master[-detailed].csv`), no auth | public static CDN CSV (`growwapi-assets.groww.in/instruments/instrument.csv`), no auth |
| Per-exchange subset | yes — `GET /instruments/{exchange}` | yes — REST `/v2/instrument/{exchangeSegment}` (banned as daily-universe fallback in tickvault) | no (single file) |
| Primary key | docs recommend `(exchange, tradingsymbol)` for storage; `instrument_token` (uint32; low byte = segment) is day-scoped and MAY BE REUSED across derivatives after expiry (LIVE-DOC) | `SECURITY_ID` (NOT unique alone — composite `(security_id, exchange_segment)` per I-P1-11) | `exchange_token` keyed with `(exchange, segment)` |
| ISIN column | **ABSENT** (12 columns, no ISIN) | present (detailed CSV) | present |
| Underlying linkage for derivatives | none in the CSV (derive from tradingsymbol; name is blank on derivative rows in the live samples + mock) | explicit `UNDERLYING_SECURITY_ID` / `UNDERLYING_SYMBOL` columns | explicit `underlying_symbol` / `underlying_exchange_token` columns |
| Freshness fields | `last_price` frozen at daily dump generation (docs-verbatim) | no price column | no price column |
| Refresh cadence | daily ("generated once everyday"); fetch once ~08:30 AM per the docs Warning (LIVE-DOC) | daily, tickvault fetched at 08:45 IST (historical; Dhan download chain retired 2026-07-13) | daily |

The ISIN absence is the load-bearing difference: any Kite↔Dhan/Groww instrument mapping must go through `(exchange, tradingsymbol)` normalization or an ISIN bridge built from the OTHER vendor's master — the ISIN-primary join used for NTM constituency (`daily-universe-scope-expansion-2026-05-27.md` §31.1) cannot be replicated Kite-side from this CSV alone.

**India-F&O coverage note (NIFTY / BANKNIFTY / SENSEX incl. BSE/BFO):** consolidated per-surface coverage table (master / quote / WS / historical, with per-surface Verified-implied vs Unknown labels) lives in `14-option-chain-composition.md` §5 — read it before any NFO/BFO capacity or coverage decision.

## 11. The two docs pages (live-site orientation, 2026-07-14)

**Verified (LIVE-DOC runner 2026-07-14):** the live site nav lists "Market quotes and instruments" at slug `market-quotes/` (footer © 2015–2025; anchors `#instruments`, `#retrieving-the-full-instrument-list`, `#csv-response-columns`). The old slug `market-data-and-instruments/` ("Market and instruments", footer © 2015–2020; anchor `#retrieving-full-instrument-list`) is NOT in the nav but still serves HTTP 200. Content differences for the instruments section: the newer page adds the storage-key/token-reuse Note (§6) and types columns `float64`/`int64`; the legacy page's quote-endpoint table also carries a copy-paste wart (its `/quote`, `/quote/ohlc`, `/quote/ltp` rows repeat the instruments-CSV descriptions). Cite the `market-quotes/` page going forward; the legacy URL may vanish without notice.

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Does `GET /instruments` require the `Authorization: token api_key:access_token` header, or is it publicly fetchable?** NARROWED by live fetch 2026-07-14: the docs curl example on both live pages prescribes the header, so the DOCUMENTED contract is authenticated; whether the server also serves an unauthenticated GET remains unprobed (needs a live keyless request, not a page read). Matters for pre-auth boot ordering (Dhan/Groww masters are public CDN files; if Kite's needs a live session, the master fetch must sequence AFTER token mint).
- ~~Exact-source attribution of the "~08:30 AM" advisory~~ — **RESOLVED by live fetch 2026-07-14:** it is the docs-page Warning admonition verbatim on BOTH live pages ("It's best to request it once a day (ideally at around 08:30 AM)…"), and forum 3059 merely quotes it. (Close the matching U-row.) **Still open (residual):** the actual server-side dump GENERATION/publish time + timezone is not published anywhere fetched — a fetch before publish silently serves yesterday's dump; probe live by diffing an early-morning fetch.
- **Full enumeration of `segment` column values** — live docs give no enum; live-observed set grew to `NSE`, `BSE`, `NFO-FUT`, `NFO-OPT`, `MCX` (docs samples + mock); `INDICES`, `BFO-…`, `CDS-…` remain Assumed. `instrument_type` is now CLOSED: the live column table enumerates exactly "EQ, FUT, CE, PE" (RESOLVED by live fetch 2026-07-14 — close the instrument_type half of the matching U-row; keep the segment half open). Matters for fail-closed CSV validation (unknown-enum handling).
- **Are index rows present in the per-exchange dump (`/instruments/NSE`) or only the full dump?** Probe on first live fetch — matters for choosing which endpoint a spot-indices consumer needs.
- **Current full-dump row count + compressed/uncompressed size** — the live docs say only "large amounts of data"; SDK docstring says "several hundred KBs / tens of thousands of entries" (dated); measure live. Matters for download budget + parse-time sizing.
- ~~Is the ×256+segment token relationship documented as a STABLE contract on the docs page?~~ — **RESOLVED by live fetch 2026-07-14: NO.** Neither live docs page states the relationship narratively; it remains SDK-implementation + ARITH-verified only, AND the market-quotes page explicitly warns instrument tokens "may be reused for different derivative instruments after each expiry" — so treat both token columns as day-scoped opaque values; the ×256 math is safe only within a single day's dump. (Close the matching U-row with this answer.)
- ~~Does the official docs page confirm index tokens anywhere?~~ — **RESOLVED by live fetch 2026-07-14 (partial):** NIFTY 50 = 256265 and SENSEX = 265 appear verbatim in the docs pages' own OHLC/LTP examples (with quote-key spellings `NSE:NIFTY 50` / `BSE:SENSEX`); NIFTY BANK = 260105 does NOT appear on any fetched docs page and stays forum-sourced. Runtime CSV resolution stays mandatory regardless (the docs values are examples, not a normative table).

## CLAIMS (for README reconciled table)

- Instruments master endpoints are `GET /instruments` (all exchanges) and `GET /instruments/:exchange` on `https://api.kite.trade` — Verified (LIVE-DOC runner 2026-07-14; fetch-log) — https://kite.trade/docs/connect/v3/market-quotes/ + https://kite.trade/docs/connect/v3/market-data-and-instruments/ ; cross-check Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py:148-149 + gokiteconnect connect.go:147-149, fetched 2026-07-13)
- The response is a gzipped CSV dump, not the JSON envelope; "the dump is generated once everyday and hence last_price is not real time" — Verified (LIVE-DOC runner 2026-07-14, verbatim on both docs pages; UPGRADED from SEARCH) — https://kite.trade/docs/connect/v3/market-quotes/ ; SDK returns raw bytes when Content-Type contains "csv" — Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py:991-992)
- CSV has exactly 12 columns: `instrument_token, exchange_token, tradingsymbol, name, last_price, expiry, strike, tick_size, lot_size, instrument_type, segment, exchange` — Verified (LIVE-DOC runner 2026-07-14, header line verbatim on both docs pages) + Verified (OFFICIAL-MOCK instruments_all.csv @ kiteconnect-mocks main c7a8123 + CLIENT-LIB-SOURCE gokiteconnect market.go:77-90) — https://kite.trade/docs/connect/v3/market-quotes/
- `instrument_type` values are exactly `EQ, FUT, CE, PE` — Verified (LIVE-DOC runner 2026-07-14, the docs column table; `FUT` UPGRADED from Assumed) — same URL
- The docs pages' own MCX sample CSV row is an 11-field typo (exchange_token missing; ARITH recovers 2522) — Verified (LIVE-DOC runner 2026-07-14, verbatim on both pages) + ARITH
- The Kite CSV carries NO ISIN column (cross-vendor joins need a symbol bridge, unlike Dhan/Groww) — Verified (LIVE-DOC runner 2026-07-14, 12-column table, absence + OFFICIAL-MOCK header + gokiteconnect struct, absence)
- `instrument_token = exchange_token*256 + segment_id`; low byte = WebSocket segment (nse 1, nfo 2, cds 3, bse 4, bfo 5, bcd 6, mcx 7, mcxsx 8, indices 9) — Verified (CLIENT-LIB-SOURCE pykiteconnect ticker.py:359-372,726 + gokiteconnect ticker.go:84-93,657) + ARITH (99/99 mock rows + all 4 live docs sample rows, zero mismatches); NOT narratively documented on the live docs pages (checked 2026-07-14) — SDK/ARITH contract only
- Exchanges may REUSE instrument tokens for different derivative instruments after each expiry; docs recommend `(exchange, tradingsymbol)` as the storage unique key, not the numeric token — Verified (LIVE-DOC runner 2026-07-14, the Note admonition, market-quotes page only — absent from the legacy page) — https://kite.trade/docs/connect/v3/market-quotes/
- Exchange enum for the per-exchange endpoint: NSE, BSE, NFO, BFO, CDS, BCD, MCX — Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py:80-86); not enumerated on the live docs pages
- Fetch-once-daily advisory "ideally at around 08:30 AM" is the docs-page Warning admonition itself — Verified (LIVE-DOC runner 2026-07-14, verbatim on both docs pages; the 2026-07-13 forum-vs-docs attribution ambiguity is CLOSED) — https://kite.trade/docs/connect/v3/market-quotes/
- Zerodha staff (forum 3059, Jan 2018): exchanges republish the files every day (expiries, delistings, symbol/series changes); an equity token "may change one day" — use exchange+tradingsymbol (e.g. NSE:INFY) as the unique key — Verified (LIVE-DOC runner 2026-07-14, forum thread fetched; UPGRADED from SEARCH) — https://kite.trade/forum/discussion/3059/do-i-need-to-download-instrument-list-daily-do-you-change-instrument-token-daily
- SDK parser contract: instrument_token→int, last_price/strike/tick_size→float, lot_size→int, expiry parsed only when exactly 10 chars — Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py:855-879); note the docs column table types the tokens as `string` (LIVE-DOC wart)
- Full-dump scale: docs say only "large amounts of data" (LIVE-DOC 2026-07-14); SDK docstring says "several hundred KBs … tens of thousands of entries" (dated) — Verified (CLIENT-LIB-SOURCE pykiteconnect connect.py:607-614) / Unknown (current size)
- Index tokens NIFTY 50 = 256265 and SENSEX = 265 (quote keys `NSE:NIFTY 50` / `BSE:SENSEX`) appear verbatim in the docs pages' own OHLC/LTP examples — Verified (LIVE-DOC runner 2026-07-14, example values, not a normative table; UPGRADED from SEARCH) — https://kite.trade/docs/connect/v3/market-data-and-instruments/ ; NIFTY BANK = 260105 (exch_token 1016) + NIFTY 50 exch_token 1001 remain SEARCH (forum, 2026-07-13) + ARITH; runtime CSV resolution mandatory, never hardcode
- The docs curl for `/instruments` prescribes `X-Kite-Version: 3` + `Authorization: token api_key:access_token` — Verified (LIVE-DOC runner 2026-07-14); whether the server ENFORCES Authorization on this endpoint is still Unknown (needs a live keyless probe, not a page read)
- The live nav page is `market-quotes/` ("Market quotes and instruments", © 2015–2025, carries the token-reuse Note); `market-data-and-instruments/` is a legacy out-of-nav duplicate (© 2015–2020, no Note) that still serves 200 — Verified (LIVE-DOC runner 2026-07-14; fetch-log + nav discovery)
