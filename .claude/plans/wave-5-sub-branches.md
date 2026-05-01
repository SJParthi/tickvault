# Wave 5 — Sub-Branch & Parallel-Session Map

> **Companion to:** `.claude/plans/active-plan-wave-5-indices-only.md` Item 17.
> **Authority:** the active plan's Parallelization Map section is canonical.
> This file is the operational checklist for splitting Wave 5 across parallel
> Claude Code sessions without merge conflicts.
> **Created:** 2026-05-01.

## Why this file exists

Operator demand 2026-05-01: *"in the future we can split it out or we can
provide parallel everything to new new claude code sessions right if it
doesn't have any dependency right dude?"*

`stream-resilience.md` per-PR-set protocol caps each sub-PR at ≤30 files and
≤3K LoC. Wave 5 has 16 active items (Item 28 already shipped via PR #415,
Item 27 is GATED on Mon May 4 verdict). Sequential execution = 12-15 hours
of wall-clock. Parallel execution per the dependency graph below = 4-5 hours.

## Branch hierarchy

```
main
└── claude/wave-5-plan-review-uocad        ← integration branch (this branch)
    │   PR #?  ← all Wave 5 items land here, then squash-merge to main
    │
    ├── claude/wave-5a-item-N-<slug>       ← Group A/B parallel sub-branches
    ├── claude/wave-5b-item-N-<slug>
    └── claude/wave-5c-item-N-<slug>
```

Each parallel session:
1. Branches off `claude/wave-5-plan-review-uocad` (NOT off main).
2. Implements ONE item per branch (commit-lint scope rule).
3. Opens a draft PR back to `claude/wave-5-plan-review-uocad` (NOT to main).
4. Once merged, the integration branch's PR squash-lands to main.

## Group dependency graph

```
Group A (Foundation)            Group B (Independent fixes)
  Item 1 (config gate)            Item 6  (core_affinity)
        │                         Item 7  (candle_aggregator key)
        │                         Item 8  (warn→error)
        │                         Item 9  (ErrorCodes)
        │                         Item 15 (drop redundant prev_close)
        │                         Item 17 (this file)
        │                         Item 22 (guarantee matrix template)
        │                         Item 26 L1+L2 (volume guarantee layer)
        ▼
Group C (Universe-driven, sequential head)
  Item 2 (universe filter — single source for ALL planner predicates)
        │
        ├── Item 3  (main feed equal-split)
        ├── Item 4  (depth-20 single-side + dynamic top-50)
        ├── Item 5  (depth-200 dynamic top-5)
        ├── Item 12 (option_movers selector universe filter)
        ├── Item 13 (boot-time prev-close routing assertion)
        ├── Item 14 (NSE_EQ feed_mode Quote downgrade)
        └── Item 19 (25-tf spec extension)
        ▼
Group D (Storage strategy, sequential)
  Item 16 (movers hybrid storage reuse) ← reads Item 12's filter contract
  Item 21 (movers store-everything Tier A/B) — partly superseded by Item 25
        ▼
Group E (Tests + review, sequential, last)
  Item 11 (burst chaos test)
  Item 10 (adversarial 3-agent re-review on the diff)
  Item 29 (cowork verification findings fold-in) — applied to deliverables

GATED (do NOT ship until Mon May 4 09:45 IST verdict)
  Item 27 (CORRECT mat view DDL) — needs 5-series monotonicity SELECT
```

## Per-item sub-branch table

Pick the recommended sub-branch name. Each is unique enough that pre-push
hooks and the GitHub web UI will not confuse them.

| Item | Group | Sub-branch | Touches | Parallel with |
|---:|:---:|---|---|---|
| 1 | A | `claude/wave-5a-item-1-subscription-scope` | `crates/common/src/config.rs`, `config/base.toml` | (must land first) |
| 6 | B | `claude/wave-5b-item-6-core-affinity` | `crates/app/src/main.rs`, runtime + watchdog | 7, 8, 9, 15, 17, 22, 26 |
| 7 | B | `claude/wave-5b-item-7-candle-aggregator-key` | `crates/core/src/pipeline/candle_aggregator.rs` | 6, 8, 9, 15, 17, 22, 26 |
| 8 | B | `claude/wave-5b-item-8-tick-flush-error-level` | `crates/storage/src/tick_persistence.rs`, meta-guard | 6, 7, 9, 15, 17, 22, 26 |
| 9 | B | `claude/wave-5b-item-9-error-codes` | `crates/common/src/error_code.rs`, new rule file | 6, 7, 8, 15, 17, 22, 26 |
| 15 | B | `claude/wave-5b-item-15-drop-prev-close` | `crates/storage/src/previous_close_persistence.rs` | 6, 7, 8, 9, 17, 22, 26 |
| 17 | B | `claude/wave-5b-item-17-parallelization-map` | this file | 6, 7, 8, 9, 15, 22, 26 |
| 22 | B | `claude/wave-5b-item-22-guarantee-matrix` | rule file template | 6, 7, 8, 9, 15, 17, 26 |
| 26 | B | `claude/wave-5b-item-26-volume-guarantee` | new module + tests | 6, 7, 8, 9, 15, 17, 22 |
| 2 | C | `claude/wave-5c-item-2-universe-filter` | `crates/core/src/instrument/subscription_planner.rs` | (head of C) |
| 3 | C | `claude/wave-5c-item-3-main-feed-split` | `crates/core/src/websocket/connection_pool.rs`, new dist module | 4, 5, 12, 13, 14, 19 |
| 4 | C | `claude/wave-5c-item-4-depth-20-redesign` | depth-20 selector + connection | 3, 5, 12, 13, 14, 19 |
| 5 | C | `claude/wave-5c-item-5-depth-200-dynamic` | depth-200 selector + connection | 3, 4, 12, 13, 14, 19 |
| 12 | C | `claude/wave-5c-item-12-option-movers-filter` | option_movers selector | 3, 4, 5, 13, 14, 19 |
| 13 | C | `claude/wave-5c-item-13-prev-close-routing-assert` | boot assertion | 3, 4, 5, 12, 14, 19 |
| 14 | C | `claude/wave-5c-item-14-nse-eq-quote` | feed_mode downgrade | 3, 4, 5, 12, 13, 19 |
| 19 | C | `claude/wave-5c-item-19-25tf-spec` | movers tracker extension | 3, 4, 5, 12, 13, 14 |
| 16 | D | `claude/wave-5d-item-16-movers-hybrid` | `crates/storage/src/movers_persistence.rs` | (post-Item-12) |
| 21 | D | `claude/wave-5d-item-21-movers-tier-ab` | movers writer scheduler | (post-Item-16) |
| 11 | E | `claude/wave-5e-item-11-burst-chaos` | `crates/storage/tests/chaos_burst_indices_only.rs` | (post-2..16) |
| 10 | E | `claude/wave-5e-item-10-adversarial-review` | review report only | last |
| 29 | E | `claude/wave-5e-item-29-cowork-foldin` | docs + audit | last |
| 27 | GATED | `claude/wave-5-item-27-mat-view-ddl` | mat view DDL | **DO NOT START before Mon May 4 09:45 IST verdict** |

## Conflict-avoidance contract

Group B items each touch a single file (or a single new file). Group C items
touch disjoint code paths AFTER Item 2 lands the canonical universe filter.
Within Group C, the head item (Item 2) MUST land first so downstream items
read the same `subscription.scope` predicate.

If two parallel sessions accidentally pick items in the same column of the
table, the second session WILL hit a merge conflict on
`active-plan-wave-5-indices-only.md` (every session ticks `[ ]` → `[x]` in
the plan). Resolve by rebasing the second branch on top of the first;
`git mergetool` is not needed for a checkbox flip.

## Practical execution recipe (for any future Claude session)

```bash
# 1. Pick an unticked item from the active plan
grep -nE "^### - \[ \] [0-9]+\." .claude/plans/active-plan-wave-5-indices-only.md

# 2. Create the sub-branch off the integration branch
git fetch origin claude/wave-5-plan-review-uocad
git switch -c claude/wave-5b-item-<N>-<slug> origin/claude/wave-5-plan-review-uocad

# 3. Implement, commit per the conventional-commit scope rule
#    (commit-lint: feat|fix|refactor|test|docs|chore|perf|security)

# 4. Push, then open a DRAFT PR targeting claude/wave-5-plan-review-uocad
git push -u origin claude/wave-5b-item-<N>-<slug>
# gh pr create --base claude/wave-5-plan-review-uocad --draft

# 5. Tick [ ] → [x] in the plan, ship a docs follow-up commit on the
#    integration branch (or include in the same PR)
```

## Already-shipped reference PRs

- PR #415 (commit `9a9a032`) — Item 28 (candles cascade volume bug fix)
  — branch + commit + test pattern reference
- PR #416 (commit `6353c18`) — Item 28 plan tick

## Open dependencies on operator action

| Action | Item gated | Status |
|---|---|---|
| DEPTH200 SELF token refresh in SSM | depth-200 boot | Reported done by operator 2026-05-01 |
| Item 26 L3 Dhan support email | Item 26 L3 close-out | Pending — operator must Gmail send GitHub link to `docs/dhan-support/2026-05-01-volume-semantic-clarification.md` |
| Mon May 4 09:45 IST monotonicity SELECT verdict | Item 27 ship | Wait — do NOT ship Item 27 before |

## Wave 5 status snapshot at file creation

```
Shipped on integration branch (this commit + earlier):
  Item 1   ✅  subscription.scope config gate
  Item 7   ✅  candle_aggregator I-P1-11 ratchet
  Item 8   ✅  tick auto-flush warn→error
  Item 9   ✅  4 new ErrorCode variants
  Item 17  ✅  this parallelization map
  Item 28  ✅  (already shipped via PR #415 to main)

Still open: 2, 3, 4, 5, 6, 10, 11, 12, 13, 14, 15, 16, 19, 21, 22, 26, 29
GATED:      27 (Mon May 4 verdict)
```
