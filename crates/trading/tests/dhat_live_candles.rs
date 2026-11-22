//! DHAT allocation test for `LiveCandleBook`: after `configure`, the per-tick
//! path (`on_tick`) and the clock (`on_clock`) allocate nothing, for 500
//! instruments × 3 frames, across bar opens, tick closes, clock closes, late
//! ticks, bad-price refusals and future-stamp refusals.
//!
//! HONEST SCOPE: this proves the book does not ALLOCATE. It says nothing about
//! speed; the bench `benches/live_candles.rs` times it.
//!
//! ONE profiler per test binary (`dhat::Profiler` is process-global).

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_trading::brutex_host::InstrumentKey;
use tickvault_trading::brutex_host::live_candles::{ClosedBars, LiveCandleBook};

mod dhat_support;

/// IST midnight in IST epoch seconds (multiple of 86,400).
const DAY: u32 = 1_779_321_600;
/// 09:15:00 IST.
const OPEN: u32 = DAY + 33_300;
const NS: i64 = 1_000_000_000;
const KEYS: u64 = 500;
const FRAMES: [u32; 3] = [1, 5, 60];
/// Measured calls (ticks plus clock readings).
const CALLS: u32 = 100_000;
/// One clock reading every this many calls.
const CLOCK_EVERY: u32 = 1_000;

fn key(id: u64) -> InstrumentKey {
    InstrumentKey {
        security_id: id,
        exchange_segment: 2,
    }
}

/// Runs `CALLS` calls starting at second `start`, returns
/// (bars emitted, late ticks, refusals) so the test can prove it did work.
fn workload(book: &mut LiveCandleBook, out: &mut ClosedBars, start: u32) -> (u64, u64, u64) {
    let mut emitted = 0u64;
    let mut late = 0u64;
    let mut refused = 0u64;
    let mut cum = [0u64; KEYS as usize + 1];
    for i in 0..CALLS {
        // About 500 ticks a second of exchange time.
        let secs = start + i / 500;
        if i % CLOCK_EVERY == 0 {
            book.on_clock(i64::from(secs) * NS + 600_000_000, out);
        } else {
            let id = u64::from(i % KEYS as u32) + 1;
            cum[id as usize] += u64::from(i % 7);
            // Every 97th tick is a second old (late in the 1 s frame once the
            // clock has passed it); every 211th has a bad price; every 307th
            // claims an hour ahead (refused, its receipt stays at `secs`).
            let t = if i % 97 == 0 {
                secs.saturating_sub(1)
            } else if i % 307 == 0 {
                secs + 3_600
            } else {
                secs
            };
            let px = if i % 211 == 0 {
                f64::NAN
            } else {
                100.0 + f64::from(i % 50)
            };
            let v = book.on_tick(
                key(id),
                t,
                i64::from(secs.min(t)) * NS + 1_000,
                px,
                cum[id as usize],
                out,
            );
            if let tickvault_trading::brutex_host::live_candles::TickVerdict::Accepted {
                late_frames,
            } = v
            {
                late += u64::from(late_frames);
            } else {
                refused += 1;
            }
        }
        emitted += out.bars().len() as u64;
        out.clear();
    }
    (emitted, late, refused)
}

#[test]
fn dhat_live_candles_zero_alloc_after_configure() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let keys: Vec<InstrumentKey> = (1..=KEYS).map(key).collect();
    let mut book = LiveCandleBook::configure(&keys, &FRAMES).expect("valid");
    let mut out = ClosedBars::with_capacity(4 * KEYS as usize);
    // Warm-up outside the measured window (first touches of every slot).
    let warm = workload(&mut book, &mut out, OPEN);
    assert!(warm.0 > 0);

    let future_before = book.counters().refused_future;
    let price_before = book.counters().refused_price;
    let mut start = OPEN + CALLS / 500 + 1;
    let mut result = (0, 0, 0);
    let (bytes, blocks) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            result = workload(&mut book, &mut out, start);
            start += CALLS / 500 + 1;
        },
    );
    eprintln!(
        "dhat_live_candles: {CALLS} calls ({KEYS} keys x {} frames): {bytes} B / {blocks} blocks; \
         bars emitted {}, late {}, refused {}",
        FRAMES.len(),
        result.0,
        result.1,
        result.2
    );
    // Non-vacuity: bars closed by tick and clock, late ticks and refusals all
    // happened inside the measured window.
    assert!(result.0 > 10_000, "bars emitted: {}", result.0);
    assert!(result.1 > 0, "late ticks: {}", result.1);
    assert!(result.2 > 0, "refusals: {}", result.2);
    assert!(book.counters().closed_by_clock > 0);
    assert!(book.counters().closed_by_tick > 0);
    assert!(
        book.counters().refused_future > future_before,
        "no future-stamp refusals in the measured window"
    );
    assert!(
        book.counters().refused_price > price_before,
        "no bad-price refusals in the measured window"
    );
    assert_eq!(out.overflowed(), 0);
    assert_eq!(
        (bytes, blocks),
        (0, 0),
        "LiveCandleBook allocated {blocks} blocks / {bytes} B over {CALLS} calls"
    );
}
