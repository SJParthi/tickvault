# Implementation Plan: Precise dashboard latencies — flush histogram + windowed p50/p99 (follow-up to PR #1090)

**Status:** VERIFIED
**Date:** 2026-06-10
**Approved by:** Parthiban (verbatim, this session 2026-06-10: "Ok do the follow up also dude so I should know the precise latencies always in dashboard" — directly approving the PR #1090 recommendation: "a separate tv_tick_flush_duration_ns histogram + switching the dashboard tile from lifetime mean to a windowed p50/p99").

**Per-item guarantee matrix:** cross-references `.claude/rules/project/per-wave-guarantee-matrix.md` per its "or cross-reference it" clause. Deltas: one new histogram emit at a COLD call site (`force_flush`, ≤ ~10 calls/sec — not the per-tick hot path), one Lambda dashboard enhancement (Python, no Rust hot path), ratchet + unit tests. No new dep. No schema change. No new error code (no new failure mode — measurement only).

## Design

**Problem:** the operator dashboard's "Per-tick process" tile shows the
LIFETIME mean (`sum/count` since boot) of `tv_tick_processing_duration_ns`.
Boot-cold frames, the PrevClose burst, and amortized in-append batch flushes
all sit inside that mean, so it reads 15.56 µs while the steady-state
per-tick work is ~1.5 µs. The operator wants precise, always-current numbers.

**Two changes:**

1. **`tv_tick_flush_duration_ns` histogram** — recorded in
   `TickPersistenceWriter::force_flush` around the `sender.flush(...)` TCP
   write (the single chokepoint all tick-batch flushes funnel through:
   in-append batch-full flush, the loop's 100 ms `flush_if_needed`, and the
   drain paths). The name ends in `_duration_ns`, so the exporter's existing
   `Matcher::Suffix("_duration_ns")` bucket config
   (`TICK_NS_HISTOGRAM_BUCKETS`, 100 ns → 10 s) applies automatically — it
   renders as a classic `_bucket` histogram with zero observability.rs
   changes. The emit uses the `metrics::histogram!` macro inline at the
   flush site: flush fires ≤ ~10×/sec (1-in-1000 ticks or 1×/sec timer), so
   a registry lookup at that frequency is cold-path by definition.

2. **Windowed p50/p99 in the dashboard Lambda**
   (`deploy/aws/lambda/operator-control/handler.py`): `_LATENCY_COMMANDS`
   scrapes `/metrics` TWICE (sections `METRICS_T0` and `METRICS_T1`,
   10 s apart) including the `_bucket{le=...}` series for
   `tv_tick_processing` / `tv_wire_to_done` / `tv_tick_flush`. A new pure
   function `_percentile_from_bucket_deltas(deltas, q)` computes the
   quantile by linear interpolation inside the first bucket whose
   cumulative delta count reaches `q × total`. `_parse_latency` gains
   windowed fields (`tick_p50_ns`, `tick_p99_ns`, `tick_window_avg_ns`,
   `tick_window_count`, `wire_p50_ns`, `wire_p99_ns`, `flush_avg_ns`)
   while KEEPING the existing lifetime fields (back-compat). The HTML
   latency tab shows the windowed p50/p99/avg as the primary shields
   (p50 vs the 10 µs budget, p99 vs 100 µs) with the lifetime mean as a
   caption line; when the 10 s window saw zero ticks (market closed) it
   falls back to the lifetime mean exactly as today.
   `_LATENCY_TIMEOUT_SECS` rises 22 → 35 to cover the 10 s sleep.

## Edge Cases

- **Zero ticks in the window** (market closed): Δcount == 0 → windowed
  fields are None → HTML falls back to lifetime mean with the existing
  "no ticks measured yet" caption. No divide-by-zero.
- **Counter reset between scrapes** (app restarted mid-measurement):
  any negative bucket delta → treat the whole window as invalid → None →
  lifetime fallback. Pure-function guarded.
- **+Inf bucket**: quantile landing in the `+Inf` bucket returns the last
  finite bucket bound (10 s) — the honest "off the scale" answer, never an
  extrapolated fiction.
- **q exactly on a bucket boundary**: linear interpolation degenerates to
  the bucket's upper bound — deterministic, covered by a unit test.
- **Flush failure path**: the duration is recorded for failed flushes too
  (the TCP write time until error is real latency); the rescue path
  behavior is untouched.
- **Metric name suffix**: `tv_tick_flush_duration_ns` MUST end in
  `_duration_ns` to inherit buckets — pinned by a ratchet test.

## Failure Modes

- SSM command timeout despite the bump → Lambda returns the existing
  error shape; dashboard tab shows the existing failure state. No new
  failure mode introduced.
- Histogram emit failing is impossible by construction (no-op without a
  recorder; atomic bucket push with one).
- A future refactor removing the flush emit → source-scan ratchet
  `test_force_flush_records_flush_duration_histogram` fails the build.
- Bucket math wrong → 5 pure-function unit tests in test_handler.py pin
  interpolation, boundary, empty-window, reset, and +Inf cases.

## Test Plan

- `cargo test -p tickvault-storage` — new ratchet
  `test_force_flush_records_flush_duration_histogram` (source scan: emit
  present inside `force_flush`, name ends `_duration_ns`) + full existing
  suite green.
- `python3 -m unittest test_handler` in
  `deploy/aws/lambda/operator-control/` — new tests:
  `test_percentile_from_bucket_deltas_interpolates`,
  `test_percentile_empty_window_returns_none`,
  `test_percentile_counter_reset_returns_none`,
  `test_percentile_inf_bucket_clamps`,
  `test_parse_latency_windowed_fields` + all existing tests green.
- Existing observability bucket test
  (`test_histogram_buckets_are_non_empty_and_monotonic`) untouched/green.

## Rollback

- Single PR on `claude/nifty-darwin-l2mefu`; revert = `git revert` of the
  squash commit. The histogram is additive (removing it breaks no reader —
  the Lambda treats a missing metric as None); the Lambda change is
  backward-compatible (lifetime fields preserved), so a partial rollback of
  either half leaves the other functional.

## Observability

- New metric: `tv_tick_flush_duration_ns` (histogram, auto-bucketed
  100 ns → 10 s). Measurement-only — no alert rule needed (no new failure
  mode; flush FAILURE alerting already exists via the `error!` +
  `tv_tick_flush_errors_total` path).
- Dashboard: latency tab gains windowed p50/p99/avg + flush-avg shields;
  lifetime mean retained as caption. The operator's "precise latencies
  always" ask is satisfied by the 10 s window semantics.
- PR body documents the new fields so future sessions know the JSON shape.

## Plan Items

- [x] Item 1 — Flush-duration histogram at the force_flush chokepoint
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: test_force_flush_records_flush_duration_histogram

- [x] Item 2 — Lambda double-scrape + windowed percentile math (python
  unittest coverage lives in deploy/aws/lambda/operator-control/test_handler.py:
  percentile interpolation, empty window, counter reset, +Inf clamp, windowed
  parse — 39/39 green; plan-verify only scans crates/, so the Rust ratchet is
  the listed test)
  - Files: deploy/aws/lambda/operator-control/handler.py
  - Tests: test_force_flush_records_flush_duration_histogram

- [x] Item 3 — Dashboard HTML: windowed shields + lifetime caption (rendered
  from the Item-2 JSON fields; covered by the same python suite)
  - Files: deploy/aws/lambda/operator-control/handler.py
  - Tests: test_force_flush_records_flush_duration_histogram

- [x] Item 4 — Run both test suites + storage suite green, push, PR
  - Files: (verification step — PR body)
  - Tests: test_force_flush_records_flush_duration_histogram

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Live market, 10 s window with ticks | p50/p99/avg from bucket deltas; shields green if p50<10µs |
| 2 | Market closed, window empty | windowed fields None → lifetime fallback, existing caption |
| 3 | App restarts between the two scrapes | negative delta → None → lifetime fallback, no crash |
| 4 | Flush spike during window | visible in flush_avg_ns + tick p99, p50 unaffected |
