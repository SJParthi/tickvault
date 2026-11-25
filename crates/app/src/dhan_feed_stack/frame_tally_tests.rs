#![cfg(test)]
//! Old-vs-new equality for the drain's per-tick savings (plan ITEM DT,
//! sub-µs drain, 2026-10-30): the per-frame counter tally and the
//! load-guarded `fetch_max`. Each test compares the new code
//! against the exact code it replaced. Also the source scans that pin the
//! tally's methods and its frame-end flush (moved here from the parent's
//! `tests` module on 2026-11-17 with the tally itself).

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use proptest::prelude::*;

use super::frame_tally::CodeRun;
use super::*;
use tickvault_common::feed::Feed;
use tickvault_core::parser::dispatcher::dispatch_frame_uncounted;
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
        lag_excluded_ltt_not_advanced: noop(),
        lag_excluded_not_new_trade: noop(),
        lag_excluded_not_a_trade: noop(),
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

// ---- Speed fix 4 (2026-11-17): rows-ingested and lag-exclusion counts ----
//
// The drain stopped counting `tv_dhan_feed_ingest_ticks_total` and three
// `tv_dhan_ws_lag_excluded_total` reasons per tick; `FrameTally` adds them
// once per frame. The oracle below is the code it replaced: a twin ingest
// folds the same packets through `ingest_tick_at` and classifies each outcome
// with the exact per-tick match the drain used before. After EVERY frame the
// counters must equal the oracle.

/// Readable cells for the five counters `FrameTally` now adds.
#[derive(Default)]
struct TallyCells {
    folded: Arc<AtomicU64>,
    ingest_ticks: Arc<AtomicU64>,
    repeat: Arc<AtomicU64>,
    not_new_trade: Arc<AtomicU64>,
    not_a_trade: Arc<AtomicU64>,
}

impl TallyCells {
    fn counters(&self) -> DrainCounters {
        let cell = |c: &Arc<AtomicU64>| metrics::Counter::from_arc(Arc::clone(c));
        let mut counters = counters_with_folded(&self.folded);
        counters.ingest_ticks = cell(&self.ingest_ticks);
        counters.lag_excluded_ltt_not_advanced = cell(&self.repeat);
        counters.lag_excluded_not_new_trade = cell(&self.not_new_trade);
        counters.lag_excluded_not_a_trade = cell(&self.not_a_trade);
        counters
    }

    fn read(&self) -> [u64; 5] {
        [
            self.folded.load(Ordering::Relaxed),
            self.ingest_ticks.load(Ordering::Relaxed),
            self.repeat.load(Ordering::Relaxed),
            self.not_new_trade.load(Ordering::Relaxed),
            self.not_a_trade.load(Ordering::Relaxed),
        ]
    }
}

/// The pre-fix-4 per-tick classification, verbatim in shape: index 0
/// `folded`, 1 `ingest_ticks`, 2 repeat, 3 not-new-trade, 4 not-a-trade.
/// A measured lag sample adds to none of them.
fn old_per_tick_counts(outcome: &IngestOutcome, totals: &mut [u64; 5]) {
    // The old `ingest_tick_at` counted a row on these two outcomes, and the
    // drain counted them as `folded`.
    if matches!(
        outcome,
        IngestOutcome::Folded { .. } | IngestOutcome::WrittenOutOfSession
    ) {
        totals[0] += 1;
        totals[1] += 1;
    }
    match outcome {
        IngestOutcome::Folded {
            repeat_quote: true, ..
        } => totals[2] += 1,
        IngestOutcome::Folded {
            advanced: false, ..
        } => totals[3] += 1,
        IngestOutcome::Folded { .. } => {}
        _ => totals[4] += 1,
    }
}

/// One packet description: (contract, ltt offset seconds, volume, price step).
type Pkt = (u32, i64, u32, u8);

/// Runs `frames` through the drain and through the oracle twin, asserting
/// the counters equal the oracle after every frame. Returns the totals.
fn run_drain_against_oracle(frames: &[Vec<Pkt>]) -> [u64; 5] {
    let cells = TallyCells::default();
    let counters = cells.counters();
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
    let mut twin = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
    let mut expected = [0_u64; 5];
    for (n, pkts) in frames.iter().enumerate() {
        let seq = 50_000 + n as u64;
        let recv_millis = 2_000 + n as u64;
        let mut bytes = Vec::new();
        let mut parsed = Vec::new();
        for &(sid, ltt_off, volume, step) in pkts {
            let ltt = u32::try_from(i64::from(SESSION_EPOCH_SECS) + ltt_off).expect("ltt");
            let packet = quote_packet(60_000 + sid, 100.0 + f32::from(step) * 0.05, ltt, volume);
            match dispatch_frame_uncounted(&packet, SESSION_RECEIPT_NANOS) {
                Ok(ParsedFrame::Tick(t) | ParsedFrame::TickWithDepth(t, _)) => parsed.push(t),
                other => panic!("a quote packet must parse as a tick, got {other:?}"),
            }
            bytes.extend_from_slice(&packet);
        }
        let frame = frame_of(bytes, seq);
        // The oracle: the old per-tick path on an identical ingest.
        twin.frame_receipt_is_wal_stamp =
            crate::tick_quote_route::receipt_is_wal_stamp(frame.received_at_nanos);
        twin.frame_wal_unbacked = !frame.wal_backed;
        for (index, tick) in parsed.iter().enumerate() {
            let index = u32::try_from(index).expect("index");
            let outcome = twin.ingest_tick_at(tick, seq, index, recv_millis);
            old_per_tick_counts(&outcome, &mut expected);
        }
        let out = drain_main_feed_frame(
            &mut ingest,
            &frame,
            SESSION_RECEIPT_NANOS,
            recv_millis,
            &counters,
        );
        assert_eq!(
            cells.read(),
            expected,
            "after frame {n}: counters must equal the per-tick oracle"
        );
        assert_eq!(
            out.tick_packets,
            pkts.len() as u64,
            "every packet reached the tick arm"
        );
    }
    expected
}

/// A fixed sequence that reaches every class: new trades, repeats (the same
/// quote again), a cumulative regression (not a new trade) and an earlier
/// day's stamp (not a trade), single- and multi-packet frames.
#[test]
fn test_fix4_counts_equal_the_per_tick_oracle_on_every_class() {
    let day = 86_400_i64;
    let frames: Vec<Vec<Pkt>> = vec![
        vec![(1, -300, 10, 0)],
        vec![(1, -300, 10, 0)],
        vec![(1, -299, 20, 1), (2, -299, 5, 2), (1, -299, 20, 1)],
        vec![(1, -298, 15, 3)],
        vec![(3, -day, 7, 4), (3, -day, 7, 4), (2, -298, 9, 2)],
        vec![(4, -2 * day, 1, 0)],
        vec![
            (2, -297, 12, 5),
            (2, -296, 3, 5),
            (1, -295, 40, 6),
            (1, -295, 40, 6),
        ],
    ];
    let totals = run_drain_against_oracle(&frames);
    assert!(totals[1] > 0, "non-vacuous: rows were written");
    assert!(totals[2] > 0, "non-vacuous: a repeat was excluded");
    assert!(
        totals[3] > 0,
        "non-vacuous: a not-new-trade packet was excluded"
    );
    assert!(
        totals[4] > 0,
        "non-vacuous: a not-a-trade packet was excluded"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Random frames of random quotes: the per-frame tally never differs from
    /// the per-tick counts, at the end of any frame.
    #[test]
    fn proptest_fix4_counts_equal_the_per_tick_oracle(
        frames in proptest::collection::vec(
            proptest::collection::vec(
                (
                    0_u32..6,
                    prop_oneof![
                        (-400_i64..-280),
                        Just(-86_400_i64),
                        Just(-172_800_i64),
                    ],
                    prop_oneof![Just(0_u32), 1_u32..60],
                    0_u8..8,
                ),
                1..6,
            ),
            1..40,
        )
    ) {
        let totals = run_drain_against_oracle(&frames);
        let packets: u64 = frames.iter().map(|f| f.len() as u64).sum();
        // Every row written is one `folded` (the fix-4 claim behind adding
        // `ingest_ticks` from the `folded` tally).
        prop_assert_eq!(totals[0], totals[1]);
        // Each packet is folded, written out of session (counted as folded
        // AND not a trade), or refused (not a trade): together they cover
        // every packet fed.
        prop_assert!(totals[0] + totals[4] >= packets);
        // A packet lands in at most one exclusion class, and the two folded
        // exclusions are folded packets.
        prop_assert!(totals[2] + totals[3] <= totals[0]);
        prop_assert!(totals[2] + totals[3] + totals[4] <= packets);
    }
}

/// The lag the drain hands to the worst-delay recorder is the one
/// `record_ws_lag` classified: the recorder over that value leaves the cells
/// exactly where the old two-computation path left them, for every edge
/// (lag 0, ±1 ms, the plausibility floor, garbage stamps).
#[test]
fn test_fix4_lag_computed_once_matches_the_two_computation_path() {
    use super::ws_lag_class::LagMaxByClass;
    let floor = tickvault_trading::candles::multi_tf_aggregator::MIN_PLAUSIBLE_EXCHANGE_TS_SECS;
    let stamps = [0_u32, 1, floor - 1, floor, floor + 1, SESSION_EPOCH_SECS];
    let codes = [
        ExchangeSegment::IdxI.binary_code(),
        ExchangeSegment::NseEquity.binary_code(),
        ExchangeSegment::NseFno.binary_code(),
        255,
    ];
    for &ts in &stamps {
        let base = (i64::from(ts) - 19_800) * 1_000_000_000;
        for delta_ms in [-2_000_i64, -1, 0, 1, 999, 1_000, 61_000] {
            for sub_ns in [0_i64, 1, 999_999] {
                let recv = base + delta_ms * 1_000_000 + sub_ns;
                for &code in &codes {
                    // Old: the max path worked the lag out itself.
                    let old_max = AtomicI64::new(0);
                    let old_class = LagMaxByClass::new();
                    if let Some(lag) = ws_lag_whole_ms(ts, recv)
                        && lag > 0
                    {
                        fetch_max_if_above(&old_max, lag);
                        old_class.note(code, lag);
                    }
                    // New: the value record_ws_lag computed, passed through.
                    let new_max = AtomicI64::new(0);
                    let new_class = LagMaxByClass::new();
                    let whole = ws_lag_whole_ms(ts, recv);
                    record_ws_lag_max_into(&new_max, &new_class, code, whole);
                    assert_eq!(
                        old_max.load(Ordering::Relaxed),
                        new_max.load(Ordering::Relaxed),
                        "ts {ts} recv {recv} code {code}"
                    );
                    for class in [
                        super::ws_lag_class::LagClass::IndexSpot,
                        super::ws_lag_class::LagClass::Fno,
                    ] {
                        assert_eq!(
                            old_class.take_published(class, 0).to_bits(),
                            new_class.take_published(class, 0).to_bits()
                        );
                    }
                    // The histogram classification of the same value is the
                    // one `ws_lag_ms` gives.
                    assert_eq!(ws_lag_from_whole_ms(whole), ws_lag_ms(ts, recv));
                }
            }
        }
    }
}

// ---- Source scans of the tally (moved from the parent's `tests` module,
// 2026-11-17, with `FrameTally` itself; same assertions, new file paths) ----

/// The tally's own source, for the scans of its methods and flush.
const TALLY_SRC: &str = include_str!("frame_tally.rs");
/// The drain's source, for the scans of the fold arm and the handle keys.
const PARENT_SRC: &str = include_str!("../dhan_feed_stack.rs");

#[test]
fn test_record_ws_lag_repeat_excluded_counts_a_repeated_quote_and_skips_the_lag_histogram() {
    // A repeat must land on the `ltt_not_advanced` exclusion counter, never
    // on the clamp or garbage counters. Since speed fix 4 (2026-11-17)
    // the drain counts it in `FrameTally` and adds it once per frame, so
    // this pins the tally's method, the frame-end flush into the
    // `ltt_not_advanced` handle, and that handle's key. The totals
    // themselves are checked against the old per-tick code by
    // `frame_tally_tests::test_fix4_counts_equal_the_per_tick_oracle_on_every_class`.
    let helper_src = TALLY_SRC;
    let helper = helper_src
        .find("fn exclude_repeat(&mut self) {")
        .map(|at| &helper_src[at..])
        .and_then(|tail| tail.find('}').map(|end| &tail[..end]))
        .expect("the repeat-exclusion tally method must exist");
    assert!(
        helper.contains("self.lag_repeat = self.lag_repeat.saturating_add(1);"),
        "a repeat must add one to the frame's repeat tally, got: {helper}"
    );
    let flush = helper_src
        .find("impl Drop for FrameTally<'_> {")
        .map(|at| &helper_src[at..])
        .and_then(|tail| tail.find("\n}\n").map(|end| &tail[..end]))
        .expect("FrameTally must flush on drop");
    assert!(
        flush.contains(
            ".lag_excluded_ltt_not_advanced\n                .increment(self.lag_repeat);"
        ),
        "the frame-end flush must add the repeat tally to `ltt_not_advanced`, got: {flush}"
    );
    assert!(
        PARENT_SRC.contains(
            "lag_excluded_ltt_not_advanced: metrics::counter!(\n            WS_LAG_EXCLUDED_COUNTER,\n            \"reason\" => \"ltt_not_advanced\""
        ),
        "the repeat handle must be the `ltt_not_advanced` series"
    );

    // Source-scan the drain: the lag is recorded AFTER the fold, and only
    // on the non-repeat arm. Recording before the fold (the pre-2026-09-23
    // shape) cannot know a packet is a repeat.
    let src = PARENT_SRC;
    let prod = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
    let fold = prod
        .find("let outcome = ingest.ingest_tick_at(tick, frame.seq, packets, recv_millis);")
        .expect("the drain's fold call must exist");
    let excluded = prod
        .find("=> tally.exclude_repeat(),")
        .expect("the drain must count repeats as excluded");
    let measured = prod
        .find("record_ws_lag(frame.connection_index, tick, received_at_nanos);")
        .expect("the drain must still record real lag");
    assert!(
        fold < excluded && fold < measured,
        "lag must be recorded after the fold, which is the only place a repeat is known"
    );
    assert_eq!(
        prod.matches("record_ws_lag(frame.connection_index").count(),
        1,
        "exactly one live lag-recording site"
    );
}

#[test]
fn test_record_ws_lag_not_a_trade_excluded_only_an_accepted_trade_is_a_lag_sample() {
    // 2026-10-05: out-of-session ticks (a contract not traded today,
    // carrying an earlier day's trade time) went into the lag histogram
    // and read as option sockets delivering most packets over 60 s. The
    // drain must record lag on the accepted-trade arm only and count
    // every other outcome on its own exclusion reason. Since speed fix 4
    // (2026-11-17) the count goes through the frame's `FrameTally`.
    let src = PARENT_SRC;
    let tally_src = TALLY_SRC;
    let helper = tally_src
        .find("fn exclude_not_a_trade(&mut self) {")
        .map(|at| &tally_src[at..])
        .and_then(|tail| tail.find('}').map(|end| &tail[..end]))
        .expect("the not-a-trade exclusion tally method must exist");
    assert!(
        helper.contains("self.lag_not_a_trade = self.lag_not_a_trade.saturating_add(1);"),
        "a refused or out-of-session tick must add one to the not_a_trade tally, got: {helper}"
    );
    let flush = tally_src
        .find("impl Drop for FrameTally<'_> {")
        .map(|at| &tally_src[at..])
        .and_then(|tail| tail.find("\n}\n").map(|end| &tail[..end]))
        .expect("FrameTally must flush on drop");
    assert!(
        flush.contains(
            ".lag_excluded_not_a_trade\n                .increment(self.lag_not_a_trade);"
        ) && flush.contains(
            ".lag_excluded_not_new_trade\n                .increment(self.lag_not_new_trade);"
        ),
        "the frame-end flush must add both tallies to their own series, got: {flush}"
    );

    let prod = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
    let fold = prod
        .find("let outcome = ingest.ingest_tick_at(tick, frame.seq, packets, recv_millis);")
        .expect("the drain's fold call must exist");
    let arm = &prod[fold..];
    let folded_arm = arm
        .find("IngestOutcome::Folded { .. } => {")
        .expect("lag must be recorded inside the accepted-trade arm");
    let measured = arm
        .find("record_ws_lag(frame.connection_index, tick, received_at_nanos);")
        .expect("the drain must still record real lag");
    let other = arm
        .find("_ => tally.exclude_not_a_trade(),")
        .expect("every other outcome must be counted as not a trade");
    assert!(
        folded_arm < measured && measured < other,
        "the lag sample must sit inside the Folded arm, before the catch-all"
    );
    // Plan PR AL-1 (2026-11-06): a folded packet that is not a new trade
    // is excluded BEFORE the arm that records lag.
    let not_advanced = arm
        .find("advanced: false, ..")
        .expect("a not-advanced fold must be excluded from the lag sample");
    assert!(
        not_advanced < folded_arm,
        "the not-advanced exclusion must come before the recording arm"
    );
    assert!(
        src.contains("\"reason\" => \"not_a_trade\""),
        "the exclusion reason label must be registered"
    );
    // Review 2026-11-06: a not-NEW-trade packet is counted under its own
    // reason, so the 2026-10-05 `not_a_trade` series keeps its meaning.
    let after: String = arm[not_advanced..].chars().take(120).collect();
    assert!(
        after.contains("tally.exclude_not_new_trade()"),
        "the not-advanced arm must count `not_new_trade`"
    );
    assert!(src.contains("\"reason\" => \"not_new_trade\""));
}
