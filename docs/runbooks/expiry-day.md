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

## Stock F&O Expiry Rollover (Fix #6 — 2026-04-24; narrowed to T-only 2026-04-28)

**Rule (STRICT T-only since 2026-04-28):** For stock F&O contracts, the
subscription planner rolls forward to the NEXT expiry ONLY when today IS
the expiry day itself (0 trading days remaining):

| Today | Days-to-expiry (trading) | Behaviour |
|-------|--------------------------|-----------|
| T-2 (Tue, with Thu expiry) | 2 | **Keep** current expiry |
| T-1 (Wed, with Thu expiry) | 1 | **Keep** current expiry (was: rolled, until 2026-04-28) |
| T-0 (Thu = expiry day) | 0 | **Roll** to next expiry |

**Why narrowed (2026-04-28):** Dhan only blocks stock F&O trading on the
expiry day itself (T-0). T-1 contracts ARE tradeable on Dhan; subscribing
to them at T-1 is correct and gives one extra trading day of liquidity
that the previous T-or-T-1 rule was prematurely dropping.

**Scope — stocks only.** NIFTY / BANKNIFTY / SENSEX (indices) keep the
nearest expiry unconditionally — they trade actively right up to expiry
seconds. The rule is applied only inside the `UnderlyingKind::Stock`
branch of the planner.

**Applied at two subscription points (both go through the same helper):**
1. **Boot-time stock F&O subscribe** (Phase 1 before 09:00 IST)
2. **09:13 IST Phase 2 dispatch** (with finalized 09:12 close prices)

**Implementation pointers:**
- Helper: `subscription_planner::select_stock_expiry_with_rollover`
  (crates/core/src/instrument/subscription_planner.rs)
- Threshold constant: `STOCK_EXPIRY_ROLLOVER_TRADING_DAYS = 0` (was 1
  until 2026-04-28)
- Trading-day math: `TradingCalendar::count_trading_days(from, to)`
  (crates/common/src/trading_calendar.rs) — half-open interval
  `(from, to]`, excludes weekends and NSE holidays.

**Ratchet tests (UPDATED 2026-04-28):**
- `test_stock_expiry_rollover_constant_is_zero` (NEW — locks constant at 0)
- `test_stock_expiry_keeps_nearest_on_t_minus_1` (REWRITTEN — asserts KEEP)
- `test_stock_expiry_rolls_only_on_t_zero` (RENAMED from `_rolls_on_t`)
- `test_stock_expiry_stays_on_t_minus_2`
- `test_stock_expiry_no_next_keeps_nearest_on_t_zero` (REWRITTEN — uses T-0)
- `test_stock_rollover_applies_to_both_optstk_and_futstk` (UPDATED — uses T-0)
- `test_index_expiry_never_rolls_via_planner`
- `test_count_trading_days_expiry_day_from_t_minus_1` / `_from_t_minus_2` / `_on_expiry_day_itself_is_zero`

**Dhan support citation:** draft support ticket
`docs/dhan-support/2026-04-24-expiry-day-non-tradeable-clarification.md`
asking Dhan to confirm the illiquidity behaviour. Once they reply, link
their confirmation here.
