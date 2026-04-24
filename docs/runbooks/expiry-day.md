# Runbook: Expiry Day (Weekly/Monthly)

## Scenario
NSE F&O weekly expiry (every Thursday) or monthly expiry.

## Morning Checks (extra, beyond normal pre-market)

- [ ] Which contracts expire today? (check instrument master)
- [ ] Do you hold any expiring contracts?
- [ ] What is the auto-square-off time? (Dhan: typically 3:25 PM for F&O)
- [ ] Set a phone alarm for 3:00 PM if holding expiring positions

## During Session

- [ ] Expect higher volatility in expiry contracts
- [ ] System may see more circuit breaker events — verify not treated as disconnect
- [ ] Near-zero premium options: verify no division-by-zero in calculations
- [ ] OI data changes rapidly — verify OI parser handles extreme values

## Approaching Expiry Time

- [ ] **3:00 PM alarm**: Check all expiring positions
- [ ] If holding expiring contracts: monitor closely
- [ ] Dhan will auto-square-off unhedged positions before 3:25 PM
- [ ] Be available and watching from 2:45 PM to 3:30 PM

## After Expiry

- [ ] Expired contracts should disappear from positions
- [ ] Verify system handles gracefully (no error on missing instrument)
- [ ] Run reconciliation after 3:30 PM if you held expiring contracts

## System Requirements Verified By Tests

- NEVER-005: Expired F&O ticks must never update live positions
- System detects expired instruments from instrument master
- OMS Gate 4 (I-P0-03): Expiry check before order submission
- Test: `gap_enforcement::test_expired_contract_rejection`
- Test: `safety_layer::dhan_validation::*`

## Stock F&O Expiry Rollover (Fix #6 — 2026-04-24)

**Rule (STRICT):** For stock F&O contracts, when today is within
**1 trading day** of the nearest expiry, the subscription planner rolls
forward to the NEXT expiry. This means:

| Today | Days-to-expiry (trading) | Behaviour |
|-------|--------------------------|-----------|
| T-2 (Tue, with Thu expiry) | 2 | **Keep** current expiry |
| T-1 (Wed, with Thu expiry) | 1 | **Roll** to next expiry |
| T-0 (Thu = expiry day) | 0 | **Roll** to next expiry |

**Why:** stock F&O contracts are illiquid / non-tradeable on expiry day
(T) and the day before (T-1). Subscribing to them wastes the 25,000-slot
main-feed capacity and depth-slot budget.

**Scope — stocks only.** NIFTY / BANKNIFTY / FINNIFTY / MIDCPNIFTY
(indices) keep the nearest expiry unconditionally — they trade actively
right up to expiry seconds. The rule is applied only inside the
`UnderlyingKind::Stock` branch of the planner.

**Implementation pointers:**
- Helper: `subscription_planner::select_stock_expiry_with_rollover`
  (crates/core/src/instrument/subscription_planner.rs)
- Threshold constant: `STOCK_EXPIRY_ROLLOVER_TRADING_DAYS = 1`
- Trading-day math: `TradingCalendar::count_trading_days(from, to)`
  (crates/common/src/trading_calendar.rs) — half-open interval
  `(from, to]`, excludes weekends and NSE holidays.

**Ratchet tests:**
- `test_stock_expiry_rolls_on_t_minus_1`
- `test_stock_expiry_rolls_on_t`
- `test_stock_expiry_stays_on_t_minus_2`
- `test_index_expiry_never_rolls_via_planner`
- `test_count_trading_days_expiry_day_from_t_minus_1` / `_from_t_minus_2` / `_on_expiry_day_itself_is_zero`

**Dhan support citation:** draft support ticket
`docs/dhan-support/2026-04-24-expiry-day-non-tradeable-clarification.md`
asking Dhan to confirm the illiquidity behaviour. Once they reply, link
their confirmation here.
