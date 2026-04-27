# Cross-Cutting Additions (C1–C13)

> Apply to ALL 14 items. Each entry below tells you WHAT to add, WHY (the ratchet that fires if you skip it), and WHERE.

| # | Addition | Why (which ratchet fails if skipped) | File / scope |
|---|----------|--------------------------------------|--------------|
| C1 | Add new `ErrorCode` variants for every new typed event (~22 new variants total, was 18) | `crates/common/tests/error_code_rule_file_crossref.rs::every_error_code_variant_appears_in_a_rule_file` AND `every_rule_file_code_has_an_enum_variant` | `crates/common/src/error_code.rs` |
| C2 | Add triage YAML rules for each new ErrorCode (known→auto-fix, novel→escalate) | Phase 6 zero-touch automation; novel signatures spam Telegram with no remediation path | `.claude/triage/error-rules.yaml` |
| C3 | Add runbook MD per new ErrorCode | `runbook_path()` cross-ref test fails if path doesn't resolve | `.claude/rules/project/<code>.md` or `docs/runbooks/<code>.md` |
| C4 | Add Criterion budget per new hot path (Items 0/3/8/9 mainly) | `scripts/bench-gate.sh` 5% regression cap | `quality/benchmark-budgets.toml` |
| C5 | Add DHAT zero-alloc test for each new hot path | Three-principle compliance; CI hard fail on `total_blocks > 0` | `crates/*/tests/dhat_*.rs` (4 new files: `dhat_tick_path_zero_alloc.rs`, `dhat_option_movers.rs`, `dhat_tick_gap_detector.rs`, `dhat_telegram_dispatcher.rs`) |
| C6 | Add chaos test per new failure mode (~7 new chaos tests) | Existing 16 chaos tests cover only old surfaces; new audit tables, sleep-wake, tick-gap, telegram-storm need their own | `crates/storage/tests/chaos_*.rs`, `crates/core/tests/chaos_*.rs` |
| C7 | Add coverage threshold entry per new file | `scripts/coverage-gate.sh` requires 100% per crate; new files default to 0% and fail | `quality/crate-coverage-thresholds.toml` |
| C8 | Schema self-heal `ALTER TABLE ADD COLUMN IF NOT EXISTS` for each new table + column | Per `observability-architecture.md` rule, every new schema must be idempotent for in-place migration | `crates/storage/src/*_persistence.rs` (each new module) |
| C9 | Feature flag / config toggle per item in `config/base.toml` | Rollback safety — if any of the 14 items misbehaves, operator flips the toggle, no redeploy. Each toggle has a flip-to-off rollback test (G6) | `config/base.toml` + `crates/common/src/config.rs` |
| C10 | Banned-pattern hook category 7 — sync `std::fs::*` in hot path (from Item 0) + B12 self-test fixtures | Mechanical guard against Item 0 regression; without fixture self-test the hook can be silently broken | `.claude/hooks/banned-pattern-scanner.sh` + `.claude/hooks/test-fixtures/` |
| C11 | Update `.claude/rules/project/disaster-recovery.md` with new RTO + scenarios | Operator panics if "WS sleep until 09:00" not documented after Items 5/6 ship. Rewrite scenarios 5/6/7/8; add scenarios 12/13 for overnight + holiday wake | `.claude/rules/project/disaster-recovery.md` |
| C12 | Split into 3 PRs (one per Wave) | 9,620 LoC in one PR is unreviewable; Wave 1 ≈ 25 files, Wave 2 ≈ 30, Wave 3 ≈ 25 | Branch sequencing — Wave 1 → Wave 2 (rebased) → Wave 3 (rebased) |
| C13 | AWS S3 lifecycle update for the 6 audit tables | Per `aws-budget.md` 100GB EBS hard cap; S3 IT at 90d, Glacier at 365d keeps ₹/mo flat. Glacier 90-day minimum honored via partition-manager idempotency-key | `deploy/aws/s3-lifecycle.json` + partition manager |

## Per-item cross-cutting matrix

For every Item I in {0..13}, check off:

| Item | C1 | C2 | C3 | C4 | C5 | C6 | C7 | C8 | C9 | C10 | C11 | C12 | C13 |
|------|----|----|----|----|----|----|----|----|----|----|----|----|----|
| 0 (hotpath) | 2 codes | y | y | 3 budgets | 1 dhat | 1 chaos | y | n/a | y | y (cat 7) | n/a | Wave 1 | n/a |
| 1 (phase2) | 2 codes | y | y | n | n | n | y | n/a | y | n | n/a | Wave 1 | n/a |
| 2 (stock movers) | 1 code | y | y | 1 budget | n | n | y | y | y | n | n/a | Wave 1 | n/a |
| 3 (option movers) | 1 code | y | y | 2 budgets | 1 dhat | 1 chaos | y | y | y | n | n/a | Wave 1 | y |
| 4 (prev_close) | 2 codes | y | y | 1 budget | n | 1 chaos | y | y | y | n | n/a | Wave 1 | y |
| 5 (main-feed sleep) | 3 codes | y | y | n | n | 1 chaos | y | n/a | y | n | y (rewrite) | Wave 2 | n/a |
| 6 (depth/OU sleep) | (reuses) | y | y | n | n | 1 chaos | y | n/a | y | n | y (rewrite) | Wave 2 | n/a |
| 7 (FAST BOOT) | 2 codes | y | y | n | n | 1 chaos | y | n/a | y | n | y (add scenario 14) | Wave 2 | n/a |
| 8 (tick-gap) | 1 code | y | y | 1 budget | 1 dhat | n | y | n/a | y | n | n/a | Wave 2 | n/a |
| 9 (audit tables) | 6+2 codes | y | y | n | n | 1 chaos | y | y x6 | y | n | n/a | Wave 2 | y |
| 10 (preopen movers) | 1 code | y | y | n | n | n | y | y | y | n | n/a | Wave 3 | n/a |
| 11 (telegram) | 3 codes | y | y | n | 1 dhat | 1 chaos | y | n/a | y | n | n/a | Wave 3 | n/a |
| 12 (selftest) | 2 codes | y | y | n | n | n | y | n/a | y | n | n/a | Wave 3 | n/a |
| 13 (SLO score) | 2 codes | y | y | n | n | n | y | n/a | y | n | n/a | Wave 3 | n/a |
| **Totals** | **~22 codes** | **14** | **14** | **~9** | **4** | **7** | **14** | **9** | **14** | **2** | **3** | **3 PRs** | **3** |

## Feature-flag list (C9 — 14 toggles)

```toml
# config/base.toml
[features]
hotpath_async_writers = true                # Item 0
phase2_emit_guard = true                    # Item 1
stock_movers_full_universe = true           # Item 2
option_movers_5s = true                     # Item 3
previous_close_persist = true               # Item 4
ws_main_sleep_until_open = true             # Item 5
ws_depth_ou_sleep_until_open = true         # Item 6
fast_boot_60s_deadline = true               # Item 7
tick_gap_detector_60s_coalesce = true       # Item 8
audit_tables_enabled = true                 # Item 9
preopen_movers = true                       # Item 10
telegram_bucket_coalescer = true            # Item 11
market_open_self_test = true                # Item 12
realtime_guarantee_score = true             # Item 13
```

Each flag has a paired test in `crates/app/tests/feature_flag_rollback_guard.rs` that:
1. Sets flag = false → asserts old code path still works
2. Sets flag = true → asserts new code path works
3. Asserts no silent default drift between releases

## C-level summary in three lines

- **Mandatory:** C1, C3, C5, C7, C8, C9 (build fails without)
- **Important:** C2, C4, C6, C10, C11, C13 (one of these missing degrades operator experience)
- **Process:** C12 (3-PR split — reviewer can complete each in one sitting)
