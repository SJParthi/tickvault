# Off-Thread Tick ILP Flush Worker — Error Codes (TICK-FLUSH-01)

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

**Trigger:** the `tick-flush-worker` thread either panicked or its loop
returned an error, and the catch_unwind supervisor (mirrors WS-SPILL-01 /
WS-GAP-05) logged `error!(code = "TICK-FLUSH-01")`, incremented
`tv_tick_flush_worker_respawn_total{reason}` (`panic` / `error`), backed
off 200ms, and re-entered the worker loop. A per-batch FLUSH failure does
NOT kill the thread — it increments `tv_tick_flush_worker_errors_total`,
returns the batch to the writer for ring/spill/DLQ rescue, drops the
worker's sender, and reconnects with the standard throttle.

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
   `pool_empty` / `queue_full` fallbacks mean QuestDB ILP is slow; check
   BOOT-01/BOOT-02 and QuestDB health.
4. Zero-loss is preserved throughout: failed batches land in the ring →
   spill → DLQ chain and the WAL replay covers any batch lost between
   worker death and process exit.

**Honest envelope:** the offload preserves the bounded zero-loss chain
exactly; it does not make QuestDB faster. If the worker cannot keep up,
the writer's inline fallback restores pre-B6 behavior (blocking flush,
now MEASURED as stall so `tv_tick_compute_duration_ns` stays honest).

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
