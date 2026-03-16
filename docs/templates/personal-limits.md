# Personal Capital Limits — Fill Before Going Live

> Write these on physical paper. Tape to your monitor.
> Review BEFORE every trading day. Do not violate. Ever.

## Capital Allocation

| Limit | Value | Notes |
|-------|-------|-------|
| Total capital allocated to algo trading | ₹ __________ | Never more than you can afford to lose entirely |
| Absolute stop loss (entire account) | ₹ __________ | Stop algo trading entirely if account drops below this |
| Stop loss as % of starting capital | ____% | |
| Monthly loss limit | ₹ __________ | Stop for rest of month if exceeded |
| Monthly limit as % of capital | ____% | |
| Daily loss limit (personal) | ₹ __________ | Lower of personal + system limit is binding |
| Daily loss limit (system-enforced) | ₹ __________ | Configured in `config.toml` as `max_daily_loss_percent` |
| Max consecutive losing days before pause | _____ days | Pause and review if exceeded |
| Max drawdown weeks before pause | _____ weeks | Pause and review if exceeded |

## The "Never" Rules

- [ ] NEVER add more capital to recover a loss
- [ ] NEVER increase position size to "make back" losses
- [ ] NEVER remove capital protection limits "just for today"
- [ ] NEVER trade with money you cannot afford to lose
- [ ] NEVER override the system on "gut feeling"

## Pre-Market Daily Check

- [ ] Pre-market reconciliation passed
- [ ] Yesterday's P&L reviewed (once, calmly)
- [ ] Today's economic events checked (RBI, budget, GDP)
- [ ] NSE market status checked
- [ ] Today's personal loss limit set mentally

## System Configuration Reference

```toml
# These values must match your personal limits above
[risk]
max_daily_loss_percent = 2.0    # → ₹__________ at your capital level
max_position_lots = 100         # Max lots per instrument
capital = 1_000_000.0           # Your actual trading capital
```
