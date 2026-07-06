# Implementation Plan: Silent Candle-Persist Bug — Seal-Writer Recovery + Loud Observability

**Status:** APPROVED
**Date:** 2026-07-06
**Approved by:** Parthiban (operator) — exam-fix directive 2026-07-06

> Guarantee matrices: cross-reference `.claude/rules/project/per-wave-guarantee-matrix.md`
> (15-row + 7-row matrices apply verbatim to this single-crate fix; no new
> tick-drop path, ring→spill→DLQ reused unchanged, DEDUP keys untouched).

Changed crate: **`crates/storage`** (files: `crates/storage/src/shadow_candle_writer.rs`,
`crates/storage/src/seal_writer_loop.rs`, `crates/storage/src/seal_writer_task.rs`).

## Design

The 2026-07-06 groww-only live exam showed ~71,361 candles sealed in RAM with
ZERO rows reaching the `candles_*` tables and ZERO log lines from the
seal-writer leg. Root cause (Verified by code read):

1. `ShadowCandleWriter` used ILP **TCP** (`tcp::addr=host:ilp_port`). ILP TCP
   flush is fire-and-forget — a server-side reject NEVER returns `Err` (the
   exact class fixed on `ws_event_audit` 2026-07-05), and QuestDB closes the
   socket after a bad row. Each 100 ms drain cycle then either "flushed Ok"
   (bytes written, rows rejected server-side) or hit broken-pipe →
   reconnect → replay → "Ok" — so `drain_once`'s AGGREGATOR-SEAL-01 error arm
   never fired and the rescue chain never engaged.
2. `run_seal_writer_loop` DROPPED every `CycleOutcome` (the Wave-6 item-1.4
   counter fan-out was never wired) and logged non-idle cycles at `debug!`
   only; the writer's reconnect-recovery line was also `debug!` (the tick
   writer's equivalent is visible). Result: total silence.

Fix (reuses the existing ring/spill/DLQ blocks — no redesign):

- **Transport:** `ShadowCandleWriter` moves to ILP-over-HTTP
  (`http::addr=host:http_port;protocol_version=1;`) so every flush gets a
  per-request server ACK — a reject surfaces as `Err`, `drain_once` fires
  `error!(code = AGGREGATOR-SEAL-01)` and rescues seals into the existing
  ring→spill→DLQ chain. `protocol_version=1` is pinned so the retained-buffer
  reconnect+replay path is version-consistent. Mirrors the production-proven
  `ws_event_audit_persistence.rs` fix (2026-07-05).
- **Recovery visibility:** the writer's reconnect-recovery log upgrades
  `debug!` → `info!` ("flush recovered via reconnect + replay (zero drop)"),
  matching the tick writer's visible line. IN-CYCLE reconnect + same-buffer
  replay are KEPT; CROSS-CYCLE buffer retention is REPLACED by rescue+discard
  (see the 2026-07-06 hostile-review hardening below).

### 2026-07-06 hostile-review hardening (same branch, post-review pass)

Three CRITICAL/HIGH review findings were verified against the code and fixed:

1. **Poison-buffer / unbounded ILP-buffer growth.** `flush()` retained the
   buffer + `pending_count` on EVERY failure while `drain_once` also rescued
   the same seals to spill/DLQ. A server-REJECTED row (non-connection `Err`,
   newly reachable via the HTTP ACK) therefore replayed on every later flush —
   one poisoned row killed candle persistence for the whole session — and a
   sustained connection outage grew the buffer without bound (each 100 ms
   cycle appended more already-rescued rows) toward the questdb-rs 100 MiB
   `max_buf_size`, after which every flush failed pre-transport FOREVER.
   Fix: new `ShadowCandleWriter::discard_pending()` called from `drain_once`'s
   flush-failure arm AFTER the rescue cascade — the rows are durably parked in
   spill/DLQ (recoverable records; `SealSpillWriter::read_all` is the replay
   scan), so the buffer copy is redundant; the next cycle starts clean and the
   buffer is bounded to ONE drain batch forever. In-cycle reconnect+replay
   inside a single `flush()` call is unchanged.
   HONEST NOTE: rows discarded this way are recovered via the spill/DLQ tier
   (loud: AGGREGATOR-SEAL-01 + rescued counters + the 60s report), not via an
   automatic buffer re-flush — spill replay wiring into the drain loop remains
   a pre-existing Wave-6 follow-up.
2. **Unbounded synchronous HTTP blocking on the shared tokio runtime.** The
   HTTP conf left questdb-rs 6.1.0 defaults in force: `retry_timeout=10s` (a
   HIDDEN internal sleep-and-resend loop inside every `sender.flush`) +
   `request_timeout=10s` — so one failed drain cycle blocked ~80-120s
   synchronously (4 outer attempts × internal retry × request timeout) inside
   a bare `tokio::spawn` on the 2-vCPU prod box, and the internal retries
   DOUBLED the writer's own reconnect ladder. Fix: the conf now pins
   `retry_timeout=0` (library retry disabled — the outer `flush()` loop is the
   single retry owner) + `request_timeout=5000` (ms), bounding a failed cycle
   to ~4 × ~7.5s ≈ 30s worst-case against a black-holed server (milliseconds
   on connection-refused); AND the loop runs each cycle under
   `tokio::task::block_in_place` on the multi-thread runtime so even that
   bounded block never pins a tokio worker's other tasks (current-thread test
   runtimes call directly).
3. **Vacuous ratchet test.** `test_ratchet_loop_wires_observability_not_silence`
   scanned `include_str!` of its own file INCLUDING the test module, so every
   needle matched its own assertion literal — deleting all production
   observability left it green (false-OK, audit Rule 11). Fix: the scan now
   splits at the first `#[cfg(test)]` marker and asserts against the
   PRODUCTION region only; verified by mutation (deleting the ticker-arm
   fan-out call fails the test).
- **Loop observability:** `run_seal_writer_loop` now (a) fans every
  `CycleOutcome` into `tv_seal_writer_drain_total{kind=submitted|flushed_rows|
  flush_failed|rescued_spill|rescued_dlq|dropped}`, (b) fires
  `error!(code = AGGREGATOR-DROP-01)` whenever a cycle truly drops seals
  (the documented CycleOutcome contract the old loop violated), and (c) emits
  an UNCONDITIONAL once-per-60s `info!` progress report
  (`SEAL_WRITER_PROGRESS_REPORT_SECS`) with rows-written/rescued/dropped since
  the last report — silence can never again mean unknown. The accumulation is
  a pure `SealWriterProgress` struct so every transition is unit-tested.

## Edge Cases

- Empty-buffer flush stays a no-op `Ok` (no reconnect round-trip; `drain_once`
  only flushes after popping ≥1 seal, so no false-OK masking).
- Idle 60s window still reports (`cycles > 0`, all other counts 0) — provably
  "idle", never "unknown".
- A flush that also commits rows retained from an earlier failed batch counts
  only the current cycle's popped seals in `flushed_rows` (documented honest
  lower bound).
- `for_test()` writers carry an empty conf — reconnect fails as a connection
  error, retries exhaust, and `flush()` returns with buffer + pending retained
  so the CALLER can rescue (in-cycle contract, proven by
  `test_shadow_writer_reattempts_flush_after_prior_failure`); `drain_once`
  then rescues to spill/DLQ and discards the buffer so the next cycle starts
  clean (`test_drain_once_discards_writer_buffer_after_flush_failure_rescue`).
- HTTP `Sender::from_conf` does not dial at construction — an unreachable
  QuestDB surfaces at flush as `Err` (loud), not at boot.
- Truly-dropped seals on BOTH the submit side (`mpsc_submit_dropped`) and the
  drain side (`rescued_dropped`) are summed into the per-cycle
  AGGREGATOR-DROP-01 emission.
- Cancellation final drain also absorbs + reports before exiting (no silent
  tail window at shutdown).

## Failure Modes

- QuestDB down: flush `Err` (connection class) → bounded IN-CYCLE
  reconnect+replay (≤3 reconnect retries, ≤350 ms of sleep backoff; each HTTP
  attempt itself bounded by the pinned `request_timeout=5000` ms + ≤ ~2.5s
  min-throughput extension, internal library retry DISABLED — honest
  worst-case blocking per failed cycle ≈ 30s against a hung server, wrapped in
  `block_in_place` so no tokio worker is pinned) → on exhaustion `drain_once`
  fires AGGREGATOR-SEAL-01, rescues seals to spill/DLQ, and DISCARDS the ILP
  buffer; the next cycle handles NEW seals cleanly and the rescued rows sit
  durably in spill/DLQ for replay.
- QuestDB rejects rows (schema drift / type mismatch): HTTP ACK returns a
  non-connection `Err` immediately → AGGREGATOR-SEAL-01 + rescue + buffer
  DISCARD, so the poisoned row is never replayed and later cycles keep
  persisting (previously: TOTAL SILENCE under TCP fire-and-forget — the exam
  bug; and pre-hardening, the retained buffer replayed the bad row forever).
- Ring + spill + DLQ all fail: `error!(code = AGGREGATOR-DROP-01)` +
  `tv_seal_writer_drain_total{kind="dropped"}` (previously: dropped silently
  because the loop discarded the CycleOutcome).
- Writer flapping between recover/fail: every recovery is a visible `info!`;
  every failed cycle increments `kind="flush_failed"`; the 60s report carries
  the aggregate.

## Test Plan

All in `crates/storage` (`cargo test -p tickvault-storage --lib --tests`):

- `shadow_candle_writer::tests::test_ilp_conf_targets_http_port_not_tcp` —
  transport ratchet (HTTP + protocol_version pin, TCP banned).
- `shadow_candle_writer::tests::test_new_constructs_even_when_questdb_unreachable`
  — rewritten for HTTP lazy semantics.
- `shadow_candle_writer::tests::test_shadow_writer_reattempts_flush_after_prior_failure`
  — recovery replays the SAME buffered seals on every attempt; no poisoning.
- `shadow_candle_writer::tests::test_shadow_writer_retains_buffer_and_pending_after_failed_flush`
  (existing) — retention for spill rescue.
- `seal_writer_loop::tests::test_progress_report_interval_constant_pinned` — 60s pin.
- `seal_writer_loop::tests::test_progress_absorb_accumulates_every_dimension`.
- `seal_writer_loop::tests::test_progress_take_returns_snapshot_and_resets`.
- `seal_writer_loop::tests::test_progress_idle_cycle_still_counts_the_cycle`.
- `seal_writer_loop::tests::test_progress_absorbs_real_failed_cycle_from_runner`
  — dead-sender runner cycle reports flush failure + spill rescues (the exact
  signature the exam session should have shown).
- `seal_writer_loop::tests::test_ratchet_loop_wires_observability_not_silence`
  — source-scan ratchet: counter fan-out + AGGREGATOR-DROP-01 emit +
  unconditional progress report + block_in_place must stay wired
  (hardened 2026-07-06: scans the PRODUCTION region only — split at
  `#[cfg(test)]` — so the needles can never match the test's own literals;
  mutation-verified).
- `shadow_candle_writer::tests::test_discard_pending_clears_buffer_and_pending_for_clean_next_cycle`
  — poison-buffer recovery primitive (NEW, 2026-07-06 hardening).
- `seal_writer_task::tests::test_drain_once_discards_writer_buffer_after_flush_failure_rescue`
  — end-to-end: failed drain rescues to spill AND leaves the writer buffer
  empty across cycles; nothing lost (all seals present in spill) (NEW).
- `shadow_candle_writer::tests::test_ilp_conf_targets_http_port_not_tcp` now
  also pins `retry_timeout=0` + `request_timeout=5000` (blocking-bound ratchet).
- Existing drain/rescue tests in `seal_writer_task.rs` continue to prove the
  error!(AGGREGATOR-SEAL-01)+rescue path on flush failure.

## Rollback

Single revert of this commit restores the previous (TCP, silent-loop) writer;
no schema change, no config change, no data migration — the `candles_*` DDL,
DEDUP keys, and ring/spill/DLQ files are untouched. Spilled seals written
while this fix is live remain readable by the unchanged `seal_spill` format.

## Observability

- New counter family `tv_seal_writer_drain_total{kind=...}` (the name promised
  by `seal_writer_task.rs` docs since Wave 6 Sub-PR #1).
- Existing `tv_shadow_candle_reconnect_attempts_total` /
  `tv_shadow_candle_reconnect_recoveries_total` unchanged.
- `error!(code = AGGREGATOR-SEAL-01)` (existing, now actually reachable for
  server rejects via the HTTP ACK) and `error!(code = AGGREGATOR-DROP-01)`
  (newly wired at the loop) — both codes already documented in
  `.claude/rules/project/wave-6-error-codes.md`.
- `info!` once per 60s: "seal writer progress (last 60s window)" with
  cycles/submitted/rows_written/flush_failures/rescued/dropped/ring_len.
- `info!` on every reconnect recovery: "shadow candle writer flush recovered
  via reconnect + replay (zero drop)".

## Plan Items

- [x] Switch `ShadowCandleWriter` to ILP-over-HTTP with pinned protocol_version
  - Files: crates/storage/src/shadow_candle_writer.rs
  - Tests: test_ilp_conf_targets_http_port_not_tcp, test_new_constructs_even_when_questdb_unreachable
- [x] Upgrade reconnect-recovery log to info!; keep reconnect+replay+retention
  - Files: crates/storage/src/shadow_candle_writer.rs
  - Tests: test_shadow_writer_reattempts_flush_after_prior_failure
- [x] Wire CycleOutcome into counters + AGGREGATOR-DROP-01 + 60s progress report
  - Files: crates/storage/src/seal_writer_loop.rs
  - Tests: test_progress_absorb_accumulates_every_dimension, test_progress_take_returns_snapshot_and_resets, test_progress_idle_cycle_still_counts_the_cycle, test_progress_absorbs_real_failed_cycle_from_runner, test_ratchet_loop_wires_observability_not_silence, test_progress_report_interval_constant_pinned
- [x] 2026-07-06 hostile-review hardening: rescue+discard poison-proof buffer, pinned HTTP timeouts + block_in_place, non-vacuous ratchet
  - Files: crates/storage/src/shadow_candle_writer.rs, crates/storage/src/seal_writer_task.rs, crates/storage/src/seal_writer_loop.rs
  - Tests: test_discard_pending_clears_buffer_and_pending_for_clean_next_cycle, test_drain_once_discards_writer_buffer_after_flush_failure_rescue, test_ilp_conf_targets_http_port_not_tcp, test_ratchet_loop_wires_observability_not_silence

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | QuestDB rejects candle rows server-side | HTTP ACK → flush Err → AGGREGATOR-SEAL-01 + spill rescue (was: silence) |
| 2 | QuestDB TCP/HTTP connection dies mid-session | reconnect + replay of the SAME buffer; recovery logged at info! |
| 3 | Ring+spill+DLQ all fail | AGGREGATOR-DROP-01 error + dropped counter |
| 4 | Healthy session | 60s progress reports with rows_written > 0 |
| 5 | Idle session (no seals) | 60s progress reports with all-zero counts — provably idle |
