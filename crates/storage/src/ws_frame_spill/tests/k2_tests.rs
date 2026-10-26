//! Plan item K2 (2026-10-26) regressions for the WAL core: a failed segment
//! sync raises the lost mark, an abandoned shutdown is counted, an unreadable
//! segment is kept for retry, an unlistable folder refuses the pass, staging
//! failures are counted, a discarded segment is synced and unregistered, the
//! queue byte budget is resolved at construction, and the replay header no
//! longer claims re-capture. A child of `ws_frame_spill::tests`, kept in its
//! own file so the parent stays under its size pin; it reuses the parent's
//! test helpers (`tmp_dir`, `Da13dCounters`, `encode_v4_record_raw`,
//! `wal_files_in`).

use super::*;

/// A frame sequence at base `b` above any wall-clock base, so the global
/// marks other tests raise cannot reach it.
fn k2_seq(b: u64) -> u64 {
    ((1u64 << 47) + b) << PACKET_INDEX_BITS
}

fn k2_replay(dir: &Path) -> anyhow::Result<WalReplayBatch> {
    replay_all_with_report_guarded(dir, usize::MAX, || None, None, WAL_REPLAY_RSS_STOP_PCT)
}

// Regression: 2026-10-26 (K2 fix 1) — a failed segment sync was logged at
// warn and treated as harmless, though a failed fsync can drop the dirty
// pages; the rescue kept deferring failed flushes to the WAL.
#[test]
fn test_regression_k2_failed_sync_raises_the_lost_mark_so_rescues_stop_deferring() {
    let s = k2_seq(1);
    crate::wal_frame_fate::note_flushed_through(s);
    assert!(
        crate::wal_frame_fate::flushed_high_seq() >= s,
        "setup: the flushed mark covers the batch"
    );
    let ok = record_sync_result(
        Err(std::io::Error::other("injected EIO")),
        Instant::now(),
        WAL_PERIODIC_FSYNC_STAGE,
    );
    assert!(!ok, "a failed sync reports failure");
    assert!(
        crate::wal_frame_fate::lost_high_seq() >= s,
        "a failed sync raises the lost mark through the flushed high-water"
    );
    assert!(
        !crate::wal_frame_fate::rescue_may_defer_to_wal(s, s, false),
        "a batch at or below the failed sync is spilled, never deferred to the WAL"
    );
    // The arm logs at error with the WS-SPILL-01 code (source scan: the
    // warn level is gone from both branches).
    let src = include_str!("../../ws_frame_spill.rs");
    let body = src
        .split("fn record_sync_result(")
        .nth(1)
        .and_then(|b| b.split("\n}\n").next())
        .expect("record_sync_result exists");
    assert!(body.contains("sync_failure_raises_lost_mark();"));
    assert_eq!(body.matches(concat!("error", "!(")).count(), 2);
    assert!(!body.contains(concat!("warn", "!(")));
}

/// A spill whose writer thread never finishes inside the shutdown budget,
/// with `persisted` and `flushed` preset. Returns the spill, the sender
/// that releases the stuck thread, and the channel's receiver (kept so the
/// queue reads empty, not disconnected).
fn k2_spill_with_stuck_writer(
    persisted: u64,
    flushed: u64,
) -> (WsFrameSpill, Sender<()>, Receiver<WalRecord>) {
    let dir = tmp_dir("k2-stuck-writer");
    let dir_guard = lock_wal_dir(&dir).expect("fresh temp dir is never contended");
    let (tx, rx) = bounded::<WalRecord>(SPILL_CHANNEL_CAPACITY);
    let (release_tx, release_rx) = bounded::<()>(1);
    let stuck = thread::spawn(move || {
        let _released = release_rx.recv_timeout(Duration::from_secs(30));
    });
    let spill = WsFrameSpill {
        spill_tx: tx,
        queued_bytes: Arc::new(AtomicU64::new(0)),
        queue_budget_bytes: wal_queue_max_bytes(),
        drop_critical: Arc::new(AtomicU64::new(0)),
        persisted_total: Arc::new(AtomicU64::new(persisted)),
        flushed_total: Arc::new(AtomicU64::new(flushed)),
        drop_counters: SpillDropCounters::new(),
        feed_health: None,
        stop: Arc::new(AtomicBool::new(false)),
        writer: std::sync::Mutex::new(Some(stuck)),
        syncer: Arc::new(WalSyncer::new()),
        syncer_thread: std::sync::Mutex::new(None),
        wal_dir: dir,
        _dir_guard: dir_guard,
        abort_drain: Arc::new(AbortDrain::default()),
    };
    (spill, release_tx, rx)
}

// Regression: 2026-10-26 (K2 fix 2) — a shutdown that abandoned a stuck
// writer with an EMPTY queue logged "final drain complete" and returned 0
// while the writer still held counted records in its segment buffer.
#[test]
fn test_regression_k2_shutdown_abandoning_a_stuck_writer_counts_its_unflushed_records() {
    let (spill, release, _rx) = k2_spill_with_stuck_writer(5, 2);
    let recorder = Da13dCounters::default();
    let outcome = metrics::with_local_recorder(&recorder, || {
        spill.shutdown_outcome(Duration::from_millis(200))
    });
    assert_eq!(
        outcome,
        WalShutdownOutcome::Abandoned {
            queued: 0,
            unflushed: 3
        },
        "an abandoned writer is a distinct outcome, never Drained"
    );
    assert_eq!(
        outcome.lost_records(),
        3,
        "shutdown returns persisted − flushed"
    );
    assert_eq!(
        recorder.get(WAL_SPILL_SHUTDOWN_INCOMPLETE_COUNTER),
        3,
        "the unflushed records reach the shutdown-incomplete counter"
    );
    let _ = release.send(());
    let src = include_str!("../../ws_frame_spill.rs");
    let body = src
        .split("fn shutdown_outcome(")
        .nth(1)
        .and_then(|b| b.split("\n    }\n}\n").next())
        .expect("shutdown_outcome exists");
    let complete_at = body.find("final drain complete").expect("the clean line");
    let abandoned_return = body
        .find("return WalShutdownOutcome::Abandoned")
        .expect("abandoned returns early");
    assert!(
        abandoned_return < complete_at,
        "an abandoned shutdown returns before the 'complete' line"
    );
}

/// K2 fix 2, the counting half: `flushed` moves only for records that
/// reached the kernel, so `persisted − flushed` is the open buffer.
#[test]
fn test_regression_k2_tally_counts_flushed_records_including_a_discards_kept_part() {
    let dir = tmp_dir("k2-flushed-count");
    let persisted = AtomicU64::new(0);
    let flushed = AtomicU64::new(0);
    let mut tally = UnflushedTally::new(&persisted).counting_flushed_into(&flushed);
    let mut w = open_new_segment(&dir).unwrap();
    let record = |seq: u64| WalRecord {
        ws_type: WsType::LiveFeed,
        frame_seq: seq,
        received_at_nanos: 1_000,
        endpoint: WalEndpoint::MainFeed,
        frame: Bytes::from(vec![seq as u8; 40]),
    };
    for seq in 1..=3 {
        write_record(&mut w, &record(seq)).unwrap();
        tally.note_written(record_disk_size(&record(seq)), seq, WsType::LiveFeed);
    }
    assert_eq!(flushed.load(Ordering::Relaxed), 0, "nothing flushed yet");
    w.flush().unwrap();
    tally.flushed();
    assert_eq!(flushed.load(Ordering::Relaxed), 3);
    // Two more written into the buffer, one of which reaches the file.
    write_record(&mut w, &record(4)).unwrap();
    tally.note_written(record_disk_size(&record(4)), 4, WsType::LiveFeed);
    w.flush().unwrap();
    write_record(&mut w, &record(5)).unwrap();
    tally.note_written(record_disk_size(&record(5)), 5, WsType::LiveFeed);
    discard_segment_writer(w, &mut tally, "test");
    assert_eq!(persisted.load(Ordering::Relaxed), 4, "record 5 is lost");
    assert_eq!(
        flushed.load(Ordering::Relaxed),
        4,
        "record 4 reached the file, so it counts as flushed"
    );
    clear_open_segment_under(&dir);
    let _ = std::fs::remove_dir_all(&dir);
}

// Regression: 2026-10-26 (K2 fix 3) — a segment that hit an I/O error on
// open/read was counted consumed, STAGED and archived by the confirm
// without one frame read. A directory named like a segment gives a real
// EISDIR from `read_to_end`.
#[test]
fn test_regression_k2_unreadable_segment_is_kept_for_retry_not_archived() {
    let dir = tmp_dir("k2-io-retry");
    let replaying = dir.join(REPLAYING_SUBDIR);
    std::fs::create_dir_all(&replaying).unwrap();
    let a = dir.join("ws-frames-77777777770000000001.wal");
    let b_name = "ws-frames-77777777770000000002.wal";
    let c = dir.join("ws-frames-77777777770000000003.wal");
    std::fs::write(
        &a,
        encode_v4_record_raw(WsType::LiveFeed, 100, 1, 0, &[1; 8]),
    )
    .unwrap();
    std::fs::write(
        &c,
        encode_v4_record_raw(WsType::LiveFeed, 300, 3, 0, &[3; 8]),
    )
    .unwrap();
    // B is a leftover in `replaying/` that cannot be read.
    std::fs::create_dir_all(replaying.join(b_name)).unwrap();

    let recorder = Da13dCounters::default();
    let first = metrics::with_local_recorder(&recorder, || k2_replay(&dir)).expect("pass 1");
    assert_eq!(first.frames.len(), 2, "A and C are read");
    assert!(
        dir.join(b_name).is_dir(),
        "the unreadable leftover goes back to the live folder"
    );
    assert!(!replaying.join(b_name).exists(), "it is not left staged");
    assert!(
        replaying.join(a.file_name().unwrap()).exists(),
        "A is staged"
    );
    assert_eq!(recorder.get(WAL_REPLAY_READ_RETRIED_COUNTER), 1);
    confirm_replayed(&dir);
    assert!(
        dir.join(b_name).is_dir(),
        "the confirm cannot archive what was never staged"
    );
    assert!(!dir.join(ARCHIVE_SUBDIR).join(b_name).exists());

    // Pass 2 retries and keeps it; pass 3 reaches the cap and gives up,
    // which is the old behaviour, counted on the corrupted counter.
    let second = k2_replay(&dir).expect("pass 2");
    assert_eq!(second.frames.len(), 0);
    assert!(dir.join(b_name).is_dir(), "attempt 2 is still kept");
    let recorder = Da13dCounters::default();
    let _third = metrics::with_local_recorder(&recorder, || k2_replay(&dir)).expect("pass 3");
    assert_eq!(
        recorder.get("tv_wal_replay_corrupted_segments_total"),
        1,
        "at the cap it is counted on the paged counter"
    );
    assert!(
        replaying.join(b_name).exists(),
        "at the cap it is staged as before (archived by the confirm)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// Regression: 2026-10-26 (K2 fix 4) — `wal_segments_in` read a listing
// error as "no segments", so a `replaying/` that could not be listed let
// the pass stage live segments beside leftovers it never saw.
#[test]
fn test_regression_k2_unlistable_replaying_folder_refuses_the_pass() {
    let dir = tmp_dir("k2-unlistable");
    let a = dir.join("ws-frames-77777777771000000001.wal");
    std::fs::write(
        &a,
        encode_v4_record_raw(WsType::LiveFeed, 100, 1, 0, &[1; 8]),
    )
    .unwrap();
    // A FILE where the folder should be: `read_dir` fails with ENOTDIR.
    std::fs::write(dir.join(REPLAYING_SUBDIR), b"not a folder").unwrap();
    assert!(try_wal_segments_in(&dir.join(REPLAYING_SUBDIR)).is_err());
    assert!(
        try_wal_segments_in(&dir.join("missing"))
            .unwrap()
            .is_empty(),
        "only a missing folder reads as empty"
    );
    let recorder = Da13dCounters::default();
    let pass = metrics::with_local_recorder(&recorder, || k2_replay(&dir));
    assert!(pass.is_err(), "the pass is refused, fail closed");
    assert!(a.exists(), "the live segment is not staged");
    assert_eq!(recorder.get(WAL_REPLAY_LISTING_REFUSED_COUNTER), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

// Regression: 2026-10-26 (K2 fix 5) — the staging `create_dir_all` and
// `rename` were `drop(..)`ed. A dangling symlink at `replaying/` lists as
// missing (empty) but refuses both the mkdir and the rename.
#[test]
fn test_regression_k2_staging_failures_are_counted_and_the_segment_stays_live() {
    let dir = tmp_dir("k2-stage-fail");
    let a = dir.join("ws-frames-77777777772000000001.wal");
    std::fs::write(
        &a,
        encode_v4_record_raw(WsType::LiveFeed, 100, 1, 0, &[1; 8]),
    )
    .unwrap();
    std::os::unix::fs::symlink(dir.join("no-such-target"), dir.join(REPLAYING_SUBDIR)).unwrap();
    let recorder = Da13dCounters::default();
    let pass = metrics::with_local_recorder(&recorder, || k2_replay(&dir)).expect("pass");
    assert_eq!(pass.frames.len(), 1);
    assert!(a.exists(), "a failed stage leaves the segment a live *.wal");
    assert_eq!(
        recorder.get(&format!(
            "{WAL_REPLAY_STAGE_FAILED_COUNTER},step=replay_stage_mkdir"
        )),
        1
    );
    assert_eq!(
        recorder.get(&format!(
            "{WAL_REPLAY_STAGE_FAILED_COUNTER},step=replay_stage_rename"
        )),
        1
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// Regression: 2026-10-26 (K2 fix 6) — a discarded segment writer was not
// synced, stayed registered as the OPEN segment (so a replay before the
// next open skipped it), and its bytes kept counting toward the next
// segment's rotation.
#[test]
fn test_regression_k2_discarded_segment_is_synced_unregistered_and_resets_the_count() {
    let src = include_str!("../../ws_frame_spill.rs");
    let production = src
        .split(concat!("\n#[cfg(", "test)]\n"))
        .next()
        .unwrap_or(src);
    let discard = production
        .split("fn discard_segment_writer(")
        .nth(1)
        .and_then(|b| b.split("\n}\n").next())
        .expect("discard_segment_writer exists");
    assert!(
        discard.contains("file.sync_data()"),
        "a discard syncs what it keeps"
    );
    for caller in [
        "discard_segment_writer(w, &mut tally, \"flush\");",
        "discard_segment_writer(w, tally, \"write_record\");",
    ] {
        let at = production.find(caller).expect("discard caller exists");
        let after = &production[at..at + 200];
        assert!(
            after.contains("clear_open_segment_under(wal_dir);"),
            "`{caller}` must clear the open-segment registration"
        );
    }
    let open = production
        .split("fn open_segment_resilient(")
        .nth(1)
        .and_then(|b| b.split("\n}\n").next())
        .expect("open_segment_resilient exists");
    assert_eq!(
        open.matches("clear_open_segment_under(wal_dir);").count(),
        2,
        "both open-failure arms clear the registration"
    );
    assert!(production.contains("bytes_written = bytes_written.min(tally.segment_bytes);"));

    // Behaviour: the caller pattern really unregisters the file.
    let dir = tmp_dir("k2-discard-unregister");
    let w = open_new_segment(&dir).unwrap();
    let path = wal_files_in(&dir).pop().expect("one segment");
    assert!(
        is_open_segment(&path),
        "setup: an open segment is registered"
    );
    let persisted = AtomicU64::new(0);
    let mut tally = UnflushedTally::new(&persisted);
    discard_segment_writer(w, &mut tally, "test");
    clear_open_segment_under(&dir);
    assert!(!is_open_segment(&path));
    assert_eq!(wal_segments_in(&dir), vec![path], "a replay now sees it");
    let _ = std::fs::remove_dir_all(&dir);
}

// Regression: 2026-10-26 (K2 fix 7) — the queue byte budget was resolved
// on the first `append`, on a socket read task.
#[test]
fn test_regression_k2_queue_byte_budget_is_resolved_at_construction() {
    let dir = tmp_dir("k2-budget");
    let spill = WsFrameSpill::new(&dir).expect("spill opens");
    assert_eq!(spill.queue_budget_bytes, wal_queue_max_bytes());
    assert!(spill.queue_budget_bytes >= WAL_QUEUE_MIN_BYTES);
    assert_eq!(spill.shutdown(Duration::from_secs(5)), 0);
    drop(spill);
    let _ = std::fs::remove_dir_all(&dir);
}

// Regression: 2026-10-26 (K2 fix 8) — the replay header claimed replayed
// frames are re-captured into the WAL; they are not.
#[test]
fn test_regression_k2_replay_header_no_longer_claims_recapture() {
    // Every literal is split with `concat!` so this test cannot match
    // its own source.
    let src = include_str!("../../ws_frame_spill.rs");
    let stale = concat!("durably RE-", "captures them into a fresh");
    assert!(!src.contains(stale), "the stale re-capture claim is back");
    assert!(src.contains(concat!("ACK-BEFORE-", "CONFIRM")));
    assert!(src.contains(concat!("they are NEVER written ", "to the WAL a")));
}
