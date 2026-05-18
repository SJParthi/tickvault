# Gap Inventory — What's STILL Missing or Much Needed

> **Status:** GAP AUDIT (no code shipped).
> **Authority:** CLAUDE.md > operator-charter-forever.md > this file.
> **Created:** 2026-05-18 in response to operator: *"what else is missing or much needed dude?"*
> **Purpose:** honest pre-bootstrap inventory of design gaps + priority ranking.

---

## §0. Auto-driver one-liner

> "Sir, we've designed the shop's roof + walls + electricity + alarm system. Still missing: (1) what the worker actually DOES every minute (strategy logic), (2) the tool that finds winning strategies (BRUTEX), (3) the safety brake (risk engine + kill switch), (4) the cash register (order placement), (5) how you code from your Mac and ship safely to AWS. Plus a few small items: AWS bill alarm, root account MFA, backup plan if something goes wrong."

---

## §1. CRITICAL gaps — must address BEFORE bootstrap

These are the gaps that BLOCK production readiness. They are NOT covered in any doc yet.

| # | Gap | Why critical | Estimated effort |
|---|---|---|---|
| 1 | **Strategy execution design** — what is a strategy, FSM, signal format, evaluation cycle | Strategy is the WHOLE point of the system; no doc defines this | 1 design doc (~300 LoC) + 1 reference strategy implementation |
| 2 | **BRUTEX backtesting tool** — operator mentioned 8+ times but is UNDEFINED | Operator says BRUTEX selects winning strategies; we don't know what BRUTEX is or where it lives | Operator clarification + design doc |
| 3 | **Risk engine + kill switch** — pre-trade checks, daily loss, position limit, halt logic | Cannot trade live without it (regulatory + capital protection) | 1 design doc + 1 PR |
| 4 | **Order placement (OMS) chain wiring** — `OmsEngine::place_order` has ZERO production call sites today (audit from earlier session) | Strategy emits signals into the void today | Production wiring PR ("Phase 1.5 wiring") |
| 5 | **Local Mac dev environment** — operator codes on Mac in IntelliJ; what's the actual workflow? | Operator stated need: "test deploy from IntelliJ without breaking live" | 1 runbook + IDE setup guide |

---

## §2. IMPORTANT gaps — address in first PR sequence

These don't block bootstrap but should land within the first 2 weeks of operation.

| # | Gap | Why important |
|---|---|---|
| 6 | **GitHub Actions deploy workflow** (`.github/workflows/deploy.yml`) — designed in operator-end-to-end-automation.md §7 but not written | Required for IntelliJ-direct deploy |
| 7 | **AWS billing alarm** — set at ₹1,500/mo to catch runaway costs | Catches misconfiguration before bill arrives |
| 8 | **Root account MFA + IAM hardening** — operator's AWS account security | Mandatory for any account holding live trading credentials |
| 9 | **Backup + DR plan** — what if EBS gp3 corrupts? S3 lifecycle hits a wall? Account locked? | Operator-charter §F demands honest envelope on each |
| 10 | **DLT-SMS + Telegram + Amazon Connect setup checklists** | Operator manual prerequisite (cannot be automated by Claude) |
| 11 | **Live position tracking** — pull from Dhan `/v2/positions` every N seconds? Or track internally from order updates? | Risk engine needs to know what's open |
| 12 | **P&L tracking** — real-time + daily cumulative + EOD digest computation | Already mentioned in 4 daily Telegrams; not yet specified |
| 13 | **Strategy parameter management** — config hot-reload, versioning, A/B testing path | Operator demand: dynamic + scalable |
| 14 | **Indicator parameter list** — RSI period? MACD 12/26/9? BB stdev? Per strategy? | yata supports configurable params; need locked defaults |
| 15 | **Holiday calendar validation** — NSE/BSE holiday list 2026 onwards in `trading_calendar.rs` | Currently embedded; needs verification against authoritative source |
| 16 | **Mac docker-compose parity test** — proof Mac dev = AWS prod exactly | `aws-budget.md` rule 10 demands; no ratchet test exists yet |

---

## §3. NICE-TO-HAVE gaps — defer to post-go-live

These improve operations but aren't strictly required for trading.

| # | Gap | Why nice |
|---|---|---|
| 17 | **Tax statement mirroring** — Dhan provides statements; mirror for income tax filing | Operator may want come March 2027 |
| 18 | **Strategy paper-trading mode** | `dry_run = true` default already locked; need actual paper-trade tracking |
| 19 | **Daily log → Claude MCP server AWS extension** — already designed in §8 of automation runbook, needs implementation | Improves daily ops |
| 20 | **Strategy enable/disable runtime control via Telegram bot command** | Operator-charter scalable demand |
| 21 | **AWS Compute Optimizer enabled** | Free; validates t4g.medium choice after 14 days |
| 22 | **AWS Health Dashboard → Telegram webhook** | Region-wide outage detection |
| 23 | **CloudWatch dashboard JSON published** | Single-page operator view |
| 24 | **Mutation testing weekly cron + cargo-fuzz** | Already in CI design; verify still wired post-slim-down |
| 25 | **Cross-region SNS replication for ap-south-1 outage** | LOCK-7 deferred earlier |

---

## §4. The BIG UNKNOWN — BRUTEX

Operator mentioned BRUTEX 8+ times across this session as the source of strategy winners. **I have NO IDEA what BRUTEX is.** Need operator clarification on:

| Question | Why I need it |
|---|---|
| Is BRUTEX a separate codebase? Or a module inside tickvault? | Determines repo location |
| Is BRUTEX live (running backtests now) or aspirational (built later)? | Determines plan sequencing |
| What inputs does BRUTEX take? Historical candles? Tick replays? | Determines data interface |
| What outputs does BRUTEX produce? Strategy parameter sets? Code? Configs? | Determines integration format |
| Where does BRUTEX run? Same instance? Separate? Local Mac? | Determines AWS architecture |
| Does BRUTEX need access to tickvault QuestDB? | Determines data sharing pattern |

**Until BRUTEX is defined, the "after backtesting we'll run those strategies" plan has a blocked dependency.** Operator must clarify.

---

## §5. The strategy execution design void (gap #1)

Today we have:
- Indicator engine (yata-based, O(1) per update) — EXISTS
- Strategy crate skeleton — EXISTS
- `Signal` type emit — EXISTS
- Strategy emitting signals into void (no consumer) — EXISTS (audit finding)
- **What a strategy actually DOES per minute** — UNDEFINED

Open questions for operator:

| Question | Possible answers |
|---|---|
| Per-minute or per-tick evaluation? | Per-minute on 1m close is typical; some strategies tick-by-tick |
| One signal per strategy per cycle, or multiple? | Standard: one BUY/SELL/HOLD per (strategy, instrument, cycle) |
| Signal carries strike/expiry, or just direction? | For options trading: must carry strike + expiry + side (CE/PE) + qty |
| Stop-loss + target: in signal or separate? | Typically a "super order" carries all 3 legs |
| Position sizing: fixed lots or dynamic? | Operator decides — risk parameter |
| Max concurrent positions per strategy? | Risk engine enforces |
| Strategy lifecycle: long-running task or fire-and-forget on each candle? | Long-running with FSM is cleaner |

**Need 1-2 hour design discussion with operator OR operator decisions on each row.**

---

## §6. The risk engine void (gap #3)

Today: `RiskEngine` library exists, 0 callers.

Must be designed before any live order:

| Pre-trade check | Status |
|---|---|
| Daily loss threshold (₹X) reached → halt all new orders | Constant exists, no enforcement wiring |
| Daily orders count threshold (Dhan 7000/day limit per `dhan-api-introduction.md`) | Not tracked |
| Position size limit per instrument | Not enforced |
| Margin available check before order | Code exists, not wired to strategy → OMS flow |
| Kill switch (manual + auto) state propagation | `dhan/traders-control.md` documents API, not integrated |
| P&L exit (auto-square-off at threshold) | API documented, not integrated |
| Tick gap detector causes risk halt | Detection exists (RISK-GAP-03), action not wired |

---

## §7. The order placement void (gap #4) — earlier audit finding

The PRIOR session's audit identified: `OmsEngine::place_order` has **ZERO production call sites in crates/app/**. Strategy emits Signal into the void. RiskEngine library has 0 callers. Kill switch method exists but unwired.

This is the "Phase 1.5 wiring" milestone with 11 items. Stays the BIGGEST blocker between "tickvault subscribes to ticks" and "tickvault actually trades."

| Wiring item | What needs to connect |
|---|---|
| 1 | Strategy `Signal` → Risk pre-check → OMS order construction |
| 2 | OMS state machine 10/26 transitions implemented |
| 3 | Order update WS events → strategy notification |
| 4 | Position tracker hooked to order_audit |
| 5 | Kill switch state shared between Risk + OMS + Strategy |
| 6 | Daily loss aggregator |
| 7 | Idempotency UUID per strategy decision |
| 8 | Rate limiter wired to OMS API client |
| 9 | Circuit breaker on OMS failures |
| 10 | Reconciliation against Dhan position book daily |
| 11 | EOD square-off path (if dry_run = false) |

**Cannot ship live trading without these.** Likely 6-8 weeks of focused work.

---

## §8. Local Mac dev environment (gap #5)

Operator stated: *"how can we directly test deploy from IntelliJ without breaking current live trading or coding?"*

The implicit assumption: Mac dev = AWS prod (per `aws-budget.md` rule 10). But:

| Item | Status |
|---|---|
| `make docker-up` on Mac brings up the slimmed stack (tickvault + QuestDB only) | Needs verification post-restructure |
| `cargo test --workspace` runs on Mac | Should work — needs verification |
| Mac can hit Dhan API for dev testing? | Limited — static IP mandate post-2026-04-01 may block; Mac IP changes |
| Mac can run option chain REST loop in test mode? | YES — Dhan accepts any IP for data APIs |
| Mac can place ORDERS in test mode? | NO — orders need registered static IP; Mac IP fluctuates |
| Mac → AWS deploy is via `git push` → CI → SSM RunCommand | DESIGNED, not implemented |
| Operator can debug a live AWS issue from Mac? | Via SSM Session Manager (`aws ssm start-session`) |

**The Mac workflow IS the IDE workflow. IntelliJ → cargo → docker-compose on Mac → git push → AWS deploys via Actions.** No special IDE plugin needed.

---

## §9. Quick-wins not yet wired

Per AWS Mumbai-options research agent (a1fde05f, earlier session):

| Action | Cost | Effort | Status |
|---|---|---|---|
| Enable AWS Compute Optimizer | Free | 1 click | NOT YET DONE |
| Wire AWS Health Dashboard → Telegram | Free | 30 min setup | NOT YET DONE |
| Buy 1-yr Compute Savings Plan when stable | Saves ~₹300/mo | One-time after 30d | Deferred (operator said on-demand for now) |
| AWS billing alarm at ₹1,500/mo | Free | 5 min setup | NOT YET DONE |
| MFA on root user | Free | 5 min setup | NOT YET DONE |
| GitHub Dependabot enable for the repo | Free | 1 click | NOT YET DONE (6 CVEs already flagged on push) |

---

## §10. Recommended NEXT 5 actions for operator

In strict priority order:

| Priority | Action | Effort | Why |
|---|---|---|---|
| 1 | **Clarify BRUTEX** — what is it, where does it live, is it built? | 5 min discussion | Unblocks dep on "future strategies running live" |
| 2 | **Strategy execution design** — answer §5 questions | 1-2 hour discussion | Critical gap #1 |
| 3 | **Risk engine spec** — daily loss, position limits, kill switch | 30 min discussion | Critical gap #3 |
| 4 | **Bootstrap path choice** — operator runs `aws-bootstrap.sh` OR pastes Cowork prompt | 1 decision | Unblocks production |
| 5 | **MFA + billing alarm + Dependabot** — 3 free 5-min wins | 15 min total | Cheap protection |

---

## §11. What I will NOT add to scope (operator-charter §F honest envelope)

| Tempting but rejected | Why |
|---|---|
| Multi-strategy ensemble framework | Operator just locked 10 strategies; framework is over-engineering |
| Real-time PCR + IV surface analytics | Option chain REST gives us greeks already; surface analytics is post-MVP |
| Backtester inside tickvault | BRUTEX is separate (or aspirational); don't duplicate |
| Web dashboard | Operator dropped Grafana per LOCK-3; CloudWatch is the dashboard |
| Mobile app | Telegram bot IS the mobile interface |
| Multi-account / multi-broker | Single Dhan account locked |
| Crypto / forex / commodity F&O | 4 IDX_I SIDs locked |
| Auto-strategy-generation (ML/GPT) | BRUTEX domain; not tickvault domain |

---

## §12. Trigger / auto-load

This rule activates when editing:
- Any file under `crates/trading/src/strategy/`
- Any file under `crates/trading/src/oms/`
- Any file under `crates/trading/src/risk/`
- `.github/workflows/*.yml`
- `scripts/aws-bootstrap.sh`
- Any file containing `BRUTEX`, `Signal`, `OmsEngine`, `RiskEngine`, `KillSwitch`
