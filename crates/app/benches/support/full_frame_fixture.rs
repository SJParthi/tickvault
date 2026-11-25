//! Shared unchanged four-Full-packet workload and frame-sample bookkeeping.
//!
//! Benchmarks and ordinary tests use this same fixture. All helpers are test/
//! benchmark support only; no live encoder setting or production API changes.

use std::time::Instant;
use tickvault_app::dhan_feed_stack::{
    DepthIngest, DrainCounters, FrameOutcome, LiveIngest, drain_main_feed_frame,
};
use tickvault_common::constants::{
    DEPTH_LEVEL_OFFSET_ASK_ORDERS, DEPTH_LEVEL_OFFSET_ASK_PRICE, DEPTH_LEVEL_OFFSET_ASK_QTY,
    DEPTH_LEVEL_OFFSET_BID_ORDERS, DEPTH_LEVEL_OFFSET_BID_PRICE, DEPTH_LEVEL_OFFSET_BID_QTY,
    FULL_OFFSET_CLOSE, FULL_OFFSET_DEPTH_START, FULL_OFFSET_HIGH, FULL_OFFSET_LOW, FULL_OFFSET_OI,
    FULL_OFFSET_OI_DAY_HIGH, FULL_OFFSET_OI_DAY_LOW, FULL_OFFSET_OPEN, FULL_QUOTE_PACKET_SIZE,
    MARKET_DEPTH_LEVEL_SIZE, MARKET_DEPTH_LEVELS, QUOTE_OFFSET_ATP, QUOTE_OFFSET_LTP,
    QUOTE_OFFSET_LTQ, QUOTE_OFFSET_LTT, QUOTE_OFFSET_TOTAL_BUY_QTY, QUOTE_OFFSET_TOTAL_SELL_QTY,
    QUOTE_OFFSET_VOLUME, RESPONSE_CODE_FULL,
};
use tickvault_common::feed::Feed;
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::pool_supervisor::CapturedFrame;
use tickvault_storage::tick_ilp::TickIlpEncoder;
use tickvault_storage::tick_persistence::TickWriter;

/// Number of sequential historical frames in one fresh ingest's pool.
pub const FRAME_POOL: u64 = 1_024;
/// Retained legacy fixture's IST epoch seconds (2026-08-15 13:20).
/// This historical workload predates tick_quotes routing; it is not a dated
/// production-session/calendar proof.
pub const SESSION_EPOCH_SECS: u32 = 1_786_800_000;
/// UTC receipt corresponding to the same fixed 13:20 IST instant.
pub const SESSION_RECEIPT_NANOS: i64 = (SESSION_EPOCH_SECS as i64 - 19_800) * 1_000_000_000;
/// Full packets per synthetic frame; a denominator, not a measured tick tail.
pub const TICKS_PER_MAIN_FRAME: u64 = 4;

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

/// One 162-byte Full packet in the wire layout `parser/full_packet.rs` reads,
/// with every field set to a non-zero value. Prices move with `n` so no
/// conversion is fed the same bits on consecutive frames; volume and the
/// trade time rise like `quote_packet`'s.
fn full_packet(security_id: u32, n: u64, ltt: u32, volume: u32) -> Vec<u8> {
    fn put_f32(b: &mut [u8], at: usize, v: f32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u16(b: &mut [u8], at: usize, v: u16) {
        b[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    let mut buf = vec![0u8; FULL_QUOTE_PACKET_SIZE];
    buf[0] = RESPONSE_CODE_FULL;
    put_u16(
        &mut buf,
        1,
        u16::try_from(FULL_QUOTE_PACKET_SIZE).unwrap_or(u16::MAX),
    );
    buf[3] = 2; // NSE_FNO, the segment live option ticks carry
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    let step = (n % 97) as f32 * 0.05;
    let ltp = 245.35_f32 + step;
    put_f32(&mut buf, QUOTE_OFFSET_LTP, ltp);
    put_u16(&mut buf, QUOTE_OFFSET_LTQ, 75);
    put_u32(&mut buf, QUOTE_OFFSET_LTT, ltt);
    put_f32(&mut buf, QUOTE_OFFSET_ATP, ltp - 1.15);
    put_u32(&mut buf, QUOTE_OFFSET_VOLUME, volume);
    put_u32(&mut buf, QUOTE_OFFSET_TOTAL_SELL_QTY, 123_450 + volume);
    put_u32(&mut buf, QUOTE_OFFSET_TOTAL_BUY_QTY, 98_760 + volume);
    put_u32(&mut buf, FULL_OFFSET_OI, 1_500_000);
    put_u32(&mut buf, FULL_OFFSET_OI_DAY_HIGH, 1_600_000);
    put_u32(&mut buf, FULL_OFFSET_OI_DAY_LOW, 1_400_000);
    put_f32(&mut buf, FULL_OFFSET_OPEN, 240.10 + step);
    put_f32(&mut buf, FULL_OFFSET_CLOSE, 238.95);
    put_f32(&mut buf, FULL_OFFSET_HIGH, 251.80 + step);
    put_f32(&mut buf, FULL_OFFSET_LOW, 236.25 + step);
    for level in 0..MARKET_DEPTH_LEVELS {
        let base = FULL_OFFSET_DEPTH_START + level * MARKET_DEPTH_LEVEL_SIZE;
        let k = u16::try_from(level).unwrap_or(0);
        let tick = level as f32 * 0.05;
        put_u32(
            &mut buf,
            base + DEPTH_LEVEL_OFFSET_BID_QTY,
            75 * (u32::from(k) + 1),
        );
        put_u32(
            &mut buf,
            base + DEPTH_LEVEL_OFFSET_ASK_QTY,
            150 * (u32::from(k) + 1),
        );
        put_u16(&mut buf, base + DEPTH_LEVEL_OFFSET_BID_ORDERS, 2 * k + 1);
        put_u16(&mut buf, base + DEPTH_LEVEL_OFFSET_ASK_ORDERS, 3 * k + 1);
        put_f32(
            &mut buf,
            base + DEPTH_LEVEL_OFFSET_BID_PRICE,
            ltp - 0.05 - tick,
        );
        put_f32(
            &mut buf,
            base + DEPTH_LEVEL_OFFSET_ASK_PRICE,
            ltp + 0.05 + tick,
        );
    }
    buf
}

pub fn full_feed_frame(n: u64) -> CapturedFrame {
    let mut bytes = Vec::with_capacity(4 * FULL_QUOTE_PACKET_SIZE);
    for i in 0..4u32 {
        bytes.extend_from_slice(&full_packet(
            50_001 + i,
            n,
            // Same rising, never-ahead trade time as `main_feed_frame`.
            SESSION_EPOCH_SECS - 300 + (n / 4) as u32,
            2 + n as u32,
        ));
    }
    captured(bytes, DhanEndpointType::MainFeed, 1_000 + n)
}

/// The Full bench's ingest: inline depth in array-row mode (`since` = 1 ns,
/// so every frame takes the book path), as production boots it.
pub fn full_ingest(encoder: TickIlpEncoder) -> LiveIngest {
    LiveIngest::new(TickWriter::for_test_with_encoder(Feed::Dhan, encoder), 8)
        .with_inline_depth(DepthIngest::for_test_book(1))
}

/// Non-vacuity check for the Full bench, outside any timing: every packet
/// of one pool must fold and store all ten depth levels, or the bench would
/// time a refusal path while reading like the production one.
pub fn assert_full_frames_fold(
    frames: &[CapturedFrame],
    encoder: TickIlpEncoder,
    drain_counters: &'static DrainCounters,
) {
    assert!(!frames.is_empty(), "Full fixture must not be empty");
    let mut ingest = full_ingest(encoder);
    let (mut folded, mut levels) = (0u64, 0u64);
    for (i, frame) in (0u64..).zip(frames) {
        let out = drain_main_feed_frame(
            &mut ingest,
            frame,
            SESSION_RECEIPT_NANOS,
            1_000 + i,
            drain_counters,
        );
        folded += out.folded;
        levels += out.inline_depth_rows;
    }
    let packets = TICKS_PER_MAIN_FRAME * frames.len() as u64;
    assert_eq!(folded, packets, "every Full packet must fold");
    assert_eq!(
        levels,
        packets * MARKET_DEPTH_LEVELS as u64 * 2,
        "every Full packet must store its ten depth levels"
    );
    assert_eq!(
        ingest.pending_rows(),
        packets,
        "every tick reached its writer"
    );
    assert_eq!(
        ingest.depth_pending_rows() as u64,
        levels,
        "depth bytes are pending"
    );
    assert_eq!(
        ingest.refusals(),
        (0, 0, 0, 0, 0, 0, 0),
        "no fold refusal path"
    );
}

/// Collect exactly one undivided duration for each complete Full-frame call.
/// The caller supplies the measured call, so ordinary tests can drive real
/// drains with deterministic durations instead of relying on wall-clock speed.
/// Allocation, ingest reconstruction and validation are outside that callback.
pub fn sample_full_frames(
    frames: &[CapturedFrame],
    encoder: TickIlpEncoder,
    sample_count: usize,
    mut measured_drain: impl FnMut(&mut LiveIngest, &CapturedFrame, u64) -> (FrameOutcome, u64),
) -> Result<Vec<u64>, &'static str> {
    if frames.is_empty() || sample_count == 0 {
        return Err("Full-frame sampling requires nonempty frames and samples");
    }
    let mut samples = Vec::with_capacity(sample_count);
    while samples.len() < sample_count {
        let mut ingest = full_ingest(encoder);
        let batch = (sample_count - samples.len()).min(frames.len());
        for frame in frames.iter().take(batch) {
            let sample_index = samples.len() as u64;
            let (outcome, duration_ns) = measured_drain(&mut ingest, frame, sample_index);
            if outcome.folded != TICKS_PER_MAIN_FRAME
                || outcome.inline_depth_rows != TICKS_PER_MAIN_FRAME * 10
            {
                return Err("Full-frame sample did not produce four ticks and forty depth levels");
            }
            samples.push(duration_ns);
        }
        if ingest.pending_rows() != batch as u64 * TICKS_PER_MAIN_FRAME
            || ingest.depth_pending_rows() as u64 != batch as u64 * TICKS_PER_MAIN_FRAME * 10
            || ingest.refusals() != (0, 0, 0, 0, 0, 0, 0)
        {
            return Err("Full-frame sample batch took a refusal or omitted its pending rows");
        }
    }
    Ok(samples)
}

/// Raw per-FRAME nearest-rank tail summary. Dividing it by four yields only
/// an amortized frame-cost estimate, never an individual-tick percentile.
#[derive(Debug, PartialEq, Eq)]
pub struct FrameTailSummary {
    /// Number of complete frame calls observed.
    pub samples: usize,
    /// Median frame duration, including the measurement clock pair.
    pub p50_ns: u64,
    /// Nearest-rank 99th percentile of complete frame calls.
    pub p99_ns: u64,
    /// Nearest-rank 99.9th percentile of complete frame calls.
    pub p999_ns: u64,
    /// Largest observed frame duration; not a universal maximum.
    pub max_ns: u64,
}

impl FrameTailSummary {
    /// Sorts after collection and returns no verdict for an empty sample set.
    pub fn from_samples(samples: &mut [u64]) -> Option<Self> {
        use tickvault_app::drain_tail_gate::quantile_of_sorted;
        samples.sort_unstable();
        Some(Self {
            samples: samples.len(),
            p50_ns: quantile_of_sorted(samples, 1, 2)?,
            p99_ns: quantile_of_sorted(samples, 99, 100)?,
            p999_ns: quantile_of_sorted(samples, 999, 1_000)?,
            max_ns: samples.last().copied()?,
        })
    }

    /// The user's strict target for this measured frame-call unit. This is
    /// unscaled and equality is a failure; the legacy amortized budget is a
    /// separate gate with its existing inclusive/multiplier semantics.
    pub const fn p99_below_one_microsecond(&self) -> bool {
        self.p99_ns < 1_000
    }
}
