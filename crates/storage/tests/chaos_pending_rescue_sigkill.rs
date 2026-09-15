//! Real Linux SIGKILL at a specific recovery boundary: the raw frame has
//! completed WAL shutdown, its decoded row is held by the independent rescue
//! consumer before writing, and a later writer acknowledgment advanced the watermark.
//! The parent kills the child without running destructors and must still
//! recover that frame with its sequence, endpoint, receipt, and bytes intact.
//!
//! The later ACK is injected through the production watermark API. This
//! tests WAL/rescue ordering, not socket supervision, QuestDB acknowledgment,
//! records still in the WAL queue, or hardware power-loss behavior.

#![cfg(target_os = "linux")]

use std::io::{BufRead, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tickvault_common::{feed::Feed, tick_types::ParsedTick};
use tickvault_storage::depth_persistence::{DEPTH_KIND_20, DEPTH_SIDE_BID, DepthRow, DepthWriter};
use tickvault_storage::tick_persistence::TickWriter;
use tickvault_storage::wal_applied_watermark::{
    AppliedSink, AppliedSnapshot, REPLAY_REORDER_SLACK_SEQ, applied_watermark,
};
use tickvault_storage::ws_frame_spill::{
    AppendOutcome, PACKET_INDEX_BITS, WalEndpoint, WsFrameSpill, WsType, next_frame_seq,
    replay_all_with_report_guarded,
};

const CHILD_MODE: &str = "TV_TEST_PENDING_RESCUE_SIGKILL";
const TEST_NAME: &str = "pending_rescue_remains_replayable_after_a_later_ack_and_sigkill";
const READY: &str = "PENDING_RESCUE_READY ";
const RECEIPT: i64 = 1_779_951_600_111_000_000;

// Cleanup also runs on an assertion failure, so a broken child cannot remain
// parked on its stdin or keep a temporary directory locked after the test.
struct ChildCase {
    child: Child,
    directory: PathBuf,
}

impl Drop for ChildCase {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn wait_for_kill(kind: &str, seq: u64) {
    let wm = applied_watermark();
    let later = (seq + 2 * REPLAY_REORDER_SLACK_SEQ) >> PACKET_INDEX_BITS << PACKET_INDEX_BITS;
    match kind {
        "tick" => wm.note_ticks_acked(later),
        "depth" => wm.note_depth_acked(later),
        _ => panic!("unknown child case"),
    }
    wm.persist_now();
    println!("{READY}{seq}");
    std::io::stdout().flush().expect("publish child readiness");
    let mut release = [0_u8; 1];
    std::io::stdin()
        .read_exact(&mut release)
        .expect("parent must kill the child before stdin closes");
    panic!("parent released the child instead of sending SIGKILL");
}

fn run_child(kind: &str) {
    let seq = next_frame_seq();
    let endpoint = if kind == "tick" {
        WalEndpoint::MainFeed
    } else {
        WalEndpoint::Depth20
    };
    let spill = WsFrameSpill::new("wal").expect("isolated WAL");
    assert_eq!(
        spill.append_with_seq_at(
            WsType::LiveFeed,
            kind.as_bytes().to_vec(),
            seq,
            RECEIPT,
            endpoint
        ),
        AppendOutcome::Spilled
    );
    assert_eq!(spill.shutdown(Duration::from_secs(5)), 0);
    assert_eq!(spill.persisted_count(), 1);
    let wm = applied_watermark();
    wm.mark_depth_untracked();

    if kind == "tick" {
        let mut producer = TickWriter::for_test(Feed::Dhan);
        let (rescue_sink, rescue_rx) = producer.split_rescue_offload();
        let tick = ParsedTick {
            security_id: 13,
            exchange_segment_code: 0,
            last_traded_price: 100.0,
            exchange_timestamp: 1_779_971_400,
            received_at_nanos: RECEIPT,
            ..Default::default()
        };
        producer
            .append_tick_with_seq(&tick, i64::try_from(seq).expect("sequence fits column"))
            .expect("append test tick");
        assert_eq!(producer.pending(), 1);
        assert_eq!(producer.discard_pending(), 1);
        // Keep the real queued payload in RAM without running its rescue.
        let held = rescue_rx.try_recv().expect("queued tick rescue");
        assert_eq!(held.rows(), 1);
        wait_for_kill(kind, seq);
        std::hint::black_box((&producer, &rescue_sink, &rescue_rx, &held));
    } else {
        let mut producer = DepthWriter::for_test(Feed::Dhan);
        let (rescue_sink, rescue_rx) = producer.split_rescue_offload();
        producer
            .append_row(&DepthRow {
                security_id: 13,
                segment: "IDX_I",
                depth_kind: DEPTH_KIND_20,
                side: DEPTH_SIDE_BID,
                level: 1,
                price: 100.0,
                quantity: 1,
                orders: 1,
                capture_seq: i64::try_from(seq).expect("sequence fits column"),
                ts_nanos: 1_699_965_000_000_000_000,
            })
            .expect("append test depth");
        assert_eq!(producer.pending(), 1);
        assert_eq!(producer.discard_pending(), 1);
        let held = rescue_rx.try_recv().expect("queued depth rescue");
        assert_eq!(held.rows(), 1);
        wait_for_kill(kind, seq);
        std::hint::black_box((&producer, &rescue_sink, &rescue_rx, &held));
    }
}

#[test]
fn pending_rescue_remains_replayable_after_a_later_ack_and_sigkill() {
    if let Ok(kind) = std::env::var(CHILD_MODE) {
        run_child(&kind);
        return;
    }
    for kind in ["tick", "depth"] {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "tv-pending-rescue-{kind}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).expect("isolated child directory");
        let child = Command::new(std::env::current_exe().expect("current test executable"))
            .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD_MODE, kind)
            .current_dir(&directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn isolated crash child");
        let mut case = ChildCase { child, directory };
        let stdout = case.child.stdout.take().expect("child stdout pipe");
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let line = line.expect("read child output");
                if let Some((_prefix, seq)) = line.split_once(READY) {
                    ready_tx
                        .send(seq.parse::<u64>().expect("child sequence"))
                        .expect("ready receiver");
                    return;
                }
            }
        });
        let seq = ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("child must reach the pending-rescue boundary within ten seconds");
        case.child.kill().expect("kill only the isolated child");
        let status = case.child.wait().expect("reap killed child");
        assert_eq!(
            status.signal(),
            Some(9),
            "a real SIGKILL is required: {status}"
        );
        reader.join().expect("readiness reader");

        let wal = case.directory.join("wal");
        let snapshot = AppliedSnapshot::load(&wal).expect("child persisted its watermark");
        let sink = if kind == "tick" {
            AppliedSink::Ticks
        } else {
            AppliedSink::Depth
        };
        assert!(
            snapshot.skip_below(sink) > seq,
            "the later ACK must overtake the tested frame"
        );
        assert!(
            !snapshot.frame_is_applied(sink, seq),
            "a pending {kind} rescue must protect its WAL frame even after a later ACK"
        );
        let replay = replay_all_with_report_guarded(&wal, usize::MAX, || None, None, 90)
            .expect("replay after process death");
        assert_eq!(
            replay.frames.len(),
            1,
            "the queued {kind} row had no completed rescue"
        );
        let frame = &replay.frames[0];
        assert_eq!(frame.frame_seq, seq);
        assert_eq!(frame.ws_type, WsType::LiveFeed);
        assert_eq!(frame.received_at_nanos, RECEIPT);
        assert_eq!(frame.frame, kind.as_bytes());
        let endpoint = if kind == "tick" {
            WalEndpoint::MainFeed
        } else {
            WalEndpoint::Depth20
        };
        assert_eq!(frame.endpoint, endpoint);
        assert!(!case.directory.join("data/tick_spill").exists());
        println!("SIGKILL_CASE kind={kind} signal=9 replayed=1 sequence={seq}");
    }
}
