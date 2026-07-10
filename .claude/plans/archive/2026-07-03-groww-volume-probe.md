# Implementation Plan: Groww RAW-TICK-PROBE — settle live `volume` field population (log-only)

**Status:** APPROVED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator directive this session: "check whether not even a single live feed has volume")

> **Guarantee matrices:** this plan cross-references
> `.claude/rules/project/per-wave-guarantee-matrix.md` (the canonical 15-row
> 100% Guarantee Matrix + 7-row Resilience Demand Matrix). Log-only Python
> sidecar change — most rows are N/A (no Rust, no hot path, no schema, no
> emission change); the applicable rows are proven in the Test Plan +
> Observability sections below.

## Plan Items

- [x] Bounded raw-tick sampler + running population counters + summary/heartbeat lines
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: RawTickProbeTests (test_dedup.py), ProbeFieldNonzeroTests, _selftest_raw_probe
- [x] Update the misleading cum_volume comment (Option A claim is Assumed, not Verified)
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: n/a (comment-only)
- [x] Unit tests + selftest extension
  - Files: scripts/groww-sidecar/test_dedup.py, scripts/groww-sidecar/groww_sidecar.py
  - Tests: test_ltp_sampling_bounded_at_n, test_summary_fires_once_with_correct_counts, test_running_counters_cover_every_drained_tick, test_always_mode_rearm_resamples_but_keeps_counters, test_heartbeat_bounded_cadence

## Design

**Question to settle:** growwapi==1.5.0 `get_ltp()` returns the FULL 16-field
decoded `StocksLivePriceProto` dict per instrument (tsInMillis, OHLC, volume,
openInterest, avgPrice, …) even though Groww's docs list only tsInMillis+ltp.
Our sidecar reads only ltp+tsInMillis and hardcodes `cum_volume: 0` ("Option
A", comment: "Groww live feed carries no volume"). Proto3 doubles have NO
presence — `volume: 0.0` appears whether the server omits it or sends zero —
so ONLY inspecting real live tick dicts tells us if the server populates it.

**The change (log-only, ZERO behavior change to emission/persistence):**

1. `RawTickProbe` class in `groww_sidecar.py`: for the FIRST
   `RAW_PROBE_SAMPLE_COUNT = 8` stock/FNO tick dicts and the first
   `RAW_PROBE_INDEX_SAMPLE_COUNT = 2` index dicts observed after subscribe,
   print ONE stderr line each:
   `RAW-TICK-PROBE kind=<ltp|index> token=<…> fields=<compact json>` — then
   ONE `RAW-TICK-PROBE-SUMMARY nonzero_volume_ticks=X/Y nonzero_oi=A/Y
   nonzero_ohlc=B/Y` line scanning those samples.
2. Cheap process-lifetime running counters (two float compares + int
   increments per drained tick, O(1)) of how many drained stock/FNO ticks had
   `volume != 0` / `openInterest != 0`, surfaced via a bounded
   `RAW-TICK-PROBE-HEARTBEAT` stderr line at most once per 300s — so even if
   the first 8 ticks are all zero we learn whether ANY tick all day carries
   volume. (No existing periodic stderr stats line exists; the heartbeat is
   the minimal new one, cadence-checked once per walker iteration.)
3. Probe observes inside the walker thread's drain paths
   (`emit_ltp_records`/`emit_index_records`, BEFORE drop/dedup decisions) —
   the O(1) NATS callbacks (the 2026-07-03 lag-fix contract) are UNTOUCHED.
4. `GROWW_RAW_PROBE=always` env re-arms the bounded sampler on every
   reconnect cycle (call site: after `write_status("subscribed", …)` in the
   reconnect loop). Default: sample once per process. Running counters are
   NEVER reset.
5. The `cum_volume` comment now says: docs list no volume; SDK exposes a
   volume field — population under live probe; emission unchanged pending
   verdict.

## Edge Cases

- **Proto3 no-presence:** `volume: 0.0` is ambiguous per-tick; the probe's
  verdict comes from the DAY-LONG running counters + raw sample dicts, not a
  single tick. `probe_field_nonzero` treats absent key / None / garbage as
  "not populated" and never raises.
- **Fewer than 8 ticks ever arrive:** the summary never prints (bounded
  probe); the heartbeat counters still carry the signal.
- **Index dicts** (`StocksLiveIndicesProto` = ts+value only) can never carry
  volume — they are sampled (2) for shape proof but never counted in the
  ltp running counters.
- **Non-dict leaf** (defensive tree shapes): ignored, no line, no count.
- **Reconnect cycles:** default mode keeps once-per-process semantics (no
  re-spam); `always` mode re-samples the first N of every cycle for a
  multi-session capture; counters continue monotonically either way.
- **JSON-unserializable value in a tick dict:** `default=str` + repr fallback
  — the sample line always prints.

## Failure Modes

- **Probe raises inside the walker:** all probe state is plain ints/lists on
  one thread; `observe` guards non-dict input; `_emit_sample` wraps
  json.dumps. Even a hypothetical raise is caught by the walker loop's
  existing broad per-iteration except (walk retries next interval; dedup
  makes retries idempotent) — capture/emission can never be lost to the
  probe.
- **Log volume risk:** hard-bounded — max 8+2 sample lines + 1 summary per
  arming, heartbeat ≤ 1/300s. No per-tick printing beyond the bound.
- **Secret-leak risk:** none — sample lines carry market-data fields only
  (the decoded proto dict has no credential); stderr is the same
  supervisor-captured stream the existing shape logs use.
- **Behavior regression risk:** zero writes changed — `_write_record`,
  dedup, drop accounting, watermark, status file are byte-identical paths;
  only stderr lines and the comment changed.

## Test Plan

- `python3 -m unittest test_dedup -v` (scripts/groww-sidecar) — 44 tests
  green including the new `ProbeFieldNonzeroTests` +
  `RawTickProbeTests` (bounded at N; full-dict JSON line; summary once with
  correct counts; no summary before N; index bound separate; running
  counters over 100 ticks are plain ints; non-dict ignored; default-mode
  rearm no-op; always-mode rearm re-samples but keeps counters; env string
  parsing; heartbeat cadence).
- `python3 groww_sidecar.py --selftest` (with the growwapi wheel) — all 5
  selftests PASS including the new `_selftest_raw_probe`.
- Hooks: `banned-pattern-scanner.sh`, `plan-gate.sh`,
  `per-item-guarantee-check.sh`, pre-push fast gates — all green (no Rust
  change; test-count guard counts `#[test]` only).

## Rollback

Single revert of this PR's squash commit restores the previous sidecar
byte-for-byte (log-only change, no schema, no config, no state file format
change). No data migration, no cleanup: the probe writes nothing to disk,
QuestDB, or the status file. `GROWW_RAW_PROBE` unset = default behavior.

## Observability

- **CloudWatch:** log group `/tickvault/prod/app`, filter `"RAW-TICK-PROBE"`
  — matches the per-tick sample lines, the summary, and the heartbeat (the
  Rust supervisor forwards sidecar stderr).
- **Verdict reading:** `RAW-TICK-PROBE-SUMMARY nonzero_volume_ticks=0/8`
  with an end-of-day `RAW-TICK-PROBE-HEARTBEAT nonzero_volume_ticks=0/…`
  ⇒ server does NOT populate volume on the `price.` subject (Option A
  confirmed). Any non-zero ⇒ volume IS populated and a follow-up PR can wire
  `cum_volume` for real (operator decision required).
- No new metrics/alerts/audit tables — deliberately log-only; the probe is
  removed or promoted by a follow-up PR after the verdict.
