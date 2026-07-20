---
paths:
  - "crates/common/src/error_code.rs"
  - "crates/storage/src/tick_flush_worker.rs"
  - "crates/storage/src/tick_persistence.rs"
---

# Off-Thread Tick ILP Flush Worker — Error Codes (TICK-FLUSH-01)

> **⚠ RETIRED 2026-07-17 (stage-2 dead-WS sweep — the dead Dhan tick chain
> deleted):** `crates/storage/src/tick_flush_worker.rs` and its owner
> `crates/storage/src/tick_persistence.rs` (plus `tick_row_builder.rs` /
> `tick_spill_drain.rs`) are DELETED — the tick writer had ZERO production
> callers after the Dhan live-WS lane retirement (2026-07-13, PR-C2/C3) and
> the Groww live-feed retirement (2026-07-15); the runtime is REST-only and
> nothing writes the `ticks` table anymore (the table stays in QuestDB,
> SEBI-retained, read-only). The `ErrorCode::TickFlush01WorkerRespawn`
> variant is RETAINED (no ErrorCode deletions in this sweep — crossref keeps
> both directions green); its emit sites are gone, so `TICK-FLUSH-01` can
> never fire again. The Trigger paths below match no live code. Content
> retained as historical audit per house convention.

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F > this file.
> **Operator directive (B6, 2026-07-03, verbatim):** *"The per-tick histogram
> folds the amortized 1-in-1000 sync ILP flush into compute (p50 30µs true
> compute, p99 475µs = flush I/O). Split compute vs flush histograms, move sync
> flush off the tick-consumer thread if hot-path rules allow, add the 10µs
> budget to benchmark-budgets.toml so bench-gate enforces it."*
> **Companion code:** `crates/storage/src/tick_flush_worker.rs`,
> `crates/storage/src/tick_persistence.rs` (`dispatch_batch` /
> `drain_failed_batches` / `take_last_stall_ns`),
> `crates/common/src/error_code.rs::ErrorCode::TickFlush01WorkerRespawn`.
> **Companion rules:** `ws-frame-spill-error-codes.md` (WS-SPILL-01 — the
> supervision pattern this mirrors), `wave-2-error-codes.md` (WS-GAP-05),
> `data-integrity.md` (the `ticks` DEDUP key that makes replays idempotent).
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires
> this file to mention every `TickFlush01*` variant verbatim —
> `TICK-FLUSH-01` and `TickFlush01WorkerRespawn` appear below.

---

## §0. Why this code exists (the B6 latency split)

Before B6, `TickPersistenceWriter::append_tick_with_seq` performed the
1-in-1000 batch ILP TCP flush **synchronously inside the tick-consumer
loop's timed span**, so `tv_tick_processing_duration_ns` p99 (~475µs) was
flush I/O, not compute (~30µs p50). B6 hands full ILP buffers to a
dedicated supervised OS thread (`tick-flush-worker`) over a BOUNDED
crossbeam channel with a pre-allocated buffer pool, so the steady-state
per-tick hot path is a zero-allocation O(1) buffer swap. The worker owns
its own questdb `Sender` (a second ILP TCP connection), records the
existing `tv_tick_flush_duration_ns` histogram per flush, and returns any
batch it cannot flush over a failed-batch channel — the writer routes
those into the UNCHANGED ring → spill → DLQ rescue chain and marks itself
disconnected so the existing throttled-reconnect + drain recovery runs.

**Ordering + idempotency:** a single worker + FIFO channel preserves batch
order; the `ticks` DEDUP key `(ts, security_id, segment, capture_seq,
feed)` makes any replay idempotent, and `capture_seq` rides inside every
job so rescue preserves TICK-SEQ-01 replay stability.

## §1. TICK-FLUSH-01 — tick flush worker thread respawned

**Severity:** High. **Auto-triage safe:** Yes (the respawn already
self-healed; queued flush batches survive because the supervisor re-enters
the loop with the SAME channels).

**Panic honesty (adversarial round 1, HIGH-1 — READ FIRST):** the
workspace release profile sets `panic = "abort"`, so in the PRODUCTION
binary a worker panic ABORTS the whole process at the panic site — this
code can never fire for a panic in release. Recovery for that case is
next-boot WAL replay (`ws_frame_spill` durably captures every frame
BEFORE the pipeline). In production the respawn path is reachable ONLY
for NON-panic fatal returns of the worker loop; the panic-respawn arms
(`reason="panic"` / `reason="job_panic"`) fire in unwind (dev/test)
builds only. No claim of in-process panic self-healing is made for
release builds.

**Trigger:** the `tick-flush-worker` loop exited abnormally and the
supervisor (mirrors WS-SPILL-01 / WS-GAP-05) logged
`error!(code = "TICK-FLUSH-01")`, incremented
`tv_tick_flush_worker_respawn_total{reason}`, backed off 200ms, and
re-entered the worker loop with a FRESH `WorkerState` (fresh ILP sender —
a possibly half-written connection is never reused). Reasons:
- `job_panic` (unwind builds): a panic mid-flush. Panic isolation is
  PER-JOB (HIGH-2 fix) — the in-flight batch is rescued to the failed
  channel and its pool pair recycled BEFORE the respawn, so the pool
  never shrinks and no batch is dropped during unwind.
- `panic` (unwind builds): a panic outside job processing (no job in
  flight).
A per-batch FLUSH failure does NOT kill the thread — it increments
`tv_tick_flush_worker_errors_total`, returns the batch to the writer for
ring/spill/DLQ rescue, drops the worker's sender, and reconnects with the
standard throttle. Writer-side, worker-failed batches are drained into
the ring both per-frame AND by the tick loop's 1s idle-drain interval
(HIGH-3 fix), so a flush failing after the day's LAST tick still reaches
ring → spill within ≤1s instead of parking in RAM until shutdown.

**Why High and not Low:** the worker is what keeps the blocking ILP flush
off the tick-consumer thread. A flapping worker means QuestDB ILP or the
host is degrading; the writer degrades gracefully (inline-flush fallback,
measured as stall + counted via
`tv_tick_flush_backpressure_block_total`), but the operator must see it.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `TICK-FLUSH-01`; the payload
   carries the panic backtrace or the error string.
2. `tv_tick_flush_worker_respawn_total` rate — a one-off at shutdown is
   benign; a sustained rate means a real bug or a dying host — inspect
   `data/logs/errors.jsonl.*` and run `make doctor`.
3. Cross-check `tv_tick_flush_worker_errors_total` and
   `tv_tick_flush_backpressure_block_total{reason}` — sustained
   `pool_empty` (worker behind) / `queue_full` fallbacks mean QuestDB ILP
   is slow (check BOOT-01/BOOT-02 and QuestDB health); sustained
   `worker_unavailable` means the worker never spawned or its channel is
   gone — inspect the spawn-failure `error!` at writer construction.
4. Zero-loss is preserved throughout: failed batches land in the ring →
   spill → DLQ chain and the WAL replay covers any batch lost between
   worker death and process exit.
5. `tv_tick_flush_deferred_stall_ns_total` (ns sum) is flush-side stall
   accrued AFTER a frame's compute record (timer-path drain/flush) —
   discarded from compute by design so it can never deflate the next
   frame's sample; a sustained rate means the timer path is doing rescue
   work (QuestDB degraded).

**Honest envelope:** the offload preserves the bounded zero-loss chain
exactly; it does not make QuestDB faster. If the worker cannot keep up,
the writer's inline fallback restores pre-B6 behavior (blocking flush,
now MEASURED as stall so `tv_tick_compute_duration_ns` stays honest).
Two bounded blocking windows are intentional (zero-loss over latency,
MEDIUM-6): the hot-path backpressure `send` blocks for at most one worker
flush (worst case bounded only by the OS TCP write timeout against a
black-holed peer — identical to the pre-B6 inline flush), and the cold
graceful-shutdown join polls up to 10s (under
`tokio::task::block_in_place` on a multi-thread runtime). And per the
panic-honesty note above: a RELEASE-build panic aborts the process;
recovery is next-boot WAL replay, not in-process respawn.

**Source:**
- `crates/common/src/error_code.rs::ErrorCode::TickFlush01WorkerRespawn`
- `crates/storage/src/tick_flush_worker.rs` (supervisor + worker loop)
- `crates/storage/src/tick_persistence.rs::dispatch_batch` /
  `drain_failed_batches` (writer-side integration)

## §2. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `TickFlush01*` variant)
- `crates/storage/src/tick_flush_worker.rs`
- `crates/storage/src/tick_persistence.rs` (dispatch/offload paths)
- Any file containing `TICK-FLUSH-01`, `TickFlush01`,
  `tv_tick_flush_worker_respawn_total`, or `tv_tick_compute_duration_ns`
