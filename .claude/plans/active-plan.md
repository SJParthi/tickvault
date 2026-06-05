# Implementation Plan: Pre-market prev-day fetch — verify + see

**Status:** APPROVED
**Date:** 2026-06-05
**Approved by:** Parthiban (chose option A — "Verify + see existing fetch")
**Crate(s) touched:** `app`

## Plan Items

- [x] Item 1 — Pure coverage-verification helpers in the `app` crate
  - Files: `crates/app/src/prev_day_ohlcv_boot.rs`
  - Tests: `coverage_tests::*` (9 tests)
- [x] Item 2 — Wire verify + CSV write after the boot fetch
  - Files: `crates/app/src/main.rs`
- [x] Item 3 — `make prev-day-show` + script (human-visible surface)
  - Files: `Makefile`, `scripts/prev-day-show.sh`
- [x] Item 4 — Operator runbook
  - Files: `docs/runbooks/prev-day-ohlcv.md`

## Design

The pre-market prev-day fetch already exists and is wired
(`run_prev_day_ohlcv_fetch` → `prev_day_ohlcv` table, returning a
`PrevDayFetchSummary { fetched, skipped, failed }`). The gap is **verification**
(did we get yesterday's candle for enough of the universe?) and **visibility**
(a human can't see it). Symmetric to the post-market cross-verify shipped in
#1020, with zero changes to the fetch itself:

1. Pure `evaluate_prev_day_coverage(expected, fetched) -> PrevDayCoverage`
   (`Ok` ≥ 90% / `Degraded` / `Empty`) + `coverage_csv(...)` formatter +
   `write_prev_day_coverage_csv(...)` FS wrapper. All unit-tested.
2. `main.rs` captures `expected = subscription_targets.len()` before the fetch,
   then after it: writes the coverage CSV and logs `info!`/`warn!`/`error!` per
   outcome (degraded/empty route to Telegram via the ERROR/WARN sinks).
3. `make prev-day-show` → `scripts/prev-day-show.sh`: prints the latest
   `data/prev-day/*.csv` or an honest "not run yet + where else to look".
   Runbook documents all surfaces.

## Edge Cases

- **No JWT on a dev box** → the existing fetch skips and returns a zero summary;
  coverage = `Empty`; the CSV records `empty`; `prev-day-show` says so. No fake data.
- **Exactly 90% coverage** → `Ok` (inclusive boundary) — pinned by a test.
- **Zero subscription targets** → `Empty` (guards divide-by-zero; pinned by a test).
- **FS write failure** (read-only `data/`) → `warn!`, fetch result still logged;
  boot never blocks.

## Failure Modes

- Wrong coverage math → boundary tests (full=100/ok, 90/ok, 89/degraded, 0/empty,
  expected=0/empty).
- `error!`/`warn!` here carry NO known code prefix, so the tag-guard needs no
  `code=` field; not a flush/persist/drain phrase, so the level meta-guard is satisfied.
- The fetch's own behaviour is unchanged (we only read its returned summary).

## Test Plan

- `cargo test -p tickvault-app prev_day_ohlcv_boot` → 25 green (9 new + 16 existing).
- `bash scripts/prev-day-show.sh` → prints the honest "no report yet" message (verified).
- `cargo fmt --check`; design-first wall self-check (impl crate `app` + this APPROVED plan referencing it); pub-fn-test guard satisfied (tests named to convention).

## Rollback

Self-contained. Revert the commit: delete the `make` target +
`scripts/prev-day-show.sh` + runbook, and remove the verify/CSV block from
`main.rs` (the pure helpers + tests can stay harmlessly). No schema, no data, no
hot-path impact.

## Observability

A new `PROOF: prev-day OHLCV coverage OK` info line on success; `coverage
DEGRADED`/`EMPTY` warn/error → Telegram on a thin/empty fetch (false-OK guard,
audit Rule 11). The per-day coverage CSV (`data/prev-day/`) is the human
surface; the `prev_day_ohlcv` QuestDB table holds the actual candles. No new
metric/table — cold-path operator tooling.

## Per-Item Guarantee Matrix (cross-reference)

This plan and every item in it are bound by the 15-row 100% guarantee matrix and
the 7-row resilience demand matrix — see
`.claude/rules/project/per-wave-guarantee-matrix.md`. All 15 + 7 rows apply to
every item in this plan.

**Honest envelope (per `wave-4-shared-preamble.md` §8):** any "100%" wording
here means "100% inside the tested envelope, with ratcheted regression
coverage" — the envelope being the 9 new pure-function unit tests + the 16
existing prev-day tests. This change is cold-path operator tooling + a
completeness check; it carries NO O(1) / zero-tick-loss / hot-path guarantee
because it does not touch the hot path. Asserting otherwise would be the
hallucination the operator forbids.
