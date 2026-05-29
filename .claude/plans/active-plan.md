# Implementation Plan: F&O-master in `instrument_lifecycle` (applicable F&O contracts)

**Status:** APPROVED
**Date:** 2026-05-29
**Approved by:** Parthiban — verbatim 2026-05-29: "go ahead dude … only fno for our applicable fno instruments right dude … if yes go ahead" (clarifying earlier "I asked you to pull ALL the FNO in instruments").

## Intent (one line)

`instrument_lifecycle` must store the **applicable F&O contracts** (the FUTSTK/OPTSTK/FUTIDX/OPTIDX *contracts* belonging to the underlyings we already track — our indices + ~211 F&O underlying stocks), as the never-delete SEBI master. The **WebSocket subscription stays at 331** (indices + spots) — the 2-connection lock is UNTOUCHED. Excludes currency/commodity F&O and non-F&O equities.

## Scope decision (locked with operator 2026-05-29)

| Goes into `instrument_lifecycle` master | Subscribed on WebSocket |
|---|---|
| indices (Index role) | ✅ yes (331) |
| F&O underlying spots (FnoUnderlying role) | ✅ yes (331) |
| **applicable F&O contracts (NEW — FnoContract role)** | ❌ NO (master/audit only) |
| currency/commodity F&O, non-F&O equities | ❌ excluded entirely |

## Plan Items

- [x] Item 1 — Rule-file contract (merged #871) (§15 protocol)
  - Files: `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md` (§5 intro + §10 step 4/6), NEW `.claude/rules/dhan/instrument-master.md`
  - Tests: n/a (docs); cross-ref via existing guards

- [x] Item 2 — Extractor retains applicable F&O contract rows
  - Files: `crates/core/src/instrument/fno_underlying_extractor.rs` (retain FUTSTK/OPTSTK contract rows whose underlying resolves), index F&O collector for FUTIDX/OPTIDX
  - Tests: `fno_underlying_extractor::tests::*` (contracts retained, currency/commodity excluded)

- [x] Item 3 — `DailyUniverse.fno_contracts` field + `FnoContract` role
  - Files: `crates/core/src/instrument/daily_universe.rs`
  - Tests: `daily_universe::tests::*` (contracts present in fno_contracts, NOT in subscription_targets)

- [x] Item 4 — Lifecycle reconcile includes contracts; subscription dispatch does NOT
  - Files: `crates/app/src/today_instrument.rs` (`extract_today_instruments` iterates subscription_targets + fno_contracts), reconcile orchestrator unchanged (reads the expanded set)
  - Tests: orchestrator tests assert lifecycle plan_size = 331 + contracts; subscription planner still 331

- [x] Item 5 — Z+ ratchet
  - Files: `crates/storage/tests/daily_universe_scope_guard.rs` or new guard
  - Tests: pin "lifecycle includes FnoContract rows AND subscription_targets excludes them"

## Z+ matrix (per per-wave-guarantee-matrix.md)

Carried by cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row). Item-specific: O(1) hot path UNAFFECTED (lifecycle is cold/boot table, never read on tick path); uniqueness = composite `(security_id, exchange_segment)` DEDUP (I-P1-11); audit = `instrument_lifecycle_audit` forensic chain; honest envelope = "~219K master rows is MB-scale QuestDB, zero RAM/hot-path impact, boot +few seconds for one-time audit-row burst, WebSocket stays 331".

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Day 1 boot | all applicable F&O contracts appear `active` in lifecycle; WS subscribes 331 |
| 2 | Contract expires | row flips to `expired_contract`, never deleted (§5) |
| 3 | Currency/commodity F&O in CSV | excluded from lifecycle |
| 4 | Subscription dispatch | reads subscription_targets only → still 331 (2-WS lock) |
| 5 | SEBI query "was contract X listed on date Y" | answerable from lifecycle + audit (§25) |
