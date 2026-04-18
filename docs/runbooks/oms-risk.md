# Runbook — OMS / Risk failures

**When it fires:** `OMS-GAP-01..06`, `RISK-GAP-01..03`,
`CircuitBreakerOpen`, `HighOrderLatency`, `HighOrderRejectionRate`,
`RateLimiterDenying`, `DailyPnlWarning`, `DailyPnlCritical`,
`DH-904/905/906`.

**Consequence if not resolved:** trading halts (auto) OR silent
incorrect P&L. Both are financially material.

## The five OMS invariants

These five facts are enforced mechanically — never bypass them:

1. **SEBI rate limit: 10 orders/sec max.** GCRA algorithm in
   `rate_limiter.rs`. Exceeding = regulatory violation.
2. **Circuit breaker: 3-state FSM.** Opens after N consecutive
   failures. **NEVER auto-reset.** Operator-only.
3. **Idempotency: UUID-v4 correlation IDs.** Every order has one.
   Duplicate submit without unique ID = double financial exposure.
4. **Dry-run mode: default = true.** Live orders require explicit
   `dry_run = false` in config. Safety invariant.
5. **State machine: 10 valid transitions (out of 26 target).** Bad
   transition attempts are rejected — never force.

## Telegram alert → action (60-second version)

| Alert | First action |
|---|---|
| 🔴 `CircuitBreakerOpen` | **Do not reset.** See "Circuit recovery" below. |
| 🔴 `RateLimiterDenying` | Regulatory risk. Check strategy config — something is spamming orders. |
| 🟠 `HighOrderLatency` (p99 > 500ms) | Check Dhan status + network + `tv-trading-flow` dashboard. |
| 🟠 `HighOrderRejectionRate` | Inspect recent rejections via `make errors-summary` — typically DH-905 (bad payload). |
| 🟡 `DailyPnlWarning` (-₹15K) | Heads up — risk engine is watching. 3 more ticks toward -₹18K and we halt. |
| 🔴 `DailyPnlCritical` (-₹18K) | Auto-halt triggered. See "Daily P&L recovery" below. |
| 🔴 `DH-904` rate-limit burst | App is in exponential backoff (10→20→40→80s). If not resolving, manual halt. |
| 🔴 `DH-905/906` | Never retry. Payload/order is wrong. Check recent strategy deploys. |

## Root-cause checklist

### 1. Is the OMS actually processing orders?

```bash
curl -sS http://localhost:9091/metrics | grep -E "tv_orders_placed_total|tv_oms_dry_run|tv_circuit_breaker_state"
```

Expected during live trading:
- `tv_oms_dry_run = 0` (LIVE mode)
- `tv_circuit_breaker_state = 0` (CLOSED, healthy)
- `tv_orders_placed_total` incrementing on signal fires

### 2. Rate limiter state

```bash
curl -sS http://localhost:9091/metrics | grep -E "tv_rate_limiter_(allowed|denied)_total"
```

`denied` > 0 = regulatory risk. Inspect immediately. Most likely
causes: runaway strategy, bot-like signal, or a replay injecting
old signals.

### 3. Reconciliation with Dhan

```bash
# Recent reconciliation mismatches
curl -sS http://localhost:9091/metrics | grep tv_oms_reconciliation_mismatches_total
# Expected: 0. Non-zero = local order book != Dhan order book.
```

Any mismatch is **critical** — you're trading with wrong state.
Halt + manual reconciliation via Dhan UI.

### 4. Risk engine state

```bash
curl -sS http://localhost:9091/metrics | grep -E "tv_risk_halted|tv_realized_pnl|tv_unrealized_pnl|tv_position_count"
```

`tv_risk_halted = 1`: halt triggered. Reasons:
- `tv_realized_pnl + tv_unrealized_pnl <= -daily_loss_threshold` →
  daily-loss auto-halt per RISK-GAP-01
- Position lot count > `max_lots` → pre-trade rejection
- Manual halt via `POST /api/risk/halt`

## Recovery

### CircuitBreakerOpen (OMS-GAP-03)

The circuit breaker opens after N consecutive order failures. Common
triggers:

- Network partition to Dhan (→ fix network)
- Token expired (→ see auth.md runbook)
- Rate limit cascade (→ see DH-904 below)
- Payload bug (→ check recent strategy deploys)

```bash
# DO NOT manually reset. The FSM provides a Half-Open state that
# automatically probes when `reset_timeout` elapses. Wait.
curl -sS http://localhost:9091/metrics | grep tv_circuit_breaker_state
# 1 = Open, 2 = Half-Open (probing), 0 = Closed (recovered)

# If stuck Open for > 10 min, investigate WHY — the breaker is
# protecting you from a real issue. Fix that, then the next probe
# closes the circuit.
```

### Daily P&L recovery

```bash
# 1. Inspect the losing positions
curl -sS http://localhost:9091/metrics | grep tv_unrealized_pnl

# 2. Decide: hold until EoD, or close early?
# - If intent is intraday: `POST /api/positions/exit-all` (exits open
#   positions AND cancels pending orders, per Dhan Ticket semantics)
# - If intent was swing: accept the mark-to-market, risk halt stays.

# 3. The halt persists until daily reset at 08:45 IST next trading day.
#    Manual override: `POST /api/risk/reset` (requires explicit confirmation).
```

### DH-904 rate-limit burst

The app auto-applies exponential backoff: 10s → 20s → 40s → 80s.
If 4 consecutive failures hit the 80s cap:

```bash
# 1. Check order volume
curl -sS http://localhost:9091/metrics | grep tv_orders_placed_total
# If the rate is > 10/sec, the strategy is mis-configured.

# 2. Halt trading
curl -X POST http://localhost:3001/api/trading/halt

# 3. Review strategy config for runaway signals.
```

### HighOrderRejectionRate

```bash
# Inspect rejection reasons
mcp__tickvault-logs__tail_errors --limit 20 --code DH-905
mcp__tickvault-logs__tail_errors --limit 20 --code DH-906

# Common fixes:
# - Wrong `quantity` on modify (must be TOTAL, not delta)
# - Invalid `securityId` (hardcoded instead of looked up)
# - Order placed before 09:15 IST (pre-market window violation)
# - STOP_LOSS_MARKET with missing triggerPrice
```

## Never do these

- **Never manually reset the circuit breaker** without fixing root cause.
- **Never retry a DH-905 or DH-906.** The payload is wrong; retrying
  won't fix it. Fix the code/config first.
- **Never bypass the rate limiter** for "just this one order". SEBI
  audit trails every order; regulatory fines compound.
- **Never reuse a correlation ID.** Idempotency protects against
  double-submission; reuse breaks it.
- **Never flip `dry_run = false` without a 2-person review.** The
  default-true invariant exists specifically to catch paper-mode
  regressions before they go live.

## Preventive measures

1. **Daily pre-market reconciliation** — 08:45 IST boot check ensures
   local state matches Dhan. Runs automatically.
2. **Weekly strategy shadow-run** — deploy new strategies in dry-run
   for a full session before flipping to live.
3. **Monthly risk limits review** — re-tune daily-loss threshold,
   max_lots, strategy-specific caps based on prior month's P&L.
4. **Pre-deploy checklist** — before any OMS code change:
   - Does it alter the idempotency scheme? → full trace review
   - Does it change correlation-ID generation? → regression test
   - Does it touch the rate limiter? → GCRA property test

## Related files

- `.claude/rules/dhan/orders.md` — Dhan order API spec
- `.claude/rules/dhan/super-order.md` — SL/TGT/trailing legs
- `.claude/rules/dhan/forever-order.md` — GTT / OCO
- `.claude/rules/project/gap-enforcement.md` — OMS + risk gap specs
- `crates/trading/src/oms/` — all OMS code
- `crates/trading/src/risk/engine.rs` — pre-trade + auto-halt
- `deploy/docker/grafana/dashboards/trading-flow.json` — live dashboard
- `scripts/auto-fix-refresh-instruments.sh` — for DATA-813 after instrument master rotates
