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
//! Adaptive: a spin that finds nothing switches the wait off; it comes back on
//! only after a sleep (with no spin before it) that a frame ended within the
//! budget. After 64 back-to-back catches the ring takes one ordinary sleep,
//! so the drain's timer and idle arms are polled during a long rush.
//!
//! Cost, measured on the x86 dev container (harness
//! `ring_wake_with_and_without_spin`): extra CPU 0 to 1.6% at gaps of about
//! 400 µs, about 19% at 100 µs, about 88% of one core during a rush of
//! 20 µs gaps. Live figure: `tv_frame_ring_spin_nanos_total`.
//!
//! # Where it runs (review 2026-11-24)
//!
//! Only when the drain shares the main runtime ([`DrainThreadPlan::Shared`],
//! the default, and the `Invalid` fallback that runs shared). With its own
//! thread (`TICKVAULT_DRAIN_THREAD=on`, the production unit) the drain sits on
//! the readers' core: at the same priority a spin would hold the core the
//! reader needs to deliver the frame, and at real-time priority every frame
//! the reader delivers mid-spin counts as drain CPU wait in the daily capacity
//! verdict (`host_capacity_minute`), which could read a false
//! "upgrade needed". So there the spin is forced off, logged once.
//!
//! O(1) per call, no allocation: the counters take a fixed name each.

use std::time::Duration;

use tickvault_core::websocket::frame_ring::{FrameRingReceiver, FrameRingSpinStats};
use tracing::info;

use crate::drain_thread::{DrainThreadPlan, drain_placement};

/// Spins that took a frame with no wake.
pub(super) const SPIN_HITS_TOTAL: &str = "tv_frame_ring_spin_hits_total";
/// Spins that found nothing and fell through to sleep.
pub(super) const SPIN_MISSES_TOTAL: &str = "tv_frame_ring_spin_misses_total";
/// Nanoseconds the drain spent spinning: the CPU the wait costs.
pub(super) const SPIN_NANOS_TOTAL: &str = "tv_frame_ring_spin_nanos_total";

/// Sets the ring's spin from the config value in microseconds (the ring
/// caps it at 1 ms), forced to 0 when the drain has its own thread. Called
/// once, where the ring is built.
pub(super) fn configure<T>(rx: &mut FrameRingReceiver<T>, spin_us: u32) {
    let (plan, _rt_priority) = drain_placement();
    let effective = effective_spin_us(spin_us, plan);
    if effective != spin_us {
        info!(
            configured_us = spin_us,
            plan = ?plan,
            "frame drain short wait off: the drain has its own thread on the \
             readers' core, where a spin would hold the core the reader needs or \
             count as drain CPU wait"
        );
    }
    rx.set_spin_before_park(Duration::from_micros(u64::from(effective)));
}

/// The spin to use for `plan`: the configured value when the drain shares the
/// main runtime, 0 when it has its own thread. Pure.
fn effective_spin_us(spin_us: u32, plan: DrainThreadPlan) -> u32 {
    match plan {
        DrainThreadPlan::Shared | DrainThreadPlan::Invalid => spin_us,
        DrainThreadPlan::Own(_) => 0,
    }
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
    fn test_effective_spin_us_is_off_for_an_own_thread_drain() {
        assert_eq!(effective_spin_us(50, DrainThreadPlan::Shared), 50);
        assert_eq!(
            effective_spin_us(50, DrainThreadPlan::Invalid),
            50,
            "runs shared"
        );
        assert_eq!(effective_spin_us(50, DrainThreadPlan::Own(None)), 0);
        assert_eq!(effective_spin_us(50, DrainThreadPlan::Own(Some(1))), 0);
        assert_eq!(effective_spin_us(0, DrainThreadPlan::Shared), 0);
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
}
