# Market Hours & Data Collection — Full Reference

> Rules summary: `.claude/rules/market-hours.md` (auto-loaded).
> This doc is for deep implementation detail only — SEBI compliance, instrument build window, candle counts.

## Market Structure

```
Market:           NSE (National Stock Exchange of India)
Segment:          F&O (Futures & Options)
Timezone:         ALWAYS Asia/Kolkata. Never UTC in user-facing code.
Holiday calendar: NSE holidays JSON — updated annually, checked before every trading day
Settlement:       T+1 for equity, same day for index options (European)
```

## Data Collection Window (save ALL ticks to QuestDB)

```
9:00 AM → 4:00 PM IST

  9:00 - 9:08   Pre-open order collection (exchange accepts orders)
  9:08 - 9:12   Pre-open order matching (price discovery)
  9:12 - 9:15   Buffer period (transition to continuous trading)
  9:15 - 15:30  Continuous trading — LIVE MARKET
  15:30 - 16:00 Post-market / closing session + settlement
```

## Live Trading Window (strategies execute orders)

```
9:15 AM → 3:29 PM IST

  - Start: First candle forms at 9:15:00
  - Stop:  3:29 PM — 1 minute BEFORE market close
  - NEVER submit orders at or after 3:30 PM
  - Cancel all open orders at 3:29 PM
```

## Chart Display Window

```
9:00 AM → 4:00 PM IST — full picture with pre/post market reference
```

## 1-Minute Candles

```
Trading candles:   375 per day (9:15 → 15:30)
Reference candles: Pre-market (9:00-9:15) + Post-market (15:30-16:00)
                   Stored separately, NOT used for trading signals
                   Useful for: gap analysis, opening range, settlement reference
```

## Instrument Build Window (daily, BEFORE market opens)

```
8:30 AM IST → Server starts, all Docker containers health-checked
8:30 AM IST → CSV download + universe build (3-10 seconds expected)
8:31 AM IST → System READY with instruments loaded
8:45 AM IST → DEADLINE — if not ready, SEV-1 alert fires
9:00 AM IST → Pre-market data collection begins (instruments MUST be loaded)

FAILURE PROTOCOL:
  CSV download fails (primary + fallback + cache ALL exhausted):
    → INSTANT Telegram alert — no waiting, no silent failure
    → System cannot start instrument-dependent operations
    → Total worst-case retry window: ~30 seconds before alert

  Build/validation fails after CSV obtained:
    → INSTANT Telegram alert with error details
    → Previous day's in-memory universe NOT usable (stale contracts)
```

## SEBI Compliance Requirements

Enforced when deploying to AWS for real money:

- Server must be physically located in India (AWS ap-south-1 Mumbai)
- Static IP mandatory (Elastic IP) for audit trail
- 2FA mandatory for every API session (totp-rs)
- Rate limit: 10 orders per second maximum (governor GCRA)
- All orders must have audit trail — every state transition logged
- Order audit logs must be retained for minimum 5 years

**Note:** During MacBook development (March–June 2026), SEBI server-location rules don't apply since no real orders are placed. But ALL other rules (2FA, rate limiting, audit logging, data retention) are enforced from day one.
