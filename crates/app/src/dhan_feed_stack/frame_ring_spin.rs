//! Short wait before sleeping on the frame drain (plan item US1-B,
//! 2026-11-24, owner card "Short wait before sleeping (Recommended)").
//!
//! When the drain finds the frame ring empty it keeps checking for
//! `[dhan_feed] frame_drain_spin_us` before it sleeps. A frame that lands in
//! that time is taken with no thread wake: the socket reader skips the
//! `Notify` wake and the drain is not rescheduled, which is where the
//! socket-to-WAL and receipt-to-fold tails came from (2026-10-26 harness:
//! the drain wake alone p50 9.7 µs, max 1.4 ms).
//!
//! Cost, honestly: the spin holds the drain's thread for up to the budget per
//! gap between frames, so up to `budget × frames per second` of one core
//! (Assumed under 5% at the measured frame rates; read the live figure from
//! `tv_frame_ring_spin_nanos_total`). While it spins, the drain's timer arms
//! wait up to the budget too. `0` turns it off.
//!
//! O(1) per call, no allocation: the counters take a fixed name each.

use std::time::Duration;

use tickvault_core::websocket::frame_ring::{FrameRingReceiver, FrameRingSpinStats};

/// Spins that took a frame with no wake.
pub(super) const SPIN_HITS_TOTAL: &str = "tv_frame_ring_spin_hits_total";
/// Spins that found nothing and fell through to sleep.
pub(super) const SPIN_MISSES_TOTAL: &str = "tv_frame_ring_spin_misses_total";
/// Nanoseconds the drain spent spinning: the CPU the wait costs.
pub(super) const SPIN_NANOS_TOTAL: &str = "tv_frame_ring_spin_nanos_total";

/// Sets the ring's spin from the config value in microseconds (the ring
/// caps it at 1 ms). Called once, where the ring is built.
pub(super) fn configure<T>(rx: &mut FrameRingReceiver<T>, spin_us: u32) {
    rx.set_spin_before_park(Duration::from_micros(u64::from(spin_us)));
}

/// Publishes the spin stats as absolute counters (local `/metrics` only; no
/// alarm, filter or EMF name). On the drain's every-1,024-frames report.
pub(super) fn publish<T>(rx: &FrameRingReceiver<T>) {
    publish_stats(rx.spin_stats());
}

fn publish_stats(stats: FrameRingSpinStats) {
    metrics::counter!(SPIN_HITS_TOTAL).absolute(stats.hits);
    metrics::counter!(SPIN_MISSES_TOTAL).absolute(stats.misses);
    metrics::counter!(SPIN_NANOS_TOTAL).absolute(stats.spin_nanos);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_core::websocket::frame_ring::{FRAME_RING_SPIN_MAX, frame_ring};

    #[test]
    fn test_configure_sets_the_spin_from_microseconds_and_zero_turns_it_off() {
        let (_tx, mut rx) = frame_ring::<u8>(4);
        configure(&mut rx, 50);
        assert_eq!(rx.spin_before_park(), Duration::from_micros(50));
        configure(&mut rx, u32::MAX);
        assert_eq!(rx.spin_before_park(), FRAME_RING_SPIN_MAX, "capped");
        configure(&mut rx, 0);
        assert_eq!(rx.spin_before_park(), Duration::ZERO);
    }

    #[test]
    fn test_publish_reads_the_ring_stats_without_panicking() {
        let (_tx, rx) = frame_ring::<u8>(4);
        publish(&rx);
        publish_stats(FrameRingSpinStats {
            hits: 3,
            misses: 2,
            spin_nanos: 150_000,
        });
    }

    /// The live lane wires both calls: configure where the ring is built,
    /// publish on the drain's report arm.
    #[test]
    fn test_live_lane_configures_and_publishes_the_spin() {
        let src = include_str!("../dhan_feed_stack.rs");
        let prod = &src[..src
            .find("\nmod subscribe_reconcile_tests")
            .unwrap_or(src.len())];
        assert!(
            prod.contains("frame_ring_spin::configure(&mut frame_rx, params.frame_drain_spin_us);")
        );
        assert!(prod.contains("frame_ring_spin::publish(&rx);"));
    }
}
