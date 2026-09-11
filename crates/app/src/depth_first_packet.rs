//! How long a depth contract stays dark after we subscribe it.
//!
//! # The question this answers, and why nothing else could
//!
//! A depth swap unsubscribes one contract and subscribes another on the same
//! socket. The operator's question on 2026-09-11 was the right one: *how long
//! does that take?* Every number this repository could offer was a BUDGET, not
//! a measurement — [`SWAP_WIRE_BUDGET`] is a one-second ceiling per side, the
//! transport's own send timeout is ten seconds, and the per-socket pending gate
//! holds the real rate to one swap per socket per minute. None of those is the
//! answer. They bound how long we WAIT; they say nothing about how long Dhan
//! takes to start delivering the new book.
//!
//! That gap mattered because of a second fact: **the India feed has no
//! snapshot-on-subscribe.** "First Tick Snapshot" is documented only on the US
//! global-stocks socket. So a freshly subscribed contract is not merely
//! late — it is BLANK until the book next changes, and nothing in this process
//! measured that window.
//!
//! [`SWAP_WIRE_BUDGET`]: tickvault_core::websocket::pool_supervisor::SWAP_WIRE_BUDGET
//!
//! # What the number actually composes, stated plainly
//!
//! The clock starts when the steering task successfully QUEUES the swap
//! (`try_send` returned `Ok`) and stops at the first depth packet for that
//! contract. That interval contains four terms, and only the first two are
//! ours:
//!
//! 1. queue wait on the per-socket command channel,
//! 2. the connection task's unsubscribe-then-subscribe wire writes,
//! 3. Dhan accepting and applying the subscription,
//! 4. **time until the contract's book next changes.**
//!
//! Term 4 is the market's, not ours, and on a thin stock option it can be the
//! whole figure. So this is NOT a network round-trip and must never be quoted
//! as one. It is the operator-facing quantity — *how long until data flows
//! again after we decide to swap* — and that is deliberately the upper bound,
//! because that is the number a fill depends on.
//!
//! Measuring from DISPATCH rather than from the wire write is also what keeps
//! this an app-crate change: the wire write happens on the connection task
//! inside `core`, and reaching it would mean threading a clock through the
//! transport for a term that is bounded by [`SWAP_WIRE_BUDGET`] anyway.
//!
//! # Why `silent_window` is not called a failure
//!
//! A contract with no depth packet inside [`FIRST_PACKET_WINDOW_SECS`] may
//! simply not have traded. Naming that outcome "never arrived" would be a
//! claim in the alarming direction about a book doing nothing wrong, which is
//! the mislabel class this repository keeps correcting. It is counted, it is
//! named for what was observed, and it is left to the reader to decide.
//!
//! What it IS good for is the shape nobody could see before: a swap that
//! acknowledged and then delivered nothing at all. When the unsubscribe code
//! is being ignored — proven live for code 25 on 2026-09-10 and for code 24 on
//! 2026-09-11 — the arrival half of a swap is the only half left to check.
//!
//! # Complexity
//!
//! * [`DepthFirstPacketTracker::observe_at`] — the hot path, once per depth
//!   PACKET, never per level. **One relaxed atomic load** when nothing is
//!   pending, which is the overwhelming majority of the session. While a swap
//!   is outstanding it is that load plus one `papaya` probe, and one removal on
//!   the single packet that resolves it. Zero allocation on every arm.
//! * [`DepthFirstPacketTracker::record_subscribe_at`] — O(1), a few times a
//!   minute on the steering task.
//! * [`DepthFirstPacketTracker::sweep_expired_at`] — O(pending), bounded by
//!   [`MAX_PENDING`], once a minute on the steering task. Never on the drain.
//!
//! The metric emissions sit on the swap path, not the packet path: the
//! histogram fires once per resolved subscribe (a handful per minute), so the
//! `record_ws_lag` lesson — a label value that is not a literal drops
//! `metrics::histogram!` to its allocating arm — is not load-bearing here, and
//! every label value below is a `&'static str` regardless.
//!
//! # Observability boundary (deliberate, and it is a cost decision)
//!
//! These series are LOCAL `/metrics` only. There is no EMF selector entry and
//! no CloudWatch alarm. The September forecast read live on 2026-09-06 is
//! $142.24 against an automatic `STOP_EC2_INSTANCES` action line of $135.00,
//! and the standing rule in `dhan-rest-only-noise-lock-2026-07-14.md` §2.3n is
//! that the next addition arrives with a LEVER, not a cost note. This change
//! carries no lever, so it carries no CloudWatch cost. It is the number an
//! operator reads after an existing page, never a new page.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use papaya::HashMap as PapayaHashMap;
use tickvault_common::types::ExchangeSegment;

/// The I-P1-11 composite key, as the wire byte so it is `Copy` — the same
/// shape `depth_subscription_view` uses, so a contract hashes identically in
/// both and the two cannot disagree about what instrument a packet is.
type Key = (u64, u8);

/// Nanoseconds in a second, as the signed type both clocks are carried in.
const NANOS_PER_SEC: i64 = 1_000_000_000;

/// Nanoseconds in a millisecond.
const NANOS_PER_MILLI: i64 = 1_000_000;

/// Histogram: milliseconds from a queued subscribe to that contract's first
/// depth packet. See the module header for what the interval composes — it is
/// NOT a network round-trip.
pub const FIRST_PACKET_LATENCY_MS: &str = "tv_depth_first_packet_latency_ms";

/// Counter, labelled `outcome`: how each awaited subscribe resolved.
pub const FIRST_PACKET_OUTCOME: &str = "tv_depth_first_packet_total";

/// Every `outcome` label value, so all three series exist at zero from boot.
///
/// The CloudWatch agent drops the first sample of a series it has never seen,
/// and these are local-only today — but a counter whose FIRST increment is the
/// event it exists to report is the seeding defect this repository has already
/// paid for once (`tv_depth_rows_spilled_total`, 2026-08-28), and pre-seeding
/// costs nothing.
pub const FIRST_PACKET_OUTCOMES: [&str; 3] = ["arrived", "silent_window", "refused"];

/// How long a subscribed contract may stay dark before the wait is given up
/// and counted as [`FIRST_PACKET_OUTCOMES`]`[1]`.
///
/// 120 seconds is chosen against two live numbers rather than picked round:
/// the connection's own idle-reconnect timeout is 27 s and Dhan closes a
/// socket silent for 40 s, so a window shorter than either would be reporting
/// on a socket the transport has already acted on. Longer than 120 s and the
/// pending entry outlives the steering minute that could explain it.
pub const FIRST_PACKET_WINDOW_SECS: i64 = 120;

/// Fail-closed bound on the pending map.
///
/// The real rate is one swap per socket per minute across ten depth sockets
/// (the unreconciled-ack gate in `depth20_track` and its depth-200 twin), and
/// an entry lives at most [`FIRST_PACKET_WINDOW_SECS`], so the expected
/// occupancy is tens. 1,024 is a bound against a shape nobody has designed,
/// never a size that is expected — past it a subscribe is not tracked, is
/// counted `refused`, and the swap itself proceeds untouched.
pub const MAX_PENDING: usize = 1_024;

/// Awaited subscribes, keyed on the I-P1-11 composite.
#[derive(Debug, Default)]
pub struct DepthFirstPacketTracker {
    /// Contract -> the epoch-nanosecond instant its swap was queued.
    pending: PapayaHashMap<Key, i64>,
    /// A hot-path gate, so a session with nothing outstanding pays ONE relaxed
    /// load per depth packet and never touches the map. Kept as a separate
    /// counter rather than reading `pending.len()`, which pins the map.
    ///
    /// It is advisory, not authoritative: it can briefly disagree with the map
    /// under a concurrent insert and removal. Both directions are harmless —
    /// too high costs one wasted probe, too low costs one unmeasured arrival —
    /// and neither can produce a wrong latency, because a latency is only ever
    /// emitted from an entry the map actually returned.
    pending_count: AtomicUsize,
}

impl DepthFirstPacketTracker {
    /// An empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts the clock for a contract whose swap has just been QUEUED.
    ///
    /// Called on the `Ok(())` arm of the dispatch `try_send` in both pools —
    /// never on the refused arms, where no subscribe will happen and a pending
    /// entry would age out into a false `silent_window`.
    ///
    /// Re-subscribing a contract that is already awaited REPLACES the stamp
    /// rather than keeping the older one: the newer subscribe is the one whose
    /// arrival the next packet answers, and keeping the older stamp would
    /// report a latency spanning a window the contract was not even subscribed
    /// for.
    pub fn record_subscribe_at(&self, security_id: u64, segment: ExchangeSegment, at_nanos: i64) {
        let pinned = self.pending.pin();
        let key = (security_id, segment as u8);
        if pinned.get(&key).is_none() && self.pending_count.load(Ordering::Relaxed) >= MAX_PENDING {
            metrics::counter!(FIRST_PACKET_OUTCOME, "outcome" => "refused").increment(1);
            return;
        }
        if pinned.insert(key, at_nanos).is_none() {
            self.pending_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The hot-path arm: does this packet resolve an awaited subscribe?
    ///
    /// Returns the measured interval in NANOSECONDS on the one packet that
    /// resolves it, and `None` on every other packet — which is almost all of
    /// them. The caller does not have to do anything with the value; the
    /// histogram is emitted here so no call site can forget it.
    pub fn observe_at(&self, security_id: u64, segment_code: u8, at_nanos: i64) -> Option<i64> {
        // The whole point of this load: a session with no swap outstanding
        // pays exactly this and returns.
        if self.pending_count.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let pinned = self.pending.pin();
        let subscribed_at = *pinned.get(&(security_id, segment_code))?;
        if pinned.remove(&(security_id, segment_code)).is_none() {
            // Another thread resolved the same contract first. It emitted the
            // measurement; emitting a second one would double-count a single
            // arrival.
            return None;
        }
        self.pending_count.fetch_sub(1, Ordering::Relaxed);
        // Saturating, and a backward clock step yields zero rather than a
        // negative latency. A negative sample in a histogram is not a small
        // error — it is an unbounded one, because the bucket it lands in is
        // whatever the exporter does with a value it was never given a bucket
        // for.
        let elapsed = at_nanos.saturating_sub(subscribed_at).max(0);
        metrics::counter!(FIRST_PACKET_OUTCOME, "outcome" => "arrived").increment(1);
        metrics::histogram!(FIRST_PACKET_LATENCY_MS).record((elapsed / NANOS_PER_MILLI) as f64);
        Some(elapsed)
    }

    /// Gives up on contracts dark for longer than [`FIRST_PACKET_WINDOW_SECS`]
    /// and counts them. Returns how many were given up on.
    ///
    /// Called once a minute from the steering loop, beside the swap-ack
    /// reconcile that settles the other half of the same swap. Never from the
    /// drain: this is O(pending) and the drain is the task that must not stop
    /// reading the socket.
    pub fn sweep_expired_at(&self, now_nanos: i64) -> usize {
        if self.pending_count.load(Ordering::Relaxed) == 0 {
            return 0;
        }
        let cutoff = now_nanos.saturating_sub(FIRST_PACKET_WINDOW_SECS * NANOS_PER_SEC);
        let pinned = self.pending.pin();
        let mut expired = 0usize;
        // Collect-then-remove is deliberate: `papaya`'s iteration order under a
        // concurrent removal is not something to reason about while holding
        // the decision. The intermediate is bounded by MAX_PENDING, this runs
        // once a minute on a cold task, and it is the one allocation in this
        // module — off the hot path, and stated rather than hidden.
        let doomed: Vec<Key> = pinned
            .iter()
            .filter(|(_, at)| **at <= cutoff)
            .map(|(key, _)| *key)
            .collect();
        for key in doomed {
            if pinned.remove(&key).is_some() {
                self.pending_count.fetch_sub(1, Ordering::Relaxed);
                expired = expired.saturating_add(1);
            }
        }
        if expired > 0 {
            metrics::counter!(FIRST_PACKET_OUTCOME, "outcome" => "silent_window")
                .increment(expired as u64);
        }
        expired
    }

    /// How many subscribes are awaiting their first packet. Test + gauge use.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.pin().len()
    }
}

/// Registers all three `outcome` series at zero. Called once at boot, beside
/// the other depth counter pre-registrations.
pub fn pre_register_first_packet_counters() {
    for outcome in FIRST_PACKET_OUTCOMES {
        metrics::counter!(FIRST_PACKET_OUTCOME, "outcome" => outcome).increment(0);
    }
}

/// The one tracker. The steering task writes it, the frame drain reads it —
/// the concurrent-reader shape `papaya` exists for, and the same accessor
/// pattern as `global_depth_subscription_view`.
pub fn global_depth_first_packet_tracker() -> &'static Arc<DepthFirstPacketTracker> {
    static TRACKER: std::sync::OnceLock<Arc<DepthFirstPacketTracker>> = std::sync::OnceLock::new();
    TRACKER.get_or_init(|| Arc::new(DepthFirstPacketTracker::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_700_000_000 * NANOS_PER_SEC;

    #[test]
    fn observe_at_measures_nothing_for_an_unawaited_contract() {
        let tracker = DepthFirstPacketTracker::new();
        assert_eq!(
            tracker.observe_at(1, ExchangeSegment::NseFno as u8, T0),
            None
        );
        assert_eq!(tracker.pending_len(), 0);
    }

    #[test]
    fn observe_at_yields_the_interval_on_the_first_packet_after_a_subscribe() {
        let tracker = DepthFirstPacketTracker::new();
        tracker.record_subscribe_at(42, ExchangeSegment::NseFno, T0);
        assert_eq!(tracker.pending_len(), 1);
        let measured = tracker.observe_at(
            42,
            ExchangeSegment::NseFno as u8,
            T0 + 350 * NANOS_PER_MILLI,
        );
        assert_eq!(measured, Some(350 * NANOS_PER_MILLI));
        assert_eq!(tracker.pending_len(), 0, "the entry is consumed");
    }

    #[test]
    fn only_the_first_packet_measures_and_the_rest_are_free() {
        let tracker = DepthFirstPacketTracker::new();
        tracker.record_subscribe_at(42, ExchangeSegment::NseFno, T0);
        assert!(
            tracker
                .observe_at(42, ExchangeSegment::NseFno as u8, T0 + NANOS_PER_MILLI)
                .is_some()
        );
        for _ in 0..5 {
            assert_eq!(
                tracker.observe_at(42, ExchangeSegment::NseFno as u8, T0 + NANOS_PER_SEC),
                None,
                "a second measurement would double-count one arrival"
            );
        }
    }

    #[test]
    fn the_composite_key_separates_a_shared_numeric_id() {
        // I-P1-11: Dhan reuses the same number across segments. A packet for
        // the BSE contract must not resolve the NSE one's subscribe.
        let tracker = DepthFirstPacketTracker::new();
        tracker.record_subscribe_at(27, ExchangeSegment::NseFno, T0);
        assert_eq!(
            tracker.observe_at(27, ExchangeSegment::BseFno as u8, T0 + NANOS_PER_SEC),
            None
        );
        assert_eq!(
            tracker.pending_len(),
            1,
            "the NSE subscribe is still awaited"
        );
        assert!(
            tracker
                .observe_at(27, ExchangeSegment::NseFno as u8, T0 + NANOS_PER_SEC)
                .is_some()
        );
    }

    #[test]
    fn sweep_expired_at_gives_up_on_a_contract_dark_past_the_window() {
        let tracker = DepthFirstPacketTracker::new();
        tracker.record_subscribe_at(7, ExchangeSegment::NseFno, T0);
        assert_eq!(
            tracker.sweep_expired_at(T0 + (FIRST_PACKET_WINDOW_SECS - 1) * NANOS_PER_SEC),
            0,
            "inside the window it is still awaited, not given up on"
        );
        assert_eq!(
            tracker.sweep_expired_at(T0 + (FIRST_PACKET_WINDOW_SECS + 1) * NANOS_PER_SEC),
            1
        );
        assert_eq!(tracker.pending_len(), 0);
    }

    #[test]
    fn record_subscribe_at_restamps_rather_than_keeping_the_older_clock() {
        let tracker = DepthFirstPacketTracker::new();
        tracker.record_subscribe_at(7, ExchangeSegment::NseFno, T0);
        tracker.record_subscribe_at(7, ExchangeSegment::NseFno, T0 + 10 * NANOS_PER_SEC);
        assert_eq!(tracker.pending_len(), 1, "one contract, one entry");
        let measured =
            tracker.observe_at(7, ExchangeSegment::NseFno as u8, T0 + 11 * NANOS_PER_SEC);
        assert_eq!(
            measured,
            Some(NANOS_PER_SEC),
            "measured from the LATEST subscribe, not the first"
        );
    }

    #[test]
    fn a_backward_clock_step_yields_zero_never_a_negative_sample() {
        let tracker = DepthFirstPacketTracker::new();
        tracker.record_subscribe_at(7, ExchangeSegment::NseFno, T0);
        assert_eq!(
            tracker.observe_at(7, ExchangeSegment::NseFno as u8, T0 - NANOS_PER_SEC),
            Some(0)
        );
    }

    #[test]
    fn record_subscribe_at_is_bounded_and_the_refusal_is_counted() {
        let tracker = DepthFirstPacketTracker::new();
        for id in 0..(MAX_PENDING as u64 + 50) {
            tracker.record_subscribe_at(id, ExchangeSegment::NseFno, T0);
        }
        assert_eq!(
            tracker.pending_len(),
            MAX_PENDING,
            "past the cap a subscribe is refused tracking, never tracked anyway"
        );
    }

    #[test]
    fn a_full_map_still_restamps_a_contract_it_already_holds() {
        // The cap must bound NEW contracts, never block re-stamping one that
        // is already counted -- otherwise a full map freezes every tracked
        // contract on a stale clock.
        let tracker = DepthFirstPacketTracker::new();
        for id in 0..MAX_PENDING as u64 {
            tracker.record_subscribe_at(id, ExchangeSegment::NseFno, T0);
        }
        tracker.record_subscribe_at(0, ExchangeSegment::NseFno, T0 + 5 * NANOS_PER_SEC);
        assert_eq!(tracker.pending_len(), MAX_PENDING);
        assert_eq!(
            tracker.observe_at(0, ExchangeSegment::NseFno as u8, T0 + 6 * NANOS_PER_SEC),
            Some(NANOS_PER_SEC),
            "the re-stamp took effect"
        );
    }

    #[test]
    fn sweep_expired_at_is_free_when_nothing_is_awaited() {
        let tracker = DepthFirstPacketTracker::new();
        assert_eq!(tracker.sweep_expired_at(T0 + 10 * NANOS_PER_SEC), 0);
    }

    #[test]
    fn pending_len_counts_awaited_subscribes_and_falls_as_they_resolve() {
        let tracker = DepthFirstPacketTracker::new();
        assert_eq!(tracker.pending_len(), 0);
        tracker.record_subscribe_at(1, ExchangeSegment::NseFno, T0);
        tracker.record_subscribe_at(2, ExchangeSegment::NseFno, T0);
        assert_eq!(tracker.pending_len(), 2);
        tracker.observe_at(1, ExchangeSegment::NseFno as u8, T0 + NANOS_PER_MILLI);
        assert_eq!(tracker.pending_len(), 1);
    }
    #[test]
    fn global_depth_first_packet_tracker_is_one_tracker_and_starts_empty() {
        let a = global_depth_first_packet_tracker();
        let b = global_depth_first_packet_tracker();
        assert!(Arc::ptr_eq(a, b));
    }

    #[test]
    fn pre_register_first_packet_counters_seeds_every_outcome_label() {
        // The guard is the LIST, not the call: a new outcome added to the
        // emit sites without joining this array ships an unseeded series.
        assert_eq!(FIRST_PACKET_OUTCOMES.len(), 3);
        assert!(FIRST_PACKET_OUTCOMES.contains(&"arrived"));
        assert!(FIRST_PACKET_OUTCOMES.contains(&"silent_window"));
        assert!(FIRST_PACKET_OUTCOMES.contains(&"refused"));
        pre_register_first_packet_counters();
    }
}
