# Candidate build and drain review

Reviewed baseline candidate `af77fb0698aeb4414136767e4907593e90ac1ddf` and its drain/main production code; guard extraction and maintenance fairness corrected locally on 2026-09-14. This review does not claim Rust compilation, runtime execution, latency, a current AWS state, or end-to-end data integrity.

## Concrete defect corrected

New inline `#[cfg(test)]` probes appeared at line 5706 of `dhan_feed_stack.rs`, before the runtime select loop. Thirty-two existing same-file source guards split at the first arbitrary `#[cfg(test)]`, dropping 469,290 characters of subsequent production source. Their assertions could no longer find the shutdown tail, depth shedding and startup wiring. Three direct integration extractors and the lane-running helper had the same defect.

Corrections retain the complete production prefix and stop at the actual top-level test-module boundary. A missing boundary fails closed in the specific-file guards. All invariant assertions remain. The generic lane-running helper detects a top-level cfg(test) module, so inline probes and top-level test constants no longer hide later production functions. Two added Rust fixtures reject the original extraction and ensure test-only assertions remain excluded.

Owned/changed files:

- `crates/app/src/dhan_feed_stack.rs`
- `crates/app/tests/deaf_socket_gauge_guard.rs`
- `crates/app/tests/ring_dwell_gauge_guard.rs`
- `crates/app/tests/dhan_live_off_phase_a_guard.rs`
- `crates/app/tests/lane_running_wiring_guard.rs`

The guard-only correction was first checked against the complete 774,526-byte production prefix and preserved it byte for byte. Subsequent root-authorized runtime changes fix the maintenance starvation condition below. Python executed source-boundary checks against real source and the counterexample fixture; `git diff --check` passed. **Those checks are not Rust tests.** Three new Rust fixtures, corrected old Rust guards, and both async drain tests remain uncompiled and unexecuted.

## Static drain and boot review

- `DrainMaintenance` has eight explicit u8 discriminants, used as bit positions 0..7. `frame_taken` is guarded by `frames_left > 0`; the normal select path does not statically reveal an underflow or invalid shift.
- The frame quantum suppresses the frame arm after 64 received frames, polls each ready maintenance source once, then the final always-ready arm restores the quantum. Per-item fairness does not bound wall-clock time: one frame can contain many packets, and maintenance scans, ranking and flushing have variable costs.
- The shutdown notification closes admission once and drains queued frames before the common seal/flush path. A producer holding an outstanding channel permit, expensive frame parsing, or slow writers can still delay completion; the main shutdown deadline remains a separate limit.
- The new paused-time test directly polls the pinned drain before each of two 61-second advances; it does not rely on a spawned task running between yields. The intended Tokio receive-budget yield leaves a large backlog. The seven timer counters are collected inside real select arms with `rx` still nonempty. This is a plausible test design, **not an observed pass**, and it does not validate snapshot contents, a live DB or real latency. Existing dev dependencies enable Tokio `test-util`.
- `ws_wal_replay_refused` in `main.rs` is assigned in both `match` arms, and a replay Err leaves the recovered vectors empty. The later refusal branch therefore prevents the no-refold confirmation on that error path. No definite uninitialized-variable, partial-move or borrow defect was found by source review; only a real build can confirm compiler acceptance.

## Maintenance starvation fixed in the candidate

The af77 logic marked a maintenance source serviced only after 64 frames had been consumed. If `rx` remained empty and `seed_rx` stayed ready, the biased seed arm could repeatedly win before every snapshot and silence arm. The busy-frame test did not cover this condition.

The corrected code marks each maintenance source serviced whenever it runs. A pass can begin before the frame quantum is exhausted; arriving frames retain the remaining frame priority, and exhaustion does not erase already-serviced bits. The final ready fallback resets the turn after budget exhaustion or a started maintenance pass. It is disabled in a fresh idle turn, so all-pending inputs park instead of spinning. Each ready maintenance source gets at most one turn per pass, even with an empty frame channel.

Added `test_busy_seed_drain_services_every_cadence_without_frames`: a finite queue of 4,096 repeated seed batches stays ready across two virtual 61-second deadlines while the frame channel stays empty. The real drain is polled directly at startup and after each deadline. Per-arm test counters require every timer to run at least twice while a work queue remains nonempty; because no frames are ever sent, that is explicitly seed backlog. The test also checks real seed registration and bounded shutdown with a live frame sender. The existing busy-frame fixture has no seed backlog and continues to require timer progress while frames remain. **Both are unexecuted Rust regressions.**

## Actual local runtime availability

No `rustc`, `cargo`, `rustup`, `rustfmt`, Docker or Podman executable was on PATH; conventional `/root/.cargo/bin`, `/usr/local/cargo/bin`, `/root/.rustup/toolchains`, `/opt/rust`, `/opt/rustup`, `/usr/local/rustup` were absent. GCC/CC, Python and Node are available. No network installs or external-device calls were made. GCC cannot compile these Rust crates. Cargo, rustfmt and aarch64 execution remain AWS validation work.

## Exact compile and test selectors

Run on the isolated AWS candidate checkout after the root has verified its content hash. Do not substitute the previously copied baseline source. Keep the production app binary unexecuted during build validation.

```sh
cargo fmt --all -- --check
cargo build --locked --offline --release --target aarch64-unknown-linux-musl -p tickvault-app --bin tickvault --bin smoke_test
cargo test --locked --offline --release --target aarch64-unknown-linux-musl -p tickvault-app --lib --no-run --message-format=json
cargo test --locked --offline --release --target aarch64-unknown-linux-musl -p tickvault-app --test deaf_socket_gauge_guard --test ring_dwell_gauge_guard --test dhan_live_off_phase_a_guard --test lane_running_wiring_guard --no-run --message-format=json
```

Use compiler-artifact JSON to identify the exact test executables. For the library executable, first verify each of the following appears in `--list`. Run each selector with `--exact --test-threads=1` and a finite per-process timeout; fail on zero selected tests, nonzero exit or timeout. The 37 selectors are also recorded in `/workspace/scratch/81296451ef3b/tickvault-audit/docs/audits/runtime-assurance-2026-09-14-followup/build-guard-selectors.txt`.

```text
dhan_feed_stack::tests::both_queues_close_before_either_join
dhan_feed_stack::tests::closing_the_queues_rescues_what_backpressure_retained
dhan_feed_stack::tests::the_writer_thread_exists_before_the_writer_is_split
dhan_feed_stack::tests::the_lane_actually_moves_the_depth_flush_off_the_drain
dhan_feed_stack::tests::a_failed_depth_offload_spawn_publishes_the_degraded_gauge
dhan_feed_stack::tests::the_two_writer_joins_share_one_deadline
dhan_feed_stack::tests::the_depth_tail_flushes_before_the_blocking_offload_join
dhan_feed_stack::tests::the_depth_writer_is_built_unconditionally_because_depth_attaches_late
dhan_feed_stack::tests::a_depth_frame_with_no_ingest_is_counted_as_a_wiring_bug_not_as_success
dhan_feed_stack::tests::both_depth_write_paths_consult_the_ingest_shed_gate_and_ticks_never_do
dhan_feed_stack::tests::top_up_unplaced_error_separates_wiring_from_capacity
dhan_feed_stack::tests::the_top_up_budget_is_derived_from_underlyings_that_newly_priced
dhan_feed_stack::tests::contract_dial_retains_its_top_up_channels
dhan_feed_stack::tests::test_dual_candle_writer_refusal_is_wired_before_any_socket
dhan_feed_stack::tests::the_production_boot_site_sizes_the_detector_at_the_authorized_ceiling
dhan_feed_stack::tests::test_drain_never_flushes_bare_on_the_async_worker
dhan_feed_stack::tests::test_stack_never_reaches_for_an_instrument_download
dhan_feed_stack::tests::test_up_gauge_is_raised_by_a_received_frame_and_never_by_a_spawn
dhan_feed_stack::tests::test_the_lane_refuses_to_dial_without_a_wal_or_a_token_manager
dhan_feed_stack::tests::test_depth_late_attach_cannot_delay_the_main_feed_dial
dhan_feed_stack::tests::depth_done_is_never_latched_on_a_successful_plan
dhan_feed_stack::tests::the_preopen_readiness_verdict_does_not_wait_for_the_mover_socket
dhan_feed_stack::tests::every_dial_site_compares_planned_against_dialed
dhan_feed_stack::tests::test_depth_late_attach_holds_only_a_weak_sender
dhan_feed_stack::tests::test_depth_deadline_gates_retries_not_the_first_attempt
dhan_feed_stack::tests::test_depth_give_up_requires_both_the_deadline_and_a_minimum_window
dhan_feed_stack::wal_refold_tests::the_replay_loop_flushes_on_size_so_one_batch_cannot_build_a_four_gb_buffer
dhan_feed_stack::silence_latch_tests::test_risk_gap_03_page_is_rate_limited_across_episodes
dhan_feed_stack::contract_attach_tests::preopen_ready_deadline_never_reaches_the_give_up_arm
dhan_feed_stack::contract_attach_tests::the_mid_session_arm_does_not_emit_the_field_the_alarm_filters_on
dhan_feed_stack::contract_attach_tests::preopen_ready_gauge_is_published_only_on_the_both_halves_success_arm
dhan_feed_stack::frame_walk_accounting_tests::stacked_disconnect_packets_are_counted_per_packet_but_logged_once
dhan_feed_stack::tests::production_scan_keeps_code_after_inline_test_probes
dhan_feed_stack::tests::test_busy_frame_drain_services_every_cadence_before_the_ring_empties
dhan_feed_stack::tests::test_frame_drain_folds_then_ends_when_every_sender_is_dropped
dhan_feed_stack::tests::test_drain_exits_on_shutdown_signal_with_the_ring_still_open
dhan_feed_stack::tests::test_busy_seed_drain_services_every_cadence_without_frames
```

Run each of the four named integration test binaries with a finite timeout and `--test-threads=1`. The new integration regression is `source_scan_keeps_production_after_inline_test_attributes` in `lane_running_wiring_guard`.

For the boot Err path also compile the app `tickvault` binary (above) and run these existing/new storage selectors after compiling `tickvault-storage --lib`:

```text
ws_frame_spill::tests::a_failed_deferred_restore_refuses_the_batch_before_confirmation
ws_frame_spill::tests::replay_all_fenced_returns_the_batch_so_a_refusal_is_visible_to_the_caller
```

Verify their exact module paths via test listing because other agents are editing the storage module. Run app integration `wal_confirm_ordering_guard` and `wal_applied_watermark_wiring_guard` too; these check wiring across startup/replay boundaries but do not simulate a running production boot.

Evidence file: `/workspace/scratch/81296451ef3b/tickvault-audit/docs/audits/runtime-assurance-2026-09-14-followup/build-review-evidence.json`.
