//! Wall-clock yardstick for plan ITEM DT step 2-3 (2026-10-30): how long a
//! frame waits from the reader's hand-off to the consumer's pick-up, under
//! three placements, with a busy neighbour on the same core.
//!
//! Placements (the consumer is `drain_thread::spawn_frame_drain_with`, the
//! production spawn path; only the future is a stand-in):
//!
//! 1. `today`: the consumer is a task on a two-worker tokio runtime (the
//!    main runtime's shape) shared with background tasks, unpinned.
//! 2. `own_thread`: the consumer on its own OS thread, pinned to core 1.
//! 3. `own_thread_rt`: the same, at `SCHED_FIFO` (drain 1, reader 2), only
//!    when the kernel's RT throttle is active (else the row says skipped).
//!
//! In every placement the reader thread is pinned to core 1 (as
//! `reader_runtime` does), sends a receipt-stamped item through the real
//! `frame_ring` every 400 µs (the live average), and one busy neighbour
//! thread pinned to core 1 spins 5 ms then sleeps 1 ms (QuestDB or a
//! writer thread landing on the hot core). Background tasks on the shared
//! runtime do ~50 µs of work every ~1 ms.
//!
//! What it is NOT: the production `run_frame_drain` (no fold, no flush
//! arms) and not the live `receipt_to_fold` stage; it measures the wait
//! for a CPU, which is what the 2-16 ms live tail is (29 Oct).
//!
//! `#[ignore]`d: a wall-clock figure must never gate CI. Run on two cores:
//! `taskset -c 0,1 cargo test --release -p tickvault-app --test drain_placement_harness -- --ignored --nocapture`

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tickvault_app::drain_thread::{DrainThreadPlan, spawn_frame_drain_with};
use tickvault_core::websocket::frame_ring::{FrameRingReceiver, frame_ring};
use tickvault_core::websocket::hot_thread_placement::{
    HotRtDecision, HotRtPlan, decide_hot_rt, read_rt_throttle,
};
use tickvault_core::websocket::reader_runtime::{
    current_thread_allows_core, pin_current_thread_to_core, set_current_thread_fifo,
};

const FRAMES: usize = 20_000;
const GAP: Duration = Duration::from_micros(400);
const HOT_CORE: usize = 1;
const BACKGROUND_CORE: usize = 0;
const TARGET_NANOS: u64 = 1_000;
const NOISE_TASKS: usize = 50;

struct Summary {
    p50: u64,
    p99: u64,
    p999: u64,
    max: u64,
    over_1us: usize,
    over_1ms: usize,
    n: usize,
}

fn summarise(mut v: Vec<u64>) -> Summary {
    v.sort_unstable();
    let at = |q: f64| v[(((v.len() as f64) * q) as usize).min(v.len() - 1)];
    Summary {
        p50: at(0.50),
        p99: at(0.99),
        p999: at(0.999),
        max: v[v.len() - 1],
        over_1us: v.iter().filter(|n| **n > TARGET_NANOS).count(),
        over_1ms: v.iter().filter(|n| **n > 1_000_000).count(),
        n: v.len(),
    }
}

fn report(name: &str, s: &Summary) {
    println!(
        "{name:<16} p50 {:>9} ns  p99 {:>9} ns  p99.9 {:>10} ns  max {:>10} ns  >1us {:>5}/{}  >1ms {}",
        s.p50, s.p99, s.p999, s.max, s.over_1us, s.n, s.over_1ms
    );
}

async fn consumer(mut rx: FrameRingReceiver<Instant>) -> Vec<u64> {
    let mut out = Vec::with_capacity(FRAMES);
    while let Some(sent) = rx.recv().await {
        out.push(u64::try_from(sent.elapsed().as_nanos()).unwrap_or(u64::MAX));
    }
    out
}

fn spawn_neighbour(stop: &Arc<AtomicBool>, core: usize) -> std::thread::JoinHandle<()> {
    let stop = Arc::clone(stop);
    std::thread::spawn(move || {
        let _ = pin_current_thread_to_core(core);
        let mut x = 0_u64;
        while !stop.load(Ordering::Relaxed) {
            let until = Instant::now() + Duration::from_millis(5);
            while Instant::now() < until {
                x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            }
            std::hint::black_box(x);
            std::thread::sleep(Duration::from_millis(1));
        }
    })
}

fn run(plan: DrainThreadPlan, rt: Option<(u8, u8)>, neighbour_core: usize) -> Summary {
    let shared = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let stop = Arc::new(AtomicBool::new(false));
    for _ in 0..NOISE_TASKS {
        let stop = Arc::clone(&stop);
        shared.spawn(async move {
            let mut x = 0_u64;
            while !stop.load(Ordering::Relaxed) {
                let until = Instant::now() + Duration::from_micros(50);
                while Instant::now() < until {
                    x = x.wrapping_add(1);
                }
                std::hint::black_box(x);
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
    }
    let neighbour = spawn_neighbour(&stop, neighbour_core);
    let (tx, rx) = frame_ring::<Instant>(65_536);
    let core = matches!(plan, DrainThreadPlan::Own(_)).then_some(HOT_CORE);
    let join = {
        let _guard = shared.enter();
        spawn_frame_drain_with(consumer(rx), plan, core, rt.map(|(_, drain)| drain))
    };
    let reader = std::thread::spawn(move || {
        let _ = pin_current_thread_to_core(HOT_CORE);
        if let Some((reader_prio, _)) = rt {
            set_current_thread_fifo(reader_prio).expect("reader SCHED_FIFO");
        }
        for _ in 0..FRAMES {
            std::thread::sleep(GAP);
            let _ = tx.try_send(Instant::now());
        }
    });
    reader.join().expect("reader");
    let samples = shared.block_on(join.join()).expect("consumer");
    stop.store(true, Ordering::Relaxed);
    neighbour.join().expect("neighbour");
    shared.shutdown_background();
    assert_eq!(
        samples.len(),
        FRAMES,
        "every frame the reader sent must reach the consumer in this placement"
    );
    summarise(samples)
}

#[test]
#[ignore = "wall-clock harness; run by hand on two cores"]
fn drain_placement_wait_for_cpu_three_ways() {
    if !current_thread_allows_core(HOT_CORE) {
        println!("core {HOT_CORE} not available; skipped");
        return;
    }
    println!(
        "{FRAMES} frames, {} us apart, neighbour on core {HOT_CORE}",
        GAP.as_micros()
    );
    let today = run(DrainThreadPlan::Shared, None, HOT_CORE);
    report("today", &today);
    let own = run(DrainThreadPlan::Own(Some(HOT_CORE)), None, HOT_CORE);
    report("own_thread", &own);
    match decide_hot_rt(HotRtPlan::Fifo(1), read_rt_throttle) {
        HotRtDecision::Apply { reader, drain } => {
            report(
                "own_thread_rt",
                &run(
                    DrainThreadPlan::Own(Some(HOT_CORE)),
                    Some((reader, drain)),
                    HOT_CORE,
                ),
            );
        }
        other => println!("own_thread_rt    skipped: {other:?}"),
    }
    // The full plan: the neighbour moved off the hot core (background core 0).
    let iso = run(DrainThreadPlan::Own(Some(HOT_CORE)), None, BACKGROUND_CORE);
    report("own_thread_iso", &iso);
    // Non-vacuity: every placement measured every frame.
    for s in [&today, &own, &iso] {
        assert_eq!(s.n, FRAMES, "a placement lost frames");
    }
}
