# Implementation Plan: Cross-verify no-fire verdict + bounded raw-body capture for Dhan charts 200-empty

**Status:** APPROVED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) — coordinator-relayed directive 2026-07-14 ("PART 1 — cross-verify no-fire regression … PART 2 — bounded raw-body capture (the account-condition vs envelope-drift discriminator + Dhan-support evidence)"), executed under `operator-session-defaults-2026-07-10.md`.

> **Guarantee matrices:** this item cross-references the 15-row + 7-row
> matrices in `.claude/rules/project/per-wave-guarantee-matrix.md` /
> `operator-charter-forever.md` §C. Cold-path log-only change: no hot-path
> allocation, no new tick-drop path, no DEDUP/table change, no new WS.

## Design

Two halves, one small PR riding the next 08:30 IST boot deploy.

**Part 1 — cross-verify no-fire (2026-07-14): VERDICT, no code fix.**
Diagnosis (diff-first): the 15:31 IST cross-verify spawn lives ONLY in
`main.rs::spawn_post_market_tasks` (the Dhan LANE seam). PR #1496 flipped
`feeds.dhan_enabled = false` (Dhan live-WS retirement, operator Q1/Q2
2026-07-13) and the Dhan-off boot takes `crates/app/src/dhan_rest_stack.rs`,
whose Phase-5 comment states cross-verify "deliberately stay lane-only per
the Phase A scope". The retirement banner in
`cross-verify-1m-error-codes.md` records the operator-authorized retirement
("with no Dhan live candles there is no live side to compare"). CloudWatch
verification (2026-07-14): boot line "Dhan REST-only stack bring-up
starting" at 08:31:18 IST; ZERO `cross_verify` events all session. NOT a
#1506 regression — #1506's 429 second-pass never gates the spawn. Honest
documentation only (PR body + a dated runbook clause).

**Part 2 — bounded raw-body capture.** For ≥14 days BOTH
`/v2/charts/intraday` and `/v2/charts/historical` return 2xx with zero
parseable candles for ALL SIDs while option-chain + WS work; no 2xx body was
ever logged. Add:
- `crates/common/src/sanitize.rs`: `REST_RAW_BODY_SAMPLE_MAX_CHARS = 600` +
  `capture_rest_raw_body_sample` (the `capture_rest_error_body` pipeline,
  widened bound, shared `capture_rest_body_bounded` internal).
- `crates/app/src/spot_1m_rest_boot.rs`: `spot_1m_fetch_once` returns body +
  `Content-Type`; on the FIRST `empty_no_rows`/`empty_stale` classification
  of the IST day (CAS day-key latch, once/day, unconditional) one structured
  `error!` (`SPOT1M-01`, `stage="raw_body_sample"`) with the 600-char
  redacted sample + total body bytes + Content-Type; the #1524 diagnostics
  probes carry `content_type`/`body_bytes`/`body_sample` per probe entry
  (diagnostics-flag-gated by construction).
- `crates/app/src/cross_verify_1m_boot.rs`: same capture once per RUN
  (`claim_once` AtomicBool, re-armed at `run_cross_verify_1m` start) under
  `CROSS-VERIFY-1M-02` `stage="raw_body_sample"` — dormant while the lane is
  retired; rides any forced/re-enabled run.
- Runbook: dated paragraph in `rest-1m-pipeline-error-codes.md`.

## Edge Cases

- Concurrent per-SID ladders concluding Empty in the same minute: the CAS
  day-key claim guarantees exactly one log line.
- A transient empty rung followed by a Found conclusion: the sample is
  stashed but never logged (logging happens only on the Empty conclusion).
- Day rollover in a long-lived process: the day key (IST nanos / day)
  re-arms the capture next day; test pins same-day stability + boundary.
- Empty body / missing Content-Type header: sample is the empty string,
  content_type `""` — still logged with `body_bytes=0` (the shape itself is
  evidence).
- JWT / credentials echoed in a 2xx body: identical redaction pipeline as
  the 300-char error capture (ratchet-tested at 600).
- Cross-verify forced re-run (`TICKVAULT_CROSS_VERIFY_NOW`): the run-start
  `store(false)` re-arms the once-per-run latch.

## Failure Modes

- The capture itself can never fail the fetch: it is a log-only side effect
  on already-materialized strings; no new `Result` paths, no persist leg.
- The stash allocates one bounded string per target-less 2xx attempt ONLY
  while today's slot is unclaimed (cold path, ≤4 SIDs × few rungs/minute,
  and zero work after the day's capture fires).
- No new ErrorCode: both emits reuse `Spot1m01FetchDegraded` /
  `CrossVerify1m02FetchDegraded` with a new `stage` value (documented in the
  runbook), so the tag-guard and cross-ref tests stay green.

## Test Plan

- `sanitize::tests`: `test_capture_rest_raw_body_sample_truncates_to_600_chars`,
  `test_capture_rest_raw_body_sample_redacts_jwt`,
  `test_capture_rest_raw_body_sample_passes_columnar_envelope_through`.
- `spot_1m_rest_boot::tests`: `test_raw_body_capture_day_key_is_stable_within_a_day`,
  `test_raw_body_capture_due_pure_semantics`,
  `test_try_claim_raw_body_capture_fires_exactly_once_per_day_key`.
- `cross_verify_1m_boot::tests`: `test_claim_once_fires_exactly_once_until_rearmed`.
- `cargo test -p tickvault-app --lib` + `-p tickvault-common --lib` green;
  fmt + clippy `-D warnings -W clippy::perf` clean.

## Rollback

Pure additive log-only change: revert the PR commit(s) — no schema, config,
alarm, or table change to unwind. The 300-char `capture_rest_error_body`
callers are byte-identical in behavior (shared internal, same bound).

## Observability

- New structured lines: `SPOT1M-01 stage="raw_body_sample"` (once per IST
  day) and `CROSS-VERIFY-1M-02 stage="raw_body_sample"` (once per run) —
  errors.jsonl → CloudWatch `/tickvault/prod/app`; log-sink-only (no new
  alarm; the existing SPOT1M-01 escalation edge is untouched).
- Probe entries gain `content_type` / `body_bytes` / `body_sample` fields in
  the existing one-line diagnostics log (diagnostics-gated).
- No new metrics; no counter/alert changes.
