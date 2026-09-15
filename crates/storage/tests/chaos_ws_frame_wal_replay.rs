//! In-process WAL replay after a clean writer shutdown, plus tail truncation.
//!
//! The fixture writes LiveFeed and OrderUpdate frames, explicitly drains and
//! stops the writer, and checks complete payloads, transport tags and replay
//! confirmation behavior. It does not send SIGKILL, restart an OS process,
//! test all endpoint types, query QuestDB, or simulate power loss. Those need
//! distinct experiments. The truncation case requires exact quarantine of
//! the damaged original, then validates a separate known-good prefix copy.
//!
//! Uses owned temporary directories without network or Docker access.

#![cfg(test)]

#[path = "support/owned_wal_replay.rs"]
mod owned_wal_replay;
use owned_wal_replay::{assert_complete_replay, claim_wal};

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType};

/// Exact original images, excluding confirmation markers and other metadata.
/// Each result must be a regular WAL file; unreadable discovery is a failure.
fn wal_segment_images(directory: &Path) -> BTreeMap<OsString, Vec<u8>> {
    let mut images = BTreeMap::new();
    for entry in std::fs::read_dir(directory).expect("read fixture WAL directory") {
        let entry = entry.expect("read fixture WAL entry");
        if entry
            .path()
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("wal")
        {
            continue;
        }
        assert!(
            entry.file_type().expect("read fixture WAL type").is_file(),
            "a fixture WAL image must be a regular file"
        );
        let bytes = std::fs::read(entry.path()).expect("read complete fixture WAL image");
        assert!(
            images.insert(entry.file_name(), bytes).is_none(),
            "duplicate fixture WAL name"
        );
    }
    images
}

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
    let dir_owner = claim_wal(&dir);
    let recovered_batch = assert_complete_replay(&dir_owner);
    let recovered = &recovered_batch.frames;
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
    let second_no_confirm_batch = assert_complete_replay(&dir_owner);
    let second_no_confirm = &second_no_confirm_batch.frames;
    assert_eq!(
        second_no_confirm.len(),
        60,
        "P7 Scenario 5 (crash-safe): un-confirmed segments MUST re-replay"
    );

    for (first, repeated) in recovered.iter().zip(second_no_confirm) {
        assert_eq!(repeated.ws_type, first.ws_type);
        assert_eq!(repeated.frame, first.frame);
        assert_eq!(repeated.frame_seq, first.frame_seq);
        assert_eq!(repeated.received_at_nanos, first.received_at_nanos);
        assert_eq!(repeated.endpoint, first.endpoint);
    }
    let replaying = dir.join("replaying");
    let staged_images = wal_segment_images(&replaying);
    assert!(
        !staged_images.is_empty(),
        "fixture must have staged WAL originals"
    );
    assert_ne!(
        recovered_batch.confirmation_id,
        second_no_confirm_batch.confirmation_id
    );
    assert!(
        !dir_owner.confirm_replayed_generation(recovered_batch.confirmation_id),
        "a superseded generation cannot ACK the newer pass"
    );

    assert_eq!(
        wal_segment_images(&replaying),
        staged_images,
        "a refused stale ACK must preserve the staged originals"
    );

    // Confirm within this fixture → archive → no re-replay. There is no DB write.
    assert!(dir_owner.confirm_replayed_generation(second_no_confirm_batch.confirmation_id));
    let third_after_confirm_batch = assert_complete_replay(&dir_owner);
    let third_after_confirm = &third_after_confirm_batch.frames;
    assert_eq!(
        third_after_confirm.len(),
        0,
        "P7 Scenario 5: confirmed (archived) segments must not double-replay"
    );

    // Confirmation markers alone are not evidence of retained originals.
    // Every exact staged WAL name and byte image must survive the ACK.
    let archive = dir.join("archive");
    assert_eq!(
        wal_segment_images(&archive),
        staged_images,
        "ACK must archive every original WAL byte under the same filename"
    );
    assert!(
        wal_segment_images(&replaying).is_empty(),
        "confirmed originals must leave replaying"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// **P7 Scenario 11** — an incomplete original cannot be acknowledged.
/// Its exact bytes survive quarantine, including every earlier valid record.
#[test]
fn chaos_wal_corrupted_tail_is_quarantined_and_prior_bytes_preserved() {
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
    // process was killed. Replay must reject the incomplete original instead
    // of returning a confirmable prefix that could archive unvalidated bytes.
    let seg = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|s| s.to_str()) == Some("wal"))
        .expect("at least one .wal segment must exist");

    let valid_prefix = std::fs::read(&seg).expect("snapshot the ten complete fixture records");
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
        // ...but do not write the 9999-byte body. The strict scanner must
        // refuse the incomplete original and preserve it in quarantine.
    }

    let damaged_original = std::fs::read(&seg).expect("snapshot the damaged original");
    assert!(damaged_original.starts_with(&valid_prefix));
    let dir_owner = claim_wal(&dir);
    assert!(
        dir_owner.replay_fenced().is_err(),
        "a truncated original must not produce an ACK-able prefix"
    );
    let quarantined = dir
        .join("quarantine")
        .join(seg.file_name().expect("segment name"));
    assert_eq!(
        std::fs::read(&quarantined).expect("quarantined original"),
        damaged_original
    );
    assert!(
        !dir.join("archive").exists(),
        "unvalidated original cannot be archived"
    );

    // This fixture already knows the exact pre-damage bytes. Read a separate
    // copy to prove all ten payloads remain in that prefix; never truncate or
    // alter the captured quarantined original to make the test pass.
    let prefix_copy_dir = dir.join("validated-prefix-copy");
    std::fs::create_dir(&prefix_copy_dir).expect("separate fixture recovery directory");
    std::fs::write(
        prefix_copy_dir.join(seg.file_name().expect("segment name")),
        &valid_prefix,
    )
    .expect("write known fixture prefix copy");
    let prefix_owner = claim_wal(&prefix_copy_dir);
    let recovered_batch = assert_complete_replay(&prefix_owner);
    let recovered = &recovered_batch.frames;
    assert_eq!(
        recovered.len(),
        10,
        "every complete pre-damage record remains recoverable from the validated copy"
    );
    for (i, rec) in recovered.iter().enumerate() {
        assert!(matches!(rec.ws_type, WsType::LiveFeed));
        assert_eq!(
            rec.frame,
            build_live_feed_frame(i as u32),
            "complete payload"
        );
    }

    assert_eq!(
        std::fs::read(&quarantined).expect("original still retained"),
        damaged_original
    );
    assert!(!dir.join("archive").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// **P7** — replaying an empty directory is a no-op, not an error.
#[test]
fn chaos_empty_wal_dir_replay_returns_zero() {
    let dir = chaos_tmp("empty-dir");
    let dir_owner = claim_wal(&dir);
    let recovered_batch = assert_complete_replay(&dir_owner);
    let recovered = &recovered_batch.frames;
    assert!(
        recovered.is_empty(),
        "empty WAL dir must yield zero recovered frames"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// **P7** — first boot claims its missing WAL directory before replay.
/// The empty owned namespace contains no fake history or sequence authority.
#[test]
fn chaos_nonexistent_wal_dir_replay_returns_zero() {
    let dir = chaos_tmp("nonexistent");
    let _ = std::fs::remove_dir_all(&dir); // make sure it is gone
    let dir_owner = claim_wal(&dir);
    let recovered_batch = assert_complete_replay(&dir_owner);
    let recovered = &recovered_batch.frames;
    assert!(recovered.is_empty());
    assert!(dir.join(".lock").is_file());
    assert!(!dir.join("sequence.tvsq").exists());
    let _ = std::fs::remove_dir_all(&dir);
}
