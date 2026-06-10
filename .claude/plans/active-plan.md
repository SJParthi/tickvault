# Implementation Plan: Per-tick latency hunt — instrument, rank, cut (tv_tick_processing_duration_ns 15.56 µs vs 10 µs budget)

**Status:** APPROVED
**Date:** 2026-06-10
**Approved by:** Parthiban (task directive 2026-06-10, verbatim: "Per-tick latency hunt — dashboard shows 15.56 µs avg vs the 10 µs budget … FIND where the 15.56 µs goes — instrument-or-bench, never guess … Cut ONLY what's proven hot … Prove the win" — the directive explicitly names the candidate cuts and the FORBIDDEN set, so this plan is the execution of an operator-issued task, not a self-initiated scope.)

**Per-item guarantee matrix:** cross-references `.claude/rules/project/per-wave-guarantee-matrix.md` per its "or cross-reference it" clause. Deltas for this PR: one new Criterion bench file (`crates/core/benches/full_tick_processing.rs`), one tiny re-export in `crates/storage/src/lib.rs::tick_persistence_testing` (mirrors the existing `f32_to_f64_clean_pub` pattern), and a clock-read consolidation in `crates/core/src/pipeline/tick_processor.rs` hot loop (no behavior change to filters, dedup, persistence, ring→spill→DLQ, capture_seq, or any FORBIDDEN area). No new workspace dep. No schema change. No new error code.

## Design

**Finding (a) — what the histogram wraps (located, not guessed):**
`tv_tick_processing_duration_ns` is registered at
`crates/core/src/pipeline/tick_processor.rs:718` and recorded at line 1646:
`m_tick_duration.record(tick_start.elapsed().as_nanos() as f64)` where
`tick_start = Instant::now()` at line 932 (immediately after `frame_receiver.recv()`).
The observe call itself is OUTSIDE its own timed window (elapsed is captured
before `.record()`), so "move histogram observe out of the timed region" is
already true — documented honestly, no change needed there.

What IS inside the timed region per tick (Quote/Tick arm):
1. `m_frames.increment` + `current_received_at_nanos()` (chrono Utc::now #1)
2. `dispatch_frame` binary parse
3. heartbeat `chrono::Utc::now().timestamp()` (clock read #2) + relaxed store
4. `tick_gap_detector::record_tick_global(.., Instant::now())` (clock read #3) + papaya insert
5. validity/window/stale-day filters (pure arithmetic)
6. `dedup_ring.is_duplicate` (FNV hash + ring compare)
7. canary 3-element scan (+ `chrono::Utc::now()` clock read #4 when SID matches)
8. optional Greeks enricher; `TickEnricher::enrich_tick` (3 papaya-class lookups)
9. `TickPersistenceWriter::append_tick[_enriched]_with_seq` → `build_tick_row_seq`
   (17-column ILP text row: 6× `f32_to_f64_clean` string round-trips, ryu/itoa
   formatting, FNV payload hash) + **amortized `force_flush()` TCP write every
   `TICK_FLUSH_BATCH_SIZE = 1000` rows — INSIDE the timed region**
10. `broadcast::Sender::send` fan-out (21-TF aggregator consumes OFF this path —
    Engine B is downstream of the broadcast, NOT in the timed region)
11. volume monotonicity guard (HashMap probe)
12. `trace!` events (no per-tick spans exist in this loop)

**Measurement plan (instrument-or-bench, never guess):**
- New Criterion bench `crates/core/benches/full_tick_processing.rs` with
  per-component groups: clock reads (chrono/Instant), parse, gap-detector
  record, enricher, ILP row build, broadcast send, and a
  `writer/append_amortized_flush` group that constructs a REAL
  `TickPersistenceWriter` pointed at a local TCP drain listener (ILP/TCP V1 is
  handshake-free) so the amortized in-region flush cost is measured with real
  syscalls — plus a composite hot-path chain.
- `build_tick_row_seq` is private; expose `build_tick_row_seq_pub` via the
  existing `tickvault_storage::tick_persistence_testing` module (same pattern
  as `f32_to_f64_clean_pub`), with a unit test in storage so the pub-fn test
  guard passes and the bench is the cross-crate call site.
- Run existing `tick_parser`, `pipeline`, `tick_gap_detector` benches for the
  baseline table.

**Cuts (only what the numbers prove hot; candidate set from the directive):**
- Consolidate redundant per-tick clock reads: reuse `received_at_nanos` for the
  heartbeat store and canary gauge (both semantically "now at tick processing"),
  and pass `tick_start` to `record_tick_global` instead of a fresh
  `Instant::now()`. Saves up to 3 clock reads/tick. Applied only if measured.
- Metrics sampling 1-in-N: applied ONLY if the recorder-backed measurement
  proves counters/histograms are a material share of 15.56 µs.
- FORBIDDEN (untouched): ring→spill→DLQ chain, dedup keys, capture_seq
  threading, `crates/trading/src/indicator/`, `crates/trading/src/strategy/`
  (operator boundary §28), the 21-TF aggregator (not in this path anyway).

## Edge Cases

- Bench must run outside market hours: component benches use synthetic
  `ParsedTick` values with crafted `exchange_timestamp` / `received_at_nanos`;
  no component bench depends on wall-clock-now being inside [09:00, 15:30) IST.
- TCP drain listener: bound to 127.0.0.1 ephemeral port; reader thread drains
  and discards; bench ends → listener thread exits on socket close. If the
  connect fails (sandbox restrictions), the writer bench group is skipped with
  an explicit eprintln-free `return` (no panic, no unwrap) so the rest of the
  bench still produces numbers.
- `TICK_FLUSH_BATCH_SIZE = 1000` auto-flush inside append: the writer bench
  iterates ≥10,000 appends so ≥10 flushes amortize into the per-append median.
- Heartbeat consolidation: `received_at_nanos` is UTC nanos; heartbeat stores
  UTC seconds — derive via `/ 1_000_000_000` (integer division, no precision
  loss for positive epoch values). Canary gauge stores epoch seconds as f64 —
  same derivation.
- Gap detector `Instant` reuse: `tick_start` is captured ≤1 µs before the old
  per-call `Instant::now()`; the detector's threshold granularity is 30 s, so
  the substitution is semantically invisible.

## Failure Modes

- Local TCP drain unavailable in CI → writer bench group self-skips (returns
  early), all other groups still run; bench-gate budgets are not registered for
  the new groups yet, so CI cannot fail on absent numbers.
- Clock-read consolidation regressing heartbeat semantics → covered by existing
  no_tick_watchdog tests + a new unit test asserting nanos→secs derivation
  equals `chrono` truth within the same second.
- Bench accidentally allocating in the measured loop → irrelevant to prod (it
  is a bench), but the composite is written allocation-free to keep numbers
  honest.
- If measured numbers show the in-bench path is ALREADY ≤10 µs, the honest
  report states the residual gap is production-side (amortized TCP flush +
  cold-cache/wakeup effects at live tick rates) and quantifies the flush share
  from the drain-listener measurement — no speculative "fix" is shipped for
  unproven components.

## Test Plan

- `cargo test -p tickvault-storage` — new unit test for
  `build_tick_row_seq_pub` (row count + non-empty buffer) + existing suite.
- `cargo test -p tickvault-core` — existing tick_processor suite (heartbeat,
  filters, dedup) must stay green after clock consolidation; new unit test for
  the nanos→secs heartbeat derivation helper if one is introduced.
- `cargo bench -p tickvault-core --bench full_tick_processing` — produces the
  component table (pasted in the PR body).
- `cargo bench -p tickvault-core --bench tick_parser --bench pipeline` before
  AND after the cuts — bench-gate budgets (`dispatch_frame` 10 ns, `pipeline`
  100 ns/tick) must stay green.
- DHAT: `cargo test -p tickvault-core --features dhat` zero-alloc tests intact.

## Rollback

- All changes land in one PR on branch `claude/nifty-darwin-l2mefu`; revert =
  `git revert <merge-sha>`. The storage re-export and the bench are additive;
  the only hot-loop diff is the clock-read consolidation, which is a pure
  refactor with identical observable semantics — reverting restores the prior
  3-extra-clock-reads behavior with no data or schema implications.

## Observability

- No metric is removed or renamed. `tv_tick_processing_duration_ns` and
  `tv_wire_to_done_duration_ns` keep identical semantics (the timed region is
  unchanged except for becoming cheaper).
- The PR body carries the ranked ns-per-component table + before/after
  Criterion numbers (§4 evidence discipline — real output pasted).
- If the flush share is proven material, a follow-up recommendation (separate
  `tv_tick_flush_duration_ns` histogram so batch-flush cost is visible apart
  from per-tick cost) is documented for operator decision — NOT shipped here,
  since it changes dashboard semantics.

## Plan Items

- [x] Item 1 — Storage testing re-export for ILP row build
  - Files: crates/storage/src/lib.rs
  - Tests: test_build_tick_row_seq_pub_appends_one_row

- [x] Item 2 — full_tick_processing component bench + TCP-drain writer bench
  - Files: crates/core/benches/full_tick_processing.rs, crates/core/Cargo.toml
  - Tests: (bench target — exercised via cargo bench; budgets intentionally not
    added to bench-gate until two stable baseline runs exist)

- [x] Item 3 — Run benches, build ranked component table (evidence)
  - Files: (no source change — PR body + report)
  - Tests: n/a (measurement step)

- [x] Item 4 — Cut proven-hot items only (clock-read consolidation; others only
  if proven)
  - Files: crates/core/src/pipeline/tick_processor.rs
  - Tests: existing tick_processor tests green; test_heartbeat_secs_derivation

- [ ] Item 5 — Before/after proof + honest report (bench-gate green, DHAT
  intact)
  - Files: (PR body)
  - Tests: cargo bench before/after paste; cargo test -p tickvault-core -p
    tickvault-storage green

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Bench run with no QuestDB / no network | writer group self-skips, all other components measured |
| 2 | Flush amortization at batch=1000 | append median includes flush/1000 share via drain listener |
| 3 | Clock consolidation under midnight rollover | heartbeat secs = nanos/1e9 — identical to chrono truth |
| 4 | All components sum ≪ 15.56 µs | honest report: residual is prod-side (flush + cold cache), no speculative cut |
