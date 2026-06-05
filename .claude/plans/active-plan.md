# Implementation Plan: Cross-verify visibility + on-demand dry-run

**Status:** APPROVED
**Date:** 2026-06-05
**Approved by:** Parthiban ("yes go ahead dude")
**Crate(s) touched:** `app`

## Plan Items

- [x] Item 1 — Pure, unit-tested scheduling decision in the `app` crate
  - Files: `crates/app/src/cross_verify_1m_boot.rs`
  - Tests: `start_decision_tests::*` (8 tests)
- [x] Item 2 — Wire `TICKVAULT_CROSS_VERIFY_NOW` on-demand override into boot
  - Files: `crates/app/src/main.rs`
- [x] Item 3 — `make cross-verify-show` (human-visible CSV window) + script
  - Files: `Makefile`, `scripts/cross-verify-show.sh`
- [x] Item 4 — `make cross-verify-now` (on-demand run)
  - Files: `Makefile`
- [x] Item 5 — Operator runbook
  - Files: `docs/runbooks/cross-verify-1m.md`

## Design

The post-market 1-minute cross-verification already exists, is wired in
`crates/app/src/main.rs`, and runs at 15:31 IST (`run_cross_verify_1m`). The
gap is purely operator **visibility** and the ability to **prove** it without
waiting for 15:31 on a live day. We add three things, reusing the existing,
proven verification with zero changes to its comparison logic:

1. A pure function `decide_cross_verify_start(now_secs_of_day, is_trading_day,
   force_now) -> CrossVerifyStart` in the `app` crate that replaces the inline
   scheduling. It is fully unit-tested. `main.rs` reads
   `TICKVAULT_CROSS_VERIFY_NOW` and passes `force_now`.
2. `make cross-verify-show` → `scripts/cross-verify-show.sh`: prints the latest
   `data/cross-verify/*.csv` or an honest "not run yet + where else to look".
3. `make cross-verify-now`: boots with the env var so the verification runs
   immediately. A runbook documents all four human-visible surfaces.

## Edge Cases

- **Forced run on a non-trading day / quiet day** → `force_now` overrides the
  gate, but the verification is fail-soft on empty `candles_1m`, so it yields an
  empty/degraded report, never fabricated rows.
- **No report on disk** → `cross-verify-show.sh` prints why (not 15:31 yet / dev
  box / booted late) instead of an empty/confusing result.
- **Dev box with no live creds** → `make cross-verify-now` stops at auth; the
  Makefile target says so up-front (no false promise).
- **Exact 15:31:00 boundary** → `>=` trigger means a boot at exactly 15:31 skips
  the schedule (mid-evening boot), matching prior behaviour. Pinned by a test.

## Failure Modes

- Wrong seconds-of-day math → the pure function has boundary tests
  (15:30 → sleep 60; 15:31 → skip; force always RunNow).
- Env var typo / casing → accepts `1` or `true` (case-insensitive); anything
  else = off (no accidental forced runs in prod).
- Behaviour drift in the real run is unchanged (we did not touch
  `run_cross_verify_1m` or the comparison/CSV/audit code).

## Test Plan

- `cargo test -p tickvault-app cross_verify_1m_boot` → 8 new decision tests +
  existing module tests green.
- `bash -n scripts/cross-verify-show.sh` + a live run → prints the honest
  "no report yet" message (verified).
- `cargo fmt --check` + design-first wall self-exemption (this diff touches one
  impl crate `app` with an APPROVED plan referencing it).

## Rollback

Self-contained. Rollback = revert the commit: delete the two `make` targets +
`scripts/cross-verify-show.sh` + the runbook, and restore the inline scheduling
block in `main.rs` (the pure function + tests can stay harmlessly, or be
reverted too). No schema, no data, no runtime/hot-path impact.

## Observability

Reuses the existing surfaces: `CROSS-VERIFY-1M-01/02` error codes, the
`cross_verify_1m_audit` QuestDB table, the per-day CSV, and the Telegram
summary. A new `"running on-demand NOW"` info log marks forced runs so they are
distinguishable from the scheduled 15:31 run in the logs. No new metric/table
needed — honest scope: this is cold-path operator tooling, not a hot path, so
no O(1) / zero-tick-loss claims apply.

## Per-Item Guarantee Matrix (cross-reference)

This plan and every item in it are bound by the 15-row 100% guarantee matrix
and the 7-row resilience demand matrix — see
`.claude/rules/project/per-wave-guarantee-matrix.md`. All 15 + 7 rows apply to
every item in this plan.

**Honest envelope (per `wave-4-shared-preamble.md` §8):** any "100%" wording
here means "100% inside the tested envelope, with ratcheted regression
coverage" — the envelope being the 8 pure-decision unit tests + the 16 existing
cross-verify tests + the design-first wall gate. This change is cold-path
operator tooling and carries NO O(1) / zero-tick-loss / hot-path guarantee,
because it does not touch the hot path; asserting otherwise would be the
hallucination the operator forbids.
