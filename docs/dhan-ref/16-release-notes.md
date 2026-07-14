# Dhan V2 Release Notes — Complete Reference

> **Source**: https://dhanhq.co/docs/v2/releases/
> **Extracted**: 2026-03-13
> **Purpose**: Track breaking changes and new features across versions
> **NOTE**: Version labels (2.0–2.5) are our internal mapping of Dhan's dated releases for easy reference. Dhan's releases page lists features by date without formal version numbers. The Python SDK (`dhanhq` on PyPI) has its own versioning (2.0.0, 2.1.0, 2.2.0rc1).

---

## Version 2.5.1 — Mar 17, 2026

### Regulatory Changes (SEBI Framework)
- **Market Price Protection (MPP)** — Effective March 21: Market orders via API are **no longer allowed**. Dhan/exchange auto-converts `MARKET` to `LIMIT` with MPP applied. `price: 0` still accepted in request. Systems must check order execution status — converted orders may remain `PENDING`. See `07-orders.md` MPP section.
- **Order rate limits** — Effective March 21: Reduced to **10 orders/second** (was 25/sec). Per-minute/hour/day limits unchanged. Already in our rate limiter.
- **Static IP mandatory** — Effective April 1: ALL API orders (place/modify/cancel/super/forever) must come from whitelisted static IP. Orders from unregistered IPs are **REJECTED by exchange**. No grace period. `GET /v2/ip/getIP` response now includes `detectedIP`, `ipMatchStatus`, `ordersAllowed` fields. See `02-authentication.md` for details.

### SDK Changes (v2.2.0rc1)
- `client-id` header now sent on ALL requests by default (not just Market Quote / Option Chain)
- `PRE_OPEN` removed from AMO time validation for place order
- Forever Order list endpoint confirmed as `GET /forever/orders` (not `/forever/all`)

---

## Version 2.5 — Feb 09, 2026

### New Features
- **Conditional Trigger Orders** — orders triggered on market conditions
- **P&L Based Exit** — auto-close positions at profit/loss thresholds
- **Exit All API** — close all positions + cancel orders in one call
- **Access Token Generation via API** — programmatic token gen with TOTP

### Improvements
- **Option Chain** — enhanced rate limits (multiple unique requests per 3s), new fields: `average_price`, `security_id`

---

## Version 2.4 — Sep 22, 2025

### New Features
- **API Key based login** — 1-year validity key + daily token generation via OAuth

### Breaking Changes
- **Access Token limited to 24 hours** (SEBI compliance — exchange guidelines on API access management)
- **Static IP mandatory** for all Order APIs (place/modify/cancel) — includes normal orders, super orders, and forever orders. NOT required for data/portfolio/non-order APIs. 7-day cooldown on IP modification.

---

## Version 2.3 — Sep 08, 2025

### New Features
- **200-level Full Market Depth** on WebSocket
- **Expired Options Data** — historical data for expired contracts with ATM±strike syntax
- **Order API rate limit changed to 10/sec** (regulatory)

---

## Version 2.2 — Mar 07, 2025

### New Features
- **Super Orders** — entry + target + stop loss in one order, with trailing SL
- **User Profile API** — token validity, active segments, data plan, DDPI, MTF status

### Improvements
- **Intraday Historical Data** — now 5 years (was limited), added OI, IST time in from/to params
- **Daily Historical Data** — added OI field
- **Live Order Update** — added `CorrelationId` and `Remarks` fields

### Breaking Changes
- **Data API rate limits changed** — 5/sec, 100K/day, no minute/hour limits

---

## Version 2.1 — Jan 06, 2025

### New Features
- **20 Market Depth** (Level 3) on WebSocket for NSE
- **Option Chain API** — full chain with greeks, OI, volume, bid/ask

### Improvements
- **`expiryCode`** now optional in Daily Historical Data

---

## Version 2.0 — Sep 15, 2024

### New Features
- **Market Quote API** — REST snapshots for LTP/OHLC/depth, 1000 instruments
- **Forever Orders** — single and OCO
- **Live Order Update** — real-time WebSocket order status
- **Margin Calculator** — pre-trade margin check

### Improvements
- **Intraday Historical Data** — OHLC+Volume for 1/5/15/25/60 min
- **GET Order APIs** — added `filledQty`, `remainingQuantity`, `averageTradedPrice`, `PART_TRADED` status
- **Live Market Feed** — auth via query params, JSON subscribe, `FULL` packet added

### Breaking Changes
- **Order Placement** — removed `tradingSymbol`, `drvExpiryDate`, `drvOptionType`, `drvStrikePrice` from request
- **Order Modification** — `quantity` = total order qty (not pending qty)
- **Daily Historical Data** — `symbol` replaced with `securityId`
- **Timestamps** — **Epoch (UNIX) time replaced Julian time** in Historical Data APIs
- **Live Market Feed** — `Market Depth` mode deprecated, replaced by `FULL` packet
- **Endpoints changed** — Trade History → `/trades`, Kill Switch → `/killswitch`
- **Error codes** — now DH-900 series
- **Instrument CSV** — new column mappings

---

## Key Takeaways for tickvault

1. **Always use v2** — v1 is deprecated
2. **24-hour tokens** — must auto-renew daily (since v2.4)
3. **Static IP required for orders** — since v2.4 (SEBI mandate)
4. **Epoch timestamps** — since v2.0, all historical data uses UNIX epoch (UTC-based)
5. **FULL packet** — since v2.0, use Full mode instead of separate depth subscription on Live Market Feed
6. **Super Orders** — since v2.2, available for combined entry+exit
7. **200-level depth** — since v2.3, separate WebSocket endpoint
8. **Option Chain enhanced** — since v2.5, `security_id` field in response
9. **Python SDK versioning** — Python SDK versions (2.0.0→2.2.0rc1) do NOT match API platform versions (2.0→2.5)
10. **SEBI algo trading framework** — SEBI directive Feb 2025, effective Aug 2025. Static IP + 2FA + API key traceability mandatory.

---

## 2026-07-13 Upstream Update (search-index spot-check + PyPI — see `verification-2026-07-13.md`)

Per repo convention this dated section supersedes without rewriting the text above.

### (a) Provenance inconsistency in THIS file (flagged, header NOT rewritten)

The header says **Extracted: 2026-03-13**, yet the "Version 2.5.1 — Mar 17, 2026" section above
postdates that extraction. Per the header NOTE, version labels are our INTERNAL mapping of
Dhan's dated releases — the 2.5.1 section was compiled from dated announcements (MPP circulars,
SDK changes) and **may never have been a releases-page entry at all**. Do not treat the header
extraction date as covering the 2.5.1 section, and do not cite "v2.5.1" as a Dhan-published
release label.

### (b) Live releases page — latest indexed entry still Feb 09, 2026

The 2026-07-13 search-index spot-check of https://dhanhq.co/docs/v2/releases/ surfaced **no
entry newer than Feb 09, 2026** (our "v2.5"). No "v2.6"-class release found. **Absence =
Unknown** — search snippets cannot prove nothing newer exists (index-lag caveat); the page was
not directly readable from the sandbox (proxy-blocked).

### (c) Python SDK — 2.2.0 final + 2.3.0rc1 (PyPI direct fetch = Verified for the SDK facts)

- `dhanhq` **2.2.0** (final) uploaded to PyPI **2026-04-24** — supersedes the header NOTE's
  "2.2.0rc1" as the latest stable.
- `dhanhq` **2.3.0rc1** uploaded **2026-07-07**. Features per the GitHub releases page
  (Secondary): **Conditional Orders**, **Global Stocks (US)** incl. a separate live feed,
  **P&L Based Exit**, **Multi-leg Margin Calculator** (documented on the new
  docs.dhanhq.co portal at api/v2/funds/calculate-multi-margin), enhanced FullDepth callback
  handling. Global Stocks is out of tickvault scope.
- SDK versions still do NOT map to platform versions (takeaway 9 stands).

---

## 2026-07-14 Upstream Update (runner-crawled live page)

**Evidence tier: Verified-live.** Raw HTML of `https://dhanhq.co/docs/v2/releases/` (runs 1–3,
sha256 `54116c5c` content-identical, latest 2026-07-14T07:58:32Z), comment-aware.
Full manifest: `00-COVERAGE-MANIFEST.md`.

1. **The live releases page DOES use formal version headings** — "Version 2.5 / 2.4 / 2.3 /
   2.2 / 2.1 / 2" — so the header NOTE's "Dhan's releases page lists features by date without
   formal version numbers" is outdated (the version numbers are upstream, not our invention).
   All six version/date pairs above are confirmed verbatim; **the latest live entry remains
   "Version 2.5 — Monday Feb 09 2026"** — nothing newer exists on the page (the only "2026"
   string on the whole page is that date).
2. **"Version 2.5.1 — Mar 17, 2026" above is NOT a Dhan-published release** — now
   Verified-live, upgrading the 2026-07-13 §(a) provenance note: zero hits for
   MPP / "Market Price Protection" / "March" / 2.5.1 anywhere on the live page. The 2.5.1
   block is our internal mapping of SDK + regulatory-comms items; keep it, but never cite it
   as a releases-page entry. Its getIP-fields sentence (the `detectedIP`/`ipMatchStatus`/
   `ordersAllowed` claim) is re-sourced per `02-authentication.md` "2026-07-14 Upstream
   Update" §(b): those fields are WIRE-OBSERVED but doc-unbacked on both live surfaces.
3. Live-only additions worth knowing: a v2.0 "Bug Fixes" subsection (realtime
   realizedProfit/unrealizedProfit in Positions; a TARGET_LEG order-modification fix); v2.0
   notes "quantity and price fields are conditionally required for modification" and
   "pre-open AMO orders can also be placed now with PRE_OPEN tag" (tension with the SDK note
   above that PRE_OPEN was removed from SDK AMO validation — the docs + annexure still list
   PRE_OPEN).
