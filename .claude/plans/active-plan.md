# Implementation Plan: Tick-conservation ledger (proof no tick is lost)

**Status:** APPROVED
**Date:** 2026-06-05
**Approved by:** Parthiban ("go ahead with whatever is recommended")
**Crate(s) touched:** `core`

## Context (verified)

The tick processor's tick branch has exactly these terminal outcomes per entry
(verified by reading `tick_processor.rs` 850–1270; integrity/late-tick/day_close
checks only *count*, they do not drop):

`ticks_processed == ticks_persisted + storage_errors + junk_ticks_filtered +
stale_day_filtered + outside_hours_filtered + dedup_filtered`

The loop is synchronous (each tick fully resolved before the next), so at the
existing 60s periodic check there are **zero in-flight ticks** → the identity is
**exact**. A correct system holds residual == 0 forever; any positive residual
delta in a window = ticks entered but vanished unaccounted = a real leak.

## Plan Items

- [ ] Item 1 — Pure, unit-tested conservation math
  - Files: `crates/core/src/pipeline/tick_processor.rs`
  - Tests: `conservation_tests::*`
- [ ] Item 2 — Wire the per-window leak check into the existing 60s block
  - Files: `crates/core/src/pipeline/tick_processor.rs`

## Design

1. Pure fn `tick_conservation_residual(processed, persisted, storage_errors,
   junk, stale_day, outside_hours, dedup) -> i64` = `processed - sum(outcomes)`.
   0 = balanced; >0 = unaccounted (leak); <0 = double-count bug.
2. Pure fn `conservation_leak_delta(current_residual, last_residual) -> i64`.
3. In the existing 60s periodic block (only when `tick_writer.is_some()`, since
   persistence-conservation is meaningless without a writer): compute the
   residual, compare to the previous window's residual; if the **delta is > 0**
   (ticks entered this window that reached no known terminal), emit
   `error!("TICK CONSERVATION LEAK …", unaccounted=delta, residual, …)` (Telegram
   via the ERROR sink) + `counter!("tv_tick_conservation_leak_total")`. Always log
   the residual at `info!` when the window had activity, so the books are visible
   even at 0 (positive false-OK avoidance, audit Rule 11).

## Edge Cases

- **No writer** (`tick_writer` None — tests / non-persist config) → skip the
  check (persistence-conservation undefined). Pinned by guarding on `is_some()`.
- **Counter wrap** → all accumulators are `u64`; residual uses `i64`; a wrap
  would surface as a large negative (double-count) which is itself alarming.
- **Constant benign offset** → we alert on the **delta**, not the absolute, so a
  flat residual never pages; only a *growing* gap (active loss) does.
- **First window** → `last_residual` seeded to the first computed residual, so
  the first delta is 0 (no spurious page at startup).

## Failure Modes

- Missed a drop reason in the identity → would show as a steady positive residual
  delta = false page. Mitigated: the identity was enumerated from source; the
  `info!` residual line makes a constant offset obvious for quick correction; and
  the check is delta-based so only *active* divergence pages.
- The check itself is cold-path (60s), not per-tick → no hot-path / O(1) risk.

## Test Plan

- `cargo test -p tickvault-core tick_processor` → `conservation_tests` green:
  balanced → 0; one unaccounted → +1; double-count → negative; delta math.
- `cargo fmt --check`; design-first wall (impl crate `core` + this APPROVED plan
  referencing it); pub-fn-test + pub-fn-wiring guards (pure fns get tests + the
  60s block is their call site).

## Rollback

Single-file change. Revert the commit: remove the two pure fns + the 60s-block
check + the one counter. No schema, no data, no hot-path behaviour change.

## Observability

New `tv_tick_conservation_leak_total` counter + a per-window `info!` residual line
+ an `error!` (Telegram) on any active leak. This IS the proof layer the operator
asked for: it makes "no tick lost between entry and a known outcome" a live,
measured fact, not a claim.

## Per-Item Guarantee Matrix (cross-reference)

This plan and every item in it are bound by the 15-row 100% guarantee matrix and
the 7-row resilience demand matrix — see
`.claude/rules/project/per-wave-guarantee-matrix.md`. All 15 + 7 rows apply.

**Honest envelope (per `wave-4-shared-preamble.md` §8):** any "100%" wording here
means "100% inside the tested envelope, with ratcheted regression coverage" — the
envelope being the pure conservation-math unit tests + the live delta check. The
ledger proves no tick is lost **between tick-branch entry and a known terminal
outcome**; it is cold-path (60s) so it carries NO O(1) / nanosecond / hot-path
claim. It does not (and cannot) prove the network delivered every Dhan packet —
that bound is the WAL frame spill, tracked separately. No illusion.
