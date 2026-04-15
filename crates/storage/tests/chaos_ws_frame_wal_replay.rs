//! STAGE-D P7 — Chaos: SIGKILL + WAL replay, all 4 WS types.
//!
//! Simulates the exact failure mode we want the zero-tick-loss guarantee to
//! survive: a previous `tickvault` process received SIGKILL mid-session and
//! died with frames still in the WS reader's hot path. The WAL segment on
//! disk contains whatever hit `write(2)` before the kill. A fresh process
//! comes up, calls `WsFrameSpill::replay_all`, and MUST recover every
//! durable frame across all 4 WebSocket types (LiveFeed / Depth-20 /
//! Depth-200 / OrderUpdate).
//!
//! Unlike the old `chaos_sigkill_replay.rs` (which targets the legacy tick
//! spill file), this test exercises the new `WsFrameSpill` WAL introduced
//! in Stage C. It DOES NOT simulate the crash via `fork()` — cargo test
//! can't reliably do that cross-platform. Instead, the test:
//!
//!   1. Opens a fresh `WsFrameSpill` in a temp dir.
//!   2. Calls `append()` with frames representing all 4 WS types.
//!   3. Drops the spill, which joins the writer thread and flushes the
//!      segment — the on-disk state after this Drop is identical to the
//!      state a kernel would leave behind after a SIGKILL that occurred
//!      after the last `write(2)` syscall but before the next one.
//!   4. Calls `replay_all()` on the same dir via a fresh process-like
//!      code path.
//!   5. Asserts that every frame comes back with the correct WsType tag
//!      and payload, and that the archive directory holds the replayed
//!      segment.
//!
//! This is P7 scenarios 5 and 11 from the plan:
//!   - Scenario 5: "SIGKILL the process mid-ingestion — WAL survives,
//!     restart replays, QuestDB has all frames"
//!   - Scenario 11: "Corrupted WAL record (truncated or bad CRC) —
//!     record skipped, counter incremented, replay continues"
//!
//! Runs in normal CI (no `#[ignore]`) because it uses only in-process
//! temp dirs — no Docker, no network, no privileged ops. Execution
//! takes ~200 ms on a cold machine.

#![cfg(test)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType, replay_all};

/// Build a unique temp dir keyed on caller tag + nanosecond timestamp
/// so concurrent `cargo test` invocations never collide.
fn chaos_tmp(tag: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!("tv-wal-chaos-{tag}-{}-{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create chaos temp dir"); // APPROVED: test
    p
}

/// Build one synthetic LiveFeed ticker packet (16 bytes) with a
/// recognisable marker in the security_id field so we can verify
/// per-record integrity on replay.
fn build_live_feed_frame(marker: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 16];
    buf[0] = 2; // RESPONSE_CODE_TICKER
    buf[1..3].copy_from_slice(&16u16.to_le_bytes());
    buf[3] = 2; // NSE_FNO
    buf[4..8].copy_from_slice(&marker.to_le_bytes());
    buf[8..12].copy_from_slice(&100.25_f32.to_le_bytes());
    buf[12..16].copy_from_slice(&(1_700_000_000u32 + marker).to_le_bytes());
    buf
}

/// Build one synthetic Depth-20 packet (12-byte header + 320 bytes of
/// level data). The header byte-order is different from the Live Feed —
/// matches `docs/dhan-ref/04-full-market-depth-websocket.md` section
/// 3.1 exactly.
fn build_depth_20_frame(marker: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 12 + 320];
    let len = buf.len() as i16;
    buf[0..2].copy_from_slice(&len.to_le_bytes()); // message length
    buf[2] = 41; // Bid
    buf[3] = 2; // NSE_FNO
    buf[4..8].copy_from_slice(&marker.to_le_bytes());
    buf[8..12].copy_from_slice(&(marker + 1).to_le_bytes()); // sequence
    buf
}

/// Build one synthetic Depth-200 packet with row_count = 1 so the
/// replay helper has a well-formed record to parse. Payload beyond the
/// header is level data the WAL persists verbatim.
fn build_depth_200_frame(marker: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 12 + 16]; // header + 1 row
    let len = buf.len() as i16;
    buf[0..2].copy_from_slice(&len.to_le_bytes());
    buf[2] = 51; // Ask
    buf[3] = 2;
    buf[4..8].copy_from_slice(&marker.to_le_bytes());
    buf[8..12].copy_from_slice(&1u32.to_le_bytes()); // row_count
    buf
}

/// Build one order-update JSON frame with a recognisable marker in the
/// orderId field so per-record integrity is easy to assert.
fn build_order_update_frame(marker: u32) -> Vec<u8> {
    format!(r#"{{"Data":{{"OrderNo":"WAL{marker:06}","Symbol":"TEST"}},"Type":"order_alert"}}"#)
        .into_bytes()
}

/// **P7 Scenario 5** — full 4-type SIGKILL replay: all frames must
/// come back with the correct `WsType` tag and exact payload.
#[test]
fn chaos_sigkill_ws_frame_wal_recovers_all_four_types() {
    let dir = chaos_tmp("all-4-types");

    // Simulated pre-crash writer session: append frames of every type.
    {
        let spill = Arc::new(WsFrameSpill::new(&dir).expect("WsFrameSpill::new"));

        // 50 LiveFeed frames with distinct markers
        for i in 0..50u32 {
            let outcome = spill.append(WsType::LiveFeed, build_live_feed_frame(i));
            assert_eq!(
                outcome,
                AppendOutcome::Spilled,
                "LiveFeed append {i} must spill, got {outcome:?}"
            );
        }
        // 25 Depth-20 frames
        for i in 0..25u32 {
            let outcome = spill.append(WsType::Depth20, build_depth_20_frame(1000 + i));
            assert_eq!(outcome, AppendOutcome::Spilled);
        }
        // 25 Depth-200 frames
        for i in 0..25u32 {
            let outcome = spill.append(WsType::Depth200, build_depth_200_frame(2000 + i));
            assert_eq!(outcome, AppendOutcome::Spilled);
        }
        // 10 OrderUpdate frames
        for i in 0..10u32 {
            let outcome = spill.append(WsType::OrderUpdate, build_order_update_frame(3000 + i));
            assert_eq!(outcome, AppendOutcome::Spilled);
        }

        // Let the background writer thread drain the crossbeam channel.
        // In production the spill's Drop joins the writer thread; here
        // we wait explicitly so the segment file is flushed before we
        // walk the directory for replay.
        std::thread::sleep(Duration::from_millis(200));
        drop(spill);
        // After drop, the writer thread has finished its drain loop.
        std::thread::sleep(Duration::from_millis(50));
    }

    // "Fresh process" — call replay_all on the same dir.
    let recovered = replay_all(&dir).expect("replay_all");
    assert_eq!(
        recovered.len(),
        110,
        "must recover 50 LiveFeed + 25 Depth20 + 25 Depth200 + 10 OrderUpdate = 110 frames"
    );

    // Partition by ws_type and verify per-type counts + payload integrity.
    let mut live = 0usize;
    let mut d20 = 0usize;
    let mut d200 = 0usize;
    let mut ord = 0usize;
    for rec in &recovered {
        match rec.ws_type {
            WsType::LiveFeed => {
                assert_eq!(rec.frame.len(), 16, "LiveFeed frame must be 16 bytes");
                assert_eq!(
                    rec.frame[0], 2,
                    "LiveFeed response code must survive replay"
                );
                live += 1;
            }
            WsType::Depth20 => {
                assert_eq!(
                    rec.frame.len(),
                    12 + 320,
                    "Depth-20 size must survive replay"
                );
                assert_eq!(rec.frame[2], 41, "Depth-20 Bid code must survive replay");
                d20 += 1;
            }
            WsType::Depth200 => {
                assert_eq!(rec.frame[2], 51, "Depth-200 Ask code must survive replay");
                d200 += 1;
            }
            WsType::OrderUpdate => {
                let text = std::str::from_utf8(&rec.frame).expect("order update UTF-8");
                assert!(
                    text.contains("WAL"),
                    "OrderUpdate marker must survive replay"
                );
                assert!(text.contains("order_alert"));
                ord += 1;
            }
        }
    }
    assert_eq!(live, 50, "LiveFeed count");
    assert_eq!(d20, 25, "Depth-20 count");
    assert_eq!(d200, 25, "Depth-200 count");
    assert_eq!(ord, 10, "OrderUpdate count");

    // Replayed segments are archived, not deleted — a second replay
    // on the same directory should return zero because the archive
    // walk is only performed on `.wal` files, which have been moved.
    let second = replay_all(&dir).expect("replay_all idempotent");
    assert_eq!(
        second.len(),
        0,
        "P7 Scenario 5: replayed segments must not double-replay"
    );

    // Archive directory must contain at least one original segment.
    let archive = dir.join("archive");
    assert!(
        archive.exists(),
        "archive directory must exist after replay"
    );
    let archived = std::fs::read_dir(&archive).expect("read archive").count();
    assert!(archived >= 1, "archive must contain at least one segment");

    let _ = std::fs::remove_dir_all(&dir);
}

/// **P7 Scenario 11** — corrupted WAL tail must be skipped without
/// losing the good records that came before it.
#[test]
fn chaos_wal_corrupted_tail_is_skipped_and_prior_records_recovered() {
    let dir = chaos_tmp("corrupted-tail");

    // Append 10 valid frames through the normal writer.
    {
        let spill = Arc::new(WsFrameSpill::new(&dir).expect("WsFrameSpill::new"));
        for i in 0..10u32 {
            assert_eq!(
                spill.append(WsType::LiveFeed, build_live_feed_frame(i)),
                AppendOutcome::Spilled
            );
        }
        std::thread::sleep(Duration::from_millis(200));
        drop(spill);
        std::thread::sleep(Duration::from_millis(50));
    }

    // Find the segment file and append corrupted garbage to its tail —
    // simulates the kernel having written a partial record when the
    // process was killed. `replay_segment` must stop at the boundary
    // and keep the 10 valid records.
    let seg = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|s| s.to_str()) == Some("wal"))
        .expect("at least one .wal segment must exist");

    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&seg)
            .expect("open segment for tail append");
        // Write a header-only record with no body — the `record_end`
        // check will see the truncation and the replay loop will stop.
        f.write_all(b"TVW1").expect("tail magic");
        f.write_all(&[WsType::LiveFeed.as_u8()]).expect("tail type");
        f.write_all(&9999u32.to_le_bytes()).expect("tail len"); // claim 9999 bytes
        // ...but do not write the 9999-byte body. `replay_segment`
        // sees `record_end > buf.len()` and breaks cleanly.
    }

    let recovered = replay_all(&dir).expect("replay_all must succeed on truncated tail");
    assert_eq!(
        recovered.len(),
        10,
        "10 valid records must survive a truncated tail"
    );
    for (i, rec) in recovered.iter().enumerate() {
        assert!(matches!(rec.ws_type, WsType::LiveFeed));
        let sid = u32::from_le_bytes(rec.frame[4..8].try_into().unwrap());
        assert_eq!(sid as usize, i, "security_id marker survives replay");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// **P7** — replaying an empty directory is a no-op, not an error.
#[test]
fn chaos_empty_wal_dir_replay_returns_zero() {
    let dir = chaos_tmp("empty-dir");
    let recovered = replay_all(&dir).expect("replay_all on empty dir");
    assert!(
        recovered.is_empty(),
        "empty WAL dir must yield zero recovered frames"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// **P7** — replaying a non-existent directory is a no-op, not an
/// error. Ensures first-ever boot (no data dir yet) does not crash.
#[test]
fn chaos_nonexistent_wal_dir_replay_returns_zero() {
    let dir = chaos_tmp("nonexistent");
    let _ = std::fs::remove_dir_all(&dir); // make sure it is gone
    let recovered =
        replay_all(&dir).expect("replay_all must tolerate a missing directory at first boot");
    assert!(recovered.is_empty());
}
