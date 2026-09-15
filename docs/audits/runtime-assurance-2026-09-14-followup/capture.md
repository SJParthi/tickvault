# TickVault phase 3 — capture and recovery fixes

Reviewed candidate: `af77fb0698aeb4414136767e4907593e90ac1ddf`. Owned and modified file: `crates/storage/src/ws_frame_spill.rs`. The other four assigned storage files were not changed. This pass first read `docs/audits/runtime-assurance-2026-09-14/capture.md` and preserves the phase 2 rescue-watermark, snapshot-coherence and deferred-restore fixes.

**Validation status:** source-reviewed; `git diff --check` passed. No Rust compiler/build/test run, AWS command, provider connection, database mutation, commit or deployment occurred in this subtask. Candle-audit peer review checked the streaming record layout, sink scope, unread-file refusal and sequence-parser correction and reported no new defect in the inspected changes. This is review evidence, not executable or production evidence.

## Concrete fixes

| Finding | Failure schedule before this change | Implemented behavior | Important boundary |
|---|---|---|---|
| Whole-segment skip inferred a range from two first records | Segment A begins at sequence 0 but contains a later sequence 1000. A delayed producer places sequence 100 in segment B. A watermark of 500 made the inferred A range end at 99 and archived its unacknowledged sequence 1000. The inverse case hides an older protected record below A's first sequence. | `segment_is_applied` walks the actual records, verifies each CRC, checks each sequence against its matching sink, and archives only if the entire closed segment is covered. A single unapplied, unsupported, truncated or corrupt record refuses this optimization. | The scan is **O(segment bytes)**. It has an 8 KiB read buffer plus 8 KiB scratch buffer, independent of frame size; these are scan buffers, not a total process-memory claim. |
| Dhan ACKs incorrectly covered unrelated streams and unknown endpoints | A mixed WAL contains order updates, TrueData or a future endpoint byte below Dhan's high-water mark. Whole-segment inference or the old per-frame endpoint fallback omitted those records without evidence of their matching consumer completing them. | `applied_sink_for(WsType, Option<raw endpoint>)` returns a sink only for Dhan main-feed/depth endpoints. Other transports, OrderUpdate endpoints and unknown raw endpoint tags remain in the replay result. The per-frame parser checks the raw tag before any fallback mapping. | Main-feed coverage still uses the lower tick/depth watermark through `AppliedSnapshot::skip_below(Ticks)`, except explicitly untracked depth. Returning unsupported records does **not** invent a live consumer for them. |
| An unreadable segment could still be staged and confirmed | `File::open` or `read_to_end` failed; the loop counted corruption, incremented `consumed`, staged the unread file, returned a normal batch, and a caller could archive it. This also applied to already-staged leftovers. | An I/O read error now emits its counter/coded error and immediately returns `Err` before `consumed` or staging. The existing phase 2 root companion sets replay-refused on `Err` and avoids confirmation. Already-staged files remain available. | This changes **I/O refusal**, not the separate parser policy for a readable file containing a corrupt prefix/suffix. Already-verified applied segments may have been archived earlier in the pass; no unsupported file gains permission from that. |
| The high-water parser overshot every next record by four bytes | `min_rec` includes the CRC, but the seek added another four CRC bytes. An ordinary multi-record segment stopped after its first record, potentially seeding a restart below later IDs already on disk. | The seek now advances by `frame_len - (filled - min_rec)`, including negative movement when reading a short older-format record over-read into its successor. The parser can find a maximum in later records. | The **outer** newest-nonempty/last-four-segment heuristic remains incomplete; this fix repairs record walking, not a proof of globally unique identities after every possible crash/reorder/corruption. Its public documentation now states that limitation. |

The replay parser now defers `frame.to_vec()` until after CRC and watermark filtering. Already-applied records inside a retained segment do not allocate a separate payload vector. The entire retained segment is still read into memory by that parser; this is not a rewrite of its existing replay-memory policy.

The public skipped-segment/byte metric names remain unchanged. Their documentation now says **validated segments not materialised as replay frames**. The bytes are read by the eligibility scan; the counters no longer mean avoided disk I/O or files archived unread. A clean fully applied last segment can now be archived as well, so three existing test expectations were adjusted to this behavior.

## Focused Rust regressions — written, not executed

Six new regression functions and one replacement for the old first-header probe test:

| Test selector | Adversarial case and expected result |
|---|---|
| `replay_checks_an_out_of_order_tail_instead_of_a_successor_header_bound` | A later sequence buried in an earlier physical segment must be returned with its exact payload; only the independently covered neighbor is archived. |
| `replay_checks_an_older_protected_record_after_a_newer_first_record` | A protected old sequence after a newer first record must survive the skip; exact sequence/payload and skip counts are checked. |
| `dhan_sink_acks_never_skip_other_transports_or_unknown_endpoints` | Mixed v2/v3/v4 order updates, TrueData, a Dhan OrderUpdate endpoint and a forward endpoint tag all survive a Dhan high-water mark; original physical record order is preserved. |
| `applied_segment_scan_checks_all_record_crcs_and_rejects_incomplete_files` | Clean records approve; legacy unsequenced, missing, empty, last-record CRC corruption, short tail and a single extra trailing byte all refuse segment archival. Replaces the old first-record-only test. |
| `applied_segment_scan_streams_large_frames_across_all_sequenced_versions` | Mixed v2/v3/v4 with empty neighbors and a 20,003-byte payload cross read/scratch boundaries, then a middle-byte flip must refuse eligibility. |
| `reseed_probe_follows_every_boundary_in_mixed_version_segments` | Six payload lengths—0, 1, 3, 4, 5, 37 bytes—across v1/v2/v3/v4 and nonmonotonic sequences must find the maximum in the final record. |
| `an_unreadable_staged_segment_refuses_the_pass_before_confirmation` | An already-staged `.wal` directory causes deterministic Unix EISDIR reading after a valid segment; no confirmable batch or archive is created. Repairing it permits both records to be returned and confirmed. |

Small targeted executable gate when Rust/AWS validation is available:

```sh
cargo test --locked -p tickvault-storage --lib replay_checks_an_
cargo test --locked -p tickvault-storage --lib dhan_sink_acks_never_skip
cargo test --locked -p tickvault-storage --lib applied_segment_scan_
cargo test --locked -p tickvault-storage --lib reseed_probe_follows_every_boundary
cargo test --locked -p tickvault-storage --lib an_unreadable_staged_segment_refuses
cargo test --locked -p tickvault-storage --lib a_second_boot_after_a_clean_session
cargo test --locked -p tickvault-storage --lib replay_never_skips_a_segment_overlapping
cargo test --locked -p tickvault-storage --lib replay_uses_the_lower_watermark
```

These commands have **not** been run. Run the phase 2 deferred-restore and watermark/rescue regressions alongside them, and compile the actual production target before any release claim.

## Assurance boundaries that remain open

| Requirement | What the code currently establishes | Why an unconditional guarantee is still unavailable |
|---|---|---|
| No lost admitted raw frames | Bounded admission queue, WAL worker, visible refusal/error counters, replay of surviving records, and protected rescue/shed ranges. | `AppendOutcome::Spilled` acknowledges process-memory admission. SIGKILL, OOM or abort can destroy queued frames and the userspace buffer before a write. |
| Survive every transient filesystem failure | The writer stays alive and retries opening a later segment; errors are counted. | A failed `write_record` or buffer flush is not retained as a complete retryable batch. Dropping the failed writer can discard buffered records. Preserving complete records across partial writes needs a bounded retry/record-boundary design, not a claim that keeping the thread alive preserved them. |
| Durable after power loss | Configured file flush/sync attempts on the background worker. | No per-record durable acknowledgement, scheduling deadline or hardware guarantee; parent-directory metadata and device behavior remain separate. |
| Never reuse a captured ID after restart | The per-segment reseed walk now follows all complete record boundaries and the live atomic ratchet never lowers an observed sequence. | The directory search still stops at the first useful candidate among four newest files. Arbitrarily delayed producers can place a higher sequence in older files; pruning or DB-only ACKed records can hide issued IDs; header corruption can overestimate. A durable reservation high-water or another explicit restart-identity design remains necessary for that stronger guarantee. |
| Replay every readable byte after corruption | Valid prefixes can be returned, and corruption/abandoned bytes are reported. | The current parser stops at mid-file corruption; the caller may later archive that readable-but-corrupt segment for manual recovery. This pass intentionally keeps that policy distinct from an unreadable I/O error. |
| Consume every transport | Dhan tick/depth ACKs no longer falsely prove another transport completed. | `main.rs` still counts/discards retired OrderUpdate/TrueData flows without a live consumer. The unknown-endpoint fallback remains a compatibility report, not a correct future decoder. |
| Never miss a broker/exchange tick | Application capture can account for data actually received and surviving its own storage boundaries. | A client cannot reconstruct a frame the broker never delivered without a documented replay/sequence contract. Connection recovery cannot imply the absence of any upstream gap. |
| Never disconnect | The existing supervisor can observe failures and reconnect according to its policies. | Network failures, provider restarts, limits, token expiry and process failure cannot be prevented unconditionally by client code. Recovery time and missed-delivery windows need measurements and provider capabilities. |
| O(1) everywhere and fixed nanoseconds | Scalar state changes and indexed/cached reads can have bounded algorithmic work under stated capacity assumptions. | Reading/CRC-checking a variable-size WAL is O(bytes). Producing N records is Ω(N). Bounded instruction count does not bound operating-system scheduling or I/O wall time. No new latency was measured here. |

The changes are reviewable corrections to specific failure paths. They are not proof that all failure permutations have been tested, that an upstream zero-loss SLA exists, or that a release is ready.

## Handoff

- Source at handoff: `/workspace/scratch/81296451ef3b/tickvault-audit/crates/storage/src/ws_frame_spill.rs`.
- Only `git diff --check` and source/peer review were completed locally. No benchmark/test count should include these new Rust tests yet.
- Root owns publication, compilation, AWS validation and the aggregate report. Do not bypass any existing approval or connection block.
