# Table Cleanup — Next-Session Handoff

> **Purpose:** everything a fresh Claude Code session needs to execute
> the QuestDB table cleanup (#T1–#T5) with zero context from the prior
> session.
> **Created:** 2026-05-20.
> **Plan status:** APPROVED — every decision locked, fully unblocked.

---

## 1. Paste-in prompt for the new session

Copy the block between the lines into a fresh Claude Code session:

```
Continue the QuestDB table cleanup. First read
.claude/plans/active-plan-table-cleanup.md — it is APPROVED, every
decision is locked, fully unblocked. Then read
.claude/plans/table-cleanup-NEXT-SESSION-HANDOFF.md.

Execute the #T1-#T5 sequence serially — one PR at a time, merge each
to main before starting the next (operator-charter §H). Start with #T1.

#T1 = candle re-architecture. The 21 candle timeframe tables become
plain QuestDB tables fed by the in-memory aggregator's periodic async
flush (RAM-first — NOT materialized views). Drop candles_1s, the 9
candles_*_shadow tables, and the QuestDB matview chain. Re-add the 12
sub-15m timeframes (2m,3m,4m,6m,7m,8m,9m,10m,11m,12m,13m,14m) as RAM
aggregator timeframes. Apply the approved column drops from the plan's
"Column cleanup" section (drop greeks iv/delta/gamma/theta/vega + the
lifecycle columns from ticks + candle tables).

This re-architects the LIVE candle engine. Per operator-charter §E run
the 3-agent adversarial review (hot-path-reviewer, security-reviewer,
general-purpose hostile) BEFORE writing code. Per audit-findings Rule
14 do NOT half-ship / skeleton it. Develop on a fresh claude/<name>
branch, open a draft PR, enable auto-merge, subscribe to PR activity.

Confirm you have read the plan, then present the #T1 implementation
approach for approval before coding.
```

---

## 2. What the prior session (2026-05-20) shipped — all merged to `main`

| PR | Title |
|---|---|
| #722 | IDX_I subscription → Quote mode (day OHLC straight from Dhan) |
| #724 | QuestDB table-cleanup plan + column audit |
| #725 | Honest proof-audit snapshot |
| #726 | Real secret-scan CI (replaced a no-op `echo` stub) |
| #727 | Option-chain minute-snapshot QuestDB persistence (proof-audit gap #2) |
| #728 | Tick rescue ring 5M → 100K (~980 MB RAM freed) |
| #729 | Table-cleanup plan decisions recorded (greeks + column drops approved) |

---

## 3. The plan + reference docs

| What | Path |
|---|---|
| **THE PLAN — read first** | `.claude/plans/active-plan-table-cleanup.md` |
| Proof-audit snapshot (open gaps #3–#6) | `.claude/plans/research/proof-audit-2026-05-20.md` |
| Resilience design (4-SID scope) | `docs/architecture/resilience-simplification-4-sids.md` |
| Serial-PR protocol (§H) | `.claude/rules/project/pr-completion-protocol.md` |
| Z+ doctrine (§E 3-agent review) | `.claude/rules/project/z-plus-defense-doctrine.md` |
| Skeleton-PR ban (Rule 14) | `.claude/rules/project/audit-findings-2026-04-17.md` |

---

## 4. Target: 37 tables → 24

**KEEP (24):** `ticks` + 21 candle TF tables (`candles_1m`…`candles_15m`
each minute + `candles_30m`, `1h`, `2h`, `3h`, `4h`, `1d`) +
`option_chain_minute_snapshot` + `historical_candles`.

**DROP (~31):** `candles_1s`, 9 `candles_*_shadow`, ~20 audit tables,
5 instrument tables, 6 misc tables — full list in the plan's DROP table.

Live universe: 4 IDX_I SIDs — NIFTY=13, BANKNIFTY=25, SENSEX=51,
INDIA VIX=21.

---

## 5. The #T1–#T5 serial sequence (one PR each, §H)

| # | Scope |
|---|---|
| #T1 | Candle re-architecture — 21 plain candle tables from the RAM aggregator; drop `candles_1s` + 9 shadow tables + matview chain; column drops |
| #T2 | Drop ~20 audit tables + their `*_persistence.rs` modules + boot DDL + notification/ErrorCode wiring + ratchets |
| #T3 | Drop 5 instrument tables + their persistence surface |
| #T4 | Drop 6 misc tables + their persistence modules |
| #T5 | Cross-verify (PR #9) lands table-free — Telegram-only against `historical_candles` |

---

## 6. Key code files for #T1 (starting points — verify before editing)

| File | Role |
|---|---|
| `crates/storage/src/materialized_views.rs` | 9 candle matviews + `candles_1s` DDL |
| `crates/storage/src/shadow_persistence.rs` | the 9 `candles_*_shadow` tables |
| `crates/storage/src/candle_persistence.rs` | candle ILP writer |
| `crates/storage/src/seal_writer_loop.rs` (+ `_runner`, `_task`) | seal writer |
| `crates/trading/src/candles/` (`cascade.rs`) | in-memory 29-TF cascade |
| `crates/app/src/main.rs` | boot wiring for all of the above |

---

## 7. Hard rules the new session MUST obey

- **§H** — one PR open at a time; merge to `main` before starting the next.
- **§E** — #T1 touches >3 crates / >1000 LoC → run the 3-agent
  adversarial review BEFORE coding.
- **Rule 14** — no skeleton / half-finished PRs. A #T item ships
  complete or not at all.
- The candle engine is **LIVE** — after #T1, verify candles still flow
  (`candles_1m` etc. receiving rows) before declaring done.
- Develop on `claude/<descriptive>` branches. Never push to `main`.
- `order_audit` (#T2) is SEBI 5-year — its drop is gated: MUST be
  re-added before `dry_run=false`. See the plan's Consequences §.

---

## 8. Open proof-audit gaps (separate track — not table cleanup)

`.claude/plans/research/proof-audit-2026-05-20.md` lists gaps #3–#6
(bench-gate not CI-wired, coverage post-merge only, Z+ enforcement
hooks absent, stale docs). Gaps #1 + #2 are already fixed (#726, #727).
These are independent of the table cleanup — tackle after #T1–#T5 or
in parallel sessions if the operator directs.
