# Groww Trade API — Complete Documentation Pack (Python SDK + REST)

> **Captured:** 2026-07-03 directly from https://groww.in/trade-api/docs/python-sdk (+ REST section)
> **Method:** Every page in the docs navigation fetched raw, `<article>` content extracted, converted to Markdown.
> **Verification (scripted, per page):** heading count, table count, and code-block count in the Markdown were compared against the live HTML for **all 26 pages — 100% match, zero loss**. Docs are not in Groww's sitemap; the in-page navigation (14 SDK + 12 REST pages) is the complete set, and all 26 are here.
> **SDK version at capture:** `growwapi` **1.5.0** (released 3rd Dec 2025 — latest per changelog)

---

## File Index

| # | File | Contents |
|---|------|----------|
| 01 | introduction | Getting started, API key/auth |
| 02 | orders | Place / Modify / Cancel / Status / Order list / Trade list |
| 03 | smart-orders | **GTT + OCO** — create/modify/cancel, CASH + FNO (not COMMODITY) |
| 04 | portfolio | Holdings + Positions (incl. `realised_pnl`) |
| 05 | margin | User margin, required-margin calculator |
| 06 | live-data | REST snapshots: Quote / LTP / OHLC |
| 07 | feed | **WebSocket streaming (now officially documented)** — `GrowwFeed`: subscribe LTP / index / market depth / order updates / position updates, sync + async callback modes |
| 08 | historical-data | Candle data |
| 09 | instruments | instrument.csv (`https://growwapi-assets.groww.in/instruments/instrument.csv`), exchange tokens, DataFrame helpers |
| 10 | user | User profile |
| 11 | backtesting | Backtesting guide |
| 12 | exceptions | SDK exception classes |
| 13 | annexures | All enums: exchange, segment, product, order type, validity, statuses |
| 14 | changelog | v0.0.1 → v1.5.0 full history |
| 15–26 | BONUS-REST-* | The parallel REST/curl docs: raw endpoints behind the SDK, auth flows (Access Token / API Key+Secret / TOTP), **checksum generation (SHA-256 of secret+epoch)**, rate limits, error codes |

## Key facts captured (verbatim from docs)

- **Feed subscription cap — now official:** "You can subscribe for up to **1000 instruments at a time**." ⚠️ Wording is "at a time" — the docs still do **not** state per-connection vs per-account, and do not mention multi-connection support. The core migration question remains officially unresolved; only the number is now confirmed.
- **Rate limits (type-level, shared within type):** Orders 10/sec · 250/min | Live Data 10/sec · 300/min | Non-Trading 20/sec · 500/min.
- **Feed data model:** LTP payloads carry `tsInMillis` + `ltp` (snapshot-style values); market depth = 5-level buy/sell book. Order + position update streams included.
- **MCX contradiction in Groww's own docs:** Changelog v1.5.0 says "Added: Commodity trading support on MCX exchange," but the REST intro still says commodities (MCX) "is not available at this time," and Smart Orders page says COMMODITY segment not supported. Treat MCX as unverified — confirm with Groww support before relying on it.
- **Auth:** three approaches — direct Access Token, API Key + Secret (checksum flow), API Key + TOTP.


---

## Local copies in this repo (docs/groww-ref/, saved 2026-07-03)

Of the 26-page capture above, the operator uploaded these 5 pages into the repo
(byte-identical content, renamed for the local convention). This file is the
index for that official pack; the older reverse-read/context docs
(`01-introduction-auth.md`, `07-feed-websocket.md`, `10-live-feed-mapping-verified.md`,
…) remain alongside — see `README.md` for the full directory map.

| # | Local file | Contents |
|---|------------|----------|
| 01 | [`01-introduction-auth-ratelimits.md`](./01-introduction-auth-ratelimits.md) | Getting started, API key/auth, rate limits |
| 07 | [`07-feed-websocket-streaming.md`](./07-feed-websocket-streaming.md) | **WebSocket streaming (officially documented)** — `GrowwFeed`: subscribe LTP / index / market depth / order + position updates; payload shapes (`tsInMillis` + `ltp`/`value`) |
| 09 | [`09-instruments-csv.md`](./09-instruments-csv.md) | instrument.csv, exchange tokens, DataFrame helpers |
| 12 | [`12-sdk-exceptions.md`](./12-sdk-exceptions.md) | SDK exception classes |
| 13 | [`13-annexures-enums.md`](./13-annexures-enums.md) | All enums: exchange, segment, product, order type, validity, statuses |

---

## 2026-07-13 full-coverage refresh

Per the 2026-07-13 full-coverage directive, the remaining doc areas of the 26-page capture were compiled into evidence-tiered reference files (gdf-ref conventions: tier labels on every claim + a consolidated unknowns file). Access note: groww.in was 403-blocked at the sandbox proxy on 2026-07-13 — these files rest on the 2026-07-03 lossless capture + 2026-07-13 live search-mediated cross-checks + the official `growwapi` 1.5.0 wheel source, with the proxy-block honestly recorded per file. Key corrections landed with this refresh: the docs define **THREE auth approaches** (not two), and the **daily ~06:00 IST token expiry IS officially documented** ("(Expires daily at 6:00 AM)", REST intro) — see `17-token-lifecycle.md`.

| # | Local file | Contents |
|---|------------|----------|
| 11 | [`11-historical-candles.md`](./11-historical-candles.md) | Historical candles: current `GET /v1/historical/candles` (12 intervals, 30/90/180-day caps, `[ts,o,h,l,c,volume,oi]`, ISO-T-vs-space timestamp wart), deprecated `/candle/range`, expiries/contracts companions, current-day-serving UNDOCUMENTED section |
| 14 | [`14-option-chain.md`](./14-option-chain.md) | `GET /v1/option-chain/exchange/{exchange}/underlying/{underlying}?expiry_date=` — full verbatim response (underlying_ltp + all strikes × CE/PE with greeks), no strike-window param, no response timestamp, rate-limit family Unknown |
| 15 | [`15-rate-limits-and-capacity.md`](./15-rate-limits-and-capacity.md) | THE capacity-truth file: official 3-family rate-limit table verbatim + freshness forensics (stale 15/s third-party tables refuted), unassigned families, per-minute-pipeline arithmetic, 429 behaviour, live-probe plan |
| 16 | [`16-orders-margins-portfolio.md`](./16-orders-margins-portfolio.md) | Orders/smart-orders/portfolio/margin endpoint inventory + full verbatim field tables (order detail 22 fields, both margin endpoints) + annexure enums + the official GA000–GA007 error-code table — **NOT used by TickVault** (doc completeness only) |
| 17 | [`17-token-lifecycle.md`](./17-token-lifecycle.md) | CORRECTED token lifecycle: 3 documented auth approaches, mint wire shapes verbatim, the officially documented daily 6:00 AM expiry + the machine-readable `expiry` response field the SDK discards, python-sdk-vs-REST wording contradiction, token-minter-lock cross-ref |
| 99 | [`99-UNKNOWNS.md`](./99-UNKNOWNS.md) | Consolidated `[U-n]` open-questions table (gdf-ref style) — every genuinely undocumented item with the exact Groww-support question or live probe that answers it |
