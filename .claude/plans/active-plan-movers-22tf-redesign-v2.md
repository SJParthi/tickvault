# Movers 22-TF v2 — Index (split-file plan to dodge inline-stream timeout)

**Status:** DRAFT v2 (pending Parthiban approval)
**Date:** 2026-04-28
**Branch:** `claude/new-session-TBQe7`
**Supersedes:** `.claude/plans/active-plan-movers-22tf-redesign.md` (v1)
**Source audits:** `.claude/plans/research/2026-04-28-movers-22tf-design-verification/01..04.md`

## Why this plan is split into 4 files

`stream-resilience.md` rule B1+B2: chat carries pointers, not bulk; each
streamed/written payload stays small. Inlining the full plan kept timing
out (Stream idle timeout — partial response received), so the body lives
in 4 separate files written via individual `Write` calls.

| File | Contents | Size |
|---|---|---|
| `.claude/plans/v2-architecture.md` | 6 NEEDS-CHANGE fixes baked in (papaya, 22 writers, ArrayString, market-hours gate, arena Vec, drop-newest mpsc); 7 components; DDL pattern; prev_close routing | small |
| `.claude/plans/v2-risks.md` | 8 unmitigated → all mitigated (rescue ring, partition manager, F&O expiry, SEBI, panic supervisor, drop-rate SLA, scheduler drift, IDX_I mid-day fallback); chaos test for 1M rows/sec; 10 failure modes; 5 open questions | small |
| `.claude/plans/v2-ratchets.md` | 37 ratchet tests + 3 banned-pattern cats + 2 make-doctor sections + 1 chaos test + 4 ratchet extensions + 7-layer obs map | small |
| `.claude/plans/v2-phases.md` | 6 phased commits + 14-gate pre-merge checklist + 9-box per phase + cross-ref to every rule file + honest 100% claim + go/no-go protocol | small |

## Audit findings baked in (14 of 14)

### 6 architectural NEEDS-CHANGE (audit 01)

| # | Finding | Where addressed |
|---|---|---|
| 1 | papaya, not std::HashMap, for shared tracker state | architecture §What changed #1; ratchet 18 |
| 2 | 22 writers, not 1 | architecture §What changed #2 + §"Why 22 writers"; ratchet 14 |
| 3 | `ArrayString<16>` so `MoverRow` is `Copy` | architecture §What changed #3; ratchet 12 |
| 4 | Market-hours gate (`is_within_market_hours_ist`) | architecture §What changed #4; ratchet 24 |
| 5 | Caller-owned arena `Vec<MoverRow>` | architecture §What changed #5; ratchet 17 |
| 6 | Drop-NEWEST (native tokio) explicitly documented | architecture §What changed #6; ratchet 15 |

### 8 unmitigated risks (audit 03)

| # | Risk | Where addressed |
|---|---|---|
| 1 | Movers-specific rescue ring | risks #1; phases 1+5 |
| 2 | mpsc saturation drop SLA | risks #2; ratchet 15; alert wired |
| 3 | Scheduler clock drift | risks #3; alert wired |
| 4 | All 22 tasks panic supervisor | risks #4; phases 3; ratchet 23 |
| 5 | IDX_I prev_close mid-day fallback | risks #5; phase 3 |
| 6 | F&O expiry filter | risks #6; phase 3 |
| 7 | Partition manager 22-table inclusion | risks #7; phase 1; ratchet 7 |
| 8 | SEBI classification 90d (not 5y) | risks #8; phase 1; ratchet 8 |

### 37 ratchets + 3 banned-pattern + 2 make-doctor + chaos test

All in `v2-ratchets.md`. Maps to audit 04 1:1.

## Plan status workflow (per `plan-enforcement.md`)

1. **DRAFT** ← current (pending approval)
2. **APPROVED** ← after Parthiban gives explicit GO on 5 open questions in `v2-risks.md`
3. **IN_PROGRESS** ← Phase 1 commit lands
4. **VERIFIED** ← `bash .claude/hooks/plan-verify.sh` passes after Phase 6
5. **ARCHIVED** ← move to `.claude/plans/archive/2026-04-28-movers-22tf-redesign-v2.md` after PR merge

## Next action

Parthiban — please review the 4 files above (each <300 lines) and answer the
5 open questions in `v2-risks.md` so this plan can move from DRAFT → APPROVED.

**No production code will be written until that explicit GO.**
