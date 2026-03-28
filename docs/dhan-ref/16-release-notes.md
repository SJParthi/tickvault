# Dhan V2 Release Notes — Complete Reference

> **Source**: https://dhanhq.co/docs/v2/releases/
> **Extracted**: 2026-03-13
> **Purpose**: Track breaking changes and new features across versions
> **NOTE**: Version labels (2.0–2.5) are our internal mapping of Dhan's dated releases for easy reference. Dhan's releases page lists features by date without formal version numbers. The Python SDK (`dhanhq` on PyPI) has its own versioning (2.0.0, 2.1.0, 2.2.0rc1).

---

## Version 2.5.1 — Mar 17, 2026

### Improvements
- **Market Order conversion**: Effective March 21 — market orders via API will be converted to limit orders with Market Protection Percentage (MPP). This is a Dhan/exchange-side conversion; API behavior unchanged.
- **Order rate limits**: Reduced to 10/sec (already enforced since v2.3).

### Breaking Changes
- **Static IP mandatory for ALL API orders**: Effective April 1 — all API orders (place/modify/cancel) must come from a whitelisted static IP. Already required since v2.4 but now strictly enforced by exchange.

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

## Key Takeaways for dhan-live-trader

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
