//! Ratchet (zero-tick-loss PR-8b): the durable WAL preserves TICK ORDER
//! exactly — frames replay in the SAME order they were appended, with zero
//! reordering. Operator hard requirement (2026-06-03): "ticks order should
//! never ever be changed."
//!
//! This is the real-time, mechanical guarantee for the persistence/recovery
//! path. Order is preserved end-to-end by construction:
//!   • WS read loop → `mpsc::Sender<Bytes>` (single producer, FIFO).
//!   • `tokio::broadcast` fan-out (FIFO per receiver; a slow receiver may
//!     drop on Lagged — AGGREGATOR-LAG-01 — but never REORDERS).
//!   • WAL spill: one crossbeam channel → one disk writer → append-ordered
//!     segment files → `replay_all` reads them in append order.
//!   • `ticks` QuestDB table: designated timestamp + dedup by
//!     `(ts, security_id, segment)` — ts-ordered.
//!
//! This test pins the WAL append→replay leg: it is the one place a reorder
//! could silently creep in (a buffering/sort/parallel-drain regression), so
//! it gets an explicit FIFO assertion the existing replay test (counts +
//! content only) does not provide.
//!
//! Coverage boundary (honest): these tests exercise the SINGLE-segment case
//! (N×~28 B ≪ `WAL_SEGMENT_MAX_BYTES` = 128 MiB → one segment file). Cross
//! segment ordering is guaranteed by `replay_all` sorting segments
//! lexicographically by a zero-padded-20-digit nanosecond filename
//! (`ws-frames-{nanos:020}.wal`) — lexicographic == chronological == append
//! order — and is NOT exercised here because forcing a 2nd segment would
//! require writing >128 MiB per run (rejected: slow + disk pressure). The
//! single-producer FIFO within a segment + that sort are the full order
//! guarantee.

use std::sync::Arc;

use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType, replay_all};

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!(
        "tv-pr8b-order-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create temp dir"); // APPROVED: test
    p
}

/// 16-byte LiveFeed ticker frame carrying `marker` in the security_id field
/// (bytes 4..8) so the replay order can be decoded back to the append index.
fn build_frame(marker: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 16];
    buf[0] = 2; // RESPONSE_CODE_TICKER
    buf[1..3].copy_from_slice(&16u16.to_le_bytes());
    buf[3] = 2; // NSE_FNO
    buf[4..8].copy_from_slice(&marker.to_le_bytes());
    buf[8..12].copy_from_slice(&100.25_f32.to_le_bytes());
    buf[12..16].copy_from_slice(&(1_700_000_000u32 + marker).to_le_bytes());
    buf
}

fn decode_marker(frame: &[u8]) -> u32 {
    u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]])
}

#[test]
fn test_wal_replay_preserves_exact_tick_order() {
    const N: u32 = 1_000;
    let dir = tmp_dir("exact");

    // Append N frames in strict ascending marker order, single producer.
    {
        let spill = Arc::new(WsFrameSpill::new(&dir).expect("WsFrameSpill::new"));
        for i in 0..N {
            assert_eq!(
                spill.append(WsType::LiveFeed, build_frame(i)),
                AppendOutcome::Spilled,
                "append {i} must spill"
            );
        }
        // Let the disk-writer thread drain, then drop (joins the writer).
        std::thread::sleep(std::time::Duration::from_millis(200));
        drop(spill);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let recovered = replay_all(&dir).expect("replay_all");
    assert_eq!(recovered.len(), N as usize, "every frame must replay");

    // THE ORDER GUARANTEE: recovered[i] is exactly the i-th appended frame.
    // Any reordering (sort, parallel drain, out-of-sequence segment read)
    // breaks this.
    for (i, rec) in recovered.iter().enumerate() {
        assert_eq!(rec.ws_type, WsType::LiveFeed);
        assert_eq!(
            decode_marker(&rec.frame),
            i as u32,
            "TICK ORDER VIOLATED: replay position {i} holds marker {} — the WAL must \
             return frames in the EXACT append order (FIFO). Operator hard invariant: \
             tick order is never changed.",
            decode_marker(&rec.frame)
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_wal_replay_order_is_strictly_monotonic() {
    // A second, independent phrasing of the same invariant: the decoded
    // markers across the whole replay stream are strictly increasing — no
    // duplicate, no gap, no swap.
    const N: u32 = 500;
    let dir = tmp_dir("monotonic");
    {
        let spill = Arc::new(WsFrameSpill::new(&dir).expect("WsFrameSpill::new"));
        for i in 0..N {
            assert_eq!(
                spill.append(WsType::LiveFeed, build_frame(i)),
                AppendOutcome::Spilled
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        drop(spill);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let recovered = replay_all(&dir).expect("replay_all");
    assert_eq!(recovered.len(), N as usize);

    let mut prev: Option<u32> = None;
    for rec in &recovered {
        let m = decode_marker(&rec.frame);
        if let Some(p) = prev {
            assert_eq!(
                m,
                p + 1,
                "TICK ORDER VIOLATED: marker {m} followed {p} — replay stream must be \
                 strictly +1 monotonic (no reorder, no gap, no duplicate)."
            );
        }
        prev = Some(m);
    }

    let _ = std::fs::remove_dir_all(&dir);
}
