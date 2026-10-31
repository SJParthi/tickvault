//! Full-frame benchmark evidence contracts, with deterministic duration inputs.
//! These tests run real drains but make no wall-clock performance assertion.

#[path = "../benches/support/full_frame_fixture.rs"]
mod full_frame_fixture;

use full_frame_fixture::{
    FRAME_POOL, FrameTailSummary, SESSION_RECEIPT_NANOS, TICKS_PER_MAIN_FRAME,
    assert_full_frames_fold, full_feed_frame, sample_full_frames,
};
use tickvault_app::dhan_feed_stack::{FrameOutcome, counters, drain_main_feed_frame};
use tickvault_app::drain_tail_gate;
use tickvault_common::constants::FULL_QUOTE_PACKET_SIZE;
use tickvault_core::parser::{ParsedFrame, dispatch_frame_uncounted};
use tickvault_storage::tick_ilp::TickIlpEncoder;

#[test]
fn test_full_frame_fixture_keeps_four_full_packets_and_all_depth_slots() {
    for n in [0, 1, 96, 97, FRAME_POOL - 1] {
        let frame = full_feed_frame(n);
        assert_eq!(frame.bytes.len(), 4 * FULL_QUOTE_PACKET_SIZE);
        let mut packets = 0;
        for (index, raw) in frame.bytes.chunks_exact(FULL_QUOTE_PACKET_SIZE).enumerate() {
            let parsed =
                dispatch_frame_uncounted(raw, SESSION_RECEIPT_NANOS).expect("complete Full packet");
            let ParsedFrame::TickWithDepth(tick, depth) = parsed else {
                panic!("the Full workload must exercise inline depth");
            };
            assert_eq!(tick.security_id, 50_001 + index as u64);
            assert_eq!(tick.exchange_segment_code, 2);
            assert_eq!(tick.last_trade_quantity, 75);
            assert_eq!(tick.volume, 2 + n as u32);
            assert_eq!(tick.open_interest, 1_500_000);
            assert!(tick.last_traded_price.is_finite() && tick.last_traded_price > 0.0);
            for (level, row) in depth.iter().enumerate() {
                assert_eq!(row.bid_quantity, 75 * (level as u32 + 1));
                assert_eq!(row.ask_quantity, 150 * (level as u32 + 1));
                assert_eq!(row.bid_orders, 2 * level as u16 + 1);
                assert_eq!(row.ask_orders, 3 * level as u16 + 1);
                assert!(row.bid_price.is_finite() && row.ask_price.is_finite());
            }
            packets += 1;
        }
        assert_eq!(packets, 4);
    }
}

#[test]
fn test_full_frame_both_encoders_produce_every_tick_and_depth_level() {
    let frames: Vec<_> = (0..FRAME_POOL).map(full_feed_frame).collect();
    for encoder in [TickIlpEncoder::Library, TickIlpEncoder::Own] {
        assert_full_frames_fold(&frames, encoder, counters());
    }
}

#[test]
fn test_full_frame_sampler_records_exactly_one_undivided_sample_per_call() {
    let frames: Vec<_> = (0..8).map(full_feed_frame).collect();
    for encoder in [TickIlpEncoder::Library, TickIlpEncoder::Own] {
        for wanted in [1, 7, 8, 9, 99, 100, 101] {
            let mut calls = 0_u64;
            let samples = sample_full_frames(&frames, encoder, wanted, |ingest, frame, index| {
                assert_eq!(index, calls);
                calls += 1;
                let outcome = drain_main_feed_frame(
                    ingest,
                    frame,
                    SESSION_RECEIPT_NANOS,
                    1_000 + index,
                    counters(),
                );
                // Deliberately not divisible by four: a division in collection
                // must fail this oracle rather than silently change the unit.
                (outcome, 1_001 + index)
            })
            .expect("real Full-frame drains");
            assert_eq!(calls, wanted as u64);
            assert_eq!(
                samples,
                (0..wanted as u64).map(|i| 1_001 + i).collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn test_full_frame_sampler_refuses_empty_and_zero_sample_workloads() {
    let frames = [full_feed_frame(0)];
    for (input, count) in [(&frames[..], 0), (&[][..], 1)] {
        let result = sample_full_frames(input, TickIlpEncoder::Library, count, |_, _, _| {
            panic!("an empty workload must never call the measured path");
        });
        assert!(result.is_err());
    }
    assert!(FrameTailSummary::from_samples(&mut []).is_none());
}

#[test]
fn test_full_frame_sampler_rejects_a_truncated_frame_and_fabricated_counts() {
    let mut truncated = full_feed_frame(0);
    truncated.bytes = truncated.bytes[..truncated.bytes.len() - 1].to_vec().into();
    let result = sample_full_frames(
        &[truncated],
        TickIlpEncoder::Library,
        1,
        |ingest, frame, i| {
            (
                drain_main_feed_frame(ingest, frame, SESSION_RECEIPT_NANOS, 1_000 + i, counters()),
                17,
            )
        },
    );
    assert!(
        result.is_err(),
        "three complete packets cannot count as four"
    );
    let result = sample_full_frames(&[full_feed_frame(0)], TickIlpEncoder::Own, 1, |_, _, _| {
        (
            FrameOutcome {
                folded: 4,
                inline_depth_rows: 40,
                ..FrameOutcome::default()
            },
            17,
        )
    });
    assert!(
        result.is_err(),
        "counts without pending writer rows cannot pass"
    );
}

#[test]
fn test_full_frame_tail_nearest_rank_and_strict_equality_boundary() {
    for (count, slow, strict) in [
        (1, 0, true),
        (1, 1, false),
        (99, 0, true),
        (99, 1, false),
        (100, 1, true),
        (100, 2, false),
        (101, 1, true),
        (101, 2, false),
        (200, 2, true),
        (200, 3, false),
    ] {
        let mut samples = vec![999; count];
        samples[..slow].fill(1_000);
        let summary = FrameTailSummary::from_samples(&mut samples).expect("nonempty tail");
        assert_eq!(summary.samples, count);
        assert_eq!(
            summary.p99_below_one_microsecond(),
            strict,
            "N={count}, slow={slow}"
        );
        assert_eq!(summary.p99_ns, if strict { 999 } else { 1_000 });
        assert_eq!(summary.max_ns, if slow == 0 { 999 } else { 1_000 });
    }
    for value in [0, 999, 1_000, 1_001, 1_024, 1_025, u64::MAX] {
        let summary = FrameTailSummary::from_samples(&mut [value]).expect("one frame");
        assert_eq!(summary.p50_ns, value);
        assert_eq!(summary.p999_ns, value);
        assert_eq!(summary.max_ns, value);
        assert_eq!(summary.p99_below_one_microsecond(), value < 1_000);
    }
}

#[test]
fn test_full_frame_amortized_budget_pass_does_not_mean_strict_frame_pass() {
    let mut samples = [2_800; 100];
    let summary = FrameTailSummary::from_samples(&mut samples).expect("frame durations");
    let amortized = drain_tail_gate::tail_verdict(&samples, TICKS_PER_MAIN_FRAME, 1_000, 1.0)
        .expect("legacy cost verdict");
    assert_eq!(summary.p99_ns, 2_800);
    assert!(!summary.p99_below_one_microsecond());
    assert_eq!(amortized.p99_ns_per_tick, 700.0);
    assert!(amortized.within);
}

#[test]
fn test_full_frame_comparison_keeps_both_encoder_call_sites_and_default() {
    let bench = include_str!("../benches/feed_drain.rs");
    let library = bench
        .split_once("c.bench_function(\"feed_drain/main_feed_frame_4_full\"")
        .expect("original Library benchmark")
        .1
        .split_once("c.bench_function(")
        .expect("next comparison")
        .0;
    assert!(library.contains("full_ingest(TickIlpEncoder::Library)"));
    let own = bench
        .split_once("c.bench_function(\"feed_drain/main_feed_frame_4_full_own\"")
        .expect("Own comparison")
        .1
        .split_once("let depth20_frames")
        .expect("preserved next case")
        .0;
    assert!(own.contains("full_ingest(TickIlpEncoder::Own)"));
    let tails = bench
        .split_once("fn full_feed_drain_tail_gates()")
        .expect("Full-frame tails")
        .1
        .split_once("let mut samples =")
        .expect("tail sampler follows the encoder tuples")
        .0;
    assert!(tails.contains("(\"library\", TickIlpEncoder::Library)"));
    assert!(tails.contains("(\"own\", TickIlpEncoder::Own)"));
    let base = include_str!("../../../config/base.toml");
    let tick_storage = base
        .split_once("[tick_storage]")
        .expect("existing setting")
        .1;
    let tick_storage = tick_storage.split("\n[").next().expect("section body");
    assert!(tick_storage.contains("own_ilp_encoder = false"));
}
