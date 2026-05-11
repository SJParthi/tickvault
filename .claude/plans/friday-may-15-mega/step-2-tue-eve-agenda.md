# Step 2 — Tue Eve Agenda (pre-loaded Mon 21:35 IST)

**Topic:** Capital Allocation + Risk Engine
**When:** Tue 2026-05-12 evening IST
**Status:** PRE-LOADED — operator can read offline before Tue discussion
**Prereq:** Step 1 wrapped Mon eve (`step-1-discussion-log-mon-eve.md`)

---

## Why this step matters

Phase 0.5 (Mon) hardens the data pipeline. Step 2 (Tue) decides the MONEY rules:
- How much can we LOSE per day before we kill switch?
- How big can ONE trade be?
- What conditions trigger immediate exit-all?
- How does the risk engine VETO a strategy signal before it becomes an order?

These rules are binding for life. Setting them too tight = no trades fire. Too loose = blown account.

---

## Auto-driver / dumb-kid framing

> "Sir, before you give your car driver the key, you tell him 3 things:
> 1. 'You can refuel up to ₹500/day. After that, walk back.' → DAILY LOSS BUDGET
> 2. 'Don't go faster than 80 km/hr. Ever.' → PER-TRADE SIZE CAP
> 3. 'If the brakes feel weird, STOP — don't continue to destination.' → KILL SWITCH
>
> Same three rules for the trading bot. Hardcoded. Mechanically enforced. Operator cannot override mid-day (you'll get emotional and lift the cap when losing — common mistake). Once set, it's a wall."

---

## Pre-loaded questions (operator answers Tue eve)

### Q1 — Daily loss budget (₹/day max)

| Option | Daily loss cap | Trades/day implied | Comment |
|---|---|---|---|
| A | ₹500 | ~1-2 small | Ultra-conservative; education-mode |
| B | ₹2,000 | ~3-5 | Conservative |
| C | ₹5,000 | ~5-10 | Moderate |
| D | ₹10,000 | ~10-20 | Aggressive (need ≥₹2-3 lakh capital) |
| E | "different — ask me X" | — | — |

**Implementation cost:** ~0 LoC — `crates/trading/src/risk/pnl_tracker.rs` already exists. We pin the constant.
**Where it goes:** `config/base.toml::[risk] max_daily_loss_inr = ...`
**Charter check:** 7-row matrix ✓ (daily-loss-cap is a kill-switch trigger; resilience envelope holds)

### Q2 — Per-trade position size cap (% of capital OR ₹ max)

| Option | Cap | Comment |
|---|---|---|
| A | 1% of capital per trade | Standard retail rule |
| B | 2% per trade | Slightly aggressive |
| C | ₹fixed per trade (e.g., ₹2,500 risk) | Risk-based, not %-based |
| D | "different" | — |

**Implementation cost:** ~50 LoC — pre-trade size calculator + reject if exceeds.
**Where it goes:** `crates/trading/src/risk/pretrade_check.rs` (already exists; needs cap wired).

### Q3 — Kill-switch auto-trigger conditions (multi-select)

| Trigger | Default | Cost to implement |
|---|---|---|
| Daily loss > budget | ✅ MANDATORY | Existing |
| 3 consecutive losing trades | OPTIONAL | ~30 LoC counter + threshold check |
| Tick gap > 30s during market hours | ✅ MANDATORY (Wave 2 WS-GAP-06) | Existing |
| QuestDB disconnect > 60s | ✅ MANDATORY (`BOOT-01`) | Existing |
| Token expiry < 4h headroom | ✅ MANDATORY (`AUTH-GAP-03`) | Existing |
| SLO score < 0.80 | ✅ MANDATORY (`SLO-02` Critical) | Existing |
| Operator Telegram bot `/kill` command | OPTIONAL | ~80 LoC bot command handler |
| NSE bhavcopy cross-check FAILED | OPTIONAL | Existing audit, kill-trigger new |

### Q4 — P&L auto-exit thresholds (per-day)

Per `dhan-ref/15-traders-control.md` + `traders-control.md` rule:
- **Profit lock:** if intraday profit reaches ₹X → auto-exit all + kill switch for the day
- **Loss stop:** if intraday loss reaches ₹Y → auto-exit all + kill switch for the day

| Option | Profit lock (₹) | Loss stop (₹) | Reasoning |
|---|---|---|---|
| A | ₹1,500 | ₹500 | 3:1 reward:risk |
| B | ₹3,000 | ₹1,000 | 3:1 at higher scale |
| C | "different" | "different" | — |

**Note:** Dhan WARNS if profit threshold < current profit OR loss threshold < current loss → exit fires IMMEDIATELY. Always set before market open.

### Q5 — Max concurrent positions

| Option | Max | Reason |
|---|---|---|
| A | 1 (one trade at a time) | Ultra-simple; clear mental model |
| B | 3 (up to 3 open) | Allows scaling in/hedging |
| C | 5 | Aggressive |

### Q6 — Margin pre-trade check policy

Per `funds-margin.md`:
- Call `POST /v2/margincalculator` BEFORE every order placement
- If `insufficientBalance > 0` → reject locally, don't send to Dhan
- This avoids wasting the 10/sec rate limit on rejected orders

| Option | Policy | Cost |
|---|---|---|
| A | Strict — every order pre-calc'd | ~100 LoC + 1 extra API call per order |
| B | Loose — only if first attempt rejects | 0 LoC; Dhan does the work |
| C | Hybrid — pre-calc large orders only (>₹50K margin) | ~150 LoC |

### Q7 — Halt conditions (auto + manual)

| Condition | Action | Default |
|---|---|---|
| Daily loss hit | Kill switch + exit-all | ✅ AUTO |
| Tick gap > 30s | Pause strategy (no new orders) | ✅ AUTO |
| Operator `/halt` Telegram cmd | Pause + exit-all + Telegram confirm | OPTIONAL |
| Pre-market readiness FAILED | Don't start trading; Telegram CRITICAL | ✅ AUTO (Phase 0.5 Item 0.5.10) |
| Token within 1h of expiry | Pause new orders; let positions ride | ✅ AUTO |

### Q8 — Recovery after halt

| Option | Policy |
|---|---|
| A | Halt = trading is OVER for the day. Resume tomorrow only. |
| B | Halt = pause 10 min, retry once. If clean → resume. If dirty → done for day. |
| C | Manual operator `/resume` Telegram cmd to come back |

---

## Charter check per item (15-row + 7-row matrix)

Every Step 2 decision MUST pass the matrix per `per-wave-guarantee-matrix.md`. Pre-checks:

| Demand | How Step 2 satisfies |
|---|---|
| 100% code coverage | `pnl_tracker.rs` + `pretrade_check.rs` already at 100% threshold |
| 100% audit coverage | Every risk decision writes to `order_audit` (SEBI 5y retention) |
| 100% testing | Unit + property + chaos `chaos_kill_switch_during_market.rs` (NEW Wed) |
| 100% monitoring | Prom: `tv_daily_pnl`, `tv_kill_switch_active`, `tv_position_count` |
| 100% logging | Every risk veto: `error!` with `code = RISK-GAP-XX` |
| 100% alerting | Kill switch fires → Telegram CRITICAL |
| Zero ticks lost | Risk engine reads from RAM cache (no DB on hot path) |
| O(1) latency | Pre-trade check ≤ 50ns; bench-gated |
| Uniqueness + dedup | Order idempotency via UUID v4 (OMS-GAP-05) |
| Real-time proof | `mcp__tickvault-logs__prometheus_query "tv_daily_pnl"` |

---

## Reference rules to load before Tue discussion

- `.claude/rules/dhan/traders-control.md` (kill switch + P&L exit endpoints)
- `.claude/rules/dhan/funds-margin.md` (margin calc + fundlimit)
- `.claude/rules/dhan/super-order.md` (target/SL legs)
- `.claude/rules/dhan/orders.md` (rate limits + idempotency)
- `crates/trading/src/risk/*` (existing risk engine code)

---

## Expected Tue eve output

1. Q1–Q8 picks locked in `00-decisions-log.md`
2. New file `step-2-discussion-log-tue-eve.md` written
3. `config/base.toml::[risk]` values pinned (no code change, just config)
4. Wed agenda updated if any Step 2 items spill over

**Estimated Tue eve duration:** 30-60 min (1 FAANG meeting style)

---

## What this file is for

Future Tue eve session opens cold → reads INDEX → reads this file → already knows the 8 questions to ask the operator → no warm-up overhead → straight to discussion.

**Token budget Tue eve:** ≤ 30% of available 5-hour limit (operator wants buffer for Wed adversarial review + APPROVE flip).
