# Implementation Plan: B6 — Latency histogram split + 10µs compute gate

**Status:** APPROVED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — B6 grounded directive 2026-07-03, this session

> **Operator directive (verbatim, 2026-07-03):** "B6 · Latency histogram split +
> 10µs gate — Branch claude/latency-histogram-split. The per-tick histogram folds
> the amortized 1-in-1000 sync ILP flush into compute (p50 30µs true compute,
> p99 475µs = flush I/O). Split compute vs flush histograms, move sync flush off
> the tick-consumer thread if hot-path rules allow, add the 10µs budget to
> benchmark-budgets.toml so bench-gate enforces it. Hot path: DHAT + Criterion +
> 3-agent adversarial review mandatory."

Crates touched: `crates/storage` (tick_persistence.rs + new tick_flush_worker.rs),
`crates/core` (pipeline/tick_processor.rs + benches/full_tick_processing.rs),
`crates/common` (error_code.rs + tests), `quality/benchmark-budgets.toml`,
`.claude/rules/project/tick-flush-worker-error-codes.md`.

## Design

The per-tick timed span in `crates/core/src/pipeline/tick_processor.rs`
(`tick_start` → `m_tick_duration.record`) today includes the amortized
1-in-1000 **synchronous** questdb ILP TCP flush performed inside
`TickPersistenceWriter::append_tick_with_seq` /
`append_tick_enriched_with_seq` (`crates/storage/src/tick_persistence.rs`).
That folds flush I/O (~hundreds of µs) into the compute histogram
(`tv_tick_processing_duration_ns` p99 ≈ 475µs vs p50 ≈ 30µs).

Three changes:

1. **Off-thread ILP flush (crates/storage).** New module
   `crates/storage/src/tick_flush_worker.rs`: a dedicated supervised OS
   thread (mirrors `ws_frame_spill.rs` catch_unwind respawn supervision)
   OWNS its own questdb `Sender` (built lazily from the same ILP conf
   string; a second ILP TCP connection — permitted, QuestDB supports
   multiple ILP connections) and drains a BOUNDED crossbeam channel of
   `FlushJob { buffer: Buffer, in_flight: Vec<(ParsedTick, i64)> }`.
   A bounded recycle channel returns drained/cleared `(Buffer, Vec)` pairs
   to the writer; the pool is pre-allocated (3 spares + the writer's
   active buffer) so the steady-state batch handoff is a zero-allocation
   O(1) `mem::replace` + `try_recv` + `try_send`. A bounded failed channel
   returns batches the worker could not flush; the writer routes them into
   the EXISTING ring → spill → DLQ rescue chain (`buffer_tick_seq`) and
   marks itself disconnected (`rescue_in_flight()` then `sender = None`)
   so the EXISTING throttled-reconnect + drain machinery takes over —
   failure semantics are behavior-equivalent to today's inline
   `force_flush` failure path (sender=None + rescue to ring), just
   delivered asynchronously via the failed channel.
   `force_flush` itself is UNCHANGED (still records
   `tv_tick_flush_duration_ns`) and remains the synchronous escape hatch
   used by the drain/recovery/shutdown paths and tests.
   **Ordering:** a single worker + FIFO channel preserves ILP batch order
   among offloaded batches. Cross-connection interleaving with the
   writer-side sync drain flushes cannot corrupt data: QuestDB orders rows
   by designated timestamp and the `ticks` DEDUP key
   `(ts, security_id, segment, capture_seq, feed)` makes any replay
   idempotent (TICK-SEQ-01).
2. **Histogram split (crates/core).** Keep
   `tv_tick_processing_duration_ns` recording the TOTAL span (back-compat
   for dashboards/alerts). Add `tv_tick_compute_duration_ns` = total minus
   the persistence stall the writer accrued this iteration
   (`TickPersistenceWriter::take_last_stall_ns()` — set on the
   blocking-send fallback, the inline-flush fallback, and the
   disconnected-branch reconnect/buffer work). In steady state stall = 0
   and compute ≈ total — that is the intended end state. The `_duration_ns`
   suffix auto-inherits the exporter bucket config
   (`Matcher::Suffix` in `crates/app/src/observability.rs`) — no
   registration code needed.
3. **Criterion 10µs gate.** New bench `composite/quote_tick_compute_only`
   in `crates/core/benches/full_tick_processing.rs`: the full live-loop
   chain including the REAL writer append under the new offload (flush I/O
   on the worker thread, not in the measured thread). Budget row
   `composite_quote_tick_compute_only = 10000` (10 µs) in
   `quality/benchmark-budgets.toml`, enforced by `scripts/bench-gate.sh`
   (absolute budget + 5% regression). Bidirectional-substring collision
   with every existing budget key and bench name checked — none (in
   particular it does NOT match `composite_quote_tick_full_chain`).

New ErrorCode `TickFlush01WorkerRespawn` ("TICK-FLUSH-01", Severity::High,
auto-triage-safe) for the supervisor respawn, with runbook
`.claude/rules/project/tick-flush-worker-error-codes.md`.

## Edge Cases

- **Worker deeply behind (no spare buffer):** the writer falls back to the
  legacy inline `force_flush` (measured as stall, counted via
  `tv_tick_flush_backpressure_block_total{reason="pool_empty"}`) —
  zero-loss preserved, natural backpressure, never a drop.
- **Work queue full with a spare in hand:** blocking `send` bounded by one
  flush duration (counted `reason="queue_full"`, measured as stall). Never
  an unbounded wait: queue cap 4 and the worker is supervised.
- **Worker channel disconnected (near-impossible under supervision):** the
  job's buffer + in_flight are restored to the writer and flushed inline.
- **QuestDB down:** worker connect/flush fails → batch returns on the
  failed channel → writer rescues to ring → `sender = None` → existing
  throttled reconnect + `drain_tick_buffer` recovery (unchanged code).
- **Deadlock impossibility:** failed-channel cap (16) exceeds the maximum
  outstanding batches (work-queue cap 4 + 1 in-process + pool 4), so the
  worker never blocks on the failed channel while the writer blocks on the
  work channel.
- **First-cycles buffer growth:** fresh pool buffers grow while absorbing
  their first batch; after one warm cycle per buffer, `clear()` retains
  capacity → zero steady-state allocation (DHAT test warms accordingly).
- **`new_disconnected` writers:** worker starts with no sender and connects
  lazily (throttled) on the first job; disconnected appends still go to the
  ring exactly as today (no job is dispatched while `sender` is None).
- **Aux rows via `buffer_mut()` (deprecated path):** unchanged risk profile
  — they are not covered by in_flight rescue, exactly as today.

## Failure Modes

- **Worker panic — HONEST envelope (adversarial round 1, HIGH-1):** the
  workspace RELEASE profile sets `panic = "abort"`, so in the production
  binary a worker panic ABORTS the whole process at the panic site — the
  catch_unwind supervision can never observe it there; recovery is
  next-boot WAL replay (`ws_frame_spill` captures every frame durably
  BEFORE the pipeline). The supervision covers NON-panic fatal returns in
  production, and panics in unwind (dev/test) builds only. No in-process
  panic self-healing is claimed for release.
- **Worker panic in unwind builds (HIGH-2 fix):** panic isolation is
  PER-JOB — the catch_unwind closure only borrows the `FlushJob`, so a
  panic mid-flush rescues the in-flight batch to the failed channel and
  recycles its pool pair BEFORE the supervisor respawns
  (`reason="job_panic"`); the supervisor re-enters with a FRESH
  `WorkerState` so a possibly half-written Sender is never reused.
  Batches queued in the channel survive the respawn (channel outlives
  the panic). Pre-fix, an unwind dropped the in-flight batch and leaked
  the pool pair (4 panics → permanent inline fallback).
- **Flush fails after the day's LAST tick (HIGH-3 fix):** the tick recv
  loop selects (biased, recv-first) over a 1s `IDLE_FLUSH_DRAIN_INTERVAL`
  whose tick calls `flush_if_needed()` even when idle, so worker-failed
  batches reach ring → spill within ≤1s instead of parking in RAM until
  graceful shutdown. Worker-side spill was evaluated and REJECTED: the
  spill/DLQ machinery is `&mut TickPersistenceWriter` state (file
  handles, counters, disk pre-flight, ring-first ordering, mid-session
  reconnect drain) — a worker-owned spill file would bypass in-session
  recovery and defer every failed batch to next boot.
- **Bounded blocking windows (MEDIUM-6, documented):** the backpressure
  `send` blocks the tokio worker thread for at most one worker flush
  (worst case bounded only by the OS TCP write timeout against a
  black-holed peer — identical to the pre-B6 inline flush); the cold
  shutdown join polls ≤10s under `tokio::task::block_in_place` when on a
  multi-thread runtime. Both are intentional zero-loss-over-latency
  trades.
- **Worker flush error:** `tv_tick_flush_worker_errors_total` + `error!`;
  sender dropped; batch returned via failed channel; writer marks itself
  disconnected and rescues — no tick lost (ring → spill → DLQ chain).
- **Failed-channel closed at worker (writer dropped mid-shutdown):** the
  batch's frames are already durably captured in the WAL
  (`ws_frame_spill`), replayed on next boot — honest envelope, logged.
- **Shutdown with jobs in flight:** `flush_on_shutdown` first drops the
  work channel (worker drains remaining jobs then exits cleanly), joins
  the worker with a 10s bounded wait, drains the failed channel into the
  ring, THEN runs the existing shutdown steps (flush → drain → disk
  spill). A worker wedged > 10s is abandoned with a warn; WAL replay
  covers the in-flight batch.
- **Offload spawn failure:** writer degrades to today's inline-flush
  behavior (offload = None), logged.

## Test Plan

- **Unit (storage, in-module):** offload dispatch resets `pending_count`
  and keeps the connection healthy against a local TCP drain listener;
  worker-failure batch routes to ring + marks writer disconnected;
  `take_last_stall_ns` drains and resets; pool pre-allocation count;
  worker fail-path returns the batch over the failed channel (bad conf).
- **Source-scan ratchets (storage):** `append_tick_with_seq` /
  `append_tick_enriched_with_seq` hot path dispatches via `dispatch_batch`
  and no longer calls `force_flush(` inline; the worker records
  `tv_tick_flush_duration_ns`; existing
  `test_force_flush_records_flush_duration_histogram` stays green
  (force_flush unchanged).
- **Source-scan ratchet (core):** `tv_tick_compute_duration_ns` is
  recorded in `tick_processor.rs` next to the total histogram and
  subtracts `take_last_stall_ns`.
- **Budget guard (common):** `quality/benchmark-budgets.toml` contains
  `composite_quote_tick_compute_only = 10000`.
- **ErrorCode cross-ref tests (common):** variant + rule file +
  runbook_path resolve (existing meta-tests enforce).
- **DHAT (storage):** `dhat_tick_flush_offload.rs` — warmed steady-state
  appends below the flush threshold are zero-allocation against a TCP
  drain listener (skips if loopback sockets are unavailable).
- **Criterion (core):** `composite/quote_tick_compute_only` +
  existing `composite/quote_tick_full_chain` +
  `writer/append_with_seq_amortized_flush` (now measures the offloaded
  append). Medians read from target/criterion estimates.
- **Regression:** `cargo test --workspace` (crates/common touched →
  workspace escalation per testing-scope.md).

## Rollback

Single-branch revert: `git revert` of the storage commit restores the
inline sync flush (force_flush is untouched, so the revert surface is the
dispatch path + constructor spawn only). The new histogram + budget row
are additive — reverting them removes the metric and the gate without
touching any consumer (no dashboard/alert references
`tv_tick_compute_duration_ns` yet). No schema change, no config change,
no DEDUP-key change → no data migration on rollback.

## Observability

- KEPT: `tv_tick_processing_duration_ns` (total span, back-compat),
  `tv_tick_flush_duration_ns` (now recorded at the worker's flush AND in
  the unchanged `force_flush` sync escape hatch).
- NEW histogram: `tv_tick_compute_duration_ns` (suffix-bucketed
  automatically by the exporter).
- NEW counters: `tv_tick_flush_worker_respawn_total{reason}` (supervisor;
  reasons `panic` / `job_panic` — unwind builds only, see Failure Modes),
  `tv_tick_flush_worker_errors_total` (flush failures),
  `tv_tick_flush_backpressure_block_total{reason}` (`queue_full` /
  `pool_empty` = worker behind / `worker_unavailable` = never spawned or
  channel gone — LOW-2 split),
  `tv_tick_flush_deferred_stall_ns_total` (ns sum of flush-side stall
  accrued AFTER a frame's compute record, discarded from compute —
  MEDIUM-4).
- Bench-gate self-disarm closed (MEDIUM-7): the two loopback-binding
  benches PANIC loudly on bind/writer-setup failure instead of silently
  skipping (a skip left no estimates.json, so bench-gate never evaluated
  the 10µs budget row and exited 0).
- NEW ErrorCode `TICK-FLUSH-01` with runbook
  `.claude/rules/project/tick-flush-worker-error-codes.md`; worker flush
  failures log `error!` (Rule 5: flush failures are error-level).
- Existing zero-loss observability unchanged: `tv_tick_buffer_size`,
  `tv_ticks_spilled_total`, `tv_ticks_dropped_total`, DLQ counters.

## Plan Items

- [x] Item 1 — Plan file (this document)
  - Files: .claude/plans/active-plan-latency-histogram-split.md
  - Tests: n/a (plan-gate + per-item-guarantee-check validate structure)

- [x] Item 2 — ErrorCode TICK-FLUSH-01 + rule file
  - Files: crates/common/src/error_code.rs, .claude/rules/project/tick-flush-worker-error-codes.md
  - Tests: test_all_variants_have_unique_code_str, every_error_code_variant_appears_in_a_rule_file, every_runbook_path_exists_on_disk (existing meta-tests cover the new variant)

- [x] Item 3 — Off-thread flush worker + writer integration
  - Files: crates/storage/src/tick_flush_worker.rs, crates/storage/src/lib.rs, crates/storage/src/tick_persistence.rs
  - Tests: test_offload_dispatch_resets_pending_and_stays_connected, test_worker_failure_routes_batch_to_ring_and_disconnects, test_take_last_stall_ns_drains_and_resets, test_append_hot_path_dispatches_offload_not_inline_flush, test_flush_worker_records_flush_duration_histogram, test_worker_fail_path_returns_batch_on_failed_channel, test_spawn_flush_offload_pool_preallocates_spares, test_try_dispatch_no_spare_returns_nospare_and_leaves_batch_intact, test_shutdown_and_join_drains_queued_jobs, test_worker_success_recycles_pair, test_failed_queue_cap_exceeds_max_outstanding_batches

- [x] Item 4 — Histogram split in tick_processor
  - Files: crates/core/src/pipeline/tick_processor.rs
  - Tests: test_compute_histogram_recorded_and_subtracts_stall

- [x] Item 5 — Compute-only bench + 10µs budget + guard
  - Files: crates/core/benches/full_tick_processing.rs, quality/benchmark-budgets.toml, crates/common/tests/bench_budget_elements_guard.rs
  - Tests: budgets_toml_pins_compute_only_10us_gate

- [x] Item 6 — DHAT steady-state allocation test
  - Files: crates/storage/tests/dhat_tick_flush_offload.rs
  - Tests: dhat_offloaded_append_steady_state_zero_alloc

- [x] Item 7 — Adversarial review round 1 fixes (see section below)
  - Files: crates/storage/src/tick_flush_worker.rs, crates/storage/src/tick_persistence.rs, crates/core/src/pipeline/tick_processor.rs, crates/core/benches/full_tick_processing.rs, .claude/rules/project/tick-flush-worker-error-codes.md
  - Tests: test_job_panic_rescues_batch_and_recycles_pool_pair, test_idle_flush_drain_interval_arm_present, test_compute_histogram_recorded_and_subtracts_stall (extended), test_worker_failure_routes_batch_to_ring_and_disconnects (extended)

## Adversarial review round 1 (2026-07-03) — findings → resolutions

| # | Severity | Finding | Resolution |
|---|---|---|---|
| HIGH-1 | security | Release `panic = "abort"` makes the catch_unwind respawn unreachable in prod — a worker panic aborts the process | Honesty fix (no redesign): module doc, rule file §1, and this plan now state release recovery = next-boot WAL replay; respawn covers non-panic fatal returns in prod + panics in unwind builds only. catch_unwind kept (correct in dev/test, harmless in release). |
| HIGH-2 | hostile | Worker panic mid-job dropped the in-flight batch during unwind + permanently leaked its pool pair (4 panics → empty pool → permanent inline fallback) | Per-job catch_unwind (closure borrows the job) — on panic the batch is pushed to the failed channel + pool pair recycled BEFORE the supervisor respawn; fresh `WorkerState` on re-entry (half-written Sender never reused). cfg(test) injection hook + `test_job_panic_rescues_batch_and_recycles_pool_pair`. |
| HIGH-3 | hostile | Failed batches parked in RAM when no more frames arrive (drain only ran per-frame) | Timer fallback chosen (worker-side spill rejected — spill/DLQ is `&mut` writer state; rationale in Failure Modes): recv loop is a biased `tokio::select!` over recv + a 1s `IDLE_FLUSH_DRAIN_INTERVAL` arm calling `flush_if_needed()` when idle. Ratchet `test_idle_flush_drain_interval_arm_present`. |
| MEDIUM-4 | hostile | Stall accrued by `flush_if_needed` (post-record) leaked into the NEXT frame's compute sample → bogus 0ns samples | Both flush_if_needed call sites drain `take_last_stall_ns()` immediately after and discard into `tv_tick_flush_deferred_stall_ns_total`. Ratchet extended in `test_compute_histogram_recorded_and_subtracts_stall`. |
| MEDIUM-5 | hostile | `drain_failed_batches` rescue I/O inside `dispatch_batch` counted as compute | Rescue work wrapped in `note_stall` (empty-channel fast path un-measured). Assertion added to `test_worker_failure_routes_batch_to_ring_and_disconnects`: drained batch adds stall > 0. |
| MEDIUM-6 | both | Blocking bounded `send` + 10s shutdown poll-join run on tokio worker threads; black-holed TCP peer bounded only by OS timeout | Envelope documented at both call sites + module doc + rule file; shutdown join wrapped in `tokio::task::block_in_place` when on a multi-thread runtime (cold path only; hot path unchanged). |
| MEDIUM-7 | both | 10µs gate silently self-disarms when the bench can't bind loopback (early return → no estimates.json → bench-gate exits 0) | Both loopback-binding benches now panic loudly with a message naming the disarmed gate (benches exempt from prod no-panic lints). |
| LOW-a | hostile | WorkerGone restore path dropped the just-taken spare pool pair | `restore_full_pair` returns the spare to the pool via a writer-side `recycle_tx` clone. |
| LOW-b | hostile | `reason="inline_fallback"` conflated "worker behind" vs "never spawned/gone" | Split into `pool_empty` / `worker_unavailable` (static `&'static str` labels). |
| LOW-c | hostile | Stale ILP rows left in `self.buffer` after failed-batch rescue | `drain_failed_batches` clears the buffer after `rescue_in_flight` (locally airtight; reconnect replaced it anyway; any replay is DEDUP-collapsed). |

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | 1000th tick arrives, worker idle, spare available | O(1) buffer swap + try_send; pending resets; zero stall recorded |
| 2 | Worker mid-flush, queue has room | try_send succeeds; no block |
| 3 | Worker > 4 batches behind | blocking send, stall measured + counted; zero loss |
| 4 | Pool exhausted | inline force_flush fallback, stall measured + counted; identical to pre-B6 behavior |
| 5 | QuestDB dies mid-session | worker fails batch → failed channel → ring rescue → writer disconnected → throttled reconnect + drain (existing machinery) |
| 6 | Worker panics | supervisor respawns (TICK-FLUSH-01), channel + queued batches survive |
| 7 | Graceful shutdown with 2 queued batches | worker drains both, joins; then existing flush/drain/spill steps |
| 8 | Compute histogram on a flush-crossing tick | records total − stall (steady state: ≈ total, stall 0) |

## Per-Item Guarantee Matrix

Cross-reference: `.claude/rules/project/per-wave-guarantee-matrix.md` (the
canonical 15-row 100% Guarantee Matrix + 7-row Resilience Demand Matrix
apply to every item above). Item-specific proof rows:

| Demand | This item's proof |
|---|---|
| 100% code coverage | new unit + ratchet + DHAT tests listed per item; coverage gate in CI |
| 100% audit coverage | N/A — no new SEBI event class; zero-loss audit surface unchanged (ring/spill/DLQ counters) |
| 100% testing coverage | unit, source-scan ratchet, DHAT, Criterion, chaos suite re-run via workspace tests |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + plan-gate all run pre-commit/push |
| 100% code performance | DHAT zero-alloc steady state + Criterion `composite_quote_tick_compute_only = 10000` ns budget + bench-gate 5% |
| 100% monitoring | 3 new counters + 1 new histogram (see Observability) |
| 100% logging | worker failures `error!` with `code = TICK-FLUSH-01` on respawn (Rule 5 / tag-guard compliant) |
| 100% alerting | TICK-FLUSH-01 routes via the 5-sink error chain; counters available for CloudWatch alarms |
| 100% security | no new input surface; conf string never logged (Secret handling unchanged); security-reviewer pass in parent session |
| 100% security hardening | no new attack surface (loopback-only test listeners; prod conf unchanged) |
| 100% bugs fixing | 3-agent adversarial review run by parent session on this diff (mandatory per directive) |
| 100% scenarios covering | 8-scenario table above, each with a test or documented equivalence |
| 100% functionalities covering | every new pub fn (`take_last_stall_ns`) has a production call site + test |
| 100% code review | parent session adversarial review before PR |
| 100% extreme check | source-scan ratchets fail the build on regression of the offload/histogram/budget |

7-row resilience matrix (item-specific):

| Demand | This item |
|---|---|
| Zero ticks lost | no new drop path: every fallback either flushes inline, blocks bounded, or rescues to ring → spill → DLQ; worker failure returns the batch |
| WS never disconnects | untouched (no WS code changed) |
| Never slow/locked/hanged | hot path swap is O(1) zero-alloc; blocking fallbacks are bounded and now MEASURED as stall |
| QuestDB never fails | absorbed exactly as today (ring/spill/DLQ + throttled reconnect), now without blocking the tick consumer in steady state |
| O(1) latency | Criterion 10µs absolute budget + 5% regression gate on the compute-only bench |
| Uniqueness + dedup | DEDUP keys untouched; capture_seq threading preserved through the offload (job carries `(tick, capture_seq)` pairs) |
| Real-time proof | new histogram + counters; existing conservation ledger unchanged |

## Z+ Defense Checklist

| Layer | Mechanism | Catches | Latency | LoC |
|---|---|---|---|---|
| L1 DETECT | `tv_tick_flush_worker_errors_total` + `tv_tick_flush_backpressure_block_total` | worker flush failures, backpressure | ms | ~10 |
| L2 VERIFY | failed-channel batch return + writer disconnect | silent worker failure | ≤100ms (flush_if_needed cadence) | ~40 |
| L3 RECONCILE | existing tick-conservation ledger (60s) + 15:40 IST tick-conservation audit | any unaccounted tick | 60s | 0 (existing) |
| L4 PREVENT | bounded channels + pre-allocated pool + inline fallback | unbounded queue growth, drops | pre-op | ~30 |
| L5 AUDIT | `tv_tick_flush_duration_ns` at the worker + stall accounting in compute histogram | hidden flush latency | continuous | ~15 |
| L6 RECOVER | catch_unwind respawn supervisor (TICK-FLUSH-01) + ring/spill/DLQ rescue | worker death, QuestDB outage | ≤200ms respawn | ~50 |
| L7 COOLDOWN | worker reconnect throttle (RECONNECT_THROTTLE_SECS) + 200ms respawn backoff | reconnect/respawn storms | 30s / 200ms | ~10 |

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage:
≤60s QuestDB outage absorbed by rescue→spill→DLQ (unchanged chain — the
offload's failed-batch path feeds the SAME chain);
≤100,000-tick ring buffer capacity (constant `TICK_BUFFER_CAPACITY`,
`crates/common/src/constants.rs`, ratcheted by
`crates/storage/tests/zero_tick_loss_alert_guard.rs`); bench-gated O(1)
hot path with the NEW 10µs absolute compute budget
(`composite_quote_tick_compute_only`); composite-key uniqueness
(capture_seq threading preserved through the offload); chaos-tested
sleep/wake unchanged. Beyond the envelope, DLQ NDJSON catches every
payload as recoverable text, and any batch lost between worker death and
process exit is recovered by WAL replay on next boot.
