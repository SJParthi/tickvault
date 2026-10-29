//! Wall-clock RING-PLUS-DECODE yardstick (plan item DT0, 2026-10-29):
//! socket receipt → WAL + ring hand-off → a stand-in consumer's pick-up →
//! decode, with and without CPU competition.
//!
//! # What this is NOT (review of DT0, 2026-10-29)
//!
//! It does not run the production drain. The consumer below is a bare
//! `recv` loop: no `run_frame_drain` select arms (500 ms flush, 5 s
//! catch-up seal, 30 s silence, idle and seed arms), no
//! `drain_main_feed_frame`, no candle fold, no row append, and it never
//! records the `receipt_to_fold` stage. So it is NOT the `receipt_to_fold`
//! stage and it cannot judge DT1 (sleeping consumer) or DT2 (the drain on
//! its own thread): both change `run_frame_drain` in the app crate, which
//! this core-crate test cannot reach. Judge those steps on the live
//! `receipt_to_fold` stage. What this CAN show is the cost of the WAL +
//! ring hand-off and of a consumer that competes for a CPU, for example
//! before and after a DT3 core-placement change on a two-core machine.
//!
//! Debug-build figures are dominated by host noise (one debug run printed
//! a worse p99.9 quiet than contended); record only release runs on two
//! otherwise idle cores.
//!
//! Live evidence, 29 Oct on the 2-vCPU r8g.large: `socket_to_wal` and
//! `drain_main_frame` p99 ≤ 32.8 µs, yet about 35 to 50 frames a session
//! over 1 ms and the worst 8 to 16 ms. The steps themselves are short; the
//! tail is the reader and the drain waiting for a CPU. This harness measures
//! only the hand-off half of that; see "What this is NOT" above.
//!
//! What it runs, per scenario:
//!
//! * a reader thread that hands each frame to a real `WalRingSink`
//!   (`accept` stamps the receipt, appends to a real WAL writer, sends to a
//!   real `frame_ring`), paced by `thread::sleep` like a socket read that
//!   blocks between frames (it does NOT spin, so it competes for a CPU only
//!   while it works);
//! * a stand-in consumer task (NOT the production drain) on a separate
//!   two-worker tokio runtime (the main runtime's shape) that receives,
//!   decodes the packet with the real dispatcher and records
//!   `received_at.elapsed()`;
//! * optionally `NOISE_TASKS` other tasks on the consumer's runtime, each
//!   doing ~50 µs of work every ~1 ms (the main runtime holds ~117
//!   tasks), and one CPU-burning thread per available CPU that spins
//!   5 ms and sleeps 1 ms.
//!
//! Scenarios: 400 µs spacing (≈ 2,600 frames a second, the live average)
//! quiet and contended, and a 12,500 frames-a-second burst (the 09:15
//! open) contended. Each prints p50 / p99 / p99.9 / max and the count over
//! 1 ms.
//!
//! `#[ignore]`d: a wall-clock figure must never become a flaky CI gate. Run
//! on two cores, the r8g.large's shape, with
//! `taskset -c 0,1 cargo test --release -p tickvault-core --test receipt_to_fold_harness -- --ignored --nocapture`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use tickvault_core::parser::dispatcher::dispatch_frame;
use tickvault_core::websocket::frame_ring::{FrameRingReceiver, frame_ring};
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::pool_supervisor::{
    CapturedFrame, FrameSink, RingByteBudget, WalRingSink,
};
use tickvault_storage::ws_frame_spill::{WsFrameSpill, WsType};

/// Frames at the live average spacing. p99.9 rests on 20 samples.
const FRAMES_STEADY: usize = 20_000;
/// The live inter-frame gap at ≈ 2,600 frames a second.
const GAP_STEADY: Duration = Duration::from_micros(400);
/// Frames in the burst scenario: two seconds at 12,500 a second.
const FRAMES_BURST: usize = 25_000;
/// 12,500 frames a second.
const GAP_BURST: Duration = Duration::from_micros(80);
/// The owner's target for every tick (2026-10-28).
const TARGET_NANOS: u64 = 1_000_000;
/// Ring slots, same order as `FRAME_RING_CAPACITY`.
const RING_SLOTS: usize = 65_536;
/// Other tasks sharing the consumer's runtime in the contended scenarios.
const NOISE_TASKS: usize = 100;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "tv_receipt_to_fold_{tag}_{}_{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// One IDX_I ticker packet with a trade time inside a session.
fn ticker_frame() -> Bytes {
    let mut buf = vec![0u8; 16];
    buf[0] = 2;
    buf[1..3].copy_from_slice(&16_u16.to_le_bytes());
    buf[3] = 0;
    buf[4..8].copy_from_slice(&13_u32.to_le_bytes());
    buf[8..12].copy_from_slice(&23_146.45_f32.to_le_bytes());
    buf[12..16].copy_from_slice(&1_779_355_000_u32.to_le_bytes());
    Bytes::from(buf)
}

struct Summary {
    p50: u64,
    p99: u64,
    p999: u64,
    max: u64,
    over_target: usize,
    samples: usize,
}

fn summarise(mut nanos: Vec<u64>) -> Summary {
    assert!(!nanos.is_empty(), "no samples");
    nanos.sort_unstable();
    let at = |q: f64| {
        let idx = ((nanos.len() as f64) * q) as usize;
        nanos[idx.min(nanos.len() - 1)]
    };
    Summary {
        p50: at(0.50),
        p99: at(0.99),
        p999: at(0.999),
        max: nanos[nanos.len() - 1],
        over_target: nanos.iter().filter(|n| **n > TARGET_NANOS).count(),
        samples: nanos.len(),
    }
}

fn report(name: &str, s: &Summary) {
    println!(
        "{name:<34} p50 {:>8} ns  p99 {:>8} ns  p99.9 {:>9} ns  max {:>10} ns  over 1ms {}/{}",
        s.p50, s.p99, s.p999, s.max, s.over_target, s.samples
    );
}

/// CPU burners: one per available CPU, each spinning 5 ms then sleeping
/// 1 ms until `stop` is set.
fn spawn_burners(stop: &Arc<AtomicBool>) -> Vec<std::thread::JoinHandle<()>> {
    let cpus = std::thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get);
    (0..cpus)
        .map(|i| {
            let stop = Arc::clone(stop);
            std::thread::Builder::new()
                .name(format!("tv-harness-burn-{i}"))
                .spawn(move || {
                    let mut x = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        let until = Instant::now() + Duration::from_millis(5);
                        while Instant::now() < until {
                            x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005));
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    std::hint::black_box(x);
                })
                .expect("burner thread")
        })
        .collect()
}

/// The stand-in consumer (NOT `run_frame_drain`): receive, decode, record
/// receipt → decoded. Returns every sample.
async fn consume(
    mut rx: FrameRingReceiver<CapturedFrame>,
    budget: Arc<RingByteBudget>,
    expect: usize,
) -> Vec<u64> {
    let mut out = Vec::with_capacity(expect);
    while let Some(frame) = rx.recv().await {
        budget.release(frame.bytes.len());
        let parsed = dispatch_frame(&frame.bytes, frame.received_at_nanos);
        std::hint::black_box(&parsed);
        out.push(u64::try_from(frame.received_at.elapsed().as_nanos()).unwrap_or(u64::MAX));
    }
    out
}

/// One scenario: `frames` frames `gap` apart through a real WAL + ring into
/// a stand-in consumer on a two-worker runtime, with or without competition.
fn run_scenario(tag: &str, frames: usize, gap: Duration, contended: bool) -> Summary {
    let dir = temp_dir(tag);
    let spill = Arc::new(WsFrameSpill::new(&dir).expect("spill"));
    let (tx, rx) = frame_ring::<CapturedFrame>(RING_SLOTS);
    let budget = Arc::new(RingByteBudget::with_slot_cap(1 << 30, RING_SLOTS));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let stop = Arc::new(AtomicBool::new(false));
    if contended {
        for _ in 0..NOISE_TASKS {
            let stop = Arc::clone(&stop);
            rt.spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    let until = Instant::now() + Duration::from_micros(50);
                    while Instant::now() < until {
                        std::hint::spin_loop();
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            });
        }
    }
    let burners = if contended {
        spawn_burners(&stop)
    } else {
        Vec::new()
    };
    let consumer_task = rt.spawn(consume(rx, Arc::clone(&budget), frames));
    let sink = WalRingSink::new(
        Arc::clone(&spill),
        tx,
        budget,
        WsType::LiveFeed,
        DhanEndpointType::MainFeed,
        0,
    );
    std::thread::sleep(Duration::from_millis(300));
    let frame = ticker_frame();
    let reader = std::thread::Builder::new()
        .name("tv-harness-reader".to_owned())
        .spawn(move || {
            let mut next = Instant::now();
            for _ in 0..frames {
                next += gap;
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                }
                let _ = sink.accept(frame.clone());
            }
            drop(sink);
        })
        .expect("reader thread");
    reader.join().expect("reader");
    let samples = rt.block_on(consumer_task).expect("consumer");
    stop.store(true, Ordering::Relaxed);
    for b in burners {
        let _ = b.join();
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        samples.len(),
        frames,
        "the consumer saw every accepted frame"
    );
    summarise(samples)
}

#[test]
#[ignore = "wall-clock harness; run by hand with taskset -c 0,1 ... --ignored --nocapture"]
fn receipt_to_fold_cost_with_and_without_cpu_competition() {
    println!(
        "ring+decode yardstick (not the production drain): {} CPUs available (production r8g.large: 2)",
        std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get)
    );
    let quiet = run_scenario("quiet", FRAMES_STEADY, GAP_STEADY, false);
    report("400us spacing, quiet", &quiet);
    let busy = run_scenario("busy", FRAMES_STEADY, GAP_STEADY, true);
    report("400us spacing, contended", &busy);
    let burst = run_scenario("burst", FRAMES_BURST, GAP_BURST, true);
    report("12,500/s burst, contended", &burst);
}
