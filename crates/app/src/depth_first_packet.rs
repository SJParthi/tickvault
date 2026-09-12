//! How long a depth contract stays dark after we subscribe it.
//!
//! # The question this answers, and why nothing else could
//!
//! A depth swap unsubscribes one contract and subscribes another on the same
//! socket. The operator's question on 2026-09-11 was the right one: *how long
//! does that take?* Every number this repository could offer was a BUDGET, not
//! a measurement — [`SWAP_WIRE_BUDGET`] is a one-second ceiling per side, the
//! transport's own send timeout is ten seconds, and the per-socket pending gate
//! holds the real rate to at most 4 swaps per socket per minute on depth-20 (`DEPTH_SWAP_COMMAND_CHANNEL_DEPTH`) and 5 pool-wide per minute on depth-200. None of those is the
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
//!   pending. ⚠ Do NOT read that as "almost always": the gate is a single
//!   PROCESS-WIDE counter, an entry lives up to `FIRST_PACKET_WINDOW_SECS`
//!   (120 s) and the sweep runs only once a minute, so at the measured
//!   2026-09-11 rate (9,461 swaps over ~385 min ≈ 24.6/min) the pending set
//!   is expected to be NON-EMPTY for most of a trading session — and after
//!   the measured 09:50 delivery cliff, when no new contract delivered at
//!   all, permanently so. The honest statement is that the idle path is
//!   cheap, not that it is the common one. While a swap
//!   is outstanding it is that load plus exactly ONE `papaya` probe — the
//!   removal IS the read, so the packet that resolves a subscribe costs the
//!   same single probe as one that does not.
//!
//! ⚠ **NOT zero-allocation in the DEALLOCATION direction**, corrected
//! 2026-09-11 after a hot-path audit. This line used to end "Zero allocation
//! on every arm", which is true of the allocating direction and false of the
//! freeing one: `pin()` returns a `seize` guard whose DROP runs that
//! collector's deferred reclamation, so the drain — which pins far more often
//! than the steering task — performs essentially all of this map's frees. The
//! honest statement is that no arm ALLOCATES; the resolving arm may FREE.
//! Recorded rather than removed because a header asserting zero allocation on
//! the frame drain is exactly what stops the next auditor looking.
//! * [`DepthFirstPacketTracker::record_subscribe_at`] — O(1), a few times a
//!   minute on the steering task.
//! * [`DepthFirstPacketTracker::sweep_expired_at`] — O(pending), bounded by
//!   [`MAX_PENDING`], once a minute on the steering task. Never on the drain.
//!
//! ⚠ The `arrived` counter and the histogram emit from `observe_at`, i.e. ON
//! THE DRAIN — an earlier version of this header said they "sit on the swap
//! path, not the packet path", which is false and pointed an auditor away
//! from the hot path. What is true is the RATE, and that is what makes it
//! safe: they fire only on the single packet that RESOLVES a subscribe (a
//! handful per minute), never per packet. Both label values are string
//! LITERALS, so the `record_ws_lag` trap — a non-literal label value dropping
//! `metrics::histogram!` to its allocating arm, ~36M allocations/hour — does
//! not apply here.
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
use tickvault_core::parser::depth::DepthFeedKind;

/// The I-P1-11 composite key: `(security_id, WIRE segment code, pool)`.
///
/// The segment byte MUST be [`ExchangeSegment::binary_code`], never
/// `segment as u8`. Dhan's annexure has **no enum 6 or 7** — `BSE_FNO` is
/// `8` — so declaration order and the wire code diverge for the last two
/// variants (`BseCurrency` 6 vs 7, `BseFno` 7 vs 8). The drain looks a packet
/// up by `header.exchange_segment_code`, the wire byte, so a stamp keyed on
/// the discriminant could never be resolved for those segments. Pinned by
/// `the_stamp_key_is_the_wire_code_for_every_segment`.
///
/// The POOL byte is what stops depth-200's already-streaming packet from
/// resolving depth-20's fresh subscribe: the two pools draw from the same
/// gainer-ordered list, so the same contract is routinely held by both, and
/// without this byte the first arrival on EITHER socket would answer the
/// other's stamp and record a fabricated near-zero latency.
type Key = (u64, u8, u8);

/// Pool byte for [`Key`]. Values are arbitrary but must be distinct and stable.
const fn pool_code(kind: DepthFeedKind) -> u8 {
    match kind {
        DepthFeedKind::Twenty => 20,
        DepthFeedKind::TwoHundred => 200,
    }
}

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

/// Every `outcome` label value, so all five series exist at zero from boot.
///
/// The CloudWatch agent drops the first sample of a series it has never seen,
/// and these are local-only today — but a counter whose FIRST increment is the
/// event it exists to report is the seeding defect this repository has already
/// paid for once (`tv_depth_rows_spilled_total`, 2026-08-28), and pre-seeding
/// costs nothing.
pub const FIRST_PACKET_OUTCOMES: [&str; 5] = [
    "arrived",
    "silent_window",
    "refused",
    "unmeasurable",
    "reordered",
];

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
/// The real rate is at most 4 swaps per socket per minute on depth-20 (`DEPTH_SWAP_COMMAND_CHANNEL_DEPTH`) and 5 pool-wide per minute on depth-200 across ten depth sockets
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

/// Nanoseconds to milliseconds as a FLOAT.
///
/// `(elapsed / NANOS_PER_MILLI) as f64` — the form this replaced — divides as
/// INTEGERS and only then widens, so every arrival under one millisecond
/// records as `0.0`. The fast end is the end this instrument exists to see,
/// so the truncation destroyed exactly the resolution that matters. Pinned by
/// `elapsed_nanos_to_millis_keeps_sub_millisecond_resolution`.
fn elapsed_nanos_to_millis(elapsed_nanos: i64) -> f64 {
    elapsed_nanos as f64 / NANOS_PER_MILLI as f64
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
    /// `may_already_be_streaming` is the CRITICAL gate, added 2026-09-11 after
    /// a hostile permutation sweep. [`Key`] carries the pool byte but NOT the
    /// socket, so a contract this process dropped — and that Dhan is still
    /// streaming because it ignored the unsubscribe (proven for code 25 on
    /// 2026-09-10 and code 24 on 2026-09-11, mean ghost tail ~374 s) — can be
    /// re-taken onto a DIFFERENT socket of the same pool, and the very next
    /// ghost packet from the OLD socket then resolves the fresh stamp. The
    /// sample recorded is ~0 ms for a socket that has delivered nothing: a
    /// fabricated zero, biasing the instrument toward zero at exactly the end
    /// it exists to measure. Same class as the pool-byte defect one level
    /// down, and the reason the pool byte alone is not sufficient.
    ///
    /// The caller answers it from
    /// `DepthSubscriptionView::classify_raw` — anything other than `Unknown`
    /// means this process has a recent record of the contract on a depth
    /// socket, so a packet for it cannot be trusted to be the new socket's
    /// first. Refusing costs ONE sample and is counted; measuring anyway costs
    /// the series' credibility. It deliberately OVER-refuses the harmless
    /// cross-pool case (`classify_raw` is pool-blind, so depth-20 holding a
    /// contract refuses a depth-200 stamp the pool byte would already have
    /// protected) because over-refusing loses a sample and under-refusing
    /// writes a number that is wrong in the reassuring direction.
    pub fn record_subscribe_at(
        &self,
        security_id: u64,
        segment: ExchangeSegment,
        pool: DepthFeedKind,
        at_nanos: i64,
        may_already_be_streaming: bool,
    ) {
        if may_already_be_streaming {
            metrics::counter!(FIRST_PACKET_OUTCOME, "outcome" => "unmeasurable").increment(1);
            return;
        }
        let pinned = self.pending.pin();
        // `binary_code()`, NOT `segment as u8` — see the `Key` docblock.
        let key = (security_id, segment.binary_code(), pool_code(pool));
        if pinned.get(&key).is_none() && self.pending_count.load(Ordering::Relaxed) >= MAX_PENDING {
            metrics::counter!(FIRST_PACKET_OUTCOME, "outcome" => "refused").increment(1);
            return;
        }
        if pinned.insert(key, at_nanos).is_none() {
            self.pending_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Drops a stamp for a swap the WIRE refused, so it never ages into a
    /// false `silent_window`. Returns whether an entry was actually dropped.
    ///
    /// The stamp sites fire on the `Ok(())` arm of `try_send`, which means the
    /// command reached a CHANNEL — not that the connection took it. The
    /// verdict arrives a minute later through the swap-ack reconcile, and on
    /// `NotHeld`/refused/sender-dropped the believed hold is reverted because
    /// the contract was never subscribed. Before this (found 2026-09-11) the
    /// stamp survived that revert and was swept at
    /// [`FIRST_PACKET_WINDOW_SECS`] into `silent_window` — a label whose own
    /// docs say the contract "may simply not have traded", reporting a dark
    /// window for a subscribe that never happened.
    ///
    /// Called from the reconcile, i.e. once a minute on the steering task,
    /// never from the drain.
    pub fn forget(&self, security_id: u64, segment: ExchangeSegment, pool: DepthFeedKind) -> bool {
        let pinned = self.pending.pin();
        // `binary_code()`, NOT `segment as u8` — see the `Key` docblock.
        if pinned
            .remove(&(security_id, segment.binary_code(), pool_code(pool)))
            .is_some()
        {
            self.pending_count.fetch_sub(1, Ordering::Relaxed);
            return true;
        }
        false
    }

    /// The hot-path arm: does this packet resolve an awaited subscribe?
    ///
    /// Returns the measured interval in NANOSECONDS on the one packet that
    /// resolves it, and `None` on every other packet — which is almost all of
    /// them. The caller does not have to do anything with the value; the
    /// histogram is emitted here so no call site can forget it.
    pub fn observe_at(
        &self,
        security_id: u64,
        segment_code: u8,
        pool: DepthFeedKind,
        at_nanos: i64,
    ) -> Option<i64> {
        // The whole point of this load: a session with no swap outstanding
        // pays exactly this and returns.
        if self.pending_count.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let pinned = self.pending.pin();
        // ONE probe, not a `get` followed by a `remove`. `remove` hands back
        // the value it took, so the read and the claim are the same operation
        // — and that is also what makes the race safe rather than merely
        // survivable: exactly one thread can receive `Some` for a given
        // arrival, so a second thread cannot double-count it. A `get` first
        // would let both threads read the stamp and then have one lose the
        // claim, which costs a probe and buys nothing.
        let subscribed_at = *pinned.remove(&(security_id, segment_code, pool_code(pool)))?;
        self.pending_count.fetch_sub(1, Ordering::Relaxed);
        // Saturating, and a backward clock step yields zero rather than a
        // negative latency. A negative sample in a histogram is not a small
        // error — it is an unbounded one, because the bucket it lands in is
        // whatever the exporter does with a value it was never given a bucket
        // for.
        // A packet whose receipt instant PRECEDES the subscribe is not a fast
        // arrival, it is an out-of-order one, and counting it as a 0 ms
        // `arrived` is the second fabricated-zero class this module had (found
        // 2026-09-11, same sweep as the ghost gate above).
        //
        // It is a ROUTINE path, not an NTP edge case: the drain's
        // `received_at_nanos` is deliberately BACK-DATED by ring dwell
        // (`dhan_feed_stack.rs` — `Utc::now()` minus `frame.received_at
        // .elapsed()`), while the stamp is a raw `Utc::now()`. Ring dwell has
        // its own alarm at 2,000 ms, so under any backlog a packet received
        // before the dispatch but drained after lands here. The old code
        // clamped it to zero with `.max(0)` and still counted `arrived`.
        //
        // The entry is CONSUMED either way — the contract has delivered a
        // packet, so it is not dark and must not later age into
        // `silent_window`; only the measurement is refused.
        if at_nanos < subscribed_at {
            metrics::counter!(FIRST_PACKET_OUTCOME, "outcome" => "reordered").increment(1);
            return None;
        }
        let elapsed = at_nanos.saturating_sub(subscribed_at);
        metrics::counter!(FIRST_PACKET_OUTCOME, "outcome" => "arrived").increment(1);
        // FLOAT division. `(elapsed / NANOS_PER_MILLI) as f64` divides as
        // INTEGERS first, so every sub-millisecond arrival records as 0.0 —
        // and the fast end is the end this instrument exists to see.
        metrics::histogram!(FIRST_PACKET_LATENCY_MS).record(elapsed_nanos_to_millis(elapsed));
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
            tracker.observe_at(
                1,
                ExchangeSegment::NseFno.binary_code(),
                DepthFeedKind::Twenty,
                T0
            ),
            None
        );
        assert_eq!(tracker.pending_len(), 0);
    }

    #[test]
    fn observe_at_yields_the_interval_on_the_first_packet_after_a_subscribe() {
        let tracker = DepthFirstPacketTracker::new();
        tracker.record_subscribe_at(
            42,
            ExchangeSegment::NseFno,
            DepthFeedKind::Twenty,
            T0,
            false,
        );
        assert_eq!(tracker.pending_len(), 1);
        let measured = tracker.observe_at(
            42,
            ExchangeSegment::NseFno.binary_code(),
            DepthFeedKind::Twenty,
            T0 + 350 * NANOS_PER_MILLI,
        );
        assert_eq!(measured, Some(350 * NANOS_PER_MILLI));
        assert_eq!(tracker.pending_len(), 0, "the entry is consumed");
    }

    #[test]
    fn only_the_first_packet_measures_and_the_rest_are_free() {
        let tracker = DepthFirstPacketTracker::new();
        tracker.record_subscribe_at(
            42,
            ExchangeSegment::NseFno,
            DepthFeedKind::Twenty,
            T0,
            false,
        );
        assert!(
            tracker
                .observe_at(
                    42,
                    ExchangeSegment::NseFno.binary_code(),
                    DepthFeedKind::Twenty,
                    T0 + NANOS_PER_MILLI
                )
                .is_some()
        );
        for _ in 0..5 {
            assert_eq!(
                tracker.observe_at(
                    42,
                    ExchangeSegment::NseFno.binary_code(),
                    DepthFeedKind::Twenty,
                    T0 + NANOS_PER_SEC
                ),
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
        tracker.record_subscribe_at(
            27,
            ExchangeSegment::NseFno,
            DepthFeedKind::Twenty,
            T0,
            false,
        );
        assert_eq!(
            tracker.observe_at(
                27,
                ExchangeSegment::BseFno.binary_code(),
                DepthFeedKind::Twenty,
                T0 + NANOS_PER_SEC
            ),
            None
        );
        assert_eq!(
            tracker.pending_len(),
            1,
            "the NSE subscribe is still awaited"
        );
        assert!(
            tracker
                .observe_at(
                    27,
                    ExchangeSegment::NseFno.binary_code(),
                    DepthFeedKind::Twenty,
                    T0 + NANOS_PER_SEC
                )
                .is_some()
        );
    }

    #[test]
    fn sweep_expired_at_gives_up_on_a_contract_dark_past_the_window() {
        let tracker = DepthFirstPacketTracker::new();
        tracker.record_subscribe_at(7, ExchangeSegment::NseFno, DepthFeedKind::Twenty, T0, false);
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
        tracker.record_subscribe_at(7, ExchangeSegment::NseFno, DepthFeedKind::Twenty, T0, false);
        tracker.record_subscribe_at(
            7,
            ExchangeSegment::NseFno,
            DepthFeedKind::Twenty,
            T0 + 10 * NANOS_PER_SEC,
            false,
        );
        assert_eq!(tracker.pending_len(), 1, "one contract, one entry");
        let measured = tracker.observe_at(
            7,
            ExchangeSegment::NseFno.binary_code(),
            DepthFeedKind::Twenty,
            T0 + 11 * NANOS_PER_SEC,
        );
        assert_eq!(
            measured,
            Some(NANOS_PER_SEC),
            "measured from the LATEST subscribe, not the first"
        );
    }

    /// ⚠ REPLACES `a_backward_clock_step_yields_zero_never_a_negative_sample`
    /// (2026-09-11). That test asserted `Some(0)` — it pinned the CLAMP, and
    /// the clamp is the defect: the drain's clock is deliberately back-dated
    /// by ring dwell while the stamp is a raw `now()`, so a packet received
    /// before the dispatch and drained after is ROUTINE under a backlog, and
    /// counting it as a 0 ms `arrived` fabricates the fastest possible sample
    /// on the instrument's own fast end.
    #[test]
    fn a_receipt_that_precedes_the_subscribe_is_refused_not_recorded_as_zero() {
        let tracker = DepthFirstPacketTracker::new();
        tracker.record_subscribe_at(7, ExchangeSegment::NseFno, DepthFeedKind::Twenty, T0, false);
        assert_eq!(
            tracker.observe_at(
                7,
                ExchangeSegment::NseFno.binary_code(),
                DepthFeedKind::Twenty,
                T0 - NANOS_PER_SEC
            ),
            None,
            "a back-dated receipt is an out-of-order sample, never a 0 ms arrival"
        );
        // CONSUMED regardless: the contract HAS delivered a packet, so it is
        // not dark and must never later age into `silent_window`.
        assert_eq!(tracker.pending_len(), 0);
        assert_eq!(
            tracker.sweep_expired_at(T0 + (FIRST_PACKET_WINDOW_SECS + 1) * NANOS_PER_SEC),
            0,
            "a reordered arrival must not also be swept as a silent window"
        );
    }

    /// The CRITICAL from the 2026-09-11 permutation sweep: `Key` carries the
    /// pool byte but not the SOCKET, so a contract still streaming from the
    /// socket it was dropped from — Dhan ignored the unsubscribe, proven for
    /// both codes — answers its own fresh stamp with a fabricated ~0 ms.
    #[test]
    fn a_contract_that_may_still_be_streaming_is_not_measured_at_all() {
        let tracker = DepthFirstPacketTracker::new();
        tracker.record_subscribe_at(7, ExchangeSegment::NseFno, DepthFeedKind::Twenty, T0, true);
        assert_eq!(
            tracker.pending_len(),
            0,
            "no stamp may exist for a contract a live ghost stream could answer"
        );
        assert_eq!(
            tracker.observe_at(
                7,
                ExchangeSegment::NseFno.binary_code(),
                DepthFeedKind::Twenty,
                T0 + NANOS_PER_SEC
            ),
            None,
            "the very next packet must resolve nothing"
        );
    }

    /// A swap that reached a CHANNEL but that the wire then refused was
    /// stamped and left to age into `silent_window` — a dark-window report for
    /// a subscribe that never happened.
    #[test]
    fn forget_stops_a_wire_refused_swap_aging_into_a_false_silent_window() {
        let tracker = DepthFirstPacketTracker::new();
        tracker.record_subscribe_at(7, ExchangeSegment::NseFno, DepthFeedKind::Twenty, T0, false);
        assert!(tracker.forget(7, ExchangeSegment::NseFno, DepthFeedKind::Twenty));
        assert_eq!(tracker.pending_len(), 0);
        assert_eq!(
            tracker.sweep_expired_at(T0 + (FIRST_PACKET_WINDOW_SECS + 1) * NANOS_PER_SEC),
            0
        );
        // Idempotent: a second reconcile of the same ack must not underflow
        // the advisory count.
        assert!(!tracker.forget(7, ExchangeSegment::NseFno, DepthFeedKind::Twenty));
        // And it is keyed on the WIRE code like every other entry point.
        tracker.record_subscribe_at(
            9,
            ExchangeSegment::BseFno,
            DepthFeedKind::TwoHundred,
            T0,
            false,
        );
        assert!(tracker.forget(9, ExchangeSegment::BseFno, DepthFeedKind::TwoHundred));
    }

    #[test]
    fn record_subscribe_at_is_bounded_and_the_refusal_is_counted() {
        let tracker = DepthFirstPacketTracker::new();
        for id in 0..(MAX_PENDING as u64 + 50) {
            tracker.record_subscribe_at(
                id,
                ExchangeSegment::NseFno,
                DepthFeedKind::Twenty,
                T0,
                false,
            );
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
            tracker.record_subscribe_at(
                id,
                ExchangeSegment::NseFno,
                DepthFeedKind::Twenty,
                T0,
                false,
            );
        }
        tracker.record_subscribe_at(
            0,
            ExchangeSegment::NseFno,
            DepthFeedKind::Twenty,
            T0 + 5 * NANOS_PER_SEC,
            false,
        );
        assert_eq!(tracker.pending_len(), MAX_PENDING);
        assert_eq!(
            tracker.observe_at(
                0,
                ExchangeSegment::NseFno.binary_code(),
                DepthFeedKind::Twenty,
                T0 + 6 * NANOS_PER_SEC
            ),
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
        tracker.record_subscribe_at(1, ExchangeSegment::NseFno, DepthFeedKind::Twenty, T0, false);
        tracker.record_subscribe_at(2, ExchangeSegment::NseFno, DepthFeedKind::Twenty, T0, false);
        assert_eq!(tracker.pending_len(), 2);
        tracker.observe_at(
            1,
            ExchangeSegment::NseFno.binary_code(),
            DepthFeedKind::Twenty,
            T0 + NANOS_PER_MILLI,
        );
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
        assert_eq!(FIRST_PACKET_OUTCOMES.len(), 5);
        assert!(FIRST_PACKET_OUTCOMES.contains(&"arrived"));
        assert!(FIRST_PACKET_OUTCOMES.contains(&"silent_window"));
        assert!(FIRST_PACKET_OUTCOMES.contains(&"refused"));
        // The two added 2026-09-11 with the fabricated-zero fixes. Both are
        // emitted INSTEAD of a sample, so an unseeded series would make "we
        // declined to measure" indistinguishable from "nothing happened".
        assert!(FIRST_PACKET_OUTCOMES.contains(&"unmeasurable"));
        assert!(FIRST_PACKET_OUTCOMES.contains(&"reordered"));
        pre_register_first_packet_counters();
    }

    /// The bug this pins: `record_subscribe_at` keyed on `segment as u8`
    /// while the drain looks a packet up by `header.exchange_segment_code`,
    /// the WIRE byte. Dhan's annexure has no enum 6 or 7 — `BSE_FNO` is 8 —
    /// so the two diverge for the last two variants and a BSE_FNO subscribe
    /// could NEVER be resolved. The old I-P1-11 test used `as u8` on both
    /// sides, so it compared 7 against 7 and asserted a self-consistent
    /// falsehood.
    #[test]
    fn the_stamp_key_is_the_wire_code_for_every_segment() {
        for segment in [
            ExchangeSegment::IdxI,
            ExchangeSegment::NseEquity,
            ExchangeSegment::NseFno,
            ExchangeSegment::NseCurrency,
            ExchangeSegment::BseEquity,
            ExchangeSegment::McxComm,
            ExchangeSegment::BseCurrency,
            ExchangeSegment::BseFno,
        ] {
            let tracker = DepthFirstPacketTracker::new();
            tracker.record_subscribe_at(7, segment, DepthFeedKind::Twenty, T0, false);
            assert_eq!(
                tracker.observe_at(
                    7,
                    segment.binary_code(),
                    DepthFeedKind::Twenty,
                    T0 + NANOS_PER_MILLI
                ),
                Some(NANOS_PER_MILLI),
                "a {segment:?} packet must resolve a {segment:?} subscribe — \
                 the stamp must key on binary_code(), never the discriminant"
            );
        }
    }

    /// Both depth pools draw from the same gainer-ordered list, so one
    /// contract is routinely held by depth-200 while depth-20 is subscribing
    /// it. Without the pool byte the already-streaming socket's next packet
    /// would answer the other pool's fresh stamp and record a fabricated
    /// near-zero latency — an error in the reassuring direction.
    #[test]
    fn a_packet_on_the_other_pool_does_not_resolve_this_pools_subscribe() {
        let tracker = DepthFirstPacketTracker::new();
        tracker.record_subscribe_at(
            42,
            ExchangeSegment::NseFno,
            DepthFeedKind::Twenty,
            T0,
            false,
        );
        assert_eq!(
            tracker.observe_at(
                42,
                ExchangeSegment::NseFno.binary_code(),
                DepthFeedKind::TwoHundred,
                T0 + NANOS_PER_MILLI
            ),
            None,
            "a depth-200 packet must not resolve a depth-20 subscribe"
        );
        assert_eq!(
            tracker.pending_len(),
            1,
            "the depth-20 subscribe is still awaited"
        );
        assert_eq!(
            tracker.observe_at(
                42,
                ExchangeSegment::NseFno.binary_code(),
                DepthFeedKind::Twenty,
                T0 + NANOS_PER_MILLI
            ),
            Some(NANOS_PER_MILLI),
            "its own pool still resolves it"
        );
    }

    #[test]
    fn elapsed_nanos_to_millis_keeps_sub_millisecond_resolution() {
        // The integer form recorded every one of these as 0.0.
        assert!((elapsed_nanos_to_millis(999_999) - 0.999_999).abs() < 1e-9);
        assert!((elapsed_nanos_to_millis(1_000) - 0.001).abs() < 1e-12);
        assert!(
            elapsed_nanos_to_millis(500_000) > 0.0,
            "a half-millisecond arrival must not read as zero"
        );
        // and the ordinary range is unchanged
        assert!((elapsed_nanos_to_millis(350 * NANOS_PER_MILLI) - 350.0).abs() < 1e-9);
        assert_eq!(elapsed_nanos_to_millis(0), 0.0);
    }
}
