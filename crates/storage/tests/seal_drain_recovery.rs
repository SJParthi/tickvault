//! Boot-time seal recovery drain — the missing reader of the spill/DLQ tiers.
//!
//! ## The bug these tests pin (verified 2026-08-11)
//!
//! `seal_writer_task::drain_once` rescues sealed candles to `SealSpillWriter`
//! / `SealDlqWriter` whenever the ILP flush to QuestDB fails. Both writers
//! ship a `read_all` + `clear_*_for_date` pair whose doc-comments promise
//! "the caller (writer task) deletes the file after `read_all` is fully
//! replayed" — but **that caller never existed**. Every `read_all` call site
//! in `crates/*/src` sat inside its own file's `#[cfg(test)] mod tests`
//! block, and no crate outside `storage` referenced the spill/DLQ types at
//! all. So candles rescued during a QuestDB outage landed in files that
//! nothing ever read back: permanent silent loss, while
//! `tv_seal_writer_drain_total{kind="rescued_spill"}` reported them ABSORBED.
//!
//! These tests pin the recovery contract end-to-end:
//!
//! | # | Test | Proves |
//! |---|------|--------|
//! | 1 | `spilled_seals_are_all_reingested_after_restart` | every seal written with the DB down comes back on restart |
//! | 2 | `boot_drain_run_twice_does_not_double_ingest` | idempotent — a second run recovers nothing |
//! | 3 | `flush_failure_leaves_file_staged_then_recovers_next_boot` | fail-closed: a dead DB never loses the file, and a later boot recovers it EXACTLY once |
//! | 4 | `dlq_ndjson_records_are_recovered_too` | tier-3 DLQ is drained, not just tier-2 spill |
//! | 5 | `archived_files_are_never_replayed_again` | confirmed history cannot re-inject |
//! | 6 | `crash_between_read_and_confirm_recovers_not_loses` | staging survives a crash mid-recovery |
//! | 7 | `corrupt_tail_is_counted_not_silently_lost` | undecodable bytes are reported, never swallowed |
//! | 8 | `runner_boot_drain_reports_pending_honestly_when_db_is_down` | the production `SealWriterRunner` seam, dead DB, honest reporting |
//! | 9 | `real_rescue_path_end_to_end_is_recovered` | seals rescued by the REAL `drain_once` cascade are recovered |
//!
//! No live QuestDB and no `unsafe`: the ILP side is exercised through the
//! `SealSink` seam (`ShadowCandleWriter::for_test()` is permanently
//! disconnected, so its `flush` can only ever fail — the success half of the
//! contract would otherwise be untestable, and therefore unproven).

use std::path::{Path, PathBuf};

use tickvault_common::feed::Feed;
use tickvault_storage::seal_absorption::SealAbsorptionPipeline;
use tickvault_storage::seal_dlq::{SealDlqRecord, SealDlqWriter};
use tickvault_storage::seal_spill::{SealSpillWriter, SerializedSeal};
use tickvault_storage::seal_writer_runner::SealWriterRunner;
use tickvault_storage::seal_writer_task::{
    SEAL_ARCHIVE_SUBDIR, SEAL_REPLAYING_SUBDIR, SealSink, drain_once, drain_recovered_seals,
};
use tickvault_storage::shadow_candle_writer::ShadowCandleWriter;
use tickvault_trading::candles::{BufferedSeal, LiveCandleState, TfIndex};

/// 2026-01-01 12:00 UTC — a fixed instant so the IST-derived filename is
/// deterministic and the tests never depend on the wall clock.
const NOW: i64 = 1_767_268_800;

// ---------------------------------------------------------------------------
// Test doubles + helpers
// ---------------------------------------------------------------------------

/// A `SealSink` that records everything it commits and can be told to fail.
///
/// Mirrors `ShadowCandleWriter` semantics exactly: `append_seal` buffers,
/// `flush` either commits the buffer or returns `Err` leaving it retained,
/// `discard_pending` drops the retained buffer.
#[derive(Default)]
struct FakeSink {
    /// Seals committed by a successful flush, in commit order.
    committed: Vec<(u64, u8, u32)>,
    /// Seals appended but not yet flushed.
    buffered: Vec<(u64, u8, u32)>,
    /// When `true`, every `flush` fails (simulates QuestDB being down).
    fail_flush: bool,
    /// How many times `flush` was called.
    flushes: usize,
    /// How many times `discard_pending` was called.
    discards: usize,
}

impl FakeSink {
    fn healthy() -> Self {
        Self::default()
    }

    fn dead() -> Self {
        Self {
            fail_flush: true,
            ..Self::default()
        }
    }

    /// Committed seals as a sorted, de-duplicated key set.
    fn committed_keys(&self) -> Vec<(u64, u8, u32)> {
        let mut keys = self.committed.clone();
        keys.sort_unstable();
        keys.dedup();
        keys
    }
}

impl SealSink for FakeSink {
    fn append_seal(&mut self, seal: &BufferedSeal) -> anyhow::Result<()> {
        self.buffered.push((
            seal.security_id,
            seal.exchange_segment_code,
            seal.state.bucket_start_ist_secs,
        ));
        Ok(())
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        self.flushes += 1;
        if self.fail_flush {
            // Buffer stays retained, exactly like the real writer.
            return Err(anyhow::anyhow!("questdb down"));
        }
        self.committed.append(&mut self.buffered);
        Ok(())
    }

    fn discard_pending(&mut self) {
        self.discards += 1;
        self.buffered.clear();
    }
}

fn unique_dirs(tag: &str) -> (PathBuf, PathBuf) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!(
        "tv-seal-drain-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let spill = root.join("spill");
    let dlq = root.join("dlq");
    std::fs::create_dir_all(&spill).expect("spill dir");
    std::fs::create_dir_all(&dlq).expect("dlq dir");
    (spill, dlq)
}

fn cleanup(spill: &Path, dlq: &Path) {
    if let Some(root) = spill.parent() {
        let _ = std::fs::remove_dir_all(root);
    }
    let _ = std::fs::remove_dir_all(dlq);
}

fn mk_seal(sid: u64, bucket: u32) -> BufferedSeal {
    let mut state = LiveCandleState::empty();
    state.bucket_start_ist_secs = bucket;
    state.open = 100.0;
    state.high = 105.5;
    state.low = 99.25;
    state.close = 101.75;
    state.volume = 4321;
    state.bucket_start_cumulative = 1000;
    state.oi = 50_000;
    state.tick_count = 7;
    BufferedSeal::new(sid, 1, TfIndex::M1, state, Feed::Dhan)
}

/// Writes `n` seals into the spill tier the same way the rescue path does.
fn spill_n(dir: &Path, n: u64) -> Vec<(u64, u8, u32)> {
    let writer = SealSpillWriter::with_spill_dir_for_test(dir.to_path_buf());
    let mut keys = Vec::new();
    for i in 0..n {
        let seal = mk_seal(1000 + i, 34_200 + (i as u32) * 60);
        writer
            .append_seal(&SerializedSeal::from(&seal), NOW)
            .expect("spill append");
        keys.push((
            seal.security_id,
            seal.exchange_segment_code,
            seal.state.bucket_start_ist_secs,
        ));
    }
    keys.sort_unstable();
    keys
}

/// Writes `n` records into the DLQ tier.
fn dlq_n(dir: &Path, n: u64) -> Vec<(u64, u8, u32)> {
    let writer = SealDlqWriter::with_dlq_dir_for_test(dir.to_path_buf());
    let mut keys = Vec::new();
    for i in 0..n {
        let seal = mk_seal(7000 + i, 40_000 + (i as u32) * 60);
        let record = SealDlqRecord::from(&SerializedSeal::from(&seal));
        writer.append_record(&record, NOW).expect("dlq append");
        keys.push((
            seal.security_id,
            seal.exchange_segment_code,
            seal.state.bucket_start_ist_secs,
        ));
    }
    keys.sort_unstable();
    keys
}

/// Count of live (un-staged) `seals-*` files directly in `dir`.
fn live_file_count(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_file())
        .filter(|e| e.file_name().to_string_lossy().starts_with("seals-"))
        .count()
}

fn subdir_file_count(dir: &Path, sub: &str) -> usize {
    let path = dir.join(sub);
    if !path.exists() {
        return 0;
    }
    std::fs::read_dir(&path)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_file())
        .count()
}

// ---------------------------------------------------------------------------
// 1 — the headline: seals written with the DB down survive a restart
// ---------------------------------------------------------------------------

#[test]
fn spilled_seals_are_all_reingested_after_restart() {
    let (spill, dlq) = unique_dirs("reingest");
    let expected = spill_n(&spill, 25);
    assert_eq!(live_file_count(&spill), 1, "one daily spill file on disk");

    // "Restart": a fresh process runs the boot drain against a healthy DB.
    let mut sink = FakeSink::healthy();
    let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 8);

    assert_eq!(outcome.files_staged, 1);
    assert_eq!(outcome.seals_recovered, 25, "every spilled seal read back");
    assert_eq!(outcome.seals_reingested, 25, "every seal re-ingested");
    assert_eq!(outcome.files_archived, 1);
    assert_eq!(outcome.files_left_pending, 0);
    assert_eq!(outcome.seals_left_pending, 0);
    assert_eq!(outcome.records_undecodable, 0);

    // Exactly once — no duplicates in what reached the sink.
    assert_eq!(sink.committed.len(), 25, "no duplicate commits");
    assert_eq!(sink.committed_keys(), expected, "byte-exact seal identity");

    // File lifecycle: gone from live + staging, present in archive.
    assert_eq!(live_file_count(&spill), 0);
    assert_eq!(subdir_file_count(&spill, SEAL_REPLAYING_SUBDIR), 0);
    assert_eq!(subdir_file_count(&spill, SEAL_ARCHIVE_SUBDIR), 1);

    cleanup(&spill, &dlq);
}

// ---------------------------------------------------------------------------
// 2 — idempotency: re-running must not double-write
// ---------------------------------------------------------------------------

#[test]
fn boot_drain_run_twice_does_not_double_ingest() {
    let (spill, dlq) = unique_dirs("twice");
    spill_n(&spill, 12);

    let mut sink = FakeSink::healthy();
    let first = drain_recovered_seals(&mut sink, &spill, &dlq, 64);
    assert_eq!(first.seals_reingested, 12);

    // Second run — nothing left to find. This is the guarantee that a
    // supervisor restart loop cannot multiply rows.
    let second = drain_recovered_seals(&mut sink, &spill, &dlq, 64);
    assert!(second.is_clean(), "second run must find nothing staged");
    assert_eq!(second.files_staged, 0);
    assert_eq!(second.seals_recovered, 0);
    assert_eq!(second.seals_reingested, 0);

    assert_eq!(sink.committed.len(), 12, "still exactly 12 after two runs");
    assert_eq!(subdir_file_count(&spill, SEAL_ARCHIVE_SUBDIR), 1);

    cleanup(&spill, &dlq);
}

// ---------------------------------------------------------------------------
// 3 — fail-closed: a dead DB never loses the file
// ---------------------------------------------------------------------------

#[test]
fn flush_failure_leaves_file_staged_then_recovers_next_boot() {
    let (spill, dlq) = unique_dirs("staged");
    let expected = spill_n(&spill, 10);

    // Boot #1: QuestDB still down.
    let mut dead = FakeSink::dead();
    let first = drain_recovered_seals(&mut dead, &spill, &dlq, 4);
    assert_eq!(first.files_staged, 1);
    assert_eq!(first.seals_recovered, 10);
    assert_eq!(first.seals_reingested, 0, "nothing committed to a dead DB");
    assert_eq!(
        first.files_archived, 0,
        "must NOT archive an unconfirmed file"
    );
    assert_eq!(first.files_left_pending, 1);
    assert_eq!(first.seals_left_pending, 10, "honest pending count");
    assert!(dead.discards >= 1, "poison buffer discarded after failure");

    // The bytes are still on disk, parked in staging for the next boot.
    assert_eq!(subdir_file_count(&spill, SEAL_REPLAYING_SUBDIR), 1);
    assert_eq!(subdir_file_count(&spill, SEAL_ARCHIVE_SUBDIR), 0);

    // Boot #2: QuestDB back up. Recovers EXACTLY the original set — no loss
    // from boot #1, and no duplication from having been read twice.
    let mut healthy = FakeSink::healthy();
    let second = drain_recovered_seals(&mut healthy, &spill, &dlq, 4);
    assert_eq!(second.files_staged, 1, "re-globbed from replaying/");
    assert_eq!(second.seals_recovered, 10);
    assert_eq!(second.seals_reingested, 10);
    assert_eq!(second.files_archived, 1);
    assert_eq!(second.seals_left_pending, 0);
    assert_eq!(healthy.committed.len(), 10, "exactly once, not twice");
    assert_eq!(healthy.committed_keys(), expected);

    cleanup(&spill, &dlq);
}

// ---------------------------------------------------------------------------
// 4 — tier 3: the NDJSON DLQ is drained too
// ---------------------------------------------------------------------------

#[test]
fn dlq_ndjson_records_are_recovered_too() {
    let (spill, dlq) = unique_dirs("dlq");
    let spilled = spill_n(&spill, 5);
    let dlqd = dlq_n(&dlq, 7);

    let mut sink = FakeSink::healthy();
    let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 64);

    assert_eq!(outcome.files_staged, 2, "spill file + dlq file");
    assert_eq!(outcome.seals_recovered, 12);
    assert_eq!(outcome.seals_reingested, 12);
    assert_eq!(outcome.files_archived, 2);

    let mut expected = [spilled, dlqd].concat();
    expected.sort_unstable();
    assert_eq!(sink.committed_keys(), expected, "both tiers recovered");

    assert_eq!(subdir_file_count(&spill, SEAL_ARCHIVE_SUBDIR), 1);
    assert_eq!(subdir_file_count(&dlq, SEAL_ARCHIVE_SUBDIR), 1);

    cleanup(&spill, &dlq);
}

// ---------------------------------------------------------------------------
// 5 — confirmed history can never re-inject
// ---------------------------------------------------------------------------

#[test]
fn archived_files_are_never_replayed_again() {
    let (spill, dlq) = unique_dirs("archive");
    spill_n(&spill, 6);

    let mut sink = FakeSink::healthy();
    drain_recovered_seals(&mut sink, &spill, &dlq, 64);
    assert_eq!(subdir_file_count(&spill, SEAL_ARCHIVE_SUBDIR), 1);

    // Three more boots. The archive must never be re-globbed, no matter how
    // many times the process restarts.
    for _ in 0..3 {
        let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 64);
        assert_eq!(outcome.files_staged, 0);
        assert_eq!(outcome.seals_recovered, 0);
    }
    assert_eq!(sink.committed.len(), 6, "archive never re-injected");

    cleanup(&spill, &dlq);
}

// ---------------------------------------------------------------------------
// 6 — a crash between read and confirm recovers rather than loses
// ---------------------------------------------------------------------------

#[test]
fn crash_between_read_and_confirm_recovers_not_loses() {
    let (spill, dlq) = unique_dirs("crash");
    let expected = spill_n(&spill, 9);

    // Simulate: the previous boot staged the file and then died BEFORE the
    // archive rename (the exact crash window the two-phase design targets).
    let staged = spill.join(SEAL_REPLAYING_SUBDIR);
    std::fs::create_dir_all(&staged).expect("staging dir");
    for entry in std::fs::read_dir(&spill).expect("read spill").flatten() {
        let path = entry.path();
        if path.is_file() {
            let name = path.file_name().expect("name").to_owned();
            std::fs::rename(&path, staged.join(name)).expect("stage");
        }
    }
    assert_eq!(live_file_count(&spill), 0);
    assert_eq!(subdir_file_count(&spill, SEAL_REPLAYING_SUBDIR), 1);

    // Next boot re-globs staging — nothing is stranded.
    let mut sink = FakeSink::healthy();
    let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 64);
    assert_eq!(outcome.files_staged, 1);
    assert_eq!(outcome.seals_reingested, 9);
    assert_eq!(sink.committed_keys(), expected);

    cleanup(&spill, &dlq);
}

// ---------------------------------------------------------------------------
// 7 — undecodable bytes are reported, never swallowed
// ---------------------------------------------------------------------------

#[test]
fn corrupt_tail_is_counted_not_silently_lost() {
    let (spill, dlq) = unique_dirs("corrupt");
    spill_n(&spill, 4);

    // Append a torn trailing record (a partial write at the moment of a
    // crash) plus a full record whose format-version byte is the legacy 0.
    let file_path = SealSpillWriter::with_spill_dir_for_test(spill.clone()).spill_path(NOW);
    let mut bytes = std::fs::read(&file_path).expect("read spill");
    let mut legacy = bytes[0..128].to_vec();
    legacy[7] = 0; // pre-renumber format marker — must be refused, not decoded
    bytes.extend_from_slice(&legacy);
    bytes.extend_from_slice(&[0xAB; 37]); // torn tail
    std::fs::write(&file_path, &bytes).expect("write spill");

    let mut sink = FakeSink::healthy();
    let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 64);

    assert_eq!(outcome.seals_recovered, 4, "the 4 good seals still recover");
    assert_eq!(outcome.seals_reingested, 4);
    assert_eq!(
        outcome.records_undecodable, 1,
        "the legacy-format record is COUNTED, not silently decoded"
    );
    // The torn tail is not a record at all — it is truncated, so it is not
    // counted as a decodable-but-bad record. The bytes survive in archive/.
    assert_eq!(subdir_file_count(&spill, SEAL_ARCHIVE_SUBDIR), 1);

    cleanup(&spill, &dlq);
}

// ---------------------------------------------------------------------------
// 8 — the production runner seam, with a genuinely dead ILP writer
// ---------------------------------------------------------------------------

#[test]
fn runner_boot_drain_reports_pending_honestly_when_db_is_down() {
    let (spill, dlq) = unique_dirs("runner");
    spill_n(&spill, 3);

    // `SealWriterRunner::for_test` uses the real (permanently disconnected)
    // `ShadowCandleWriter`, so this is the true production shape against a
    // dead QuestDB.
    let mut runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 8);
    assert_eq!(runner.spill_dir(), spill.as_path());
    assert_eq!(runner.dlq_dir(), dlq.as_path());

    let outcome = runner.boot_drain();
    assert_eq!(outcome.files_staged, 1);
    assert_eq!(outcome.seals_recovered, 3);
    assert_eq!(outcome.seals_reingested, 0, "a dead DB commits nothing");
    assert_eq!(outcome.files_archived, 0, "and confirms nothing");
    assert_eq!(outcome.seals_left_pending, 3, "reported, not hidden");
    assert!(!outcome.is_clean());

    // The seals are still on disk — recoverable, never dropped.
    assert_eq!(subdir_file_count(&spill, SEAL_REPLAYING_SUBDIR), 1);

    cleanup(&spill, &dlq);
}

// ---------------------------------------------------------------------------
// 10 — wiring ratchet: the drain must stay CALLED, not merely exist
// ---------------------------------------------------------------------------

#[test]
fn boot_drain_stays_wired_into_the_writer_loop() {
    // The original bug was not a missing function — `read_all` existed for
    // months. The bug was that NOTHING CALLED IT. A recovery drain that
    // exists but is never invoked reproduces the exact silent-loss class, so
    // the call site itself is ratcheted here.
    let loop_src = include_str!("../src/seal_writer_loop.rs");
    let (prod, _tests) = loop_src
        .split_once("#[cfg(test)]")
        .expect("seal_writer_loop.rs must contain a test module marker");

    assert!(
        prod.contains("runner.boot_drain()"),
        "the writer loop MUST call runner.boot_drain() at startup — without a \
         caller, seals rescued to spill/DLQ are never read back (silent loss)"
    );
    assert!(
        prod.contains("record_boot_drain_observability(&boot)"),
        "the boot-drain outcome must be fanned into counters, not dropped"
    );
    assert!(
        prod.contains("NOT QuestDB"),
        "a window that rescues seals to disk must say so as an error — \
         'rescued' must never be reported as 'absorbed'"
    );

    // And the runner must actually reach the recovery function.
    let runner_src = include_str!("../src/seal_writer_runner.rs");
    let (runner_prod, _) = runner_src
        .split_once("#[cfg(test)]")
        .expect("seal_writer_runner.rs must contain a test module marker");
    assert!(
        runner_prod.contains("drain_recovered_seals("),
        "SealWriterRunner::boot_drain must delegate to drain_recovered_seals"
    );
}

// ---------------------------------------------------------------------------
// 9 — end-to-end through the REAL rescue cascade
// ---------------------------------------------------------------------------

#[test]
fn real_rescue_path_end_to_end_is_recovered() {
    let (spill, dlq) = unique_dirs("e2e");

    // Drive the genuine production rescue path: submit seals into the
    // absorption pipeline, then `drain_once` against a disconnected writer so
    // the flush fails and the cascade spills them to disk.
    let mut pipeline =
        SealAbsorptionPipeline::with_capacity_and_dirs_for_test(64, spill.clone(), dlq.clone());
    let mut writer = ShadowCandleWriter::for_test();
    let mut expected = Vec::new();
    for i in 0..8u64 {
        let seal = mk_seal(2000 + i, 34_200 + (i as u32) * 60);
        expected.push((
            seal.security_id,
            seal.exchange_segment_code,
            seal.state.bucket_start_ist_secs,
        ));
        pipeline.submit(seal, NOW);
    }
    let drained = drain_once(&mut pipeline, &mut writer, 64, NOW);
    assert!(
        !drained.flushed_ok,
        "disconnected writer must fail its flush"
    );
    assert_eq!(
        drained.rescued_to_spill + drained.rescued_to_dlq,
        8,
        "all 8 seals rescued to disk by the real cascade"
    );
    expected.sort_unstable();

    // Restart: the boot drain must find every single one of them.
    let mut sink = FakeSink::healthy();
    let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 64);
    assert_eq!(
        outcome.seals_recovered, 8,
        "recovered every seal the rescue cascade wrote"
    );
    assert_eq!(outcome.seals_reingested, 8);
    assert_eq!(outcome.seals_left_pending, 0);
    assert_eq!(
        sink.committed_keys(),
        expected,
        "byte-exact identity through rescue → disk → recovery"
    );

    cleanup(&spill, &dlq);
}
