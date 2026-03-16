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
