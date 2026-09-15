//! In-process WAL replay after a clean writer shutdown, plus tail truncation.
//!
//! The fixture writes LiveFeed and OrderUpdate frames, explicitly drains and
//! stops the writer, and checks complete payloads, transport tags and replay
//! confirmation behavior. It does not send SIGKILL, restart an OS process,
//! test all endpoint types, query QuestDB, or simulate power loss. Those need
//! distinct experiments. The truncation case appends a partial record and
//! checks that earlier valid payloads remain available.
//!
//! Uses owned temporary directories without network or Docker access.

#![cfg(test)]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tickvault_storage::ws_frame_spill::{
    AppendOutcome, WsFrameSpill, WsType, confirm_replayed, replay_all,
};

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

/// Build one order-update JSON frame with a recognisable marker in the
/// orderId field so per-record integrity is easy to assert.
fn build_order_update_frame(marker: u32) -> Vec<u8> {
    format!(r#"{{"Data":{{"OrderNo":"WAL{marker:06}","Symbol":"TEST"}},"Type":"order_alert"}}"#)
        .into_bytes()
}

/// Clean drain preserves both fixture transport types and complete payloads.
#[test]
fn chaos_clean_shutdown_wal_replays_live_and_order_payloads_until_confirmed() {
    let dir = chaos_tmp("live-and-order");

    // One in-process writer session, containing exactly two transport types.
    {
        let spill = WsFrameSpill::new(&dir).expect("WsFrameSpill::new");

        // 50 LiveFeed frames with distinct markers
        for i in 0..50u32 {
            let outcome = spill.append(WsType::LiveFeed, build_live_feed_frame(i));
            assert_eq!(
                outcome,
                AppendOutcome::Spilled,
                "LiveFeed append {i} must spill, got {outcome:?}"
            );
        }
        // PR #4 (2026-05-19): Depth20/Depth200 frame replay paths retired
        // alongside the 4-IDX_I LOCKED_UNIVERSE WS scope lock. Only
        // LiveFeed + OrderUpdate survive in the replay matrix.
        // 10 OrderUpdate frames
        for i in 0..10u32 {
            let outcome = spill.append(WsType::OrderUpdate, build_order_update_frame(3000 + i));
            assert_eq!(outcome, AppendOutcome::Spilled);
        }

        assert_eq!(spill.shutdown(Duration::from_secs(5)), 0);
        drop(spill);
    }

    // Replay through the public API in this same process.
    let recovered = replay_all(&dir).expect("replay_all");
    assert_eq!(
        recovered.len(),
        60,
        "must recover 50 LiveFeed + 10 OrderUpdate = 60 frames"
    );

    // Partition by ws_type and verify per-type counts + payload integrity.
    let mut live = 0usize;
    let mut ord = 0usize;
    for (index, rec) in recovered.iter().enumerate() {
        let (expected_type, expected_frame) = if index < 50 {
            (WsType::LiveFeed, build_live_feed_frame(index as u32))
        } else {
            (
                WsType::OrderUpdate,
                build_order_update_frame(3000 + (index - 50) as u32),
            )
        };
        assert_eq!(rec.ws_type, expected_type, "transport tag at index {index}");
        assert_eq!(
            rec.frame, expected_frame,
            "complete payload at index {index}"
        );
        match rec.ws_type {
            WsType::LiveFeed => {
                assert_eq!(rec.frame.len(), 16, "LiveFeed frame must be 16 bytes");
                assert_eq!(
                    rec.frame[0], 2,
                    "LiveFeed response code must survive replay"
                );
                live += 1;
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
            WsType::TruedataFeed => panic!(
                "this fixture appends no TrueData frames — replay produced a \
                 transport tag that was never written, meaning the tag byte \
                 was mis-decoded"
            ),
        }
    }
    assert_eq!(live, 50, "LiveFeed count");
    assert_eq!(ord, 10, "OrderUpdate count");

    // Crash-safety contract (zero-loss MEDIUM fix 2026-06-30): replayed
    // segments are STAGED in `replaying/`, not archived, until the caller
    // confirms durable re-capture. WITHOUT a confirm, a second replay
    // re-returns the same frames (un-confirmed → re-replayed). This is the
    // fix: a second crash before persist no longer strands frames.
    let second_no_confirm = replay_all(&dir).expect("replay_all re-replays un-confirmed");
    assert_eq!(
        second_no_confirm.len(),
        60,
        "P7 Scenario 5 (crash-safe): un-confirmed segments MUST re-replay"
    );

    for (first, repeated) in recovered.iter().zip(&second_no_confirm) {
        assert_eq!(repeated.ws_type, first.ws_type);
        assert_eq!(repeated.frame, first.frame);
    }

    // Confirm within this fixture → archive → no re-replay. There is no DB write.
    confirm_replayed(&dir);
    let third_after_confirm = replay_all(&dir).expect("replay_all idempotent after confirm");
    assert_eq!(
        third_after_confirm.len(),
        0,
        "P7 Scenario 5: confirmed (archived) segments must not double-replay"
    );

    // Archive directory must contain the confirmed segment(s) AFTER confirm.
    let archive = dir.join("archive");
    assert!(
        archive.exists(),
        "archive directory must exist after confirm"
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
        let spill = WsFrameSpill::new(&dir).expect("WsFrameSpill::new");
        for i in 0..10u32 {
            assert_eq!(
                spill.append(WsType::LiveFeed, build_live_feed_frame(i)),
                AppendOutcome::Spilled
            );
        }
        assert_eq!(spill.shutdown(Duration::from_secs(5)), 0);
        drop(spill);
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
        assert_eq!(
            rec.frame,
            build_live_feed_frame(i as u32),
            "complete payload"
        );
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
