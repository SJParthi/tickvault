//! What the depth pools are ACTUALLY holding, readable from the tick drain.
//!
//! # Why this exists
//!
//! `top_volume_rank` carries a `subscribed` column, and its own module docs
//! call that column the thing that earns the table: it is the difference
//! between "we ranked it first" and "we were watching it". One query then
//! audits the whole steering decision.
//!
//! The answer lives in the depth connection tasks. `Depth20LiveSocket::held`
//! and `RebalanceSocket::held` are the wire truth, and both are owned `&mut`
//! by their own per-minute loops — physically unreachable from the frame
//! drain, which is where the snapshot is taken.
//!
//! Writing `false` there would not be a placeholder, it would be a lie in
//! every row: contracts that DO hold a socket would be recorded as unwatched,
//! and the audit the column exists for would read backwards. That is the
//! false-OK class this repository keeps removing, so the honest fix is to
//! publish what the pools hold rather than to guess it.
//!
//! # Why TWO slots and not one merged set
//!
//! Depth-20 and depth-200 are steered by two INDEPENDENT loops. A single
//! `ArcSwap` written by both would have each publisher overwrite the other's
//! contribution on every minute — the last writer wins and the other pool's
//! instruments read as unsubscribed until it publishes again. Two slots, one
//! owner each, and a read that checks both: no clobbering is possible because
//! no slot has two writers.
//!
//! # Complexity
//!
//! * publish — O(n) in that pool's held instruments (≤250 for depth-20, ≤5
//!   for depth-200), once per minute, on the steering loop's own task. Never
//!   on the drain.
//! * read — **O(1)**: two `ArcSwap` loads and at most two hash probes.
//!
//! The read is what sits on the snapshot path, and it is the one that has to
//! be O(1). It allocates nothing: `ArcSwap::load` yields a guard over the
//! existing `Arc`, and `HashSet::contains` borrows the key.
//!
//! # What a stale read means, stated rather than hidden
//!
//! The view is refreshed once per minute by each pool, so a snapshot taken
//! between a swap and the next publish reports the PREVIOUS minute's set. The
//! error is bounded by one steering interval and is in the direction that
//! matters least: a contract that has just been swapped in reads `false` for
//! under a minute, which understates coverage rather than overstating it. A
//! reader auditing "did the heaviest contract get a socket?" is therefore
//! never told yes when the answer was no.

use std::collections::HashSet;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tickvault_common::types::ExchangeSegment;

/// The I-P1-11 composite key. `security_id` ALONE is not unique — Dhan reuses
/// the same numeric id across segments — so the set is keyed on the pair, and
/// the segment is stored as its wire byte so the key is `Copy` and hashes
/// without touching an enum's derive.
type Key = (u64, u8);

/// One pool's published set, plus the two-slot view over both pools.
///
/// Cloned by `Arc` at the boot site: the two steering loops each hold a handle
/// and write their own slot, the drain holds a handle and only reads.
#[derive(Debug, Default)]
pub struct DepthSubscriptionView {
    /// What the five depth-20 sockets hold. Written ONLY by the depth-20
    /// tracking loop.
    depth20: ArcSwap<HashSet<Key>>,
    /// What the five depth-200 sockets hold. Written ONLY by the depth-200
    /// rebalance loop.
    depth200: ArcSwap<HashSet<Key>>,
}

impl DepthSubscriptionView {
    /// An empty view — nothing subscribed, which is the truthful state before
    /// either pool has dialled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the depth-20 slot with what that pool now holds.
    ///
    /// Takes an iterator rather than a slice so the caller can feed it
    /// straight from `Depth20LiveSocket::held` across five sockets without
    /// building an intermediate `Vec`.
    pub fn publish_depth20<I>(&self, held: I)
    where
        I: IntoIterator<Item = (u64, ExchangeSegment)>,
    {
        self.depth20.store(Arc::new(Self::collect(held)));
    }

    /// Replaces the depth-200 slot with what that pool now holds.
    pub fn publish_depth200<I>(&self, held: I)
    where
        I: IntoIterator<Item = (u64, ExchangeSegment)>,
    {
        self.depth200.store(Arc::new(Self::collect(held)));
    }

    fn collect<I>(held: I) -> HashSet<Key>
    where
        I: IntoIterator<Item = (u64, ExchangeSegment)>,
    {
        held.into_iter()
            .map(|(id, segment)| (id, segment.binary_code()))
            .collect()
    }

    /// Whether this instrument holds a depth subscription on EITHER pool.
    ///
    /// O(1): two lock-free loads and at most two hash probes. This is the call
    /// that sits on the snapshot path.
    // WIRING-EXEMPT: the READER lands with the drain's snapshot arm, which is
    // the next change. It is written and tested here rather than there because
    // the PUBLISH half has to exist first -- a snapshot arm that reads an
    // unpublished view would record `subscribed = false` for every row, which
    // is the exact lie this module was built to prevent. The two publishers
    // above ARE wired, so the dormant surface is one accessor, not a subsystem.
    #[must_use]
    pub fn is_subscribed(&self, security_id: u64, segment: ExchangeSegment) -> bool {
        let key = (security_id, segment.binary_code());
        self.depth20.load().contains(&key) || self.depth200.load().contains(&key)
    }

    /// How many instruments each pool last published, as `(depth20, depth200)`.
    ///
    /// For the boot log and the tests. Deliberately NOT summed: the two pools
    /// have different budgets (250 and 5) and a single total would hide a pool
    /// that published nothing behind the other pool's count — which is exactly
    /// the state an operator needs to see.
    #[must_use]
    pub fn published_counts(&self) -> (usize, usize) {
        (self.depth20.load().len(), self.depth200.load().len())
    }
}

/// The process-wide view.
///
/// A global for the same reason `seal_writer_runner::global_seal_sender` is
/// one: the writer and the reader sit on opposite sides of three `tokio::spawn`
/// boundaries whose signatures already carry a dozen arguments each, and
/// threading a handle through them would put the wiring's cost in the places
/// least related to it.
///
/// The ownership argument that makes the two slots safe is UNAFFECTED by this
/// being global: each slot still has exactly one writer, and the accessor
/// hands out a shared reference rather than a mutable one. `run_depth_rebalance`
/// still takes its view as an explicit parameter -- the global is used only at
/// the spawn boundary -- so the loop stays testable against a private instance.
///
/// Defaults to an EMPTY view, which is the truthful answer before either pool
/// dials: nothing is subscribed yet.
#[must_use]
pub fn global_depth_subscription_view() -> &'static Arc<DepthSubscriptionView> {
    static VIEW: std::sync::OnceLock<Arc<DepthSubscriptionView>> = std::sync::OnceLock::new();
    VIEW.get_or_init(|| Arc::new(DepthSubscriptionView::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NSE_FNO: ExchangeSegment = ExchangeSegment::NseFno;
    const IDX: ExchangeSegment = ExchangeSegment::IdxI;

    #[test]
    fn new_starts_with_nothing_subscribed() {
        // The truthful pre-dial state. A view that defaulted to `true` would
        // claim coverage the process has not got.
        let view = DepthSubscriptionView::new();
        assert!(!view.is_subscribed(42, NSE_FNO));
        assert_eq!(view.published_counts(), (0, 0));
    }

    #[test]
    fn publish_depth20_makes_an_instrument_read_subscribed() {
        let view = DepthSubscriptionView::new();
        view.publish_depth20([(42, NSE_FNO), (43, NSE_FNO)]);
        assert!(view.is_subscribed(42, NSE_FNO));
        assert!(view.is_subscribed(43, NSE_FNO));
        assert!(!view.is_subscribed(44, NSE_FNO));
    }

    #[test]
    fn is_subscribed_keys_on_the_composite_so_a_segment_collision_cannot_answer() {
        // I-P1-11: Dhan reuses the same numeric id across segments. A set keyed
        // on the bare id would report an index as subscribed because a
        // same-numbered option is.
        let view = DepthSubscriptionView::new();
        view.publish_depth20([(27, NSE_FNO)]);
        assert!(view.is_subscribed(27, NSE_FNO));
        assert!(
            !view.is_subscribed(27, IDX),
            "id 27 on IDX_I is a DIFFERENT instrument and holds no depth socket"
        );
    }

    #[test]
    fn publish_depth200_does_not_clobber_the_depth20_slot() {
        // The whole reason for two slots. With one shared slot, whichever loop
        // published last would erase the other pool's instruments.
        let view = DepthSubscriptionView::new();
        view.publish_depth20([(1, NSE_FNO)]);
        view.publish_depth200([(2, NSE_FNO)]);
        assert!(
            view.is_subscribed(1, NSE_FNO),
            "depth-20 survived the depth-200 publish"
        );
        assert!(view.is_subscribed(2, NSE_FNO));
        // And in the other order.
        view.publish_depth200([(3, NSE_FNO)]);
        assert!(view.is_subscribed(1, NSE_FNO));
        assert!(
            !view.is_subscribed(2, NSE_FNO),
            "a republish REPLACES its own slot"
        );
        assert!(view.is_subscribed(3, NSE_FNO));
    }

    #[test]
    fn a_republish_replaces_rather_than_accumulates() {
        // A swap removes an instrument as well as adding one. If publish
        // merged instead of replacing, a contract swapped OFF the wire would
        // read subscribed forever — the overstating direction, which is the
        // one that makes the audit column lie.
        let view = DepthSubscriptionView::new();
        view.publish_depth20([(1, NSE_FNO), (2, NSE_FNO)]);
        view.publish_depth20([(2, NSE_FNO), (3, NSE_FNO)]);
        assert!(
            !view.is_subscribed(1, NSE_FNO),
            "the swapped-off leg is gone"
        );
        assert!(view.is_subscribed(2, NSE_FNO));
        assert!(view.is_subscribed(3, NSE_FNO));
        assert_eq!(view.published_counts(), (2, 0));
    }

    #[test]
    fn publishing_an_empty_set_clears_the_slot() {
        // An empty depth pool is a real state (nothing steerable dialled), and
        // it must read as "nothing subscribed" rather than keeping the last
        // non-empty set alive.
        let view = DepthSubscriptionView::new();
        view.publish_depth20([(1, NSE_FNO)]);
        view.publish_depth20(std::iter::empty());
        assert!(!view.is_subscribed(1, NSE_FNO));
        assert_eq!(view.published_counts(), (0, 0));
    }

    #[test]
    fn published_counts_stay_separate_so_a_silent_pool_is_visible() {
        let view = DepthSubscriptionView::new();
        view.publish_depth20([(1, NSE_FNO), (2, NSE_FNO), (3, NSE_FNO)]);
        // depth-200 never published: the pair must still say so.
        assert_eq!(view.published_counts(), (3, 0));
    }

    #[test]
    fn global_depth_subscription_view_is_one_view_and_starts_empty() {
        // Two calls must hand back the SAME view: two views would mean the
        // depth loops publish into one and the drain reads the other, which
        // fails silently as "nothing is ever subscribed".
        let a = global_depth_subscription_view();
        let b = global_depth_subscription_view();
        assert!(std::sync::Arc::ptr_eq(a, b));
        // Empty before anything publishes -- the truthful pre-dial answer.
        assert!(!a.is_subscribed(u64::MAX, NSE_FNO));
    }
}
