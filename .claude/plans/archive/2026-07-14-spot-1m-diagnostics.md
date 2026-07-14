# Implementation Plan: spot-1m serving-delay diagnostics (empty-class split + one-shot probes)

**Status:** APPROVED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) — urgent diagnostic rider, relayed via the coordinator session 2026-07-14 (the 21/21-empty-minutes morning; must ship before today's 15:46 IST deploy so tomorrow's session has discriminating evidence per-minute)

> **Per-item guarantee matrix:** this plan cross-references
> `.claude/rules/project/per-wave-guarantee-matrix.md` (the 15-row + 7-row
> matrices) per that rule's cross-reference clause. Log-only cold-path
> instrumentation — no hot-path, no order path, no persist-path change.

## Design

The 2026-07-14 morning ran 21/21 session minutes `outcome="empty"` on all 4
spot SIDs (2xx, no transport errors, no 429s, healthy token) WITH the #1499
day-granular window live. Key hypothesis: Dhan serves same-day intraday
candles with a DELAY (the 15:31 cross-verify only proves same-day data works
AT 15:31). This rider instruments the fetcher so tomorrow's per-minute
evidence discriminates vendor serving-delay from request shape:

1. **Empty-classification split (unconditional)** in
   `crates/app/src/spot_1m_rest_boot.rs`: the single `empty` outcome splits
   into `empty_no_rows` (2xx body parsed to ZERO candles for the day) vs
   `empty_stale` (candles present, none at the target minute). `empty_stale`
   measures the SERVING LAG = target minute open − newest candle minute open
   (whole seconds, clamped ≥ 0) into a new `tv_spot1m_serving_lag_ms`
   histogram (dedicated 1 s→6 h buckets via `Matcher::Full` in
   `crates/app/src/observability.rs` — the generic `_ms` 60 s cap would
   collapse every meaningful sample into `+Inf`). The coalesced
   `stage="minute_failed"` log gains `empty_no_rows` / `empty_stale` /
   `rows_in_response` / `last_candle_ist` / `max_serving_lag_secs`.
   Counter labels stay STATIC: `ok | empty_no_rows | empty_stale | error`.
2. **One-shot side-by-side + alternate-shape probes (config-gated,
   LOG-ONLY)**: two probe moments per day (first session fire after boot +
   a configurable second instant, default 11:00 IST), each issuing ≤3
   bounded requests for ONE SID spaced ~300 ms apart: (a) the cross-verify's
   byte-exact day window (both builders share `intraday_request_body` —
   byte-equality unit-pinned against a literal fixture), (b) the previous
   trading day's full window (settled-data serving), (c) same-day with
   `toDate = now`. All bodies + response shapes logged side by side in ONE
   structured `info!` line. Gate: `[spot_1m_rest] diagnostics` in
   `crates/common/src/config.rs` (`Spot1mRestConfig`), serde default OFF,
   `config/base.toml` ON for the investigation window.

## Edge Cases

- A later ladder rung's zero-candle parse must not launder an earlier stale
  view into `no_rows` — the freshest NON-empty 2xx stats win.
- Candles BEYOND the target with the exact target missing (pathological
  gap): lag clamps to 0, never negative.
- Probe due but < 20 s before the next minute boundary → DEFER to the next
  fire (never eats a fetch minute); probe due but no token → defer.
- A mid-session boot after 11:00 IST: the first-fire probe runs first, the
  second follows on a later fire (gating truth-table pinned).
- Previous-trading-day walk is bounded (14 days) and predicate-injected;
  a never-open calendar yields `None` (shape b simply skipped).
- Out-of-day label/`toDate` inputs clamp inside the day, never panic.
- Pre-rider TOMLs (no `diagnostics` keys) keep deserializing byte-identically
  (`#[serde(default)]` on every new field; manual `Default` == serde
  defaults).

## Failure Modes

- A probe request fails (transport / non-2xx / 429): the failure rides the
  same bounded redacted capture (`spot_1m_fetch_once`) into the side-by-side
  line; a 429 increments `tv_spot1m_rate_limited_total`. Probes never retry.
- A pathological probe overrun past a boundary is still counted by the
  existing H2 missed-boundary accounting (the probe runs BEFORE it) — never
  a silent skipped minute.
- Persist / edge / backfill semantics are UNCHANGED: both empty classes
  still count as an empty minute for `minute_fully_failed` and the
  SPOT1M-01 escalation edge; SPOT1M-02 paths untouched.
- Task respawn re-arms the one-shot latches — worst case a few extra
  bounded log-only probes, honestly documented.

## Test Plan

Unit tests (pure fns, `cargo test -p tickvault-app --lib` +
`-p tickvault-common --lib`):
- `test_classify_empty_body_no_rows_vs_stale` — the split.
- `test_serving_lag_secs_math_and_clamp` — lag math + clamp.
- `test_body_stats_rows_and_newest_minute`,
  `test_empty_diagnostics_aggregate_folds_max`,
  `test_ist_nanos_minute_label`.
- `test_probe_crossverify_request_byte_equality_fixture` — probe body ==
  cross-verify body, pinned to a literal serialized-JSON fixture.
- `test_probe_to_now_body_todate_is_now` — shape c differs ONLY in toDate.
- `test_should_run_probe_gating` + `test_probe_has_room_boundary_math` —
  probe gating.
- `test_previous_trading_day_walks_weekends_and_holidays`.
- `test_summarize_probe_candles_shape_and_sentinel`.
- Updated 3-tuple assertions in the existing
  `parse_intraday_columnar_for_minutes` tests.
- `test_spot_1m_rest_diagnostics_defaults_off_with_1100_second_probe`
  (config round-trip, `crates/common/src/config.rs`).
- `test_histogram_buckets_are_non_empty_and_monotonic` extended with
  `SPOT1M_SERVING_LAG_MS_BUCKETS`.
Plus: `cargo fmt --check`, `cargo clippy --workspace -- -D warnings
-W clippy::perf`, the spot wiring guards
(`crates/app/tests/spot_1m_rest_wiring_guard.rs`), and plan-gate.

## Rollback

`git revert` of the single PR restores the pre-split `empty` label and
removes the probes. Runtime rollback without a deploy: set
`[spot_1m_rest] diagnostics = false` (probes off; the classification split
is pure accounting with zero behaviour change and needs no runtime switch).
No schema, no table, no dependency, no hot-path change — revert is clean.

## Observability

- Counter `tv_spot1m_fetch_total{outcome}` gains the static
  `empty_no_rows` / `empty_stale` values (replacing `empty`).
- New histogram `tv_spot1m_serving_lag_ms` (dedicated buckets, /metrics
  local — no CloudWatch EMF entry, ₹0 cost delta).
- Coalesced `SPOT1M-01 stage="minute_failed"` line gains the split +
  `rows_in_response` + `last_candle_ist` + `max_serving_lag_secs` fields.
- ONE structured probe `info!` line per probe moment with every request
  body + response shape side by side.
- Runbook: dated 2026-07-14 paragraph in
  `.claude/rules/project/rest-1m-pipeline-error-codes.md` §1.

## Plan Items

- [x] Empty-classification split + serving-lag histogram + coalesced-log
      fields — Files: crates/app/src/spot_1m_rest_boot.rs,
      crates/app/src/observability.rs — Tests:
      test_classify_empty_body_no_rows_vs_stale,
      test_serving_lag_secs_math_and_clamp,
      test_empty_diagnostics_aggregate_folds_max
- [x] One-shot side-by-side + alternate-shape probes (config-gated,
      log-only) — Files: crates/app/src/spot_1m_rest_boot.rs,
      crates/common/src/config.rs, crates/app/src/main.rs,
      crates/app/src/dhan_rest_stack.rs, config/base.toml — Tests:
      test_probe_crossverify_request_byte_equality_fixture,
      test_should_run_probe_gating, test_probe_has_room_boundary_math,
      test_probe_to_now_body_todate_is_now,
      test_previous_trading_day_walks_weekends_and_holidays,
      test_summarize_probe_candles_shape_and_sentinel,
      test_spot_1m_rest_diagnostics_defaults_off_with_1100_second_probe
- [x] Runbook dated paragraph — Files:
      .claude/rules/project/rest-1m-pipeline-error-codes.md — Tests: n/a
      (docs; error_code_rule_file_crossref stays green)
