# Groww Trading API — Reference (for the tickvault Groww second feed)

> **🎯 IMPLEMENTATION SCOPE (operator 2026-06-19): the Groww LIVE FEED ALONE.**
> We build ONLY the streaming live tick feed → 1-minute candles
> (`07-feed-websocket.md` LTP + index value). Everything else in these docs —
> REST snapshot (`get_quote`/`get_ltp`/`get_ohlc`), order/position updates,
> option chain, greeks, historical candles, NSE/BSE EOD, indices ingestion — is
> **REFERENCE ONLY, explicitly OUT OF SCOPE**. No order placement, no portfolio,
> no commodities. Groww historical/backtest fetching from TickVault is governed
> by `.claude/rules/project/groww-second-feed-scope-2026-06-19.md` §33
> (LIVE-FEED-ONLY) / §37 (the narrow BruteX-S3 consumption grant); any future
> REST consumer requires its own dated operator grant recorded in the rule file
> FIRST.
>
> **Why this exists:** the Groww second feed (operator lock 2026-06-19,
> `.claude/rules/project/groww-second-feed-scope-2026-06-19.md`) is implemented
> as **native tickvault Rust** — **brutex is reference only, no code pulled.**
> These are the authoritative Groww docs the implementation is built against.
> Mirrors the `docs/dhan-ref/` + `docs/gdf-ref/` patterns.
>
> **Source:** Groww Trading API docs (`growwapi` Python SDK + REST/curl twins).
> Saved verbatim 1:1 for reference. The SDK is **reference for the protocol + data
> shapes only** — we do NOT depend on or vendor the Python package.
> **Single source of open questions:** `99-UNKNOWNS.md` (U-1 … U-24) — inline
> `U-<n>` flags across the pack resolve there.

---

## ⚠ Evidence-tier legend + provenance (read before trusting any label)

**Provenance, stated honestly:** the pack's authority is a **26-page lossless
scripted capture of the official docs taken 2026-07-03** (per-page heading /
table / code-block counts compared against the live HTML — 100% match, zero
loss; see `00-INDEX.md`). On **2026-07-13** the pack was refreshed with live
cross-checks — but `groww.in` was **403-blocked at the sandbox egress proxy**
(WebFetch 403; `curl` via proxy → CONNECT policy denial; web.archive.org
equally blocked), so no direct byte-verification against the live pages was
possible that date. The 2026-07-13 cross-checks therefore came via
**search-backend extraction of the named live pages** and **forensically dated
2026 third-party mirrors**, plus the official `growwapi` 1.5.0 wheel read as
raw source from PyPI. The live docs have PROVABLY drifted in ≥1 place since
2026-07-03 (a "COMMODITY" mention on the live-data page absent from the
capture) — capture-only rows should be re-verified from an unblocked network
(the box) before contracting.

| Tier | Meaning | Trust |
|---|---|---|
| **Verified-capture** | verbatim from the 2026-07-03 lossless 26-page official capture (includes verified-ABSENCE claims grep'd over all 26 pages) | Highest available |
| **Verified-live-crosscheck** | independently matched on 2026-07-13 via live-page search extraction (substantive, URL-cited, not byte-verbatim) | Freshness signal on top of capture |
| **Verified-mirror** | matched by forensically DATED third-party verbatim page scrapes/mirrors (2026-dated mirrors corroborate; 2025-dated mirrors expose stale generations) | Corroboration; never sole authority |
| **Verified (SDK 1.5.0)** | read from the official `growwapi` 1.5.0 wheel source (PyPI latest as of 2026-07-13) | Authoritative for client behaviour |
| **Assumed** | single source or inferred; stated with the inference | Verify before contracting |
| **Unknown** | genuinely undocumented → `99-UNKNOWNS.md` with the exact probe/support question | — |

## Reconciled master claims table (the load-bearing facts)

Adjudicated 2026-07-13 across parallel research + adversarial refutation passes
over the capture, the wheel, and same-day live cross-checks. Conflicts were
resolved by mirror dating + full-corpus greps; the verdicts below are the
pack's authority — cite rows as **[R#n]**.

| # | Claim | Tier | Source file |
|---|---|---|---|
| 1 | Official rate-limit table: Orders **10/s · 250/min** · Live Data **10/s · 300/min** · Non Trading **20/s · 500/min**; limits pool at TYPE level ("If the limit for one API within a type is exhausted, all other APIs in that type will also be rate-limited") | Verified-capture + Verified-mirror (2026-dated) | `15-rate-limits-and-capacity.md` §1–§2 |
| 2 | **No per-day column** exists in the official table; no burst semantics beyond per-second | Verified-capture (absence) | `15-…` §1 |
| 3 | The circulating **Orders 15/s + per-day-cap table is 2025-STALE** (every mirror carrying it dates 2025; every 2026 mirror carries 10/250 · 10/300 · 20/500); the 2026-07-13 search backend regurgitated the stale table as if official | Verified-mirror (refutation) | `15-…` §2 |
| 4 | The Orders limit demonstrably changed **15 → 10 between 2025-12-16 and 2026-03-22** — the numbers CAN move; present-tense currency needs the day-0 live re-read | Verified-mirror | `15-…` §2, probe §6.7 |
| 5 | `/v1/historical/*` and `/v1/option-chain/*` appear in **NO documented rate-limit family** (Live Data row enumerates only quote/LTP/OHLC) | Verified-capture (absence) | `15-…` §3, U-4 |
| 6 | LTP/OHLC batch cap: "**Up to 50 instruments** … for each API call"; quote is single-instrument only; 1-call-vs-50-requests billing undocumented (Assumed 1) | Verified-capture + Verified-live-crosscheck (2026-07-13) | `15-…` §1, U-9 |
| 7 | WS live feed: "up to **1000** instruments at a time" / "upto 1000 subscriptions are allowed at a time" — per-connection vs per-account UNSTATED | Verified-capture / Unknown (scope) | `00-INDEX.md`, `07-feed-websocket-streaming.md`, U-14 |
| 8 | Historical candles: current endpoint `GET /v1/historical/candles` (documented on the Backtesting page), **12 intervals** `1minute…1month` | Verified-capture + Verified-live-crosscheck + SDK | `11-historical-candles.md` §1, §3 |
| 9 | Per-request range caps: **30 days** (1–5 min) / **90 days** (10–30 min) / **180 days** (1 h–1 month); numeric rows capture+mirror-verified only (never surface in live snippets) | Verified-capture + Verified-mirror | `11-…` §4, U-3 |
| 10 | Data "available from 2020" (equities, indices, FNO); the landing page's "up to 3 months" line is marketing shorthand that pollutes search extraction | Verified-live-crosscheck (from-2020) | `11-…` §4 |
| 11 | Deprecated `GET /v1/historical/candle/range`: "deprecated and will NOT work in the future"; DIFFERENT caps (7/15/30/150/365/1080 d/No-Limit) + "Last 3 months" intraday; 6-element epoch-second candles | Verified-capture | `11-…` §7 |
| 12 | Current-endpoint candle = `[timestamp, o, h, l, c, volume, oi]` (7 elements); OI FNO-only (`null` otherwise); example timestamp is ISO-`T` while schema says space-separated — parser must accept both; timezone never stated (Assumed IST) | Verified-capture / Assumed (IST) | `11-…` §5, U-6 |
| 13 | **Current-day serving + just-closed-minute freshness: UNDOCUMENTED** — zero statements across all 26 pages (grep-verified 2026-07-13); the only adjacent hint is the OHLC note pointing interval-candle users at the historical surface | Verified-capture (absence) / Unknown (live behaviour) | `11-…` §6, U-1/U-2 |
| 14 | Option chain: `GET /v1/option-chain/exchange/{ex}/underlying/{ul}?expiry_date=` returns the WHOLE chain — "all available strikes", per-leg greeks (delta/gamma/theta/vega/rho/iv) + ltp/OI/volume; NO strike-window param, NO pagination, NO response timestamp, NO per-leg bid/ask | Verified-capture + SDK | `14-option-chain.md` |
| 15 | Chain path sits OUTSIDE `/live-data/`; chain family + strike counts + payload size undocumented (~90–110 strikes ≈ ~100–300 KB is an Assumed order of magnitude) | Verified-capture (path) / Unknown / Assumed (size) | `14-…` §4–§5, U-4/U-11/U-12 |
| 16 | Access-token expiry "**(Expires daily at 6:00 AM)**" is **OFFICIALLY DOCUMENTED** (REST intro, 1st Approach) — previously mislabeled third-party-only | Verified-capture + Verified-live-crosscheck | `17-token-lifecycle.md` §2 |
| 17 | The token-mint response carries a machine-readable **`expiry`** field (+ `tokenRefId`/`sessionName`/`isActive`) — the official SDK DISCARDS everything but `token` | Verified-capture + SDK | `17-…` §4, U-19 |
| 18 | The docs define **THREE auth approaches** (direct Access Token / API-key+Secret checksum / API-key+TOTP) — the python-sdk page shows only two | Verified-capture | `17-…` §1 |
| 19 | TOTP-flow wording contradiction: python-sdk "No Expiry" vs REST intro "Requires daily approval"; empirically the minter runs unattended daily since 2026-07-02 | Verified-capture (both wordings) / Unknown (official intent) | `17-…` §5, U-19 |
| 20 | Checksum flow: `checksum = SHA-256(secret + epoch_seconds)`, timestamp "Valid for 10 minutes"; one mint endpoint `POST /v1/token/api/access` serves both flows via `key_type` | Verified-capture + SDK | `17-…` §3 |
| 21 | One-active-token semantics UNDOCUMENTED (contrast Dhan); the UI manages MULTIPLE named tokens — the shared-minter lock exists precisely to avoid probing this live | Unknown | `17-…` §5, U-17 |
| 22 | **No fair-use / ban / cooldown / Retry-After / adaptive-throttle language anywhere** in the REST docs (grep over ALL 26 pages, 2026-07-13); the only guidance is the exceptions page's "throttle the request rate" | Verified-capture (absence) | `15-…` §5, U-7 |
| 23 | Docs silence ≠ absence of adaptive control: the WS surface MEASURABLY runs an undocumented adaptive cap (33 fresh → 7 after churn, ~35–40 min penalty, 2026-07-06) — assume REST may too | Verified (internal measurement) | `15-…` §5, U-8 |
| 24 | SDK 1.5.0 has **ZERO client-side throttling/retry/backoff and zero validation on market-data params**; `timeout=None` default = infinite hang; 429 → `GrowwAPIRateLimitException` with nothing read from the body | Verified (SDK 1.5.0) | `11-…` §9, `15-…` §5 |
| 25 | GA error codes: GA000, GA001, GA003–GA007 (**no GA002**); envelope `{"status":"FAILURE","error":{code,message,metadata}}`; the wire `message` is request-specific, not the table's generic string | Verified-capture | `16-orders-margins-portfolio.md` §5 |
| 26 | The orders annexure lists 12 order-status values, but the doc's OWN examples all show undocumented `"order_status": "OPEN"`; validity enum = DAY only (no IOC); MCX availability is a three-way doc contradiction that drifted live post-capture | Verified-capture (contradictions) / Unknown (wire truth) | `16-…` §2, §4 |
| 27 | Wire protocol (feed): NATS over `wss://socket-api.groww.in`, protobuf ticks with `tsInMillis` (epoch **milliseconds**); ed25519 NKey + per-session socket-token mint | Verified (SDK reverse-read; live-proven in production since 2026-06) | wire table below, `02-verified-endpoints.md` |

## File map

### Official doc pack (operator-uploaded 2026-07-03 — prefer these where they overlap)

Captured verbatim from `https://groww.in/trade-api/docs/python-sdk` (+ REST
section) on 2026-07-03 (SDK 1.5.0, scripted 100%-match verification per page).
Index: [`00-INDEX.md`](./00-INDEX.md).

| File | What it covers |
|---|---|
| `00-INDEX.md` | Index of the 26-page official capture + key facts + the 2026-07-13 refresh map |
| `01-introduction-auth-ratelimits.md` | Getting started, API key/auth, rate limits *(superseded on rate limits + token lifecycle — banner)* |
| `07-feed-websocket-streaming.md` | **Official** `GrowwFeed` streaming docs: subscribe_ltp / subscribe_index_value / market depth / order + position updates; payload shapes |
| `09-instruments-csv.md` | instrument.csv, exchange tokens, DataFrame helpers |
| `12-sdk-exceptions.md` | SDK exception classes |
| `13-annexures-enums.md` | All enums: exchange, segment, product, order type, validity, statuses |

### 2026-07-13 full-coverage additions (evidence-tiered, gdf-ref conventions)

| File | What it covers |
|---|---|
| [`11-historical-candles.md`](./11-historical-candles.md) | Historical candles: current `GET /v1/historical/candles` (12 intervals, 30/90/180-day caps, 7-element candles, timestamp warts), deprecated `/candle/range`, expiries/contracts companions, the current-day-serving UNDOCUMENTED section — **reference-only scope banner** |
| [`14-option-chain.md`](./14-option-chain.md) | Full verbatim option-chain endpoint + response (greeks per leg), documented-absence facts, family question — **reference-only scope banner** |
| [`15-rate-limits-and-capacity.md`](./15-rate-limits-and-capacity.md) | THE capacity-truth file: official table verbatim + mirror-dating forensics (stale 15/s tables refuted), unassigned families, the three-way per-minute-pipeline arithmetic, 429 behaviour, live-probe plan |
| [`16-orders-margins-portfolio.md`](./16-orders-margins-portfolio.md) | Orders/smart-orders/portfolio/margin inventory + full verbatim field tables + annexure enums + GA codes — **NOT used by TickVault** (doc completeness only) |
| [`17-token-lifecycle.md`](./17-token-lifecycle.md) | CORRECTED token lifecycle: 3 documented auth approaches, mint wire shapes, the OFFICIAL daily 6:00 AM expiry, the `expiry` response field the SDK discards, contradictions recorded |
| [`99-UNKNOWNS.md`](./99-UNKNOWNS.md) | Consolidated U-1…U-24 open-questions table — exact support questions + live probes, blocking classification, priority ordering |

### 2026-06 capture + reverse-read wire notes

| File | What it covers |
|---|---|
| `01-introduction-auth.md` | Auth (API-key+secret OR **API-key+TOTP**), `get_access_token`, rate limits *(superseded on rate limits + token lifecycle — banner)* |
| `02-instruments.md` | Instruments master CSV schema + lookup (exchange / exchange_token / groww_symbol / isin) |
| `02-verified-endpoints.md` | Verified endpoint inventory incl. the per-session socket-token mint |
| `03-live-data-rest.md` | REST snapshot: `get_quote` / `get_ltp` / `get_ohlc` / option chain / greeks (pull-based) |
| `04-annexures.md` | Constants & enums (order status, segments, exchanges, products) |
| `05-exceptions.md` | Exception taxonomy (auth / authorisation / bad-request / rate-limit / timeout / feed-connection / feed-not-subscribed) |
| `06-changelog.md` | SDK changelog (latest 1.5.0, 3 Dec 2025) |
| `07-feed-websocket.md` | **Live streaming feed** (`GrowwFeed`): subscribe LTP / index / depth / order+position updates; data shapes |
| `08-master-groww-nse-bse-context.md` | Combined context: Groww SDK + NSE/BSE EOD market-data + backtesting candles *(superseded on the historical endpoint — banner)* |
| `09-prompt-nse-indices-data.md` | NSE indices ingestion brief (taxonomy, endpoints) |
| `10-live-feed-mapping-verified.md` | Live-feed mapping verification notes |
| `SSM-SETUP.md` | *(superseded by the token-minter lock — banner)* |
| `instrument-sample.csv` | First 500 rows of the instruments master (header + sample) |

## Instruments master CSV — NOT vendored (fetched at runtime)

The full master is **~23.5 MB / ~164k rows**, daily-changing. Like the Dhan
instrument master, it is **downloaded at runtime, never committed** (a 23 MB
daily-stale blob in git is an anti-pattern). Only a 500-row sample lives here.

- **Canonical URL:** `https://growwapi-assets.groww.in/instruments/instrument.csv`
- **Columns:** `exchange, exchange_token, trading_symbol, groww_symbol, name,
  instrument_type, segment, series, isin, underlying_symbol,
  underlying_exchange_token, expiry_date, strike_price, lot_size, tick_size,
  freeze_quantity, is_reserved, buy_allowed, sell_allowed, internal_trading_symbol,
  is_intraday`
- **Subscription key:** the live feed identifies an instrument by
  `(exchange, segment, exchange_token)` — e.g. `{exchange:"NSE", segment:"CASH",
  exchange_token:"2885"}` for RELIANCE. `exchange_token` (NOT trading symbol) is
  the wire key.
- **ISIN** column lets us join Groww instruments to the Dhan/NSE universe by
  the immutable security identity (same ISIN-primary join used for NTM).

## Verified WIRE protocol (reverse-read from `growwapi==1.5.0` SDK source)

The SDK docs above describe the *Python interface*; the **native Rust client**
needs the underlying wire protocol, which Groww does not narratively document.
These facts were read directly from the SDK source and are the build target:

| Layer | Verified fact |
|---|---|
| Transport | **NATS** over `wss://socket-api.groww.in` (TLS WebSocket), **protobuf** payloads |
| Access token | `POST https://api.groww.in/v1/token/api/access`, header `Authorization: Bearer <api_key>` + `x-api-version: 1.0`, body `{"key_type":"totp","totp":"<6-digit>"}` (TOTP flow) OR `{"key_type":"approval","checksum":...,"timestamp":...}` (secret flow) → `{token}` (full mint wire shapes + response fields: `17-token-lifecycle.md`) |
| Socket session | client generates an **Ed25519 NKey** locally → `POST https://api.groww.in/v1/api/apex/v1/socket/token/create/`, `Authorization: Bearer <access_token>`, body `{"socketKey":"<nkey_public>"}` → `{token: <JWT>, subscriptionId}` |
| NATS auth | connect with `user_credentials = (JWT, NKey seed)` (NOT a URL param / post-connect message); `ping_interval = 60s`; NATS auto-reconnect |
| Subscribe | NATS SUB to a subject string = `<prefix><subscriptionId>`; LTP prefixes e.g. `/ld/eq/nse/price.`, `/ld/fo/nse/price.`, `/ld/indices/nse/price.`; depth `…/book.` |
| Tick payload | protobuf `StocksLivePriceProto` (all `double`): `tsInMillis` (**epoch MILLISECONDS** ✅), `ltp`, `open/high/low/close`, `volume`, `value`, `openInterest`, bid/offer qty, ranges; depth `StocksMarketDepthProto { tsInMillis, buyBook[], sellBook[] }` |
| Limits | up to **1000 subscriptions** [R#7]; Live-Data REST 10/sec, 300/min [R#1] |

**Honesty note:** Groww publishes no formal wire spec; the wire facts are
reverse-read from the official SDK source — and since 2026-06 they are
live-proven daily by the production Groww feed. Token-mint facts are
REFERENCE-ONLY for TickVault: the bruteX-owned minter Lambda is the sole
minter (`.claude/rules/project/groww-shared-token-minter-2026-07-02.md`).

## Native-Rust mapping (Groww second-feed PRs — shipped)

| PR | Slice | Groww doc / fact used |
|---|---|---|
| PR-2 | access-token auth (api_key + TOTP → access_token) — *mint since removed per the 2026-07-02 token-minter lock* | `17-token-lifecycle.md` + access-token wire fact |
| PR-3 | NKey + socket-token + NATS connect + subscribe | socket-session + NATS facts |
| PR-4 | protobuf tick decode → WAL (`WsType::Groww`) + reconnect | tick-payload fact + `07-feed-websocket.md` |
| PR-5 | 1-minute aggregation → shared `candles_1m` (`feed='groww'`) | ms timestamps |
| PR-6 | live-1m vs backtest parity — *CANCELLED per §33; replaced by the §37 BruteX-S3 cross-verify* | `08-master…` (superseded) |
| PR-7 | functional per-feed enable/disable + observability + chaos | — |
