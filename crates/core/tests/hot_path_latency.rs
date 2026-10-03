//! Per-call latency of the socket read task's work, as a DISTRIBUTION
//! (p50 / p99 / p99.9 / max), not a mean. Companion to
//! `tickvault-trading`'s `hot_path_latency` (decode + candle fold).
//!
//! Stages, each the code the read task runs per frame:
//! 1. `classify_frame` on a frame of 100 stacked quote packets (the read task
//!    walks a frame looking for a stacked disconnect; O(packets)).
//! 2. `WsFrameSpill::append` of a 162-byte frame handed over as `Bytes`
//!    (the write-ahead-log hand-off; a `try_send`, the disk write is on the
//!    writer thread and is NOT timed here).
//!
//! `#[ignore]`d: wall-clock figures never gate CI. Run it in release:
//!
//! ```text
//! cargo test --release -p tickvault-core --test hot_path_latency -- --ignored --nocapture
//! ```

use std::hint::black_box;
use std::time::Instant;

use bytes::Bytes;
use tickvault_common::constants::RESPONSE_CODE_QUOTE;
use tickvault_core::websocket::connection::classify_frame;
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType};

const SAMPLES: usize = 500_000;
const QUOTE_PACKET_SIZE: usize = 50;

struct Dist {
    p50: u64,
    p99: u64,
    p999: u64,
    max: u64,
}

fn dist(mut ns: Vec<u64>) -> Dist {
    ns.sort_unstable();
    let at = |q: f64| ns[((ns.len() as f64 - 1.0) * q) as usize];
    Dist {
        p50: at(0.50),
        p99: at(0.99),
        p999: at(0.999),
        max: ns[ns.len() - 1],
    }
}

fn report(stage: &str, d: &Dist) {
    eprintln!(
        "LATENCY | {stage:<44} | p50 {:>6} ns | p99 {:>6} ns | p99.9 {:>7} ns | max {:>9} ns",
        d.p50, d.p99, d.p999, d.max
    );
}

fn time_each(samples: usize, mut f: impl FnMut(usize)) -> Dist {
    let mut ns = Vec::with_capacity(samples);
    for i in 0..samples {
        let t = Instant::now();
        f(i);
        ns.push(t.elapsed().as_nanos() as u64);
    }
    dist(ns)
}

fn stacked_quotes(count: usize) -> Vec<u8> {
    let mut frame = Vec::with_capacity(count * QUOTE_PACKET_SIZE);
    for i in 0..count {
        let mut p = [0u8; QUOTE_PACKET_SIZE];
        p[0] = RESPONSE_CODE_QUOTE;
        p[1..3].copy_from_slice(&(QUOTE_PACKET_SIZE as u16).to_le_bytes());
        p[3] = 2;
        p[4..8].copy_from_slice(&(1_000 + i as u32).to_le_bytes());
        frame.extend_from_slice(&p);
    }
    frame
}

#[test]
#[ignore = "wall-clock harness; run with --release -- --ignored --nocapture"]
fn read_task_latency_distribution() {
    let floor = time_each(SAMPLES, |_| {});
    report("timer floor (two Instant reads)", &floor);

    let frame = stacked_quotes(100);
    let d = time_each(SAMPLES, |_| {
        black_box(classify_frame(
            DhanEndpointType::MainFeed,
            black_box(&frame),
        ));
    });
    report("classify frame, 100 stacked quotes", &d);

    let dir = std::env::temp_dir().join(format!(
        "tv-hot-path-latency-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let spill = WsFrameSpill::new(&dir).expect("WAL must open under a fresh temp dir");
    let payload = Bytes::from(vec![7u8; 162]);
    let mut refused: u64 = 0;
    let d = time_each(SAMPLES, |_| {
        let outcome = spill.append(WsType::LiveFeed, payload.clone());
        if !matches!(outcome, AppendOutcome::Spilled) {
            refused += 1;
        }
        black_box(outcome);
    });
    report("WAL hand-off, 162 B frame", &d);
    eprintln!("LATENCY | WAL hand-off refused {refused} of {SAMPLES} (queue full)");
    drop(spill);
    let _ = std::fs::remove_dir_all(&dir);
}
