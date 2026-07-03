# Implementation Plan: SLO tick_freshness — fractional coverage instead of binary worst-gap

**Status:** APPROVED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — standing directive: "go ahead with the plan always; no false alarms; everything automated"

> Guarantee matrices: cross-reference `.claude/rules/project/per-wave-guarantee-matrix.md`
> (15-row + 7-row) — this is a single-file, cold-path, one-crate fix; the rows that
> apply are named in the Test Plan / Observability sections below; all others are
> N/A — cold-path observability-only change, no new table, no hot-path code, no new
> failure mode beyond the one being fixed.

## Design

Root cause (D2 investigation 2026-07-03, verified against live AWS EMF lines +
app logs): the `tick_freshness` input to the SLO composite score at
`crates/app/src/main.rs` (SLO scheduler, "---- Tick_freshness ----" block) was
BINARY — `0.0` if the WORST tick gap across the ENTIRE tick-gap-detector map was
≥ 30s (only INDIA VIX SID 21 excluded). That design fit the 4-SID Indices4Only
era. Under the ~775-SID DailyUniverse, ~33 illiquid/suspended SIDs are always
silent (NT-15 seeding, 2026-07-01, guarantees every subscribed SID is a map key
whose gap grows monotonically from boot), so the dimension was a permanent 0
during market hours → the multiplicative score was flat 0.0 all session →
`tv-prod-realtime-guarantee-critical` fired all day — noise that masks real
degradation.

Fix (minimal, honest — option 2 of the D2 note, smallest diff matching the
multiplicative-score design intent): replace the binary input with fractional
coverage computed by a new pure function in `crates/app/src/main.rs`:

```
compute_tick_freshness(silent, universe) = clamp(1 − silent/universe, 0, 1)
```

- silent = count of map entries with gap ≥ `SLO_TICK_FRESHNESS_DEGRADED_SECS`
  (30s, unchanged) during market hours, EXCLUDING
  `SLO_TICK_FRESHNESS_EXCLUDED_SIDS` (INDIA VIX = 21, unchanged).
- universe = tick-gap-detector map size (`TickGapDetector::len()` — the NT-15
  seeded subscribed universe).
- Off-hours pin to `1.0` — unchanged.
- Silent-set enumeration uses the existing `scan_gaps_top_n(now, universe)`
  (bounded O(n log cap), O(cap) space) on the 10s SLO scheduler — a cold-path
  task, NOT the tick hot path (flagged honestly: the walk is O(universe), the
  same scan the 60s coalescer already performs; no new hot-path allocation).
- 33/775 silent → 0.957 → Healthy; a genuine broad outage (dead socket →
  most SIDs silent) still collapses the dimension toward 0 → Critical pages.
- Weakest-dimension reporting (SLO-02 payload via `evaluate_slo_score` in
  `crates/core/src/instrument/slo_score.rs`) is UNCHANGED — the multiplicative
  formula and its ratchet tests are untouched; only the caller-side input
  computation in crates/app changes.
- The retired `SLO_TICK_FRESHNESS_SCAN_TOP_N = 10` constant is deleted (its
  only consumer was the binary check).

## Edge Cases

- Empty detector map (pre-seed boot window): `universe == 0` → returns `1.0`
  ("no evidence of staleness"); the 60s uniform boot grace covers the same
  window anyway.
- `silent > universe` (racing map growth between `len()` and the scan):
  clamped to `0.0`, never negative.
- All SIDs silent: exact `0.0` — broad outage still zeroes the dimension.
- INDIA VIX silent: excluded from the numerator (as before); its ≤1-row
  presence in the denominator skews the fraction by ≤ 1/775 ≈ 0.13% —
  documented, negligible, deliberate (avoids a per-SID map lookup).
- Detector not installed (`global_tick_gap_detector()` returns None): `1.0`,
  same fail-open behaviour as before (binary path returned worst_gap 0 → 1.0).
- Off-hours / holidays: pinned `1.0` via the existing `in_market` gate —
  unchanged (verified live: score reads 1 off-hours).
- Gap entries below 30s cannot appear (detector threshold
  `TICK_GAP_THRESHOLD_SECS_DEFAULT = 30` equals the SLO threshold); the
  explicit `gap >= SLO_TICK_FRESHNESS_DEGRADED_SECS` filter pins the semantic
  even if the detector threshold ever diverges.

## Failure Modes

- Regression back to binary semantics: caught by the 6 new unit tests pinning
  the fractional math (`test_compute_tick_freshness_*` in main.rs tests).
- False-healthy on a real outage: impossible for broad outages — 700/775
  silent → 0.097 < 0.80 Critical band (pinned by
  `test_compute_tick_freshness_broad_outage_drops_below_critical_band`). A
  NARROW outage (e.g. exactly one liquid SID dies) no longer zeroes the score —
  that is the intended honest trade-off; per-SID silence remains covered by the
  existing WS-GAP-06 tick-gap coalescer Telegram digest + the
  `tv_tick_gap_instruments_silent` gauge, which are unchanged.
- NaN/degenerate values: `compute_tick_freshness` clamps; downstream
  `evaluate_slo_score` + the `dim_clamp` gauge guard both clamp again
  (defense-in-depth, unchanged).
- Scheduler stall from the scan: the scan is bounded (O(universe log universe),
  universe ≤ 1200 by `MAX_DAILY_UNIVERSE_SIZE`) on a 10s cold-path tick — no
  hot-path impact; DHAT/bench budgets do not apply (not a hot path, flagged
  per O(1)-honesty rule §4).

## Test Plan

- `cargo check -p tickvault-app` — compile gate.
- `cargo test -p tickvault-app --lib` (scoped per `testing-scope.md`; change is
  app-crate only) — includes the 6 new unit tests:
  - `test_compute_tick_freshness_fractional_33_of_775_stays_healthy` (the live
    D2 signature: 33/775 → 0.957 > 0.95)
  - `test_compute_tick_freshness_zero_silent_is_one`
  - `test_compute_tick_freshness_all_silent_is_zero`
  - `test_compute_tick_freshness_empty_universe_is_one`
  - `test_compute_tick_freshness_clamps_when_silent_exceeds_universe`
  - `test_compute_tick_freshness_broad_outage_drops_below_critical_band`
- Existing `slo_score.rs` ratchet tests (multiplicative formula, weakest
  dimension, clamps) run unchanged in CI — the formula is untouched.
- Full 22-category battery runs post-merge in CI as always.

## Rollback

Single-commit revert (`git revert <sha>`) restores the binary worst-gap
computation verbatim — no schema change, no config change, no migration, no
state. The alarm returns to its prior flat-firing behaviour, nothing else is
affected.

## Observability

- No metric names change: `tv_realtime_guarantee_score` and
  `tv_realtime_guarantee_dimension{name="tick_freshness"}` now carry fractional
  values in (0,1] during market hours instead of a flat 0 — the CloudWatch
  fallback filter `tv-prod-realtime-guarantee-score-fallback` and the alarm
  `tv-prod-realtime-guarantee-critical` (Minimum < 0.80, 2×60s) read the same
  fields and need NO terraform change (D2 verified the filter reads the true
  value correctly).
- Expected post-deploy behaviour: the alarm returns to OK during market hours
  under normal ~33/775 silence; it still fires on genuine broad degradation
  (score < 0.80).
- SLO-02/SLO-01 edge-triggered `warn!`/`info!` logs with `weakest` payload are
  unchanged (Telegram-suppressed per the 2026-05-11 operator directive).
- Per-SID silence visibility is unchanged: `tv_tick_gap_instruments_silent`
  gauge + the WS-GAP-06 60s coalesced digest continue to name silent SIDs.

## Plan Items

- [x] Replace binary tick_freshness input with fractional coverage
  - Files: crates/app/src/main.rs
  - Tests: test_compute_tick_freshness_fractional_33_of_775_stays_healthy,
    test_compute_tick_freshness_zero_silent_is_one,
    test_compute_tick_freshness_all_silent_is_zero,
    test_compute_tick_freshness_empty_universe_is_one,
    test_compute_tick_freshness_clamps_when_silent_exceeds_universe,
    test_compute_tick_freshness_broad_outage_drops_below_critical_band

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | 33 of 775 SIDs silent (live D2 signature) | tick_freshness ≈ 0.957 → score Healthy; alarm OK |
| 2 | 0 silent | 1.0 |
| 3 | All 775 silent (dead socket) | 0.0 → score 0 → Critical alarm fires |
| 4 | 700/775 silent (broad outage) | 0.097 < 0.80 → Critical alarm fires |
| 5 | Off-hours | pinned 1.0 (unchanged) |
| 6 | Empty detector map at boot | 1.0 (plus 60s uniform boot grace) |
| 7 | INDIA VIX silent | excluded from numerator (unchanged) |
