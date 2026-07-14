# Zerodha Kite Connect v3 — Option-Chain COMPOSITION (there is NO chain endpoint)

> **Source:** `https://kite.trade/docs/connect/v3/market-quotes/` · `https://kite.trade/docs/connect/v3/market-data-and-instruments/` (live official pages — NOT directly fetchable from this sandbox); kite.trade forum threads 8928 / 11646 / 14832 / 14843 (option-chain / greeks availability)
> **Fetched-Verified:** 2026-07-13 via CLIENT-LIB-SOURCE (pykiteconnect@master `connect.py`; gokiteconnect@master `market.go`; pykiteconnect@master `ticker.py`) + OFFICIAL-MOCK (kiteconnect-mocks@main `instruments_all.csv`, `instruments_nse.csv`, `quote.json`) + SEARCH (live docs + forum extraction; kite.trade, web.archive.org, r.jina.ai all proxy-blocked — no ARCHIVE-DOC/MIRROR-LIVE tier attainable, and the docs TOC could NOT be captured — see `zerodha-probe`).
> **Evidence tiers:** see README legend.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. Any chain consumer requires a dated operator grant per `.claude/rules/project/no-rest-except-live-feed-2026-06-27.md`.
> **Related:** `07-market-quotes.md` (the `/quote` legs of the composition), `99-UNKNOWNS.md`.

---

## 1. The headline fact: Kite Connect has NO option-chain endpoint

- **Verified-absence (CLIENT-LIB-SOURCE, SDK level):** the COMPLETE `_routes` dict in pykiteconnect@master `connect.py` (lines 111–171, fetched 2026-07-13, https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/connect.py) contains **no option-chain, no expiry-list, and no greeks route** — the full inventory is session/user/orders/trades/portfolio/MF/instruments/margins/historical/trigger_range/quote×3/GTT/order-margins/contract-note. Cross-check: grep of every `.go` file in gokiteconnect@master finds zero option-chain/greeks API surface (only unrelated `option_premium` fields inside margin responses).
- **SEARCH (kite.trade forum, extracted 2026-07-13):** Zerodha staff answers confirm the absence at product level — a native option-chain API is "on the to-do list" (forum 14832 `option-chain-api-availability`), and *"Zerodha does not have APIs for the calculation of option greeks"* / greeks "have to be calculated at your side" (forum 14843, 8928, 11646).
- **HONESTY BOUNDARY — docs-TOC-level absence is NOT claimed as Verified:** the live docs TOC was unreachable from this sandbox (probe: all HTML routes CONNECT-403), so "the docs tree contains no option-chain page" could not be grep-confirmed the way groww-ref absence claims were. The SDK-route absence + staff-forum confirmations make the absence **Verified at SDK level / SEARCH at docs level**; the TOC check is an operator-paste item → OPEN QUESTIONS.
- **Compare: Dhan** ships a dedicated chain API — `POST /v2/optionchain` + `POST /v2/optionchain/expirylist`, PascalCase request fields (`UnderlyingScrip` int, `UnderlyingSeg`, `Expiry`), extra `client-id` header, rate limit **1 unique request per 3 seconds**, response `data.oc` keyed by decimal strike strings with per-leg greeks + IV (`docs/dhan-ref/06-option-chain.md`, `.claude/rules/dhan/option-chain.md`). **Compare: Groww** ships `GET /v1/option-chain/exchange/{exchange}/underlying/{underlying}?expiry_date=…` returning the WHOLE chain — all available strikes with per-leg greeks (delta/gamma/theta/vega/rho/iv) + ltp/OI/volume, no strike-window param, no pagination (`docs/groww-ref/14-option-chain.md`, `docs/groww-ref/README.md` [R#14]). On Kite you BUILD the chain yourself.

## 2. The composition recipe (instruments master → filter → batched `/quote`)

The only way to produce an option chain on Kite Connect:

```
Step 1 (daily, cold):  GET /instruments/NFO   (and /instruments/BFO for BSE F&O)
                       → gzipped CSV master dump
Step 2 (in-memory):    filter rows: name == <underlying>  AND  expiry == <chosen date>
                       AND instrument_type IN (CE, PE)   → the chain's legs
Step 3 (per refresh):  GET /quote?i=NFO:<sym1>&i=NFO:<sym2>&…   batched ≤500 legs/call
                       → per-leg LTP, OI, volume, 5-level depth, circuit limits
```

### Step 1 — the instruments master

- **Endpoints (Verified, CLIENT-LIB-SOURCE `connect.py` `_routes`):** `GET /instruments` (`market.instruments.all`, ALL exchanges) and `GET /instruments/{exchange}` (`market.instruments`). Exchange constants include `NFO` (NSE F&O) and `BFO` (BSE F&O) (`connect.py` lines 80–86).
- **Format (Verified, CLIENT-LIB-SOURCE + OFFICIAL-MOCK):** CSV, not JSON — `connect.py`'s response handler special-cases `"csv" in content-type` and `_parse_instruments` reads it with `csv.DictReader`; mock `instruments_all.csv` (https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/instruments_all.csv) confirms the header: `instrument_token, exchange_token, tradingsymbol, name, last_price, expiry, strike, tick_size, lot_size, instrument_type, segment, exchange`. **SEARCH (docs `market-data-and-instruments/` extraction):** the dump is **gzipped** on the wire.
- **Size (Verified, CLIENT-LIB-SOURCE `connect.py` `instruments()` docstring):** *"the results could be large, several hundred KBs in size, with tens of thousands of entries"*.
- **Refresh discipline (SEARCH, docs extraction 2026-07-13):** fetch **once a day (~08:30 AM)** and store locally — do NOT poll intraday. **Compare: Dhan/Groww** — same daily-static-master pattern (Dhan Detailed CSV, `docs/dhan-ref/09-instrument-master.md`; Groww master CSV, `docs/groww-ref/09-instruments-csv.md`).
- **Identity wart (SEARCH, docs extraction — LOAD-BEARING):** the docs recommend keying on **`exchange` + `tradingsymbol`**, NOT the numeric `instrument_token`, because exchanges may **REUSE tokens for different derivatives after expiry**. This is the Kite analog of the repo's I-P1-11 composite-key law and of Dhan's "derivative SecurityIds are unstable" rule (`.claude/rules/dhan/instrument-master.md` rule 3). Needs live paste for the verbatim sentence → OPEN QUESTIONS.

### Step 2 — filtering a chain out of the master

**Verified (OFFICIAL-MOCK `instruments_all.csv` rows):** option rows look like

```csv
12073986,47164,BANKNIFTY18JAN23500CE,,2528.4,2018-01-25,23500.0,0.05,40,CE,NFO-OPT,NFO
12074242,47165,BANKNIFTY18JAN23500PE,,35.15,2018-01-25,23500.0,0.05,40,PE,NFO-OPT,NFO
```

- Chain selectors available per row: `expiry` (ISO date), `strike` (float), `instrument_type` (`CE`/`PE`), `segment` (`NFO-OPT`), `lot_size`, `tick_size`. Expiry dates come from the master itself — enumerate `DISTINCT expiry` for the underlying; there is **no expiry-list endpoint** (contrast Dhan's `/optionchain/expirylist`).
- **Underlying-matching wart (Verified, OFFICIAL-MOCK):** in the mock F&O rows the `name` column is EMPTY for options — the underlying is recoverable from the `tradingsymbol` prefix (`BANKNIFTY…`). Whether the live NFO dump populates `name` for options is unconfirmed (the mock is a truncated 100-row historical sample) → OPEN QUESTIONS. Robust filtering should treat tradingsymbol-prefix parsing as fallback, with the strike/expiry columns as the authoritative leg identity.
- **Assumed (order of magnitude):** an index chain is ~90–110 strikes × 2 legs ≈ **~200 instruments per expiry** — same magnitude the groww-ref pack assumes for the Groww chain (`docs/groww-ref/README.md` [R#15]). Not a Kite-verified number; actual strike counts come from the live master.

### Step 3 — batched `/quote` for the legs

- Full per-leg snapshot via `GET /quote` (see `07-market-quotes.md` §4): LTP, volume, OI + `oi_day_high/low`, 5-level depth, circuit limits, `ohlc` — everything a chain view needs EXCEPT greeks/IV (§3 below).
- **Caps + rate constraints (SEARCH — see `07-market-quotes.md` §3/§8):** ≤**500** instruments per `/quote` call; quote family = **1 request/second**, API-key-level.
- **ARITH (from the SEARCH caps):** one ~200-leg index chain (one expiry) = **1 `/quote` call** → refreshable at ~1 Hz best-case. Whole-chain-family math: N legs total across underlyings/expiries needs `⌈N/500⌉` calls at 1/s — e.g. 3 index underlyings × 1 expiry ≈ 600 legs ≈ 2 calls ≈ 2 s per full sweep; per-leg OHLC-only sweeps stretch to 1000/call. Derivation: cap × rate from `07-market-quotes.md` §3/§8.
- **Compare (cadence):** Dhan delivers the WHOLE chain (all strikes, greeks included) in ONE call per underlying at 1-per-3s (`docs/dhan-ref/06-option-chain.md`); Groww in ONE call per underlying+expiry inside the 10/s+300/min Live Data pool (`docs/groww-ref/14-option-chain.md`). Kite's composed chain costs more calls but returns per-leg DEPTH, which neither Dhan's nor Groww's chain payload carries (Groww chain has no per-leg bid/ask — [R#14]; Dhan chain carries top_bid/ask prices but not 5-level depth per `docs/dhan-ref/06-option-chain.md`).

### Streaming alternative for the composed chain

**Verified (CLIENT-LIB-SOURCE pykiteconnect `ticker.py` `_parse_binary`, lines 780–823):** the WebSocket `full` mode packet (184 bytes) carries `oi`, `oi_day_high`, `oi_day_low`, `last_trade_time` + market depth — so a composed chain's legs can be subscribed on the WS (SEARCH: up to 3000 instruments/connection) instead of polled over REST. Same composition step 1–2 applies; only step 3 changes transport.

## 3. Greeks / IV: not provided — self-compute

- **Verified-absence (CLIENT-LIB-SOURCE, SDK level):** no greeks/IV route exists in pykiteconnect `_routes` or anywhere in gokiteconnect; no greeks fields exist in the `/quote` response (`07-market-quotes.md` §4 field table) or the WS binary packets (`ticker.py` `_parse_binary` — full-mode fields end at depth).
- **SEARCH (forum 14843 / 8928 / 11646):** staff-confirmed — *"Zerodha does not have APIs for the calculation of option greeks"*; users must compute greeks/IV themselves (Black-76/BS from the composed chain's LTPs). The Kite WEB/APP UI's option-chain-with-greeks (zerodha.com Z-Connect announcement) is a UI feature, NOT exposed via Kite Connect.
- **Compare:** BOTH Dhan and Groww chain endpoints return per-leg greeks + IV in the payload (`docs/dhan-ref/06-option-chain.md` rule 10: delta/theta/gamma/vega all f64; `docs/groww-ref/14-option-chain.md`: delta/gamma/theta/vega/rho/iv). A Kite-fed chain consumer must add a greeks computation stage that the other two vendors give for free.

## 4. Constraint summary (what the composition imposes)

| Constraint | Value | Tier |
|---|---|---|
| Master dump freshness | daily fetch (~08:30), gzipped CSV, tens of thousands of rows | SEARCH + CLIENT-LIB-SOURCE (size docstring) |
| Leg identity | `exchange:tradingsymbol` composite (tokens reused post-expiry) | SEARCH (needs live paste) |
| Legs per `/quote` call | ≤500 (≤1000 if OHLC/LTP-only) | SEARCH (needs live re-read) |
| Chain refresh cadence floor | 1 quote call/s → ~1 Hz for a ≤500-leg chain; `⌈N/500⌉` s per sweep beyond | ARITH over SEARCH caps |
| Greeks/IV | absent — self-compute | Verified-absence (SDK) + SEARCH (staff) |
| Expiry discovery | from the master dump only (no expiry-list endpoint) | Verified-absence (SDK routes) |
| Per-leg depth | 5-level bid/ask included via `/quote` (unlike Dhan/Groww chains) | Verified (OFFICIAL-MOCK quote.json) |

## 5. India F&O coverage note — NIFTY / BANKNIFTY / SENSEX incl. BSE/BFO (consolidated)

> Added 2026-07-13 (completeness-review H2): the per-surface coverage verdict for the tickvault-relevant Indian index/F&O universe, consolidated from the scattered facts in 07/08/09/10 + this file. **Read the tier column** — several load-bearing cells are IMPLIED-only or Unknown and carry day-0 probes.

| Surface | NSE index values (NIFTY 50 / NIFTY BANK) | NSE F&O (NFO — index futures/options) | BSE index (SENSEX) | BSE F&O (BFO — SENSEX futures/options) |
|---|---|---|---|---|
| **Instruments master** | Verified-implied: index rows live inside the NSE dump with segment `INDICES` (WS segment 9 Verified; CSV segment string Assumed; index tokens 256265/260105 SEARCH+ARITH — `09-…` §4/§8). Whether `/instruments/NSE` includes index rows or only the full dump: **Unknown** (probe) | Verified: `GET /instruments/NFO` exists (SDK constant `EXCHANGE_NFO`); mock carries real NFO option rows (`BANKNIFTY18JAN23500CE`, segment `NFO-OPT`) — `09-…` §3/§4 | SEARCH+ARITH: SENSEX token 265 = 1×256+9; `BSE:SENSEX` spelling forum-sourced — `09-…` §8 | Verified-implied: `EXCHANGE_BFO` constant exists in both SDKs; NO BFO row appears in the 99-row mock sample. Whether SENSEX WEEKLY options appear in the BFO dump: **Unknown** (probe) |
| **REST quotes (`/quote` family)** | Verified-implied: `i=NSE:NIFTY 50`-style prefixes accepted (exchange constants); index payloads presumably omit depth/volume — exact index-quote shape **Unknown** (no index mock) | Verified-implied: `NFO:` prefix is a documented exchange constant; quote mock is NSE-equity only — F&O-quote OI fields exist in the schema (`oi`, `oi_day_high/low`) | Same as NSE index — **Unknown** shape | Verified-implied: `BFO:` prefix constant exists (`07-…` §2); no BFO example anywhere — **Unknown** (probe: `i=BFO:SENSEX…` weekly option) |
| **WebSocket ticks** | Verified (parser level): segment 9 = `indices` has a dedicated 28/32-byte index packet layout in BOTH SDKs — index token subscription is a first-class parser path (`10-…` §7/§9). That NIFTY/SENSEX tokens ACTUALLY stream: **Verified-implied only** (no fetched artifact shows a live index tick) | Verified (parser level): segment 2 (NFO) uses the tradable 44/184-byte layouts incl. OI fields — designed for F&O. Live delivery: implied | Same as NSE index (segment 9 covers BSE indices too — token 265 ends in 9) | Verified (parser level): segment 5 = `bfo` exists in both SDK maps with the default ÷100 divisor. **No fetched artifact demonstrates a live BFO tick** — subscribe-token-265/BFO-token probes are day-0 items |
| **Historical candles** | **Unknown**: whether index tokens (256265/265) serve historical candles is stated NOWHERE in fetched artifacts (widely done in the ecosystem — Assumed yes; probe) | Verified-implied: the `continuous=1` discussion (`08-…` §7) presumes NFO contract historical works; per-interval caps assumed identical — **Unknown** whether F&O minute-depth matches equities | **Unknown** (probe token 265) | **Unknown** — whether BFO (SENSEX futures/options) historical candles are served at all is unstated anywhere; day-0 probe |

**Bottom line (honest):** the parser/enum plumbing for ALL four columns is Verified at SDK level (segment maps, exchange constants, packet layouts), but ZERO fetched artifact demonstrates live SENSEX/BFO data end-to-end, and index/BFO historical coverage is undocumented. The SENSEX/BFO cells are exactly the tickvault §36-FUTIDX-on-SENSEX analog risk (the GDF-trial context) — every **Unknown** above is a day-0 probe in `99-UNKNOWNS.md`.

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Docs-TOC absence confirmation:** open `https://kite.trade/docs/connect/v3/` and paste the sidebar/TOC — confirms at docs level that no option-chain/greeks page exists (today only SDK-level absence + forum SEARCH). Matters: upgrades the §1 absence claim to the pack's strongest tier.
- **Token-reuse wording verbatim:** open `https://kite.trade/docs/connect/v3/market-data-and-instruments/` and paste the instrument_token-reuse + "fetch once a day" sentences. Matters: leg identity key design (I-P1-11 analog) — currently SEARCH-only.
- **`name` column population for live NFO option rows:** the 2018 mock shows empty `name` on options; confirm on a live dump (or paste a live CSV sample row). Matters: underlying-filter correctness vs tradingsymbol-prefix parsing.
- **NFO/BFO dump size + strike counts today:** row counts per exchange and strikes-per-index-expiry from a live dump. Matters: replaces the Assumed ~200-legs/chain magnitude with measured numbers for the ARITH cadence math.
- **Instruments endpoint rate/abuse policy:** any documented limit on `/instruments` itself (beyond "once a day" guidance) — paste from the same page. Matters: retry/backoff design for the daily fetch.
- **Tradingsymbol grammar for options:** the official symbology (weekly vs monthly encodings, e.g. `NIFTY24D0524000CE` vs `BANKNIFTY18JAN23500CE`) — paste from docs/support page if documented. Matters: prefix-parsing robustness across weekly/monthly formats.
- **SENSEX/BFO end-to-end coverage (§5 Unknowns, day-0 probes):** (a) does the BFO dump carry SENSEX weekly options; (b) does `GET /quote?i=BFO:SENSEX…` return data; (c) does subscribing token 265 (SENSEX index) and a BFO token stream WS ticks; (d) do index tokens (256265/265) and BFO contracts serve historical candles? Probe all four in the first live session. Matters: the tickvault SENSEX-futures analog (§36 FUTIDX) cannot be planned on implied coverage.

## CLAIMS (for README reconciled table)

- Kite Connect v3 has NO option-chain, expiry-list, or greeks endpoint — the chain must be COMPOSED client-side — **Verified-absence (CLIENT-LIB-SOURCE: complete pykiteconnect `_routes` dict + full gokiteconnect grep) + SEARCH (staff forum 14832/14843: chain API "on the to-do list", no greeks APIs)** — https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/connect.py
- Composition recipe = daily `GET /instruments/{NFO|BFO}` master → filter by underlying+expiry+CE/PE → batched `GET /quote` for the legs — **Verified (CLIENT-LIB-SOURCE routes + OFFICIAL-MOCK instruments_all.csv columns) / SEARCH (caps)**
- Instruments master is a gzipped CSV with columns `instrument_token, exchange_token, tradingsymbol, name, last_price, expiry, strike, tick_size, lot_size, instrument_type, segment, exchange`; option rows carry `CE`/`PE` + `NFO-OPT` — **Verified (OFFICIAL-MOCK instruments_all.csv + gokiteconnect Instrument struct)** — https://raw.githubusercontent.com/zerodha/kiteconnect-mocks/main/instruments_all.csv
- Master dump is "several hundred KBs … tens of thousands of entries"; fetch once daily (~08:30) and store locally — **Verified (CLIENT-LIB-SOURCE `instruments()` docstring) + SEARCH (docs market-data-and-instruments extraction)**
- Instrument tokens are REUSED for different derivatives after expiry — key legs on `exchange:tradingsymbol` — **SEARCH (docs extraction 2026-07-13; live-paste pending)**
- A composed chain refreshes at best ~1 Hz for ≤500 legs (1 quote call/s × 500-instrument cap); `⌈N/500⌉` seconds per sweep beyond — **ARITH over SEARCH caps (07-market-quotes.md §3/§8)**
- Greeks/IV must be self-computed; Dhan and Groww both return per-leg greeks in their single-call chain endpoints, Kite returns none — **Verified-absence (SDK) + SEARCH (forum) / repo compare: `docs/dhan-ref/06-option-chain.md`, `docs/groww-ref/14-option-chain.md`**
- The composed Kite chain carries per-leg 5-level DEPTH (via `/quote`) and WS full-mode OI (184-byte packet, `oi`/`oi_day_high`/`oi_day_low`) — capabilities the Dhan/Groww chain payloads lack — **Verified (OFFICIAL-MOCK quote.json + CLIENT-LIB-SOURCE ticker.py `_parse_binary` lines 780–823)**
- India-F&O coverage (§5): parser/enum plumbing for NSE indices, NFO, SENSEX and BFO is Verified at SDK level (segment map slots 2/5/9, exchange constants, packet layouts), but NO fetched artifact demonstrates live SENSEX/BFO data end-to-end, and index/BFO HISTORICAL coverage is undocumented — **Verified (SDK plumbing) / Unknown (live delivery + historical; day-0 probes)**
