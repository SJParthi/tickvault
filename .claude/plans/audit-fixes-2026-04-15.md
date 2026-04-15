# Implementation Plan: Audit Fixes — WS hot path + QuestDB persistence gaps

**Status:** APPROVED
**Date:** 2026-04-15
**Approved by:** Parthiban (via session chat: "Fix everything bro everything looks valid dude")
**Sibling plan:** `active-plan.md` (zero-loss big overhaul — orthogonal, do not block each other)

## Context

Two parallel code audits (WebSocket hot path + QuestDB persistence) surfaced 10
concrete gaps against the "zero tick loss / never stall / never corrupt" bar.
This plan executes those fixes one gap per commit, in blast-radius order, so
each commit is independently bisectable and reviewable. Parthiban answered the
open question on DB-2: take option **(a) full ring+spill** (not fail-fast) to
match the zero-loss stance.

## Plan Items (ordered by blast radius, highest first)

- [x] **DB-3 + DB-4**: Add `segment` to `DEDUP_KEY_MOVERS` — done in commit for DB-3/DB-4
  - Files: `crates/storage/src/movers_persistence.rs`
  - Tests: `test_dedup_key_movers_includes_segment`, `test_dedup_key_movers_exact_format`, `test_dedup_key_movers_columns_exist_in_both_ddls`

- [x] **DB-8**: Fsync deep-depth disk spill writes — done
  - Files: `crates/storage/src/deep_depth_persistence.rs`
  - Tests: `test_spill_durability_write_flush_sync_sequence`, `test_spill_write_all_alone_may_not_be_durable`, `test_spill_open_sync_all_makes_file_discoverable`

- [ ] **DB-5**: Fix indicator_snapshot dedup doc-comment regression bait
  - Files: `crates/storage/src/indicator_snapshot_persistence.rs`
  - Tests: none (doc-only)

- [ ] **DB-6**: Exponential backoff + max attempts on indicator_snapshot + movers reconnect
  - Files: `crates/storage/src/indicator_snapshot_persistence.rs`, `crates/storage/src/movers_persistence.rs`
  - Tests: `test_indicator_snapshot_reconnect_has_backoff`, `test_movers_reconnect_has_backoff`

- [ ] **DB-7**: Flush-failure metric counter + upgrade log level to ERROR
  - Files: `crates/storage/src/indicator_snapshot_persistence.rs`
  - Tests: `test_indicator_snapshot_flush_failure_metric_increments`

- [ ] **DB-1**: Ring-buffer + disk-spill rescue on `IndicatorSnapshotWriter`
  - Files: `crates/storage/src/indicator_snapshot_persistence.rs`
  - Tests: `test_indicator_snapshot_rescues_on_reconnect_fail`, `test_indicator_snapshot_ring_overflow_spills`, `test_indicator_snapshot_drains_fifo_on_recover`

- [ ] **DB-2**: Ring-buffer + disk-spill rescue on `MoversWriter` (stock + option)
  - Files: `crates/storage/src/movers_persistence.rs`
  - Tests: `test_stock_movers_rescues_on_reconnect_fail`, `test_option_movers_rescues_on_reconnect_fail`, `test_movers_ring_overflow_spills`

- [ ] **WS-1**: Watchdog panic supervisor — read loop must not hang on watchdog panic
  - Files: `crates/core/src/websocket/connection.rs`
  - Tests: `test_watchdog_panic_fires_notify_on_unwind`, integration kill-task test

- [ ] **WS-2**: Per-frame-drop metric when WAL is absent
  - Files: `crates/core/src/websocket/connection.rs`
  - Tests: `test_frame_drop_without_wal_increments_counter`

## Commit hygiene

Each plan item = exactly one commit (or two if the test needs a separate file).
Each commit message follows the conventional format and references the audit
gap ID (DB-N / WS-N). No squashing. No amending. CLAUDE.md rule.

## Scenarios (acceptance criteria)

| # | Scenario | Expected result after this plan is VERIFIED |
|---|---|---|
| 1 | NIFTY movers stored as both IDX_I and NSE_EQ on same day | Two distinct rows, no UPSERT collision |
| 2 | `kill -9` of process mid-spill on deep-depth ring overflow | All bytes written before kill are on disk after reboot |
| 3 | QuestDB down for 60s during indicator snapshot window | Zero lost snapshots, FIFO drain on reconnect |
| 4 | QuestDB down for 60s during movers window | Zero lost movers, FIFO drain on reconnect |
| 5 | Indicator snapshot writer reconnect storm (QuestDB flapping) | Bounded reconnect attempts with backoff, not a tight loop |
| 6 | Watchdog task panics due to unexpected state | Read loop detects, logs panic, fires notify, reconnects |
| 7 | Tick processor task killed mid-trading | Per-drop counter increments, Telegram alert, ops notified |

## Plan verification

Before declaring VERIFIED, run `bash .claude/hooks/plan-verify.sh` which
mechanically checks every `[x]` has a matching test function name in the
codebase and every listed file actually got touched.
