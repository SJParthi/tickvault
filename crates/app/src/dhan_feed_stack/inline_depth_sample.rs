//! The sampled `drain_inline_depth` hot-path stage (2026-11-17, plan ITEM DT
//! "PR 0, measure first"). A MEASUREMENT, never a speed-up.
//!
//! `drain_main_tick` reads 2,104 ns mean live, and a production-shape harness
//! attributed about 400 to 500 ns of a Full tick to the inline depth append.
//! This times that append on the live drain for 1 in
//! [`INLINE_DEPTH_SAMPLE_EVERY`] Full packets that reach the call (a packet
//! whose depth is shed never does), so the share can be read from the minute
//! table instead of assumed.
//!
//! Cost per packet: one counter add and one branch; the clock is read (twice)
//! only on a sampled packet, which then also pays the stage's four relaxed
//! atomics. The counter is a plain `u32` on the single-owner [`LiveIngest`]
//! (`&mut`, the drain task only), so no atomic is added per tick. No
//! allocation (DHAT: `crates/app/tests/dhat_live_ingest_seam.rs`, both Full
//! gates). A sampled packet's time is already inside its frame's
//! `drain_main_frame` and `drain_main_tick`; the two clock reads it adds
//! (Assumed about 20 to 40 ns) land in those stages for that one packet.
//!
//! [`LiveIngest`]: super::LiveIngest

use std::time::{Duration, Instant};

use tickvault_storage::hot_path_telemetry::{Stage, record_stage};

/// One Full packet in this many has its inline depth timed. A power of two,
/// so the sample check is a mask and a `u32` wrap keeps the cadence.
pub(super) const INLINE_DEPTH_SAMPLE_EVERY: u32 = 64;

const SAMPLE_MASK: u32 = INLINE_DEPTH_SAMPLE_EVERY - 1;
const _: () = assert!(INLINE_DEPTH_SAMPLE_EVERY.is_power_of_two());

/// Counts the inline depth calls and times every 64th.
#[derive(Debug, Default)]
pub(super) struct InlineDepthSampler {
    /// Inline depth calls seen; wraps (2^32 is a multiple of 64).
    calls: u32,
}

impl InlineDepthSampler {
    /// Runs `append` (the inline depth append) and, on every 64th call,
    /// records its time on [`Stage::DrainInlineDepth`]. Only `append` is
    /// inside the clock. O(1); no allocation.
    #[inline]
    pub(super) fn time<R>(&mut self, append: impl FnOnce() -> R) -> R {
        self.time_with(append, |elapsed| {
            record_stage(Stage::DrainInlineDepth, elapsed);
        })
    }

    /// [`Self::time`] with the recorder injected, so a test can count the
    /// samples exactly (the stage itself is a process-wide static).
    #[inline]
    fn time_with<R>(&mut self, append: impl FnOnce() -> R, record: impl FnOnce(Duration)) -> R {
        self.calls = self.calls.wrapping_add(1);
        if self.calls & SAMPLE_MASK != 0 {
            return append();
        }
        let start = Instant::now();
        let out = append();
        record(start.elapsed());
        out
    }

    /// Inline depth calls seen so far (wraps at 2^32).
    #[cfg(test)]
    pub(super) fn calls(&self) -> u32 {
        self.calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dhan_feed_stack::{
        CapturedFrame, DepthIngest, DhanEndpointType, LiveIngest, TickWriter, counters,
        drain_main_feed_frame,
    };
    use std::cell::Cell;
    use tickvault_common::constants as k;
    use tickvault_common::feed::Feed;
    use tickvault_storage::hot_path_telemetry::stage_snapshot;

    /// An exchange time inside a session (epoch seconds), as the DHAT gates.
    const SESSION_EPOCH_SECS: u32 = 1_786_800_000;
    /// The matching receipt (UTC nanos, 13:20:00 IST).
    const SESSION_RECEIPT_NANOS: i64 = (SESSION_EPOCH_SECS as i64 - 19_800) * 1_000_000_000;

    #[test]
    fn test_exactly_one_in_64_calls_is_recorded() {
        let mut s = InlineDepthSampler::default();
        let recorded = Cell::new(0u32);
        let mut sampled_at = Vec::new();
        for n in 1..=(INLINE_DEPTH_SAMPLE_EVERY * 5 + 63) {
            let before = recorded.get();
            let out = s.time_with(|| n * 2, |_| recorded.set(recorded.get() + 1));
            assert_eq!(out, n * 2, "the append's result passes through");
            if recorded.get() != before {
                sampled_at.push(n);
            }
        }
        assert_eq!(recorded.get(), 5);
        assert_eq!(sampled_at, vec![64, 128, 192, 256, 320]);
        assert_eq!(s.calls(), INLINE_DEPTH_SAMPLE_EVERY * 5 + 63);
    }

    #[test]
    fn test_cadence_survives_the_u32_wrap() {
        let mut s = InlineDepthSampler {
            calls: u32::MAX - 63,
        };
        let recorded = Cell::new(0u32);
        for _ in 0..(INLINE_DEPTH_SAMPLE_EVERY * 3) {
            s.time_with(|| (), |_| recorded.set(recorded.get() + 1));
        }
        assert_eq!(recorded.get(), 3);
    }

    /// The clock covers the append and nothing before or after it: work done
    /// before the call (20 ms) is not in the sample, the append's own 3 ms is,
    /// and the record runs after the append returned.
    #[test]
    fn test_only_the_append_is_inside_the_clock() {
        let mut s = InlineDepthSampler {
            calls: INLINE_DEPTH_SAMPLE_EVERY - 1,
        };
        std::thread::sleep(Duration::from_millis(20));
        let appended = Cell::new(false);
        let sample = Cell::new(None);
        s.time_with(
            || {
                std::thread::sleep(Duration::from_millis(3));
                appended.set(true);
            },
            |elapsed| {
                assert!(appended.get(), "recorded after the append returned");
                sample.set(Some(elapsed));
            },
        );
        let elapsed = sample.get().expect("the 64th call is sampled");
        assert!(elapsed >= Duration::from_millis(3), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(18), "{elapsed:?}");
        // The 65th call is not sampled.
        s.time_with(|| (), |_| panic!("the 65th call must not be sampled"));
    }

    fn full_packet(security_id: u32, ltp: f32, ltt: u32) -> Vec<u8> {
        let mut buf = vec![0u8; k::FULL_QUOTE_PACKET_SIZE];
        buf[0] = k::RESPONSE_CODE_FULL;
        buf[1..3].copy_from_slice(&(k::FULL_QUOTE_PACKET_SIZE as u16).to_le_bytes());
        buf[3] = 1; // NSE_EQ
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());
        buf[k::QUOTE_OFFSET_LTP..k::QUOTE_OFFSET_LTP + 4].copy_from_slice(&ltp.to_le_bytes());
        buf[k::QUOTE_OFFSET_LTT..k::QUOTE_OFFSET_LTT + 4].copy_from_slice(&ltt.to_le_bytes());
        for level in 0..k::MARKET_DEPTH_LEVELS {
            let base = k::FULL_OFFSET_DEPTH_START + level * k::MARKET_DEPTH_LEVEL_SIZE;
            let step = level as u32 + 1;
            buf[base..base + 4].copy_from_slice(&(100u32 * step).to_le_bytes());
            buf[base + 4..base + 8].copy_from_slice(&(200u32 * step).to_le_bytes());
        }
        buf
    }

    /// Four Full packets in one live main-feed frame.
    fn full_frame(seq: u64, n: u64) -> CapturedFrame {
        let mut bytes = Vec::with_capacity(4 * k::FULL_QUOTE_PACKET_SIZE);
        for i in 0..4u32 {
            bytes.extend_from_slice(&full_packet(
                7_000 + i,
                100.0 + (n % 97) as f32 * 0.05,
                SESSION_EPOCH_SECS + (n % 600) as u32,
            ));
        }
        CapturedFrame {
            seq,
            endpoint: DhanEndpointType::MainFeed,
            connection_index: 0,
            received_at: std::time::Instant::now(),
            received_at_nanos: tickvault_storage::ws_frame_spill::receipt_nanos_from(
                std::time::Instant::now(),
            ),
            wal_backed: true,
            bytes: bytes.into(),
        }
    }

    /// The live drain counts every Full packet that reaches the inline depth
    /// append, and the stage gains at least one sample per 64 of them (the
    /// stage is process-wide, so other tests can only add to the delta).
    #[test]
    fn test_drain_samples_one_in_64_full_packets_with_inline_depth() {
        const FRAMES: u64 = 64; // x4 packets = 256 Full packets
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8)
            .with_inline_depth(DepthIngest::for_test());
        let before = stage_snapshot(Stage::DrainInlineDepth);
        let mut depth_rows = 0u64;
        for n in 0..FRAMES {
            let out = drain_main_feed_frame(
                &mut ingest,
                &full_frame(500 + n, n),
                SESSION_RECEIPT_NANOS,
                1_000,
                counters(),
            );
            depth_rows += out.inline_depth_rows;
        }
        let after = stage_snapshot(Stage::DrainInlineDepth);
        assert_eq!(depth_rows, FRAMES * 4 * 10, "every packet wrote its depth");
        assert_eq!(ingest.inline_depth_sample.calls(), 256);
        assert!(after.count - before.count >= 256 / u64::from(INLINE_DEPTH_SAMPLE_EVERY));
    }

    /// Without an inline depth sink the append is never called, so nothing is
    /// counted or timed: the clock sits only around the depth call.
    #[test]
    fn test_no_inline_depth_sink_counts_nothing() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
        for n in 0..32u64 {
            let out = drain_main_feed_frame(
                &mut ingest,
                &full_frame(900 + n, n),
                SESSION_RECEIPT_NANOS,
                1_000,
                counters(),
            );
            assert_eq!(out.inline_depth_rows, 0);
        }
        assert_eq!(ingest.inline_depth_sample.calls(), 0);
    }
}
