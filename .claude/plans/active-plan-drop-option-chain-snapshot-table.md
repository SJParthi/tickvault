# Implementation Plan: B — drop the `option_chain_minute_snapshot` QuestDB table

**Status:** VERIFIED
**Date:** 2026-06-23
**Approved by:** Parthiban — AskUserQuestion 2026-06-23 "Minimal: drop the TABLE only".
**Branch:** `claude/drop-option-chain-snapshot-table` (off origin/main).

## Context (verified)

The `option_chain_minute_snapshot` QuestDB table is created every boot
(`main.rs:2677`) but the snapshot scheduler that writes it is **disabled**
(`config/base.toml [option_chain_minute_snapshot] enabled = false`, since
2026-06-02 — account lacks the Option Chain Data API). So today the table is a
dormant, empty, never-written table. Operator wants it gone.

**Minimal scope (operator-locked):** remove the QuestDB TABLE — its creation +
its write-path — and KEEP everything else: the scheduler structure, the RAM
`SnapshotCache` (strategy consumer), the option-chain REST client, prev-OI →
candle `oi_pct_from_prev_day`, the `[option_chain_minute_snapshot]` config (it
gates the scheduler), the `OptionChain01-08` error codes, the notification.

## Plan Items

- [x] **B1 — remove the boot DDL call** (table no longer created)
  - Files: crates/app/src/main.rs (remove `ensure_option_chain_minute_snapshot_table(...)` ~L2677)
- [x] **B2 — remove the QuestDB write-path from the scheduler** (keep RAM-cache population + fetch loop)
  - Files: crates/core/src/option_chain/snapshot_scheduler.rs (remove storage import, the `persist_snapshot_to_questdb(...)` call, the `persist_snapshot_to_questdb` + `map_option_chain_to_snapshot_rows` fns, the 2 snapshot-persist metrics + increments, and their tests)
- [x] **B3 — delete the persistence module** (now unused)
  - Files: delete crates/storage/src/option_chain_minute_snapshot_persistence.rs; remove `pub mod …` from crates/storage/src/lib.rs
- [x] **B4 — remove the table from the partition manager**
  - Files: crates/storage/src/partition_manager.rs (list entry + its test ref)
- [x] **B5 — update the wiring guard** (table DDL is intentionally gone)
  - Files: crates/core/src/auth/secret_manager.rs (remove the `ensure_option_chain_minute_snapshot_table` wiring assertion; KEEP the `spawn_snapshot_scheduler` assertion)

## Design
Surgical removal of one dormant table's create+write path. The scheduler keeps
fetching + populating the RAM cache (for the strategy / prev-OI); it simply no
longer mirrors the snapshot into QuestDB. No strategy code touched (§28 safe).

## Edge Cases
- Scheduler re-enabled later → populates RAM cache, no QuestDB write (intended).
- A pre-existing deployment still has the table on disk → harmless (orphaned, like other retired tables); a `DROP TABLE IF EXISTS` migration line is optional follow-up.

## Failure Modes
- Compilation: removing the storage import/fns is contained; build after each crate.
- pub-fn-wiring guard: `ensure_…table` + the removed fns lose call sites — handled by deleting them (not orphaning).

## Test Plan
- `cargo build -p tickvault-storage -p tickvault-core -p tickvault-app`
- `cargo test -p tickvault-core --lib option_chain::snapshot_scheduler` (remaining scheduler tests green)
- `cargo test -p tickvault-app --test per_feed_boot_isolation_guard` (unaffected, stays green)
- `cargo test -p tickvault-core --lib` secret_manager wiring guards
- `cargo clippy --workspace -- -D warnings` + `cargo fmt --check`

## Rollback
- Single feature branch; `git revert`. The deleted module is recoverable from git history; re-adding the boot DDL call restores the table.

## Observability
- The 2 snapshot-persist Prometheus metrics are removed with the write-path (they only counted table writes). No ErrorCode/runbook removed (OptionChain01-08 stay — they're scheduler fetch errors, not table writes).

## Scenarios
| # | Scenario | Expected |
|---|----------|----------|
| 1 | boot | `option_chain_minute_snapshot` table NOT created |
| 2 | scheduler disabled (today) | no change in behaviour (was never writing) |
| 3 | scheduler re-enabled later | RAM cache populated, no QuestDB table write |

## Per-item guarantee matrix
Cross-refs `.claude/rules/project/per-wave-guarantee-matrix.md`. Honest envelope:
bounded removal of a dormant table's create+write path; no hot-path change; the
strategy-facing RAM cache + prev-OI are untouched; regression-safe via the
existing scheduler + boot-isolation guards staying green.
