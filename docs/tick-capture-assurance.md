# Tick capture assurance at the market-open burst

Audit baseline: `d78c431`, 2026-09-13. Local candidate changes described below are not yet validated/deployed; baseline file references refer to that commit. This document records static source evidence, not a successful market-open load run. No production traffic, broker credentials, live data, or running service was changed by this audit.

**Supported goal:** measure and minimize loss for frames actually delivered to this process, with bounded buffering, observable refusal and replay. **Not supported:** a guarantee of every exchange tick, zero WebSocket disconnections under every failure, or O(1) total work for arbitrary input size. Provider/network outages, finite memory/disk, and process/host failure prevent those universal promises.

## Capture boundary comparison

| Stage | What the code does | Failure boundary / remaining risk | Evidence |
|---|---|---|---|
| Provider → TCP/WebSocket | Reads bounded WebSocket messages and classifies binary data/control | Ticks never delivered by the provider cannot be reconstructed from local WAL. Local sequence numbers prove local identity, not exchange completeness. Oversize messages close the connection. | `crates/core/src/websocket/connection.rs:130-157,1582-1612`; local sequence minted in `pool_supervisor.rs:3643-3650` |
| Stacked data + disconnect | Detects the disconnect code, logs discarded leading bytes, returns Closed | **Concrete remaining bug:** leading market packets are discarded before WAL/ring capture. Correctly reconnecting does not recover those bytes. | `connection.rs:1621-1672` |
| Frame → WAL queue | Nonblocking byte/record-bounded enqueue; immutable Bytes shared with ring | Enqueued is not durable. WAL refusal still allows live processing, so a WAL-drop counter does not necessarily mean the tick was lost. | `pool_supervisor.rs:3670-3685,3756-3759`; `crates/storage/src/ws_frame_spill.rs:1118-1202` |
| Frame → decode ring | Reserves class slots and bytes, uses try_send, releases refused reservations | Ring-only refusal means delayed recovery if WAL eventually persists; refusal by both ring and WAL is real capture loss. Disk failure after WAL enqueue can also invalidate that recovery assumption. | `pool_supervisor.rs:3692-3748` |
| WAL queue → file | Dedicated writer with bounded retry backoff, periodic fsync | Queued records and user-space buffering can vanish on abrupt process exit. Default 1s fsync is a cadence floor, not a hard deadline during blocked I/O. Failed segment creation counts errors but cannot persist the record. | `ws_frame_spill.rs:547-579,1317-1324,1786-1818` |
| Decode → validated tick | Walks packets and tracks malformed/non-tick/truncated outcomes | Not every binary packet is a trade tick. Invalid packets and abandoned trailing bytes must remain separate from missing valid ticks; decoder work scales with packet count. | `crates/app/src/dhan_feed_stack.rs:7085-7114` |
| Tick buffer → writer | Offloads batches; full queue restores rows; sustained/full-width batches spill | Bounded retry retention avoids unbounded memory, but prolonged overload consumes rescue capacity. | `crates/storage/src/tick_persistence.rs:2519-2580` |
| Rescue queue → file | Dedicated queue normally offloads disk operations | **Hot-path coupling remains:** full/dead rescue queue falls back to synchronous directory traversal, free-space probing and file writes on drain. Stalled drain can increase ring/WAL backlog. Do not “fix” by silently dropping instead. | `tick_persistence.rs:2590-2622,2643-2685` |
| Tick spill → applied watermark | Successful spill can acknowledge the range as applied | **Durability gap:** spill uses write_all + File::flush without fsync, yet in-order rescue advances acknowledgement. A host/power failure can lose unsynced spill after WAL is considered applied. Actual removal timing must also be checked. | `tick_persistence.rs:1325-1331,2690-2709,3125-3127` |
| QuestDB write → query visibility | ACKed flush advances watermark; sink-suspect checks defer replay confirmation | HTTP acknowledgement is not proof of query freshness while WAL apply lags/suspends. Read-side liveness/counts alone are insufficient. | `tick_persistence.rs:3098-3105`; `dhan_feed_stack.rs:11513-11531`; runtime suspension check in `main.rs:2231-2238` |
| Restart / cleanup | WAL applied/unapplied tracking guards replay; shutdown has budgets | Restart is not a substitute for continuous capacity. Destructive recovery workflows and external deletion can erase the safety layers together. | `dhan_feed_stack.rs:12785-12819`; separately documented emergency recovery workflow |

## Hard resource bounds in this revision

| Resource | Bound / semantics | Source |
|---|---|---|
| Main-feed WebSocket message | `162 × 5000 × 2 = 1,620,000 bytes`; larger vendor coalescing remains a refusal | `connection.rs:130-157` |
| WAL queue | 524,288 records, also byte bounded | `ws_frame_spill.rs:358` |
| WAL queued payload budget | Resolved memory ceiling / 16, clamped to 256 MiB–2 GiB | `ws_frame_spill.rs:360-391` |
| Tick writer handoff | Four batches | `tick_persistence.rs:2879` |
| Tick rescue handoff | Two batches | `tick_persistence.rs:2793` |
| Tick producer retention | Two retained flush spans, secondary 32 MiB producer buffer ceiling | `tick_persistence.rs:2899,2945,2559-2571` |
| Tick spill | Checks actual free-space reserve; can refuse with StorageFull | `tick_persistence.rs:1304-1317` |

Buffer seconds must be computed from **measured bytes/sec and frames/sec**, not an assumed 5000 fps. For queue capacity Q, current backlog B, arrival rate A and durable service rate S, overload headroom is approximately `(Q−B)/(A−S)` while A>S, with the tighter byte/record limit prevailing. This does not cover sudden larger bursts or blocked storage deadlines. Multiple buffers share payloads and other allocations; do not simply sum their limits as independent useful headroom.

## Priority changes proposed to coordinator

1. Preserve a stacked data+disconnect payload before issuing the disconnect event. Prefer a retained pending-close state: first deliver data exactly once, then report original disconnect without waiting for another network read. Test standalone disconnect, stacked data/control, malformed lengths, 805 cause handling, and exact WAL/ring capture order. Must retain reconnect behavior and frame bounds.
2. Make successful tick-spill persistence mean storage sync completed before acknowledging the WAL range, with file/directory creation durability considered. Failed sync must leave the range unapplied. Benchmark on a disposable volume; adding synchronous fsync to the drain fallback can worsen burst stalls.
3. Remove the inline disk rescue dependency only after establishing a safe alternative backed by confirmed durable WAL and explicit unapplied-range retention. A larger queue alone postpones overload; it does not solve it.
4. Improve test claims: the test named `chaos_sigkill_ws_frame_wal_recovers_all_four_types` currently drops the Rust object and sleeps (`crates/storage/tests/chaos_ws_frame_wal_replay.rs:109-116`), rather than killing a process. The similarly named zero-loss guard waits for writes and clean drop (`zero_tick_loss_sla_guard.rs:61-68,118-134`). These are round-trip tests, not abrupt-process/power-loss certification.

## Loss accounting that humans can trust

Show separate columns for received frames/bytes, valid decoded tick rows, non-tick protocol packets, decode refusals/abandoned bytes, WAL queue refusals, WAL write failures, ring deferrals, tick spill rows, unrecoverable rescue failures, replay outstanding, and committed query freshness. `tv_ticks_dropped_total` increments even on a successful rescue (`tick_persistence.rs:3125-3143`), so its name alone must not be presented as permanent data loss. WAL refusal increments frame-based counters, while a frame can contain many ticks; do not sum frame and row counters or convert one frame into one trade.

## Existing isolated tests — commands for the AWS test checkout

Run only in a separate test checkout with disposable test files, not against production QuestDB or the live WAL directories. These commands do not require real broker credentials; tests use fixtures/mocks. None was executed by this reviewer during this source audit.

```bash
cargo test -p tickvault-core --lib test_main_feed_cap_admits_a_full_connection_burst_not_a_subscribe_batch
cargo test -p tickvault-core --lib test_wal_ring_sink_full_ring_is_lag_not_capture_loss
cargo test -p tickvault-core --lib test_wal_ring_sink_refuses_on_the_byte_bound_before_the_count_bound
cargo test -p tickvault-core --lib test_wal_ring_sink_returns_the_reservation_when_the_count_bound_refuses
cargo test -p tickvault-core --lib test_supervisor_keepalive_prevents_the_pre_open_reconnect_storm
cargo test -p tickvault-core --lib a_frame_delivering_socket_is_never_redialled_by_the_frame_watchdog
cargo test -p tickvault-storage --lib a_full_queue_keeps_the_rows_and_never_reports_them_as_dropped
cargo test -p tickvault-storage --lib the_producer_stops_widening_the_batch_and_spills_instead
cargo test -p tickvault-storage --lib a_full_rescue_queue_writes_inline_rather_than_dropping
cargo test -p tickvault-storage --lib shutdown_flushes_the_buffer_so_replay_sees_every_record
cargo test -p tickvault-storage --test chaos_ws_frame_wal_disk_io_failure
cargo test -p tickvault-storage --test chaos_ws_frame_wal_replay
cargo test -p tickvault-storage --test zero_tick_loss_sla_guard
cargo test -p tickvault-core --test chaos_ws_e2e_wal_durability
cargo test -p tickvault-core --release --test stress_chaos_core test_stress_parse_500k_mixed_packets
cargo test -p tickvault-core --release --test stress_chaos_core test_stress_parse_1m_tickers_throughput
```

Record pass/fail/ignored counts, exact SHA, CPU/memory/volume limits and duration. Some permission-failure cases behave differently under root; inspect skips. Parser-throughput tests alone are not end-to-end ingestion capacity.

## Missing market-open acceptance experiment

On a disposable replica: replay representative 09:15 message-size/coalescing and instrument mix at a stated target rate and burst duration; include duplicate/stale/malformed frames, compressed burst arrival, slow QuestDB responses, WAL apply suspension, bounded disk stalls, full rescue queue, abrupt child-process termination, and recoverable network disconnects. Run producer/consumer reconciliation by known fixture identities and prove no unaccounted valid rows within that specific envelope. Measure p99 capture scheduling latency, queue occupancy in records AND bytes, disk fsync latency, query lag, reconnect causes and successful replay completion. Account for infrastructure/network limits separately.

For every guarantee, state its boundary: e.g. “N known input ticks sent and received in this fixture; N unique rows recovered after failure X under hardware Y.” Never promote one successful run into all permutations, uninterrupted upstream service, or universal zero loss.

## Implemented remediation with targeted AWS tests passed

The stacked-disconnect fix now delivers the original binary bytes once as a Frame, then returns the original disconnect reason from a pending enum on the next recv without reading the socket. The downstream dispatcher accepts the embedded disconnect packet as control; preceding and following packets remain intact. Close/redial clears pending state. Tests cover 804/805/808, standalone control, malformed tails and packet decoding.

The tick spill candidate differentiates Buffered drain fallback from Synced worker persistence. Both worker paths perform file sync_data and directory-tree sync before treating a rescue as landed. Sync errors preserve the WAL range. Buffered fallback introduces no fsync on the drain and now preserves its WAL range as unconfirmed even when the spill write succeeds. Counters retain their historical meanings (spilled is not synonymous with storage-synced); logs include storage_synced for this distinction. Fault-injection tests cover file-sync and directory-sync failure, ordering, no callbacks in Buffered mode, and isolated applied-watermark behavior. These are syscall-boundary tests, not actual power-loss tests.

Additional candidate test filters:

```bash
cargo test -p tickvault-core --lib stacked_disconnect_preserves_all_bytes_then_reports_close_once
cargo test -p tickvault-core --lib standalone_disconnect_and_explicit_close_never_replay_a_stale_reason
cargo test -p tickvault-core --lib malformed_stacked_disconnect_stays_data_without_scheduling_close
cargo test -p tickvault-storage --lib buffered_spill_never_syncs_or_marks_the_wal_range_durable
cargo test -p tickvault-storage --lib worker_sync_failures_preserve_wal_and_success_requires_file_then_directory
```

## Completed AWS validation snapshot

The AWS targeted campaign passed 71 tick-storage, 130 WAL, 60 connection, 181 supervisor, 37 parser/stress and 3 WebSocket-WAL tests, with zero failures. These are overlapping sections of the consolidated 760-test total, not additional totals to add across documents. The candidate repairs and fault fixtures passed within this scope. Buffered inline fallback still retains an unconfirmed WAL range; synced worker rescue requires file and directory sync before acknowledging. Syscall-failure fixtures do not simulate every host/power-loss behavior or a complete production market-open burst.

Source implementation remains undeployed. SQL read-only checks and healthy services are distinct from applying view DDL or deploying a new executable. The test evidence file records the checked source and binary hashes; the final Top Volume cutoff retest and timing campaign completed separately at 18:57:13 UTC.
