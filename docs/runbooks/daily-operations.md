# Daily Operations Ritual

## Before Market Open (8:30 AM - 9:10 AM IST)

- [ ] Pre-market checks complete (automated at 08:45)
- [ ] Reconciliation passed (automated)
- [ ] Check yesterday's P&L once — then close it
- [ ] Review today's economic events (RBI announcements? Budget? GDP data?)
- [ ] Check NSE market status: https://www.nseindia.com
- [ ] Set today's personal loss limit in your mind — decide now, not during loss
- [ ] Verify Grafana dashboard accessible
- [ ] Verify Telegram alerts working (startup message received)

## During Market Hours (9:15 AM - 3:30 PM IST)

- [ ] Do NOT watch P&L tick by tick
- [ ] Set Grafana dashboard to refresh every 5 minutes, not real-time
- [ ] Check alerts — react to CRITICAL only, queue HIGH for 5-minute check
- [ ] Do not add positions based on "feeling"
- [ ] Take one break between 11 AM - 12 PM away from the screen

## Market Close (3:30 PM - 4:00 PM IST)

- [ ] Review day's P&L — once, calmly
- [ ] Check QuestDB data completeness:
  ```sql
  SELECT count(), min(ts), max(ts) FROM ticks
  WHERE ts >= today()
  ```
- [ ] Review any alerts that fired during the day
- [ ] Log any manual interventions in `docs/templates/override_log.md`
- [ ] Note any anomalies for system improvement
- [ ] Shutdown checklist: system halted cleanly, Valkey persisted, disk OK

## Weekly (Sunday Evening)

- [ ] Review week's P&L vs expectations
- [ ] Review override log — patterns?
- [ ] Review alert log — too many? Too few?
- [ ] Any Dhan API changes announced?
- [ ] Any system improvements needed?
- [ ] Disk space check: `df -h` (QuestDB grows fast on volatile days)

## Warning Signs to Stop

If any of these are true, reduce position size or stop entirely:

- Checking P&L every few minutes obsessively
- Feeling anxious when the system is running
- Overriding the system more than once per week
- Losing sleep over positions
- Trading with money that affects daily life if lost
- Adding capital to "recover losses"
- Personal relationships affected by trading results
