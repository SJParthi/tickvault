//! Chaos: QuestDB accepts the `top_volume_<tf>` table setup but refuses every
//! board write for the whole outage (top_volume spill plan, PR-C, 2026-10-13).
//!
//! The 2026-10-13 incident lost 10,277 board rows in two ways: sweeps dropped
//! at the frame drain because the writer queue was full, and batches dropped
//! by the writer after a failed flush. This test drives the real lane split
//! (drain producer -> formatter thread -> flush thread) against a mock
//! QuestDB and asserts:
//!
//! 1. the drain's hand-off never reports `QueueFull` (the formatter no longer
//!    waits on the network, so the drain's queue keeps emptying);
//! 2. every staged row is on disk in the spill folder, and none is lost;
//! 3. once QuestDB accepts writes again, the replay posts every spilled row
//!    back, with no session-window filter.
//!
//! The outage is compressed: 4 cadences x 10 rounds of 2,000-row sweeps
//! (80,000 rows), handed off every 100 ms — about 2.5x the production worst
//! rate of four sweeps a second. The spill's free-space floor is lowered to 0
//! through `TopVolumeSpillSettings`, because production requires 40 GiB free
//! and neither a build machine nor CI has that.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tickvault_common::config::QuestDbConfig;
use tickvault_storage::tick_spill_replay::{SpillSinkView, replay_spill_dir, write_url};
use tickvault_storage::top_volume_rank_persistence::{
    SnapshotCadence, TopVolumeHandoff, TopVolumeLabelMap, TopVolumeOffload, TopVolumeRankRow,
    TopVolumeRankWriter, TopVolumeSpillSettings, ensure_top_volume_tables,
};

const ROWS_PER_SWEEP: usize = 2_000;
const ROUNDS: usize = 10;

/// A mock QuestDB on 127.0.0.1: `/exec` (DDL) answers 200; `/write` answers
/// 503 while `refuse` is set, else 204, and counts the ILP lines it accepted.
fn spawn_mock_questdb(refuse: Arc<AtomicBool>, accepted_lines: Arc<AtomicUsize>) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind"); // APPROVED: test
    let port = listener.local_addr().expect("addr").port(); // APPROVED: test
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let refuse = Arc::clone(&refuse);
            let accepted = Arc::clone(&accepted_lines);
            std::thread::spawn(move || {
                let mut req: Vec<u8> = Vec::new();
                let mut buf = vec![0_u8; 65_536];
                let mut body_start = None;
                let mut content_len = 0_usize;
                loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    if body_start.is_none()
                        && let Some(i) = req.windows(4).position(|w| w == b"\r\n\r\n")
                    {
                        body_start = Some(i + 4);
                        let head = String::from_utf8_lossy(&req[..i]).to_ascii_lowercase();
                        content_len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                    }
                    if let Some(start) = body_start
                        && req.len() >= start + content_len
                    {
                        break;
                    }
                }
                let is_write = req.starts_with(b"POST /write");
                let resp: &[u8] = if !is_write {
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                } else if refuse.load(Ordering::SeqCst) {
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    let start = body_start.unwrap_or(req.len()).min(req.len());
                    let lines = req[start..].iter().filter(|b| **b == b'\n').count();
                    accepted.fetch_add(lines, Ordering::SeqCst);
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                };
                let _ = stream.write_all(resp);
                let _ = stream.flush();
            });
        }
    });
    port
}

fn row(cadence: SnapshotCadence, ts_nanos: i64, security_id: i64) -> TopVolumeRankRow<'static> {
    TopVolumeRankRow {
        snapshot_ts_ist_nanos: ts_nanos,
        cadence,
        family: "stock_option",
        feed: "dhan",
        segment: "NSE_FNO",
        contract: "",
        security_id,
        underlying_id: 2885,
        per_lot_quantity: 200,
        total_lots_traded: 42,
        volume_percentage_change: 4_150,
        volume: Some(8_500),
        percentage_change: Some(1.25),
        open_percentage_change: Some(-0.5),
        subscribed: false,
    }
}

fn lines_in(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "ilp"))
                .map(|e| {
                    std::fs::read(e.path())
                        .map(|b| b.iter().filter(|c| **c == b'\n').count())
                        .unwrap_or(0)
                })
                .sum()
        })
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chaos_top_volume_outage_spills_every_row_and_the_replay_posts_them_back() {
    let refuse = Arc::new(AtomicBool::new(true));
    let accepted = Arc::new(AtomicUsize::new(0));
    let port = spawn_mock_questdb(Arc::clone(&refuse), Arc::clone(&accepted));
    let cfg = QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: port,
        pg_port: 1,
        ilp_port: 1,
    };
    // The table setup is accepted, so the tables read as keyed and batches
    // are SENT (and refused) rather than spilled as unkeyed.
    assert!(
        ensure_top_volume_tables(&cfg).await,
        "the mock accepts the DDL"
    );

    // A folder named `top_volume`, so the replay picks the top_volume policy.
    let root = std::env::temp_dir().join(format!(
        "tv-chaos-top-volume-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    let dir = root.join("top_volume");
    let TopVolumeOffload {
        mut producer,
        mut row_writer,
        rows,
        flusher,
        shutdown: _,
    } = TopVolumeRankWriter::new(&cfg).split_rows_for_offload(
        "unmapped",
        TopVolumeSpillSettings {
            dir: dir.clone(),
            enabled: true,
            free_reserve_bytes: 0,
        },
    );
    let flush_thread = std::thread::spawn(move || flusher.run());
    let lost = Arc::new(AtomicUsize::new(0));
    let lost_in_writer = Arc::clone(&lost);
    let writer_thread = std::thread::spawn(move || {
        while let Ok(batch) = rows.recv() {
            let out = row_writer.write_batch(batch);
            lost_in_writer.fetch_add(out.lost + out.append_failed, Ordering::SeqCst);
        }
    });

    // 22:00 IST on 2024-05-18, outside every exchange window: replay must
    // still post these (top_volume has no window filter).
    let base_ts: i64 = (1_715_990_400_i64 + 22 * 3_600) * 1_000_000_000;
    let labels = Arc::new(TopVolumeLabelMap::new());
    let (mut staged, mut queue_full) = (0_usize, 0_usize);
    for round in 0..ROUNDS {
        for (c, cadence) in SnapshotCadence::ALL.into_iter().enumerate() {
            let ts = base_ts + i64::try_from(round * 4 + c).unwrap_or(0) * 1_000_000_000;
            for sid in 0..ROWS_PER_SWEEP {
                assert!(producer.stage(&row(cadence, ts, i64::try_from(sid).unwrap_or(0))));
            }
            match producer.hand_off(Arc::clone(&labels)) {
                TopVolumeHandoff::Sent(n) => staged += n,
                TopVolumeHandoff::QueueFull(n) => queue_full += n,
                other => panic!("unexpected hand-off outcome {other:?}"),
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    // Shutting down the drain side closes the row queue; the formatter then
    // closes the flush queue.
    drop(producer);
    writer_thread.join().expect("formatter thread"); // APPROVED: test
    flush_thread.join().expect("flush thread"); // APPROVED: test

    assert_eq!(queue_full, 0, "no sweep may be dropped at the frame drain");
    assert_eq!(staged, ROUNDS * 4 * ROWS_PER_SWEEP);
    assert_eq!(
        lost.load(Ordering::SeqCst),
        0,
        "no row lost on the formatter"
    );
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        0,
        "QuestDB accepted nothing"
    );
    assert_eq!(lines_in(&dir), staged, "every staged row is on disk");

    // QuestDB is back: one replay round posts every spilled row.
    refuse.store(false, Ordering::SeqCst);
    let client = tickvault_storage::http_client::build_probe_client(30).expect("client"); // APPROVED: test
    let mut view = SpillSinkView::current();
    view.suspect = false;
    let outcome = replay_spill_dir(&dir, &write_url("127.0.0.1", port), &client, &view).await;
    assert_eq!(outcome.files_failed, 0);
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        staged,
        "the replay posts every spilled row, none filtered"
    );
    let _ = std::fs::remove_dir_all(&root);
}
