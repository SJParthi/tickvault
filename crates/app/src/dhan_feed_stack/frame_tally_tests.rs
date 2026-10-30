//! Old-vs-new equality for the drain's per-tick savings (plan ITEM DT,
//! sub-µs drain, 2026-10-30): the per-frame counter tally and the
//! load-guarded `fetch_max`. Each test compares the new code
//! against the exact code it replaced.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use proptest::prelude::*;

use super::*;
use tickvault_common::feed::Feed;
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_storage::tick_persistence::TickWriter;

proptest! {
    /// The guarded `fetch_max` leaves the cell exactly where the plain one
    /// does, for any sequence of samples and resets.
    #[test]
    fn proptest_fetch_max_if_above_equals_fetch_max(
        ops in proptest::collection::vec((any::<i64>(), any::<bool>()), 0..200)
    ) {
        let guarded = AtomicI64::new(0);
        let plain = AtomicI64::new(0);
        for (value, reset) in ops {
            if reset {
                prop_assert_eq!(
                    guarded.swap(0, Ordering::Relaxed),
                    plain.swap(0, Ordering::Relaxed)
                );
            }
            fetch_max_if_above(&guarded, value);
            plain.fetch_max(value, Ordering::Relaxed);
            prop_assert_eq!(guarded.load(Ordering::Relaxed), plain.load(Ordering::Relaxed));
        }
    }

    /// Every pushed code is counted exactly once, under its own code: the
    /// per-code sums of the flushed runs equal the per-code packet counts.
    #[test]
    fn proptest_code_runs_count_every_packet_once(
        codes in proptest::collection::vec(prop_oneof![Just(4u8), Just(5u8), Just(8u8), any::<u8>()], 0..300)
    ) {
        let mut expected = [0_u64; 256];
        for &code in &codes {
            expected[usize::from(code)] += 1;
        }
        let mut got = [0_u64; 256];
        let mut run = CodeRun::new();
        let mut flushes = 0_usize;
        for &code in &codes {
            if let Some((done, packets)) = run.push(code) {
                prop_assert!(packets > 0);
                got[usize::from(done)] += packets;
                flushes += 1;
            }
        }
        if let Some((done, packets)) = run.take() {
            got[usize::from(done)] += packets;
            flushes += 1;
        }
        prop_assert_eq!(run.take(), None, "take empties the run");
        prop_assert_eq!(expected, got);
        // One flush per run of equal codes: the number of code changes + 1.
        let runs = if codes.is_empty() {
            0
        } else {
            1 + codes.windows(2).filter(|w| w[0] != w[1]).count()
        };
        prop_assert_eq!(flushes, runs);
    }
}

/// An exchange time inside a session (IST epoch seconds), as the bench uses.
const SESSION_EPOCH_SECS: u32 = 1_786_800_000;
/// A receipt instant inside the same session (UTC nanos).
const SESSION_RECEIPT_NANOS: i64 = (SESSION_EPOCH_SECS as i64 - 19_800) * 1_000_000_000;

fn quote_packet(security_id: u32, ltp: f32, ltt: u32, volume: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 50];
    buf[0] = 4;
    buf[1..3].copy_from_slice(&50u16.to_le_bytes());
    buf[3] = 2; // NSE_FNO
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&ltp.to_le_bytes());
    buf[14..18].copy_from_slice(&ltt.to_le_bytes());
    buf[22..26].copy_from_slice(&volume.to_le_bytes());
    buf
}

fn oi_packet(security_id: u32, oi: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 12];
    buf[0] = 5;
    buf[1..3].copy_from_slice(&12u16.to_le_bytes());
    buf[3] = 2;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&oi.to_le_bytes());
    buf
}

fn frame_of(bytes: Vec<u8>, seq: u64) -> CapturedFrame {
    CapturedFrame {
        seq,
        endpoint: DhanEndpointType::MainFeed,
        connection_index: 3,
        received_at: std::time::Instant::now(),
        received_at_nanos: SESSION_RECEIPT_NANOS,
        wal_backed: true,
        bytes: bytes.into(),
    }
}

/// `DrainCounters` whose `folded` series reads back through `cell`; every
/// other series is a no-op.
fn counters_with_folded(cell: &Arc<AtomicU64>) -> DrainCounters {
    let noop = metrics::Counter::noop;
    DrainCounters {
        folded: metrics::Counter::from_arc(Arc::clone(cell)),
        non_tick: noop(),
        main_feed_disconnects: noop(),
        unparseable: noop(),
        seq_unrepresentable: noop(),
        aggregator_refused: noop(),
        write_failed: noop(),
        ingest_ticks: noop(),
        ingest_seq_refused: noop(),
        tick_unknown_segment: noop(),
        verdict_ltt_backward: noop(),
        verdict_volume_jump: noop(),
        refused_price: noop(),
        refused_timestamp: noop(),
        refused_slot: noop(),
        refused_session: noop(),
        seals_emitted: noop(),
        seals_dropped: noop(),
        seals_rescued: noop(),
        flush_ok: noop(),
        flush_failed: noop(),
        depth_flush_failed: noop(),
        depth_unconsumed: noop(),
        shed_inline_depth: noop(),
        shed_dedicated_depth: noop(),
        frames_wal_unbacked: noop(),
        depth_unbacked_not_shed: noop(),
        depth_rows: noop(),
        depth_refused: noop(),
        depth_implausible_kept: noop(),
        depth_dropped: noop(),
        depth_disconnects: noop(),
        depth_length_mismatch: noop(),
        depth_ghost: noop(),
        depth_unsubscribed_grace: noop(),
        depth_ghost_unsubscribes: noop(),
        depth_ghost_exhausted: noop(),
        truncated: noop(),
        main_feed_length_mismatch: noop(),
        abandoned_bytes: noop(),
        unknown_skipped: noop(),
    }
}

/// The `folded` counter, added once per frame, ends equal to the sum of the
/// frames' `folded` outcomes: what the per-packet increment it replaced
/// would have reached. Frames mix Quote and OI packets (two code runs), and
/// some end in a truncated packet, so the early return path is covered too.
#[test]
fn test_folded_counter_total_equals_the_per_packet_count() {
    let cell = Arc::new(AtomicU64::new(0));
    let counters = counters_with_folded(&cell);
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
    let mut expected = 0_u64;
    let mut truncated_frames = 0_u64;
    for n in 0..512_u32 {
        let mut bytes = Vec::new();
        for i in 0..(1 + n % 4) {
            bytes.extend_from_slice(&quote_packet(
                40_000 + i,
                100.0 + (n % 50) as f32 * 0.05,
                SESSION_EPOCH_SECS - 300 + n / 4,
                2 + n,
            ));
            if (n + i) % 3 == 0 {
                bytes.extend_from_slice(&oi_packet(40_000 + i, 1_000 + n));
            }
        }
        if n % 7 == 0 {
            // A trailing partial Quote: the walk returns early.
            bytes.extend_from_slice(&quote_packet(40_100, 1.0, SESSION_EPOCH_SECS, 1)[..20]);
            truncated_frames += 1;
        }
        let out = drain_main_feed_frame(
            &mut ingest,
            &frame_of(bytes, 10_000 + u64::from(n)),
            SESSION_RECEIPT_NANOS,
            1_000 + u64::from(n),
            &counters,
        );
        expected += out.folded;
        assert_eq!(
            cell.load(Ordering::Relaxed),
            expected,
            "after frame {n} the counter must equal the folded total"
        );
    }
    assert!(expected > 0, "non-vacuous: some packets must fold");
    assert!(
        truncated_frames > 0,
        "non-vacuous: the early return must run"
    );
}

/// Wall-clock cost of the operations this change removes from each tick,
/// on whatever host runs it (`#[ignore]`d so a timing never becomes a flaky
/// gate; run with `cargo test --release -p tickvault-app --lib
/// atomic_costs_this_change_saves -- --ignored --nocapture`).
///
/// A production counter increment is an atomic add on the exporter's cell
/// (`Counter::from_arc` over an `AtomicU64` here, the same shape). The
/// window maximum's `fetch_max` is measured against the guarded form on a
/// cell that already holds a larger value, the common case.
#[test]
#[ignore = "wall-clock harness, run by hand"]
fn atomic_costs_this_change_saves() {
    const N: u64 = 20_000_000;
    let cell = Arc::new(AtomicU64::new(0));
    let counter = metrics::Counter::from_arc(Arc::clone(&cell));
    let start = std::time::Instant::now();
    for _ in 0..N {
        counter.increment(std::hint::black_box(1));
    }
    let per_increment = start.elapsed().as_nanos() as f64 / N as f64;

    let max = AtomicI64::new(i64::MAX / 2);
    let start = std::time::Instant::now();
    for i in 0..N {
        max.fetch_max(std::hint::black_box(i as i64 % 500), Ordering::Relaxed);
    }
    let per_fetch_max = start.elapsed().as_nanos() as f64 / N as f64;
    let start = std::time::Instant::now();
    for i in 0..N {
        fetch_max_if_above(&max, std::hint::black_box(i as i64 % 500));
    }
    let per_guarded = start.elapsed().as_nanos() as f64 / N as f64;
    assert_eq!(cell.load(Ordering::Relaxed), N);
    println!(
        "counter increment {per_increment:.2} ns, fetch_max {per_fetch_max:.2} ns, \
         guarded fetch_max (no raise) {per_guarded:.2} ns"
    );
}
