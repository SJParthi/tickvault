//! Plan item PF-5 (2026-11-06): what does the frame drain pay per sealed
//! candle to hand it to the seal writer, and would batching the hand-off per
//! frame make it cheaper?
//!
//! Today the drain calls `SealSender::try_send` once per seal (a bounded
//! `crossbeam_channel` array, pre-sized at `SEAL_MPSC_CAPACITY`). The batched
//! alternative measured here is a channel whose item is a fixed array of up to
//! [`BATCH`] seals, sent once per full or final group of a frame's seals (a
//! fixed-size item, because the queue must not allocate). Three consumer
//! shapes:
//!
//! * `idle`: nothing reads the queue during the burst (it is emptied, untimed,
//!   when half full);
//! * `cycling`: a thread empties the queue every 100 ms, the seal writer's
//!   cycle, while the producer hands off one burst per simulated frame
//!   (`FRAME_GAP`), so a burst sometimes overlaps the writer's take loop: this
//!   is the production shape;
//! * `spinning`: a thread calls `try_recv` without pause, the worst case.
//!
//! `#[ignore]`d: a wall-clock figure must never be a flaky CI gate (house
//! pattern, as `scan_silence_sweep_cost_at_the_authorized_ceiling`). Run in
//! release:
//!
//! `cargo test --release -p tickvault-storage --test seal_handoff_burst_harness -- --ignored --nocapture`

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use tickvault_common::constants::EXCHANGE_SEGMENT_NSE_FNO;
use tickvault_common::feed::Feed;
use tickvault_storage::seal_writer_runner::{SEAL_MPSC_CAPACITY, SealSender};
use tickvault_trading::candles::{BufferedSeal, LiveCandleState, TfIndex};

/// Seals per batch item in the batched prototype: one per fold timeframe, the
/// most one tick can seal.
const BATCH: usize = 8;
/// Burst sizes (seals handed off by one frame) measured.
const BURSTS: [usize; 6] = [1, 2, 4, 8, 16, 64];
/// Bursts timed per configuration.
const ROUNDS: usize = 200_000;
/// Simulated time between two frames (the drain's other per-frame work).
const FRAME_GAP: Duration = Duration::from_micros(5);
/// The seal writer's cycle.
const WRITER_CYCLE: Duration = Duration::from_millis(100);

/// The batched prototype's item.
#[derive(Clone, Copy)]
struct SealBatch {
    len: u8,
    seals: [BufferedSeal; BATCH],
}

#[derive(Clone, Copy, PartialEq)]
enum Consumer {
    Idle,
    Cycling,
    Spinning,
}

impl Consumer {
    fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Cycling => "cycling",
            Self::Spinning => "spinning",
        }
    }
}

fn seal(i: usize) -> BufferedSeal {
    let mut state = LiveCandleState::empty();
    state.bucket_start_ist_secs = 1_786_800_000;
    state.open = 100.0 + i as f64;
    state.high = state.open + 1.0;
    state.low = state.open - 1.0;
    state.close = state.open;
    state.volume = i as u64;
    BufferedSeal::new(
        i as u64,
        EXCHANGE_SEGMENT_NSE_FNO,
        TfIndex::M1,
        state,
        Feed::Dhan,
    )
}

fn quantile(sorted: &[f64], q: f64) -> f64 {
    let idx = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Per-seal ns over the bursts: (p50, p99, p99.9, mean).
fn summarise(mut per_seal: Vec<f64>) -> (f64, f64, f64, f64) {
    let mean = per_seal.iter().sum::<f64>() / per_seal.len() as f64;
    per_seal.sort_by(f64::total_cmp);
    (
        quantile(&per_seal, 0.5),
        quantile(&per_seal, 0.99),
        quantile(&per_seal, 0.999),
        mean,
    )
}

/// Starts the consumer thread for `mode`, draining `rx` with `take`.
fn start_consumer<T: Send + 'static>(
    mode: Consumer,
    rx: &Receiver<T>,
    stop: &Arc<AtomicBool>,
    take: fn(T),
) -> Option<std::thread::JoinHandle<()>> {
    if mode == Consumer::Idle {
        return None;
    }
    let rx = rx.clone();
    let stop = Arc::clone(stop);
    Some(std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            if mode == Consumer::Cycling {
                std::thread::sleep(WRITER_CYCLE);
            }
            while let Ok(item) = rx.try_recv() {
                take(item);
            }
        }
    }))
}

/// Runs `ROUNDS` bursts through `send_burst`, one per simulated frame, and
/// returns each burst's time divided by its seal count.
fn time_bursts<T: Send + 'static>(
    mode: Consumer,
    burst: usize,
    pool: &[BufferedSeal],
    (tx, rx): (Sender<T>, Receiver<T>),
    capacity: usize,
    take: fn(T),
    send_burst: impl Fn(&Sender<T>, &[BufferedSeal]),
) -> Vec<f64> {
    let stop = Arc::new(AtomicBool::new(false));
    let consumer = start_consumer(mode, &rx, &stop, take);
    let mut out = Vec::with_capacity(ROUNDS);
    for r in 0..ROUNDS {
        if mode == Consumer::Idle && tx.len() >= capacity / 2 {
            while rx.try_recv().is_ok() {}
        }
        let base = (r * burst) % (pool.len() - burst);
        let t = Instant::now();
        send_burst(&tx, &pool[base..base + burst]);
        out.push(t.elapsed().as_nanos() as f64 / burst as f64);
        if mode == Consumer::Cycling {
            while t.elapsed() < FRAME_GAP {
                std::hint::spin_loop();
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    if let Some(h) = consumer {
        h.join().expect("consumer thread"); // APPROVED: test harness
    }
    out
}

fn run_per_seal(mode: Consumer, burst: usize, pool: &[BufferedSeal]) -> Vec<f64> {
    let channel: (SealSender, Receiver<BufferedSeal>) =
        crossbeam_channel::bounded(SEAL_MPSC_CAPACITY);
    time_bursts(
        mode,
        burst,
        pool,
        channel,
        SEAL_MPSC_CAPACITY,
        |s| {
            black_box(s);
        },
        |tx, seals| {
            for s in seals {
                black_box(tx.try_send(*s).is_ok());
            }
        },
    )
}

fn run_batched(mode: Consumer, burst: usize, pool: &[BufferedSeal]) -> Vec<f64> {
    let cap = SEAL_MPSC_CAPACITY / BATCH;
    let channel = crossbeam_channel::bounded::<SealBatch>(cap);
    time_bursts(
        mode,
        burst,
        pool,
        channel,
        cap,
        |b| {
            for s in &b.seals[..usize::from(b.len)] {
                black_box(s);
            }
        },
        |tx, seals| {
            // What the drain would do: collect the frame's seals into a stack
            // batch as the fold emits them, then send each full or final batch.
            let mut batch = SealBatch {
                len: 0,
                seals: [seals[0]; BATCH],
            };
            for s in seals {
                batch.seals[usize::from(batch.len)] = *s;
                batch.len += 1;
                if usize::from(batch.len) == BATCH {
                    black_box(tx.try_send(batch).is_ok());
                    batch.len = 0;
                }
            }
            if batch.len > 0 {
                black_box(tx.try_send(batch).is_ok());
            }
        },
    )
}

#[test]
#[ignore = "wall-clock harness; run in release with --ignored --nocapture"]
fn seal_handoff_burst_cost_per_seal_vs_batched() {
    let pool: Vec<BufferedSeal> = (0..4_096).map(seal).collect();
    // One Instant pair, measured, so the per-seal figures can be read net of it.
    let mut pair = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let t = Instant::now();
        pair.push(t.elapsed().as_nanos() as f64);
    }
    let (pair_p50, pair_p99, _, _) = summarise(pair);
    println!(
        "Instant pair alone: p50 {pair_p50:.0} ns, p99 {pair_p99:.0} ns \
         (inside every burst sample, divided by the burst size)"
    );
    for mode in [Consumer::Idle, Consumer::Cycling, Consumer::Spinning] {
        for burst in BURSTS {
            // Warm both channels' pages and caches, untimed.
            let _ = run_per_seal(mode, burst, &pool);
            let per_seal = run_per_seal(mode, burst, &pool);
            let _ = run_batched(mode, burst, &pool);
            let batched = run_batched(mode, burst, &pool);
            // Every round must have produced a sample, or the percentiles
            // below would summarise a short run.
            assert_eq!(per_seal.len(), ROUNDS, "per-seal run lost rounds");
            assert_eq!(batched.len(), ROUNDS, "batched run lost rounds");
            let (a50, a99, a999, amean) = summarise(per_seal);
            let (b50, b99, b999, bmean) = summarise(batched);
            println!(
                "consumer {:8} burst {burst:3}: per-seal p50 {a50:6.1} p99 {a99:6.1} \
                 p99.9 {a999:7.1} mean {amean:6.1} | batched p50 {b50:6.1} p99 {b99:6.1} \
                 p99.9 {b999:7.1} mean {bmean:6.1} (ns per seal)",
                mode.name(),
            );
        }
    }
}
