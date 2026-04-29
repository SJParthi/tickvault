# Movers 22-TF v3 — Index (split-file plan, final scope)

**Status:** APPROVED v3 — 2026-04-28 by Parthiban ("yes dude go ahead gooooooooooo")
**Date:** 2026-04-28
**Branch:** `claude/new-session-TBQe7`
**Supersedes:** v1 → v2 → v3
**Source audits:** `.claude/plans/research/2026-04-28-movers-22tf-design-verification/01..04.md`

## Why split into 4 files

`stream-resilience.md` rule B1+B2 — chat carries pointers, not bulk; each Write payload stays small. Inlining failed 3 times with stream-idle-timeout. Body now lives in 4 separate files, each Written via individual tool call.

| File | Contents |
|---|---|
| `.claude/plans/v2-architecture.md` | 6 architectural fixes; 8 components; final 26-col schema; depth integration; expiry rollover |
| `.claude/plans/v2-risks.md` | 8 movers risks + 2 expiry-rollover risks; 12 failure modes; chaos test; 7 open questions |
| `.claude/plans/v2-ratchets.md` | 47 ratchets + 5 rewrites + 3 banned-pattern + 2 make-doctor + 1 chaos |
| `.claude/plans/v2-phases.md` | 7-commit phasing + 16 verification gates + 9-box per phase + go/no-go |

## Final scope — every item

### A. Movers core (22 tables, unified)

1. 22 `movers_{T}` tables (1s, 5s, 10s, 15s, 30s, 1m..15m, 30m, 1h)
2. Unified per-timeframe (NOT split per category) — `segment` + `instrument_type` distinguish
3. DEDUP UPSERT KEYS(ts, security_id, segment) — I-P1-11 composite-key
4. papaya tracker — lock-free O(1) hot path
5. 22 isolated ILP writers
6. Caller-owned arena Vec per writer — zero per-snap alloc
7. Drop-NEWEST mpsc(8192) + 0.1% SLA alert
8. Market-hours gate `[09:00, 15:30]` IST
9. `MoversSupervisor` — respawns within 5s
10. Rescue ring — buffers ILP writes during QuestDB outage
11. All 22 tables in partition manager
12. 90-day SEBI classification (NOT 5y)
13. 9 new candle materialized views (4m, 6m, 7m, 8m, 9m, 11m, 12m, 13m, 14m)

### B. MoverRow 26-column schema

13 base + 4 derivative-meta + 4 OI/spot + 5 depth = 26 columns; ~296 B; `Copy`; 5 cache lines.

### C. UI tabs (24 panels — final, trimmed)

| Category | Sub-tabs | Count |
|---|---|---|
| Stocks | Price Movers (Gainers / Losers) | 2 |
| Index | Price Movers (Gainers / Losers) | 2 |
| Options | Highest OI, OI Gainers, OI Losers, Top Volume, Top Value, Price Gainers, Price Losers | 7 |
| Futures | Premium, Discount, Top Volume, OI Gainers, OI Losers, Price Gainers, Price Losers | 7 |
| **Depth (NEW)** | Widest Spread, Tightest Spread, Top Bid Pressure, Top Ask Pressure, NIFTY ATM ladder, BANKNIFTY ATM ladder | 6 |
| **Total** | | **24** |

### D. Filters / dropdowns

- Global timeframe (22 options)
- Options + Futures: expiry-month dropdown (April/May/June from `derivative_contracts.expiry_date`)
- Options + Futures: All / Index / Stock
- Stocks: Gainers ↔ Losers, F&O Stocks toggle

### E. Depth integration (Option A + C, NOT B)

| Item | Decision |
|---|---|
| Option A: 5 depth columns on movers | YES |
| Option C: 6 depth panels from existing tables | YES |
| Option B: full ladder snapshots × 22 | NO (too heavy: ~4B rows/day) |
| Existing depth-20/200 WS system | UNCHANGED |
| `MarketDepthCache` reused via papaya O(1) lookup | YES |

### F. Stock F&O expiry rollover (NEW)

| Aspect | Old | New |
|---|---|---|
| Constant | `STOCK_EXPIRY_ROLLOVER_TRADING_DAYS = 1` | `= 0` |
| Wed with Thu expiry (T-1) | ROLL | KEEP Thursday |
| Thu IS expiry (T-0) | ROLL | ROLL |
| Reason | avoid expiry-day risk | **Dhan disallows expiry-day stock F&O trading** |
| Rule file | `depth-subscription.md` 2026-04-24 §6 | UPDATED |
| Runbook | `expiry-day.md` | UPDATED |

### G. Ratchets (47 + 5 rewritten + 3 banned-pattern + 2 make-doctor + 1 chaos)

See `v2-ratchets.md` for the full table.

### H. 7 phased commits

See `v2-phases.md`. Recommendation: **ship Phase 5 (expiry rollover) as separate PR first** to keep movers PR under 3K LoC.

## Audit findings still baked in (14 of 14)

| # | Finding | Where |
|---|---|---|
| 1 | papaya, not std::HashMap | architecture; ratchet 18 |
| 2 | 22 writers, not 1 | architecture; ratchet 14 |
| 3 | ArrayString<16> Copy MoverRow | architecture; ratchet 12 |
| 4 | Market-hours gate | architecture; ratchet 24 |
| 5 | Caller-owned arena Vec | architecture; ratchet 17 |
| 6 | Drop-NEWEST mpsc | architecture; ratchet 15 |
| 7 | Movers-specific rescue ring | risks #1; phases 1+5 |
| 8 | mpsc saturation drop SLA | risks #2; ratchet 15 |
| 9 | Scheduler clock drift alert | risks #3 |
| 10 | All-22-tasks-panic supervisor | risks #4; ratchet 23 |
| 11 | IDX_I prev_close mid-day fallback | risks #5 |
| 12 | F&O expiry filter | risks #6 |
| 13 | Partition manager 22-table inclusion | risks #7; ratchet 7 |
| 14 | SEBI 90d classification | risks #8; ratchet 8 |

## NEW additions in v3 (beyond v2)

| # | Addition |
|---|---|
| 15 | Categorization columns (`instrument_type`, `underlying_security_id`, `expiry_date`, `strike_price`, `option_type`) |
| 16 | OI columns (`open_interest`, `oi_change`, `oi_change_pct`) |
| 17 | Spot column (`spot_price` from `SharedSpotPrices`) |
| 18 | Depth columns (`best_bid`, `best_ask`, `spread_pct`, `bid_pressure_5`, `ask_pressure_5` from `MarketDepthCache`) |
| 19 | 24-panel Grafana dashboard with expiry-month dropdown |
| 20 | Stock F&O expiry rollover policy change (T-1 → T-only) + 5 rewritten ratchets + rule + runbook |
| 21 | 7 new Dhan-UI ratchets (38–47) |

## Plan status workflow

1. **DRAFT** ← (was) v3 pending approval
2. **APPROVED** ← Parthiban "yes dude go ahead gooooooooooo" 2026-04-28
3. **IN_PROGRESS** ← current (Phase 5 done; Phases 6+7 next, then 8-13)
4. **VERIFIED** ← `bash .claude/hooks/plan-verify.sh` green after Phase 13
5. **ARCHIVED** ← `.claude/plans/archive/2026-04-28-movers-22tf-redesign-v3.md` after final PR merge

## Phase progress tracker

| Phase | Description | Status | Commit |
|---|---|---|---|
| 5 | Stock F&O expiry rollover T-only | DONE | `d428835` |
| 6 | Depth-200 URL wipe-off (619 LoC removed + 2 new ratchets) | DONE | `33547aa` |
| 7 | Depth-20 dynamic top-150 selector (selector + types + 13 tests) | DONE | `f3b7baa` |
| 9 | MoverRow 26-column Copy struct + 22-tf constants (8 ratchets) | DONE | `73b820e` |
| 8 | 22 movers_{T} tables + DDL + DEDUP + partition mgr + S3 (7 ratchets) | DONE | `08e34fb` |
| 13 | Banned-pattern hooks (cats 8/9/10) + make doctor sections 8/9 | DONE | `e2263c0` |
| 11 | 3 ErrorCodes + Triage + 24-panel Grafana + 4 alerts + runbook | DONE | `bec60a1` |
| **7b** | **Phase 7 follow-up — main.rs boot wiring for the 3 dynamic depth-20 slots + async runner** | **NEXT** (~150 LoC) | — |
| 10 | papaya tracker + arena snapshot + scheduler + supervisor + market-hours gate | After 7b | — |
| 12 | Tests + chaos (47 ratchets + 2 DHAT + 3 Criterion + 1 chaos test) | After 10 | — |

## Resume protocol (for new Claude Code sessions)

If this session ends prematurely, a fresh session can resume by:

1. Checkout the branch: `git checkout claude/new-session-TBQe7`
2. Read this file (the index) — phase progress tracker above is the source of truth
3. Read the most recent commit message to see the last completed phase
4. Read the 4 split files (`v2-architecture.md`, `v2-risks.md`, `v2-ratchets.md`, `v2-phases.md`) for full context
5. Continue from the **NEXT** phase per the tracker above

Last commit on this branch: `33547aa` (Phase 6 — depth-200 wipe-off complete).
Next action: **Phase 7** — implement `crates/core/src/instrument/depth_20_dynamic_subscriber.rs` per the design in `v2-architecture.md` Section I.

## Phase 7 implementation guide (for resume)

| Step | Action |
|---|---|
| 1 | Create `crates/core/src/instrument/depth_20_dynamic_subscriber.rs` (new module) |
| 2 | Add module to `crates/core/src/instrument/mod.rs` |
| 3 | Implement selector function querying `option_movers` (existing table): `SELECT security_id FROM option_movers WHERE ts > now() - 90s AND change_pct > 0 ORDER BY volume DESC LIMIT 150` |
| 4 | Implement 1-minute `tokio::time::interval` recompute loop with `MissedTickBehavior::Skip` |
| 5 | Add `is_within_market_hours_ist()` gate (audit-findings Rule 3) |
| 6 | Implement set-diff: `leavers = previous - current; entrants = current - previous` |
| 7 | Emit `DepthCommand::Swap20 { unsubscribe_messages: leavers, subscribe_messages: entrants }` over the 3 dynamic-slot mpsc channels |
| 8 | Promote `DEPTH-DYN-01` and `DEPTH-DYN-02` from RESERVED to defined `ErrorCode` variants in `crates/common/src/error_code.rs` |
| 9 | Add Triage YAML entries in `.claude/triage/error-rules.yaml` |
| 10 | Update `wave-4-error-codes.md` (RESERVED → defined) |
| 11 | Add Telegram event variants `Depth20DynamicTop150Empty`, `Depth20DynamicSwapBroken` (edge-triggered per audit-findings Rule 4) |
| 12 | Boot wiring in `crates/app/src/main.rs` — spawn supervisor task that owns the 3 dynamic-slot mpsc Senders (slots 3/4/5) |
| 13 | Add 4 new ratchet tests (48-51): selector returns ≤150, filter > 0, 1-min interval, delta-swap correctness |
| 14 | Run `cargo test -p tickvault-core` |
| 15 | Commit + push as `feat(depth-20): dynamic top-150 selector + 1-minute Swap20 rebalance` |

## Next action — STOP

Parthiban — please review the 4 files (each <300 lines) and answer the 7 open questions in `v2-risks.md`, especially the new ones:

- Q6: Approve expiry rollover T-1 → T-only?
- Q7: Approve Option A + C depth integration (NOT B)?

Plus: ship Phase 5 (rollover) as separate PR first, or bundle with movers?

**No production code will be written until that explicit GO.**
