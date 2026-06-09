# Implementation Plan: WAL spill writer thread — survive I/O errors + respawn on panic + alarm the silent drop (zero-tick-loss hardening)

**Status:** VERIFIED
**Date:** 2026-06-09
**Approved by:** Parthiban — AskUserQuestion 2026-06-09 "Zero-tick-loss hardening first" (renewed "no Dhan frame ever lost, in any situation").
**Crate(s) touched:** `tickvault-storage` (`ws_frame_spill.rs`), `tickvault-common` (`error_code.rs` + metrics catalog test) + a new runbook rule file.

## Context

A hostile reliability audit of the full Dhan-frame → durable-persist path found the path is well
defended (capture-before-broadcast is real and ordered; ring→spill→DLQ tiers are alarmed) with ONE
genuine unmitigated loss vector — the WAL frame-spill **writer thread**:

1. **`append()` `Disconnected` arm is SILENT** (`ws_frame_spill.rs:201`): when the background writer
   thread has died, `spill_tx.try_send` returns `Disconnected`, and the arm returns
   `AppendOutcome::Dropped` with **no `error!`, no counter** — unlike the `Full` arm directly above
   (:175-200) which increments `tv_ws_frame_spill_drop_critical` + `tv_ticks_lost_total` and logs.
2. **No supervisor/respawn for the writer thread**: `writer_loop` propagates any disk I/O error
   (`write_record?`, `flush?`, `open_new_segment?`) out of the thread, the thread exits, and from
   then on EVERY frame hits the silent `Disconnected` arm → the durable WAL floor is permanently
   gone with near-zero signal. Contrast WS-GAP-05 (pool supervisor) and DISK-WATCHER-01 (disk
   watcher) which both respawn.

This is the exact "a Dhan frame lost in any situation, silently" case the operator forbids.

## Design

Two layers — prevention first, then detection:

1. **Prevention — resilient `writer_loop` (never dies on transient I/O):** restructure so a per-batch
   `write_record`/`flush`/segment-rotation I/O error does NOT return out of the thread. On error:
   `error!(code = WS-SPILL-01)` + `tv_ws_frame_spill_write_errors_total++`, attempt to open a FRESH
   segment, brief bounded backoff on repeated failure, and **continue draining the channel**. The
   only `Ok(())` return is `rx.recv()` reporting all senders dropped (clean shutdown — preserves the
   existing roundtrip test semantics).
2. **Prevention — respawn-on-panic wrapper:** the spawned thread body wraps `writer_loop(&rx, …)` in
   `std::panic::catch_unwind(AssertUnwindSafe(…))` inside a `loop`. On a caught panic (or belt-and-
   suspenders Err return), `error!(code = WS-SPILL-01)` + `tv_ws_frame_spill_writer_respawn_total++`,
   bounded backoff, and **re-enter `writer_loop` with the SAME `rx`** (borrowed, still alive → channel
   never becomes `Disconnected` from a panic). Clean channel-close exits the outer loop.
3. **Detection — alarm the `Disconnected` arm:** mirror the `Full` arm — `drop_critical++`,
   `tv_ws_frame_spill_drop_critical{reason}`, `tv_ticks_lost_total{source="spill_writer_dead"}`,
   `error!(code = WS-SPILL-01, "CRITICAL: WAL spill writer DEAD — frame dropped")`. With (1)+(2) this
   arm is now practically unreachable, but it is the honest last-resort signal.
4. **New `ErrorCode::WsSpill01WriterDead`** (`code_str "WS-SPILL-01"`, `Severity::Critical`,
   `runbook_path` → new rule file) wired into `all()` + `from_str` + the cross-ref rule file.
5. **Metrics:** two new presence-guarded counters in `crates/common/tests/metrics_catalog.rs`.

Net: a transient disk hiccup or a writer panic no longer tears down the durable WAL floor — the
thread self-heals and keeps capturing every Dhan frame; if the impossible happens, it is loud.

## Edge Cases

- Clean shutdown (all `Sender`s dropped) → `rx.recv()` Err → `writer_loop` returns `Ok(())` → outer
  loop breaks → thread exits. (Preserves `test_append_spill_and_replay_roundtrip`.)
- Panic mid-`write_record` → partial bytes in the current segment → tail corruption → existing
  replay stops-at-boundary logic handles it (already tested by `test_replay_detects_crc_corruption`).
- Repeated `open_new_segment` failure (disk full at rotation) → bounded backoff + continue; records
  during the outage are counted in `tv_ws_frame_spill_write_errors_total` (honest), thread stays
  alive so post-recovery frames are durable again.
- Respawn hot-loop (writer panics every iteration) → bounded `WAL_WRITER_RESPAWN_BACKOFF` sleep caps
  CPU; each respawn increments the counter so the alert fires.

## Failure Modes

- New code adds NO `unwrap`/`expect`/panic path (rust-code rule). `catch_unwind` only CATCHES panics.
- `AssertUnwindSafe` is sound here: the only state crossing the boundary is `&Receiver` (Send) +
  `&AtomicU64` + `&Path` — no `&mut` aliasing, no lock that could be left poisoned.
- Hot path `append()` unchanged in the happy (`Spilled`) path — still O(1), zero-alloc, non-blocking.
- Worst residual: disk down for the entire session AND app crashes → frames drained-but-not-durable
  are lost, but now COUNTED + ALARMED (was silent). This is the documented beyond-envelope tier.

## Test Plan

`cargo test -p tickvault-storage --lib ws_frame_spill` + targeted tests + `-p tickvault-common`:
- `test_writer_survives_io_error_and_keeps_persisting` — inject a transient write failure (first
  segment dir made read-only / a failing writer), assert later frames still persist + the thread did
  NOT exit (channel still accepts, `persisted_count` advances after recovery).
- `test_writer_respawns_after_panic` — force a panic in the writer body (test-only hook), assert the
  thread respawns and subsequent `append`s still `Spilled` (never `Dropped`).
- `test_disconnected_arm_alarms` — construct a spill whose writer is stopped, assert `append` →
  `Dropped` AND `drop_critical_count()` incremented (the arm is no longer silent).
- Existing green tests MUST stay green: `test_append_spill_and_replay_roundtrip`,
  `test_replay_detects_crc_corruption`, `chaos_ws_frame_wal_replay` (4), `chaos_ws_frame_spill_saturation` (3),
  `chaos_disk_full_ulimit` (1), `chaos_zero_tick_loss` (5), `ws_frame_order_preservation_guard` (2),
  `zero_tick_loss_sla_guard` (3), `zero_tick_loss_alert_guard` (4).
- `crates/common` ErrorCode ratchets: `error_code_rule_file_crossref`, `error_code_tag_guard`,
  `test_all_list_length_matches_catalogue_size`, `from_str` roundtrip — all green.
- `crates/common/tests/metrics_catalog.rs` — new metric names present.
- `FULL_QA=1` workspace test (crates/common touched → escalates per testing-scope rule).

## Rollback

Single self-contained PR; revert restores the prior behavior exactly (the `Disconnected` arm reverts
to the silent return, the writer reverts to exit-on-error). No schema change, no data migration, no
wire-format change — the on-disk WAL record format is untouched. Feature is always-on (resilience is
not gated); rollback = `git revert <sha>`.

## Observability

- New `error!(code = WS-SPILL-01)` at the drop + respawn sites → 5-sink fan-out + Telegram.
- New counters `tv_ws_frame_spill_write_errors_total`, `tv_ws_frame_spill_writer_respawn_total`
  (presence-guarded by metrics_catalog). Existing `tv_ws_frame_spill_drop_critical` +
  `tv_ticks_lost_total{source="spill_writer_dead"}` now also fire on writer death.
- New rule file `.claude/rules/project/ws-frame-spill-error-codes.md` documents WS-SPILL-01 triage.

## Per-item guarantee matrix

All 15 "100% everything" + 7 resilience rows from `per-wave-guarantee-matrix.md` apply. Hot path
`append()` happy-path unchanged (O(1), zero-alloc, DHAT-safe); the new resilience is on the writer
thread (cold path). Zero-tick-loss envelope strengthened: the one silent loss vector becomes
self-healing + alarmed. Ratcheted by the 3 new unit tests + the unchanged chaos/guard suite + the
ErrorCode cross-ref/tag guards.

## Plan Items

- [x] Resilient `writer_loop` (survive per-batch I/O error: alarm + reopen segment + continue; only exit on clean channel-close)
  - Files: crates/storage/src/ws_frame_spill.rs
  - Tests: test_writer_survives_unwritable_dir_then_recovers, test_open_segment_resilient_returns_none_on_unopenable_path
- [x] Respawn-on-panic wrapper around the writer thread (catch_unwind + bounded backoff + re-enter with same rx)
  - Files: crates/storage/src/ws_frame_spill.rs
  - Tests: test_writer_respawns_after_panic_sentinel
- [x] Alarm the `Disconnected` arm in `append()` (drop_critical + counters + error! WS-SPILL-02)
  - Files: crates/storage/src/ws_frame_spill.rs
  - Tests: test_disconnected_arm_alarms
- [x] New `ErrorCode::WsSpill01WriterRespawn` + `WsSpill02FrameDropped` (code_str/severity/runbook/from_str/all) + cross-ref rule file
  - Files: crates/common/src/error_code.rs, .claude/rules/project/ws-frame-spill-error-codes.md
  - Tests: every_error_code_variant_appears_in_a_rule_file, test_all_list_length_matches_catalogue_size, test_code_str_follows_expected_prefix_pattern
- [x] Register the 2 new counters in the metrics catalog
  - Files: crates/common/tests/metrics_catalog.rs
  - Tests: metrics_catalog_every_required_metric_is_emitted

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Writer hits a transient disk write error mid-session | Thread alarms + reopens segment + keeps draining; later frames persist; channel never `Disconnected` |
| 2 | Writer thread panics | Caught, alarmed (WS-SPILL-01), respawned with same rx; `append` keeps returning `Spilled` |
| 3 | Writer genuinely dead at an `append` instant | `append` → `Dropped` AND loud (counter + error!) — no longer silent |
| 4 | Clean shutdown (all senders dropped) | Writer drains remaining + exits cleanly (roundtrip test unaffected) |
