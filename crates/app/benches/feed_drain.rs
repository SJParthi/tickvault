//! Criterion benchmarks for the two per-frame drains of the live lane (audit
//! PROOF-9, 2026-10-11): the main-feed frame walk and the depth frame walk.
//!
//! Before this file no benchmark measured either. Both run once per frame on
//! the frame-drain task, depth at about 24 times the tick volume, so a
//! slowdown in either one stalls the socket drain without failing any check.
//!
//! Targets (budget keys in `quality/benchmark-budgets.toml`, per-element
//! counts in its `[elements]` table):
//!   - `feed_drain/main_feed_frame_4_quotes`: one frame stacking four Quote
//!     packets (decode, lag record, fold, row append). Budget per tick.
//!   - `feed_drain/main_feed_frame_4_full`: one frame stacking four Full
//!     packets (NSE_FNO) with every price field set (LTP, ATP, day OHLC and
//!     all ten depth prices), so the f32-to-f64 price conversions and the
//!     inline depth book rows are inside the timed region. The ingest is
//!     built with inline depth in array-row mode, the production shape
//!     (`[depth_storage] array_rows` on). Budget per tick, the same
//!     `feed_drain_main_feed` key (added 2026-10-29, plan ITEM DT step 0).
//!   - `feed_drain/main_feed_frame_4_full_own`: identical Full bytes and
//!     depth path through the existing Own tick encoder. The original Full
//!     case remains Library; the production default remains unchanged.
//!   - `feed_drain/depth20_frame_4x20`: one frame stacking four depth-20
//!     packets (80 levels). Budget per level.
//!   - `feed_drain/depth200_packet`: one depth-200 packet (200 levels).
//!     Budget per level: depth is O(levels) by nature, never O(1).
//!
//! Every level and tick is appended to an in-memory ILP buffer, which would
//! grow without bound across Criterion's millions of iterations. So the
//! ingest is rebuilt every `FRAME_POOL` frames, outside the timed region
//! (`iter_custom`); the timed region holds drain calls only.
//!
//! Companion zero-allocation evidence: `tests/dhat_live_ingest_seam.rs` and
//! `tests/dhat_depth_drain.rs`.
//!
//! # The p99 gate (plan item K6, 2026-10-26)
//!
//! After Criterion, Quote and both Full encoders have per-frame p99 measured over
//! 65,536 frames and divided by the four ticks in a frame; above the same
//! `feed_drain_main_feed` per-tick budget the median is held to (times
//! `BENCH_BUDGET_MULTIPLIER`, as the median arm), the run exits 1 and the
//! bench job fails. Divided values are amortized frame cost, not tick p99. Full
//! also reports the separate unscaled strict <1000 ns/frame diagnostic; that
//! diagnostic does not replace or control the existing amortized gate. With
//! `BENCH_TAIL_GATE_DEFER=1` (set by `bench.yml`) it
//! prints `BENCH-TAIL-GATE-FAILED` and exits 0 instead, and a later step
//! fails the run on that line after the Criterion baseline is saved. The
//! amortized check lives in `tickvault_app::drain_tail_gate`,
//! whose self-test (an over-budget sample must fail) runs first. Only under
//! `cargo bench` (`--bench`): a test-mode run of this binary is debug code
//! and its timings mean nothing.
//!
//! Run: `cargo bench --bench feed_drain -p tickvault-app`

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group};

use tickvault_app::dhan_feed_stack::{
    DepthIngest, DrainCounters, LiveIngest, counters, drain_depth_frame, drain_main_feed_frame,
};
use tickvault_app::drain_tail_gate;
use tickvault_common::constants::{DEEP_DEPTH_FEED_CODE_ASK, DEEP_DEPTH_FEED_CODE_BID};
use tickvault_common::feed::Feed;
use tickvault_core::parser::depth::DepthFeedKind;
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::pool_supervisor::CapturedFrame;
use tickvault_storage::tick_ilp::TickIlpEncoder;
use tickvault_storage::tick_persistence::TickWriter;

#[path = "support/full_frame_fixture.rs"]
mod full_frame_fixture;
use full_frame_fixture::{
    FRAME_POOL, FrameTailSummary, SESSION_EPOCH_SECS, SESSION_RECEIPT_NANOS, TICKS_PER_MAIN_FRAME,
    assert_full_frames_fold, full_feed_frame, full_ingest, sample_full_frames,
};

/// A receipt instant the depth drain's own unit tests use.
const DEPTH_RECEIPT_NANOS: i64 = 1_779_355_000_000_000_000;

fn captured(bytes: Vec<u8>, endpoint: DhanEndpointType, seq: u64) -> CapturedFrame {
    CapturedFrame {
        seq,
        endpoint,
        connection_index: 5,
        received_at: Instant::now(),
        received_at_nanos: tickvault_storage::ws_frame_spill::receipt_nanos_from(Instant::now()),
        wal_backed: true,
        bytes: bytes.into(),
    }
}

/// One 50-byte Quote packet in the wire layout `parser/quote.rs` reads.
fn quote_packet(security_id: u32, ltp: f32, ltt: u32, volume: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 50];
    buf[0] = 4; // Quote
    buf[1..3].copy_from_slice(&50u16.to_le_bytes());
    buf[3] = 0; // IDX_I
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&ltp.to_le_bytes());
    buf[14..18].copy_from_slice(&ltt.to_le_bytes());
    buf[22..26].copy_from_slice(&volume.to_le_bytes());
    buf
}

fn main_feed_frame(n: u64) -> CapturedFrame {
    let mut bytes = Vec::with_capacity(200);
    for i in 0..4u32 {
        bytes.extend_from_slice(&quote_packet(
            13 + i,
            100.0 + (n % 97) as f32 * 0.05,
            // Rising, about one second per four frames, and never ahead of
            // the receipt instant (the pool spans about 256 s before it).
            SESSION_EPOCH_SECS - 300 + (n / 4) as u32,
            2 + n as u32,
        ));
    }
    captured(bytes, DhanEndpointType::MainFeed, 1_000 + n)
}

/// One depth packet: 12-byte header, then `rows` 16-byte levels.
fn depth_packet(security_id: u32, feed_code: u8, rows: u32, depth200: bool, n: u64) -> Vec<u8> {
    let levels = rows as usize;
    let mut p = vec![0u8; 12 + levels * 16];
    let len = u16::try_from(p.len()).unwrap_or(u16::MAX);
    p[0..2].copy_from_slice(&len.to_le_bytes());
    p[2] = feed_code;
    p[3] = 2; // NSE_FNO
    p[4..8].copy_from_slice(&security_id.to_le_bytes());
    if depth200 {
        p[8..12].copy_from_slice(&rows.to_le_bytes());
    }
    for i in 0..levels {
        let base = 12 + i * 16;
        let price = 100.0_f64 + i as f64 * 0.05 + (n % 97) as f64 * 0.05;
        p[base..base + 8].copy_from_slice(&price.to_le_bytes());
        p[base + 8..base + 12].copy_from_slice(&(10 * (i as u32 + 1)).to_le_bytes());
        p[base + 12..base + 16].copy_from_slice(&(i as u32 + 1).to_le_bytes());
    }
    p
}

fn depth20_frame(n: u64) -> CapturedFrame {
    let mut bytes = Vec::with_capacity(4 * 332);
    for (sid, code) in [
        (60_001u32, DEEP_DEPTH_FEED_CODE_BID),
        (60_001, DEEP_DEPTH_FEED_CODE_ASK),
        (60_002, DEEP_DEPTH_FEED_CODE_BID),
        (60_002, DEEP_DEPTH_FEED_CODE_ASK),
    ] {
        bytes.extend_from_slice(&depth_packet(sid, code, 20, false, n));
    }
    captured(bytes, DhanEndpointType::Depth20, 1_000 + n)
}

fn depth200_frame(n: u64) -> CapturedFrame {
    let code = if n.is_multiple_of(2) {
        DEEP_DEPTH_FEED_CODE_BID
    } else {
        DEEP_DEPTH_FEED_CODE_ASK
    };
    captured(
        depth_packet(70_001, code, 200, true, n),
        DhanEndpointType::Depth200,
        1_000 + n,
    )
}

/// Times `iters` drain calls, rebuilding the ingest every `FRAME_POOL`
/// frames with the timer stopped.
fn run_timed<I>(
    iters: u64,
    frames: &[CapturedFrame],
    fresh: impl Fn() -> I,
    mut drain: impl FnMut(&mut I, &CapturedFrame, u64),
) -> Duration {
    let mut total = Duration::ZERO;
    let mut done = 0_u64;
    while done < iters {
        let mut ingest = fresh();
        let batch = (iters - done).min(FRAME_POOL);
        let start = Instant::now();
        for k in 0..batch {
            let i = done + k;
            drain(&mut ingest, &frames[k as usize], i);
        }
        total += start.elapsed();
        done += batch;
        drop(ingest);
    }
    total
}

/// Per-frame samples collected for each workload. A fresh ingest consumes
/// each 1,024-frame pool in order, as `run_timed` does. Construction is outside
/// the timer; initial state/cache/page touches inside a drain remain measured.
const PERCENTILE_SAMPLES: usize = 64 * FRAME_POOL as usize;

/// Measures the main-feed drain per frame and returns the samples sorted
/// (plan item K5, 2026-10-26; gated since K6).
///
/// Criterion's own budget check reads the MEDIAN of its sample means, which
/// says nothing about the tail: a drain that is 2 µs almost always and
/// 2 ms once in a hundred frames passes it. This reads the tail directly —
/// one `Instant` pair per frame, into a `Vec` sized before the loop, sorted
/// after it. Sample bookkeeping does not allocate between the clock reads;
/// allocation inside the drain requires separate DHAT evidence.
///
/// **What it is not:** the clock pair costs tens of nanoseconds per frame
/// and is inside each sample, so these figures read slightly high; and this
/// is a synthetic frame pool on the bench host, not the live lane. The live
/// tail is `tv_hot_path_stage_p99_ns{stage="drain_main_frame"}` on the box.
fn main_feed_drain_samples(
    frames: &[CapturedFrame],
    drain_counters: &'static DrainCounters,
) -> Vec<u64> {
    let mut samples: Vec<u64> = Vec::with_capacity(PERCENTILE_SAMPLES);
    let mut i = 0u64;
    while samples.len() < PERCENTILE_SAMPLES {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
        for frame in frames {
            let start = Instant::now();
            black_box(drain_main_feed_frame(
                &mut ingest,
                black_box(frame),
                SESSION_RECEIPT_NANOS,
                1_000 + i,
                drain_counters,
            ));
            samples.push(u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX));
            i += 1;
        }
    }
    samples.sort_unstable();
    let at = |num: usize, den: usize| -> u64 {
        drain_tail_gate::quantile_of_sorted(&samples, num, den).unwrap_or(0)
    };
    println!(
        "feed_drain/main_feed_frame_4_quotes percentiles over {} frames: \
         p50 = {} ns, p99 = {} ns, p99.9 = {} ns, max = {} ns (per FRAME, 4 ticks each; \
         includes one Instant pair per frame)",
        samples.len(),
        at(1, 2),
        at(99, 100),
        at(999, 1_000),
        samples.last().copied().unwrap_or(0),
    );
    samples
}

/// The p99 gate (plan item K6). `Err` fails the bench run.
fn main_feed_budget() -> Result<(u64, f64), String> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../quality/benchmark-budgets.toml"
    );
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
    let budget =
        drain_tail_gate::budget_from_toml(&text, drain_tail_gate::MAIN_FEED_DRAIN_BUDGET_KEY)
            .ok_or_else(|| {
                format!(
                    "no positive `{}` in [budgets] of {path}",
                    drain_tail_gate::MAIN_FEED_DRAIN_BUDGET_KEY
                )
            })?;
    let env_value = std::env::var(drain_tail_gate::BUDGET_MULTIPLIER_ENV).ok();
    let multiplier = drain_tail_gate::budget_multiplier(env_value.as_deref()).ok_or_else(|| {
        format!(
            "{} = {env_value:?} is not a number in [1, {}]",
            drain_tail_gate::BUDGET_MULTIPLIER_ENV,
            drain_tail_gate::MAX_BUDGET_MULTIPLIER
        )
    })?;
    Ok((budget, multiplier))
}

fn main_feed_drain_tail_gate() -> Result<(), String> {
    let (budget, multiplier) = main_feed_budget()?;
    // A gate that can not fail proves nothing: check it fails first.
    drain_tail_gate::self_test(budget, TICKS_PER_MAIN_FRAME)?;

    let frames: Vec<CapturedFrame> = (0..FRAME_POOL).map(main_feed_frame).collect();
    let samples = main_feed_drain_samples(&frames, counters());
    let verdict = drain_tail_gate::tail_verdict(&samples, TICKS_PER_MAIN_FRAME, budget, multiplier)
        .ok_or_else(|| "no samples".to_string())?;
    println!(
        "feed_drain p99 gate: p99 = {} ns per frame; amortized {:.0} ns/tick (not tick p99), ceiling {:.0} ns \
         per tick (budget {budget} x {multiplier}): {}",
        verdict.p99_ns_per_frame,
        verdict.p99_ns_per_tick,
        verdict.ceiling_ns_per_tick,
        if verdict.within { "PASS" } else { "FAIL" },
    );
    if verdict.within {
        Ok(())
    } else {
        Err(format!(
            "main-feed drain p99 {:.0} ns per tick is over the {:.0} ns ceiling",
            verdict.p99_ns_per_tick, verdict.ceiling_ns_per_tick
        ))
    }
}

/// Full frame tails for both existing encoders on identical input bytes.
/// The legacy budget still gates amortized frame cost; strict frame p99 is
/// reported separately in its actual unit and never scaled by the CI factor.
fn full_feed_drain_tail_gates() -> Result<(), String> {
    let (budget, multiplier) = main_feed_budget()?;
    drain_tail_gate::self_test(budget, TICKS_PER_MAIN_FRAME)?;
    let frames: Vec<CapturedFrame> = (0..FRAME_POOL).map(full_feed_frame).collect();
    let mut failures = Vec::with_capacity(2);
    let drain_counters = counters();
    for (label, encoder) in [
        ("library", TickIlpEncoder::Library),
        ("own", TickIlpEncoder::Own),
    ] {
        let mut samples =
            sample_full_frames(&frames, encoder, PERCENTILE_SAMPLES, |ingest, frame, i| {
                let start = Instant::now();
                let outcome = black_box(drain_main_feed_frame(
                    ingest,
                    black_box(frame),
                    SESSION_RECEIPT_NANOS,
                    1_000 + i,
                    drain_counters,
                ));
                let elapsed = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                (outcome, elapsed)
            })?;
        let summary = FrameTailSummary::from_samples(&mut samples)
            .ok_or_else(|| "no Full-frame tail samples".to_string())?;
        println!(
            "feed_drain Full {label}: {} complete FRAME calls; p50={} ns/frame, p99={} ns/frame, \
             p99.9={} ns/frame, max={} ns/frame; unscaled strict p99 <1000 ns/frame: {} \
             (one Instant pair per frame; no individual-tick tail measurement)",
            summary.samples,
            summary.p50_ns,
            summary.p99_ns,
            summary.p999_ns,
            summary.max_ns,
            if summary.p99_below_one_microsecond() {
                "PASS"
            } else {
                "FAIL"
            },
        );
        let verdict =
            drain_tail_gate::tail_verdict(&samples, TICKS_PER_MAIN_FRAME, budget, multiplier)
                .ok_or_else(|| "no Full-frame amortized verdict".to_string())?;
        println!(
            "feed_drain Full {label} legacy amortized-cost gate: frame p99 / {} = {:.3} ns/tick \
             (not tick p99), ceiling {:.3} ns/tick (budget {budget} x {multiplier}): {}",
            TICKS_PER_MAIN_FRAME,
            verdict.p99_ns_per_tick,
            verdict.ceiling_ns_per_tick,
            if verdict.within { "PASS" } else { "FAIL" },
        );
        if !verdict.within {
            failures.push(format!(
                "Full {label} amortized frame cost {:.3} ns/tick exceeds {:.3}",
                verdict.p99_ns_per_tick, verdict.ceiling_ns_per_tick
            ));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn bench_feed_drain(c: &mut Criterion) {
    let drain_counters = counters();

    let main_frames: Vec<CapturedFrame> = (0..FRAME_POOL).map(main_feed_frame).collect();
    c.bench_function("feed_drain/main_feed_frame_4_quotes", |b| {
        b.iter_custom(|iters| {
            run_timed(
                iters,
                &main_frames,
                || LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8),
                |ingest, frame, i| {
                    black_box(drain_main_feed_frame(
                        ingest,
                        black_box(frame),
                        SESSION_RECEIPT_NANOS,
                        1_000 + i,
                        drain_counters,
                    ));
                },
            )
        });
    });

    let full_frames: Vec<CapturedFrame> = (0..FRAME_POOL).map(full_feed_frame).collect();
    assert_full_frames_fold(&full_frames, TickIlpEncoder::Library, drain_counters);
    c.bench_function("feed_drain/main_feed_frame_4_full", |b| {
        b.iter_custom(|iters| {
            run_timed(
                iters,
                &full_frames,
                || full_ingest(TickIlpEncoder::Library),
                |ingest, frame, i| {
                    black_box(drain_main_feed_frame(
                        ingest,
                        black_box(frame),
                        SESSION_RECEIPT_NANOS,
                        1_000 + i,
                        drain_counters,
                    ));
                },
            )
        });
    });

    assert_full_frames_fold(&full_frames, TickIlpEncoder::Own, drain_counters);
    c.bench_function("feed_drain/main_feed_frame_4_full_own", |b| {
        b.iter_custom(|iters| {
            run_timed(
                iters,
                &full_frames,
                || full_ingest(TickIlpEncoder::Own),
                |ingest, frame, i| {
                    black_box(drain_main_feed_frame(
                        ingest,
                        black_box(frame),
                        SESSION_RECEIPT_NANOS,
                        1_000 + i,
                        drain_counters,
                    ));
                },
            )
        });
    });

    let depth20_frames: Vec<CapturedFrame> = (0..FRAME_POOL).map(depth20_frame).collect();
    c.bench_function("feed_drain/depth20_frame_4x20", |b| {
        b.iter_custom(|iters| {
            run_timed(
                iters,
                &depth20_frames,
                DepthIngest::for_test,
                |depth, frame, _| {
                    black_box(drain_depth_frame(
                        depth,
                        black_box(frame),
                        DEPTH_RECEIPT_NANOS,
                        DepthFeedKind::Twenty,
                        drain_counters,
                    ));
                },
            )
        });
    });

    let depth200_frames: Vec<CapturedFrame> = (0..FRAME_POOL).map(depth200_frame).collect();
    c.bench_function("feed_drain/depth200_packet", |b| {
        b.iter_custom(|iters| {
            run_timed(
                iters,
                &depth200_frames,
                DepthIngest::for_test,
                |depth, frame, _| {
                    black_box(drain_depth_frame(
                        depth,
                        black_box(frame),
                        DEPTH_RECEIPT_NANOS,
                        DepthFeedKind::TwoHundred,
                        drain_counters,
                    ));
                },
            )
        });
    });
}

criterion_group!(benches, bench_feed_drain);

/// Set to `1` by `bench.yml`: a p99 miss then prints [`TAIL_GATE_FAILED_MARKER`]
/// and exits 0, and a later workflow step fails the run on that line after the
/// Criterion baseline is saved. Unset (a local `cargo bench`), a miss exits 1.
const TAIL_GATE_DEFER_ENV: &str = "BENCH_TAIL_GATE_DEFER";
/// The line every failed p99 gate prints (same text in `seal_path`).
const TAIL_GATE_FAILED_MARKER: &str = "BENCH-TAIL-GATE-FAILED";

/// `criterion_main!` plus the p99 gate. The gate runs only under
/// `cargo bench` (Criterion's `--bench` flag); a test-mode run of this
/// binary is unoptimised and its timings mean nothing.
fn main() {
    benches();
    Criterion::default().configure_from_args().final_summary();
    if std::env::args().any(|a| a == "--bench") {
        // Evaluate both before deciding exit, so a Quote failure cannot hide
        // either Full encoder's measurement. Existing defer behavior remains.
        let results = [main_feed_drain_tail_gate(), full_feed_drain_tail_gates()];
        let mut failed = false;
        for result in results {
            if let Err(why) = result {
                failed = true;
                eprintln!("{TAIL_GATE_FAILED_MARKER} feed_drain p99 gate: {why}");
            }
        }
        if failed && std::env::var(TAIL_GATE_DEFER_ENV).as_deref() != Ok("1") {
            std::process::exit(1);
        }
    }
}
