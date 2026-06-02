# Release Notes — DhanHQ v2

> Source: https://dhanhq.co/docs/v2/releases/ · API v2 · Audited 2 Jun 2026

## Version 2.5 — 2026-02-09
**New features**
- **Conditional Trigger Orders** — place orders that fire when a market condition is met (`/conditional-trigger`).
- **P&L-based exit (Trader's Control)** — auto-close a position at a set profit/loss %.
- **Exit All API** — close all open positions and orders in one call.
- **Access-token generation via API** — when TOTP is configured, with built-in regeneration logic (`/authentication/#generate-token`).

**Improvements**
- **Option Chain API** — enhanced rate limits (multiple unique requests for different expiries/strikes, each within 3 s); added `average_price` and `security_id` fields.

## Version 2.4 — 2025-09-22
In line with SEBI guidelines on retail participation in algo trading; auth-module changes.
**New features**
- **API-key based login** — generate API key (1-year validity) and a daily access token by verifying Dhan credentials.

**Breaking changes**
- **Access token valid for 24 hours only.**
- **Static IP required** for all order APIs (place/modify/cancel — normal, super, forever).

## Version 2.3 — 2025-09-08
**New features**
- **Full Market Depth** — depth up to **200 levels** on WebSocket (extends 20-level depth).
- **Historical (expired) options data** — fetch expired-contract data on a rolling basis using ATM±n strikes; rolling IV/OI/volume.
- Order-API rate limit changed to **10 orders/sec**.

## Version 2.2 — 2025-03-07
**New features**
- **Super Orders** — combine entry + target + stop-loss (with trailing) into a single order, across all segments.
- **User Profile API** — token validity, active segments, Data-API status/validity, DDPI/MTF flags.

**Improvements**
- **Intraday Historical Data** — now **5 years** of data; added `oi` param; `fromDate`/`toDate` accept IST time.
- **Daily Historical Data** — added optional `oi` param.
- **Live Order Update** — added `CorrelationId` and `Remarks` keys.

**Breaking changes**
- **Rate-limit changes** — Data APIs: no limits on minute/hour frames; up to **100,000 requests/day**; **5/sec** on seconds frame.

## Version 2.1 — 2025-01-06
**New features**
- **20-level Market Depth** (Level-3) on WebSocket for all NSE instruments.
- **Option Chain API** — full chain (OI, greeks, volume, top bid/ask, price) in one request, across NSE/BSE/MCX.

**Improvements**
- `expiryCode` is now an optional field in Daily Historical Data.

## Version 2 — 2024-09-15
**New features**
- **Market Quote** — LTP / Quote (with OI) / Market Depth for up to 1000 instruments per call.
- **Forever Orders** — single & OCO.
- **Live Order Update** — real-time order status via WebSocket, across platforms.
- **Margin Calculator** — required margin & available balance before order placement.

**Improvements**
- **Intraday Historical Data** — OHLC + volume for last 5 trading days across 1/5/15/25/60-min.
- **GET Order APIs** — `filledQty`, `remainingQuantity`, `averageTradedPrice` added; `PART_TRADED` status flag added.
- **Live Market Feed** — query-param auth; subscribe/unsubscribe via JSON; `FULL` packet (LTP + Quote + OI + depth in one).

**Breaking changes**
- **Order placement** — deprecated `tradingSymbol`, `drvExpiryDate`, `drvOptionType`, `drvStrikePrice`; added `PRE_OPEN` AMO.
- **Order modification** — `quantity` must be the placed order quantity (not pending). `quantity`/`price` are conditionally required.
- **Daily Historical Data** — `symbol` replaced with `securityId`.
- **Error messages** — categorised into the `DH-900` series.
- **Security ID list** — comprehensive remapping with new tags.
- **Timestamps** — **Epoch/UNIX** time (key `timestamp`) instead of Julian, in Historical Data APIs.
- **`Market Depth` mode deprecated** in Live Market Feed — replaced by the `FULL` packet.
- **Endpoint changes** — Trade History → `/trades`; Kill Switch → `killswitch`.

**Bug fixes**
- `realizedProfit` & `unrealizedProfit` now real-time in Positions API.
- `TARGET_LEG` modification fixed in Order Modification API.
