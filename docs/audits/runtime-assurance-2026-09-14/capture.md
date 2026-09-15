# TickVault phase 2 — capture and recovery audit

Source base: `6637b52ec8d2604ba06b83b3f898f79aa7a85943` in `/workspace/scratch/81296451ef3b/tickvault-audit`. This audit began read-only; root subsequently authorized isolated source fixes and regression tests. No provider traffic, production data mutation, deployment, or commit occurred. Local Rust tooling is unavailable, so the changes below are source-reviewed and `git diff --check` clean; compilation and execution are pending the root agent's AWS campaign.

## Findings and changes

| Finding | Reproducible failure schedule | Change | Evidence still required |
|---|---|---|---|
| An unread deferred WAL segment could be archived after its restore failed | A prior crash leaves two segments in `replaying/`. Replay reads the first, then defers the second. Renaming the second back to the live directory fails, but the function previously returned a normal batch. `confirm_replayed` globs every staged segment, including the unread second one. | `ws_frame_spill::replay_all_with_report_guarded` now returns `Err` on any deferred restore failure before returning a confirmable batch. Root also changed `main.rs` to mark a replay `Err` as refused; this companion change is necessary because main previously confirmed on an error. | Run the deterministic EISDIR regression and relevant replay wiring guards. |
| An independent rescue could be overtaken by a later normal writer ACK | Producer hands batch A to its rescue queue. That worker remains stalled. The normal writer becomes available and ACKs later batch B, beyond the one-second replay slack. A crash destroys A's RAM payload; the old watermark skipped A's surviving raw WAL frame. | Both `TickWriter::discard_pending` and `DepthWriter::discard_pending` now call `note_unapplied_range` **before** publishing a rescue batch. The bucket remains protected until confirmed replay. | Run the new two-case real SIGKILL integration test. |
| Depth intentionally shed under disk pressure was absent from the unapplied map | Inline/dedicated depth A is skipped by the shedding gate, later depth B ACKs, and next boot's watermark filters A before the recovery decoder can see it. | Root owns the change: both shed arms in `dhan_feed_stack` now call `applied_watermark().note_unapplied(frame.seq)`. | Run root's shed regressions and relevant app tests. |
| Watermark snapshots could combine old buckets with a newer ACK | Snapshot copies the bucket array; another thread marks failed frame A and ACKs later B; snapshot reads B's high-water mark and serializes it beside the old clean map. A subsequent crash incorrectly skips A. | `AppliedWatermark` now coordinates load-bearing mutations using an active-writer count and generation. Snapshot retries at most twice. Incoherent RAM reads return a value that skips nothing; persistence retains the prior coherent file rather than persisting a sticky conservative overflow. | Run the synchronized concurrent regression, overlapping-writer test, and busy-persistence regression. |

The rescue fix trades some extra idempotent replay for safety: even a successfully completed independent rescue retains its bucket mark until replay confirms the backlog. It does not claim that queued rescue payloads are already on disk. The snapshot coordination adds three atomic RMW operations per load-bearing mutation; it adds nothing to successful `WalRingSink::accept`. Repeated marks in an already-published bucket retain their original fast return. The bounded snapshot copy is two attempts over 128 buckets; it cannot wait indefinitely for a descheduled writer.

## Exact added regressions

1. Storage unit test `ws_frame_spill::tests::a_failed_deferred_restore_refuses_the_batch_before_confirmation` creates two tiny TVW4 segments in `replaying/`. The existing injectable RSS probe creates a conflicting destination directory *after* the listing and first segment read, then triggers the memory stop. This deterministically produces a restore error without filling a disk or relying on permissions. The test requires an error, both source files intact, no archive, and successful two-frame retry after removing the obstruction.
2. Integration target `chaos_pending_rescue_sigkill`, test `pending_rescue_remains_replayable_after_a_later_ack_and_sigkill`, runs two Linux children, one tick and one depth. Each child has one completed raw WAL record and a real decoded payload handed off to the independent rescue consumer but not written. A later ACK is explicitly injected through the production watermark API. After the child persists the watermark and signals readiness, the parent sends a real SIGKILL and requires termination signal 9. The parent checks that the watermark has advanced beyond the tested frame but still refuses to call it applied, then replays and compares frame count, sequence, endpoint, transport, receipt, and bytes. Each child has a ten-second readiness deadline and an RAII kill/reap/temporary-directory cleanup guard. The child's working directory is isolated scratch, so even unexpected relative-path writes cannot touch production paths.
3. Storage unit test `wal_applied_watermark::tests::snapshot_retries_when_a_failure_and_later_ack_race_the_bucket_copy` synchronizes an actual worker thread at the exact old-map/new-HWM seam. The first attempt must be invalidated; the second includes the new unapplied bucket and preserves A.
4. Storage unit test `wal_applied_watermark::tests::concurrent_snapshot_writers_never_look_idle_after_one_finishes` ensures two simultaneous writers cannot appear idle through an odd/even counter mistake, and a still-active writer causes conservative return within the bounded attempt count.
5. Storage unit test `wal_applied_watermark::tests::a_busy_snapshot_does_not_replace_the_last_coherent_watermark_file` persists a known snapshot, holds an active mutation while a later ACK occurs, verifies that persistence keeps the old file, then verifies successful update after the mutation ends.

Suggested exact commands:

```sh
cargo test -p tickvault-storage --lib a_failed_deferred_restore_refuses_the_batch_before_confirmation
cargo test -p tickvault-storage --lib wal_applied_watermark::tests
cargo test -p tickvault-storage --test chaos_pending_rescue_sigkill -- --nocapture
```

The new SIGKILL test is deliberately narrow. It does **not** prove socket supervisor behavior, actual QuestDB commit acknowledgment, replay of records still in the WAL producer queue, or hardware power-loss durability. It directly targets the newly identified independent-rescue ordering hole.

## Existing evidence that must not be overstated

| Existing check/path | What it actually establishes | What it does not establish |
|---|---|---|
| `core/tests/chaos_ws_e2e_wal_durability.rs` | A real local WebSocket server and a custom `drive_reader` can feed the WAL; the test waits for writer progress before dropping. | It substitutes its own reader loop, does not run the complete production supervisor/decoder/DB path, and never kills the process. |
| Original `storage/tests/chaos_ws_frame_wal_replay.rs` | In-process append, wait, drop, replay, and staging/confirm behavior. | Its original names/comments claimed SIGKILL and four WebSocket types although it did not kill a process and exercised LiveFeed plus OrderUpdate. The verification agent owns corrections. |
| Original `storage/tests/chaos_disk_full_ulimit.rs` | A child can be constrained by a per-file size limit. | The original parent accepted any nonzero child exit and searched stderr even though `2>&1` moved child errors to stdout; it could pass a child panic. EFBIG is not identical to ENOSPC. The verification agent owns the finite, strict replacement and payload checks. |
| `storage/tests/chaos_seal_sigkill_spill_replay.rs` | A completed spill can be read repeatedly. | It explicitly uses no subprocess and is not a real SIGKILL experiment, despite its name and some comments. |

## Remaining limitations and acceptance gaps

- `WsFrameSpill::append_with_seq_at` succeeds when the bounded queue accepts the record. It does not synchronously wait for a write or sync. A process death can still destroy accepted queued records and the writer's userspace buffer. `WalRingSink` comments that claim an unconditional kill-between-append-and-ring guarantee exceed that implementation.
- A write/flush failure in `persist_record_resilient` or `writer_loop` reports the I/O error and reopens the segment; it does not retain and retry every affected raw record. If the ring also sheds, the file system fails, or the process dies before the alternative sink lands, zero loss is not guaranteed. `persisted_count` counts successful `write_record` calls into `BufWriter`; it is not an fsync acknowledgment.
- The fixed `REPLAY_REORDER_SLACK_SEQ` is an engineering allowance, not a proof that OS scheduling or multi-producer publication cannot reorder frames longer than one second. Segment-skip range inference also assumes sequence/file order. This audit does not certify arbitrary scheduler pauses or a mathematical contiguous-ACK guarantee.
- The QuestDB suspension probe is periodic. It cannot by itself make every HTTP success an immediate proof that rows are queryable. The new SIGKILL test intentionally injects the ACK, so it is not evidence about database commit behavior.
- Malformed/unsupported WAL records and unknown endpoints are counted and may be archived for manual inspection; the current policy does not automatically recover every valid suffix after mid-file corruption. Avoid claiming every arbitrary corrupt file is losslessly recovered.
- A full production-supervisor experiment combining mixed data/control frames, reconnects, bounded-ring pressure, partial I/O, independent rescue stalls, process kills, and an isolated QuestDB sink remains a separate acceptance campaign. Local tests must not be represented as attacks against a live provider or production system.
- Hardware power loss, a hung fsync, storage-device failure, and file/directory metadata durability require their own fault model and experiments. Passing SIGKILL tests proves a process-failure boundary only.

## Owned files at source freeze

- `crates/storage/src/ws_frame_spill.rs`
- `crates/storage/src/tick_persistence.rs`
- `crates/storage/src/depth_persistence.rs`
- `crates/storage/src/wal_applied_watermark.rs`
- `crates/storage/tests/chaos_pending_rescue_sigkill.rs`

No additional edits are planned pending compiler/test feedback from root.
