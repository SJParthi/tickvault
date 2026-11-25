//! The sampled `drain_inline_depth` hot-path stage (2026-11-17, plan ITEM DT
//! "PR 0, measure first"). A MEASUREMENT, never a speed-up.
//!
//! `drain_main_tick` reads 2,104 ns mean live, and a production-shape harness
//! attributed about 400 to 500 ns of a Full tick to the inline depth append.
//! This times that append on the live drain for about 1 in
//! [`INLINE_DEPTH_SAMPLE_EVERY`] calls, so the share can be read from the
//! minute table instead of assumed.
//!
//! WHICH calls: every Full packet that reaches `append_inline_depth`, refused
//! ones included (an unmappable segment, a `security_id` too large, a
//! `capture_seq` refusal, or rows outside the depth arrival window return 0
//! rows in a few nanoseconds and can be sampled). A packet whose depth is
//! shed never reaches the call and is not counted.
//!
//! WHEN: the gap to the next sample is 32 + (6 bits of a xorshift32 step),
//! 32..=95 calls, mean about 63.5 (measured 63.76 over 1,000,000 calls). A
//! fixed every-64th cadence is phase-locked to the frame shape: with 1, 2, 4,
//! 8, 16, 32 or 64 depth packets per frame it always picked the same position
//! in the frame (the last of a 4-packet frame, warm caches), which biases the
//! stage. The jitter spreads the samples over the positions (4-packet frames,
//! 1,000,000 calls: 3,924 / 3,895 / 3,891 / 3,973). The sequence is fixed (a
//! constant seed), so a test can count it exactly; it is not random.
//!
//! Cost per call: one counter add, one compare and one branch; the clock is
//! read (twice) only on a sampled call, in a `#[cold]` function that also pays
//! the xorshift step and the stage's four relaxed atomics. The state is plain
//! `u32`s on the single-owner [`LiveIngest`] (`&mut`, the drain task only), so
//! no atomic is added per tick. No allocation (DHAT:
//! `crates/app/tests/dhat_live_ingest_seam.rs`, both Full gates). A sampled
//! packet's time is already inside its frame's `drain_main_frame` and
//! `drain_main_tick`; the two clock reads it adds (Assumed about 20 to 40 ns)
//! land in those stages for that one packet.
//!
//! What a sample can include besides staging (rare): the ninth contract-name
//! table in one batch makes the append hand the batch off
//! (`DepthWriter::stage_pin_after_cut`), possibly reserving a fresh batch, or
//! rescue it when the queue is full (an inline spill write); the unsplit
//! synchronous writer flushes over HTTP. Read the median as the staging cost;
//! the max and p99.9 can hold one of those.
//!
//! [`LiveIngest`]: super::LiveIngest

use std::time::{Duration, Instant};

use tickvault_storage::hot_path_telemetry::{Stage, record_stage};

/// The mean gap between samples, about this many calls (32..=95, mean 63.5).
pub(super) const INLINE_DEPTH_SAMPLE_EVERY: u32 = 64;

/// Shortest gap; the gap is this plus `GAP_JITTER_MASK & xorshift`.
const GAP_MIN: u32 = INLINE_DEPTH_SAMPLE_EVERY / 2;
const GAP_JITTER_MASK: u32 = INLINE_DEPTH_SAMPLE_EVERY - 1;
const _: () = assert!(INLINE_DEPTH_SAMPLE_EVERY.is_power_of_two());
/// The first sampled call (the gaps after it are jittered).
const FIRST_SAMPLE_AT: u32 = INLINE_DEPTH_SAMPLE_EVERY;
/// Non-zero xorshift32 seed (a fixed sequence, so tests can count it).
const JITTER_SEED: u32 = 0x9E37_79B9;

/// xorshift32 (Marsaglia), one step. Never returns 0 for a non-zero input.
#[inline]
fn xorshift32(mut x: u32) -> u32 {
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}

/// Counts the inline depth calls and times about one in 64, at jittered gaps.
#[derive(Debug)]
pub(super) struct InlineDepthSampler {
    /// Inline depth calls seen; wraps (compared with `wrapping` arithmetic).
    calls: u32,
    /// The call number of the next sample.
    next_at: u32,
    /// xorshift32 state for the next gap.
    rng: u32,
}

impl Default for InlineDepthSampler {
    fn default() -> Self {
        Self {
            calls: 0,
            next_at: FIRST_SAMPLE_AT,
            rng: JITTER_SEED,
        }
    }
}

impl InlineDepthSampler {
    /// Runs `append` (the inline depth append) and, on a sampled call,
    /// records its time on [`Stage::DrainInlineDepth`]. Only `append` is
    /// inside the clock. O(1); no allocation.
    #[inline]
    pub(super) fn time<R>(&mut self, append: impl FnOnce() -> R) -> R {
        self.time_with(append, Instant::now, |elapsed| {
            record_stage(Stage::DrainInlineDepth, elapsed);
        })
    }

    /// [`Self::time`] with the clock and the recorder injected, so a test
    /// can drive time and count the samples exactly (the stage itself is a
    /// process-wide static).
    #[inline]
    fn time_with<R>(
        &mut self,
        append: impl FnOnce() -> R,
        now: impl FnMut() -> Instant,
        record: impl FnOnce(Duration),
    ) -> R {
        self.calls = self.calls.wrapping_add(1);
        if self.calls != self.next_at {
            return append();
        }
        self.sampled(append, now, record)
    }

    /// The sampled call: sets the next gap, then reads the clock around the
    /// append only. Out of line and cold, so the unsampled path stays small.
    #[cold]
    #[inline(never)]
    fn sampled<R>(
        &mut self,
        append: impl FnOnce() -> R,
        mut now: impl FnMut() -> Instant,
        record: impl FnOnce(Duration),
    ) -> R {
        self.rng = xorshift32(self.rng);
        self.next_at = self
            .next_at
            .wrapping_add(GAP_MIN + (self.rng & GAP_JITTER_MASK));
        let start = now();
        let out = append();
        record(now().saturating_duration_since(start));
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
    use std::cell::{Cell, RefCell};
    use tickvault_common::constants as k;
    use tickvault_common::feed::Feed;
    use tickvault_storage::hot_path_telemetry::stage_snapshot;

    /// An exchange time inside a session (epoch seconds), as the DHAT gates.
    const SESSION_EPOCH_SECS: u32 = 1_786_800_000;
    /// The matching receipt (UTC nanos, 13:20:00 IST).
    const SESSION_RECEIPT_NANOS: i64 = (SESSION_EPOCH_SECS as i64 - 19_800) * 1_000_000_000;

    /// The call numbers a fresh sampler times among calls `1..=last`.
    fn sampled_calls(last: u32) -> Vec<u32> {
        let mut s = InlineDepthSampler::default();
        let t0 = Instant::now();
        let hit = Cell::new(false);
        let mut at = Vec::new();
        for n in 1..=last {
            hit.set(false);
            s.time_with(|| (), || t0, |_| hit.set(true));
            if hit.get() {
                at.push(n);
            }
        }
        at
    }

    /// The fixed jittered sequence: first sample at call 64, then gaps of
    /// 32..=95. Pinned exactly so the DHAT gates' counts are exact too.
    #[test]
    fn test_sample_sequence_is_jittered_and_fixed() {
        let mut s = InlineDepthSampler::default();
        let recorded = Cell::new(0u32);
        for n in 1..=1_000u32 {
            let out = s.time_with(|| n * 2, Instant::now, |_| recorded.set(recorded.get() + 1));
            assert_eq!(out, n * 2, "the append's result passes through");
        }
        assert_eq!(s.calls(), 1_000);
        let at = sampled_calls(1_000);
        assert_eq!(recorded.get() as usize, at.len());
        assert_eq!(&at[..5], &[64, 121, 215, 305, 390]);
        for w in at.windows(2) {
            let gap = w[1] - w[0];
            assert!(
                (GAP_MIN..=GAP_MIN + GAP_JITTER_MASK).contains(&gap),
                "gap {gap}"
            );
        }
        // Calls 1..=256 of the drain test below hold three samples.
        assert_eq!(sampled_calls(256).len(), 3);
    }

    /// The DHAT Full gates warm up 16 calls, then measure calls 17..=10,016:
    /// exactly 149 samples fall inside (`SAMPLED_FULL_PACKETS` there).
    #[test]
    fn test_dhat_window_holds_149_samples() {
        let inside = sampled_calls(10_016)
            .into_iter()
            .filter(|n| (17..=10_016).contains(n))
            .count();
        assert_eq!(inside, 149);
    }

    /// The jitter breaks the phase lock: with 4 depth packets per frame the
    /// samples land on every position in the frame, not on one (a fixed
    /// every-64th cadence always picked position 4).
    #[test]
    fn test_samples_spread_over_frame_positions() {
        let mut per_position = [0u32; 4];
        for n in sampled_calls(200_000) {
            per_position[((n - 1) % 4) as usize] += 1;
        }
        let total: u32 = per_position.iter().sum();
        for count in per_position {
            assert!(
                count * 5 > total,
                "every position gets over a fifth: {per_position:?}"
            );
        }
        // Mean gap about 64 (32..=95).
        let mean = 200_000 / total;
        assert!((60..=68).contains(&mean), "mean gap {mean}");
    }

    #[test]
    fn test_cadence_survives_the_u32_wrap() {
        let mut s = InlineDepthSampler {
            calls: u32::MAX - 10,
            next_at: 5, // wraps past u32::MAX first
            rng: JITTER_SEED,
        };
        let recorded = Cell::new(0u32);
        let t0 = Instant::now();
        for _ in 0..(16 + 3 * 95) {
            s.time_with(|| (), || t0, |_| recorded.set(recorded.get() + 1));
        }
        assert!(recorded.get() >= 3, "{}", recorded.get());
    }

    /// The clock covers the append and nothing before or after it. A fake
    /// clock (no wall time, so no flaky limit): 20 ms pass before the call
    /// and are not in the sample, the append's own 3 ms are, the clock is
    /// read exactly twice (before and after the append), and the record runs
    /// after the append returned.
    #[test]
    fn test_only_the_append_is_inside_the_clock() {
        let mut s = InlineDepthSampler {
            calls: FIRST_SAMPLE_AT - 1,
            ..InlineDepthSampler::default()
        };
        let base = Instant::now();
        let clock = Cell::new(Duration::ZERO);
        let events = RefCell::new(Vec::new());
        let now = || {
            events.borrow_mut().push("read");
            base + clock.get()
        };
        clock.set(clock.get() + Duration::from_millis(20)); // work before the call
        let sample = Cell::new(None);
        s.time_with(
            || {
                events.borrow_mut().push("append");
                clock.set(clock.get() + Duration::from_millis(3));
            },
            now,
            |elapsed| {
                events.borrow_mut().push("record");
                sample.set(Some(elapsed));
            },
        );
        clock.set(clock.get() + Duration::from_millis(50)); // work after
        assert_eq!(sample.get(), Some(Duration::from_millis(3)));
        assert_eq!(*events.borrow(), vec!["read", "append", "read", "record"]);
        // The next call is not sampled (the gap is at least 32).
        s.time_with(
            || (),
            || panic!("an unsampled call must not read the clock"),
            |_| panic!("the next call must not be sampled"),
        );
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
    /// append, and the stage gains at least the 3 samples the fixed sequence
    /// puts in calls 1..=256 (the stage is process-wide, so other tests can
    /// only add to the delta).
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
        assert!(after.count - before.count >= sampled_calls(256).len() as u64);
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
