# Implementation Plan: Live Market Dashboard (Dhan web.dhan.co Clone)

**Status:** DRAFT
**Date:** 2026-04-07
**Approved by:** pending

## Summary

Build a full live market dashboard webpage at `/portal/market-dashboard` that replicates
Dhan's web.dhan.co market views — showing Index tickers, Stock Price Movers (Gainers/Losers),
Option Movers (OI/Volume/Price), Index Dashboard (all NSE indices OHLC), and full Options Chain
with Greeks. All data from our live tick pipeline + QuestDB. Dark theme, auto-refreshing,
mobile-responsive, AWS-deployable with auth.

## Data Sources (Already Available)

| Data | Source | API |
|------|--------|-----|
| Index LTP/Change/OHLC | `ticks` table (segment=IDX_I) | NEW: `/api/market/indices` |
| Stock LTP/Change/Volume | In-memory `TopMoversSnapshot` | EXISTS: `/api/top-movers` (needs symbol names) |
| Options Chain + Greeks | `option_greeks` + `dhan_option_chain_raw` | EXISTS: `/api/option-chain` |
| PCR | QuestDB `pcr_snapshots` | EXISTS: `/api/pcr` |
| Previous Close | `previous_close` table | Already used by tick processor |
| Option Movers | `option_movers` table in QuestDB | NEW: `/api/market/option-movers` |

## Plan Items

- [ ] Item 1: Add symbol name lookup to TopMoversResponse (MoverEntry currently has only security_id)
  - Files: crates/core/src/pipeline/top_movers.rs, crates/api/src/handlers/top_movers.rs
  - Change: Add `symbol: String` field to MoverEntry, lookup from instrument registry
  - Tests: test_mover_entry_includes_symbol

- [ ] Item 2: Add `/api/market/indices` endpoint — live index data from ticks
  - Files: crates/api/src/handlers/market_data.rs (new), crates/api/src/lib.rs
  - Returns: Array of {name, ltp, change, change_pct, open, high, low, prev_close}
  - Source: QuestDB query on `ticks` for IDX_I segment + `previous_close`
  - Tests: test_indices_response_format, test_indices_empty_when_no_data

- [ ] Item 3: Add `/api/market/option-movers` endpoint — top options by OI/Volume/Price change
  - Files: crates/api/src/handlers/market_data.rs, crates/api/src/lib.rs
  - Returns: {highest_oi, oi_gainers, oi_losers, top_volume, price_gainers, price_losers}
  - Source: QuestDB query on `option_movers` table
  - Tests: test_option_movers_response_format

- [ ] Item 4: Build `/portal/market-dashboard` HTML page with all 5 tabs
  - Files: crates/api/static/market-dashboard.html (new), crates/api/src/handlers/static_file.rs, crates/api/src/lib.rs
  - Tabs: Stocks | Options | Index | (using existing /api/option-chain for Options Chain)
  - Features: Index ticker bar (top), auto-refresh 3s, dark theme, mobile responsive
  - Tests: test_market_dashboard_html_not_empty, test_market_dashboard_handler_returns_ok

- [ ] Item 5: Register new routes and test
  - Files: crates/api/src/lib.rs
  - Tests: route registration test

- [ ] Item 6: Build, clippy, fmt, test
  - Files: n/a
  - Tests: cargo build + cargo test --workspace

## Page Layout (matching Dhan web.dhan.co)

```
┌─────────────────────────────────────────────────────────────────┐
│ NIFTY 22,819 -148 (-0.65%) │ BANKNIFTY 52,029 -579 (-1.10%)  │
│ FINNIFTY 24,390 -213       │ VIX 25.79 +0.32 │ MIDCAP 12,455 │
├─────────────────────────────────────────────────────────────────┤
│ [Stocks] [Options] [Index]                                      │
├─────────────────────────────────────────────────────────────────┤
│ STOCKS TAB:                                                     │
│ [Gainers] [Losers] [Most Active] [F&O Stocks]                  │
│ Name         LTP      Change   Change%  Value(Cr)  Volume      │
│ HDFC Bank    766.00   -5.00    -0.65%   xxx        xxx         │
│ ...                                                             │
├─────────────────────────────────────────────────────────────────┤
│ OPTIONS TAB:                                                    │
│ [Highest OI] [OI Gainers] [OI Losers] [Top Volume] [Price +/-] │
│ Name                Spot    LTP   Change  Change%  OI    Volume │
│ BANKNIFTY 28APR ... 52,055  630   -215    -25.47%  92640  ...  │
├─────────────────────────────────────────────────────────────────┤
│ INDEX TAB:                                                      │
│ [Global] [NSE] [BSE]                                           │
│ Name          LTP      Change   Change%  Open     High    Low  │
│ Nifty 50      22,819   -148     -0.65%   22,838   22,843  ... │
│ Nifty Bank    52,029   -579     -1.10%   52,258   52,290  ... │
│ ...                                                             │
└─────────────────────────────────────────────────────────────────┘
```

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Market open 9:15+ | All tabs populated with live data, auto-refreshing every 3s |
| 2 | Pre-market 9:00-9:15 | Index bar shows pre-open prices, movers may be empty |
| 3 | Post-market 15:30+ | Last known data displayed, marked as "Market Closed" |
| 4 | Mobile browser | Responsive layout, horizontal scroll on tables |
| 5 | AWS deployment | Accessible via Traefik reverse proxy with basic auth |
