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
//! Depth-20 and depth-200 are steered independently. A single `ArcSwap`
//! written by both would have each publisher overwrite the other's
//! contribution on every minute — the last writer wins and the other pool's
//! instruments read as unsubscribed until it publishes again. Two slots with
//! one owner each, and a read that checks both, removes that clobbering.
//!
//! # Concurrency: the invariant is ONE WRITER TASK, not one writer per slot
//!
//! ⚠ CORRECTED 2026-09-09. This section used to close with *"no clobbering is
//! possible because no slot has two writers"*, and to describe the pools as
//! *"two INDEPENDENT loops"*. Both were wrong, in opposite directions, and
//! together they read as a safety proof that does not hold:
//!
//! * **It does not cover three of the five fields.** `depth20` and `depth200`
//!   do have one publisher each. But `dropped`, `published_once` and
//!   `last_publish_secs` are written by BOTH publishers — the `dropped` field
//!   doc says so itself — and EVERY write in this module is a
//!   load-rebuild-store on an `ArcSwap`, which is not atomic. Two concurrent
//!   publishers would lose drops.
//! * **There are not two loops.** There is ONE. Both publishes are made by
//!   `depth_rebalance::publish_depth_subscriptions`' single caller — see the
//!   real invariant below — so the present-tense "independent loops" claim
//!   described a topology the code does not have, which is exactly what makes
//!   a future split look free when it is not.
//!
//! The invariant this module actually relies on is therefore stronger and is
//! stated here rather than inferred: **all publishing happens on the single
//! `run_depth_rebalance` task.** `publish_depth_subscriptions`
//! (`depth_rebalance.rs`) is the only production caller of either
//! `publish_depth20*` or `publish_depth200*`, and it is called from two points
//! in one loop body, so the two publishes are serialised by construction.
//! Pinned by `the_view_has_exactly_one_publishing_call_site`.
//!
//! **Splitting depth-20 and depth-200 onto separate tasks is NOT a free
//! change.** It requires `ArcSwap::rcu` on all three `ArcSwap` fields first —
//! and the counter and `warn!` in `record_dropped` must be hoisted OUT of the
//! retry closure before that, or a contended retry double-reports the refusal.
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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tickvault_core::parser::depth::DepthFeedKind;

use arc_swap::ArcSwap;
use tickvault_common::types::ExchangeSegment;

/// The I-P1-11 composite key. `security_id` ALONE is not unique — Dhan reuses
/// the same numeric id across segments — so the set is keyed on the pair, and
/// the segment is stored as its wire byte so the key is `Copy` and hashes
/// without touching an enum's derive.
type Key = (u64, u8);
/// What the drain concludes about one depth packet's instrument, against the
/// last published sets.
///
/// Only [`DepthFrameClass::Ghost`] is actionable, and it is deliberately the
/// NARROW verdict: an instrument this process itself told a socket to drop,
/// whose grace has elapsed, and which is still being delivered. An instrument
/// the view has never held reads `Unknown`, never `Ghost` — a contract swapped
/// IN between two publishes is exactly that shape for under a minute, and
/// treating it as a ghost would redial a healthy socket on every swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepthFrameClass {
    /// In a published held set — a normal frame.
    Held,
    /// Dropped by a publish less than [`GHOST_GRACE_SECS`] ago. The vendor is
    /// allowed a moment to act on the unsubscribe; frames in this window are
    /// counted but earn no redial.
    RecentlyDropped,
    /// Dropped by a publish at least [`GHOST_GRACE_SECS`] ago and STILL
    /// arriving: the unsubscribe was ignored or lost. The one verdict that
    /// asks the socket to redial.
    Ghost,
    /// Not held and not recently dropped, or no publish has happened yet.
    Unknown,
}

/// How long after a publish dropped an instrument its frames still count as
/// "recently dropped" rather than "ghost". Re-exported from the connection
/// supervisor so the drain and the redial register agree on one number.
pub const GHOST_GRACE_SECS: i64 = tickvault_core::websocket::pool_supervisor::GHOST_GRACE_SECS;

/// How long a dropped instrument stays in the dropped map before a publish
/// evicts it. Longer than the redial cooldown (180 s) by design: a ghost that
/// survives its first redial must still read as a ghost at the next one, and
/// the map is the only memory of what this process ever dropped.
pub const DROPPED_RETENTION_SECS: i64 = 600;

/// Oldest LAST PUBLISH a ghost verdict may be reached against, in seconds.
///
/// Two graces. Each pool publishes once a minute, so a healthy steering task
/// keeps this age under ~60 s; a frame clock 180 s past the last publish is
/// a forward clock step or a dead steering loop, and in either case the
/// dropped map is not evidence a redial can act on. The bound holds a
/// forward NTP step of up to 180 s to at most the drops that were ALREADY
/// past the grace, and refuses everything beyond it.
pub const GHOST_VERDICT_MAX_PUBLISH_AGE_SECS: i64 = 2 * GHOST_GRACE_SECS;

/// Hard cap on the dropped map. A publish drops at most a socket's worth of
/// instruments per pool per minute (≤ 4 swaps × 5 sockets × 2 pools), so ten
/// minutes of retention is a few hundred entries; the cap is a fail-closed
/// bound against a shape nobody has designed, never a size that is expected.
pub const MAX_DROPPED_TRACKED: usize = 4096;

/// Counter: dropped-map inserts refused at [`MAX_DROPPED_TRACKED`]. Expected 0.
pub const DROPPED_REFUSED_COUNTER: &str = "tv_depth_view_dropped_refused_total";

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
    /// Instruments that LEFT either slot at a publish, stamped with the epoch
    /// seconds of that publish. The drain's ghost detector reads it; only the
    /// two publishers write it. Bounded by [`MAX_DROPPED_TRACKED`] and evicted
    /// after [`DROPPED_RETENTION_SECS`].
    dropped: ArcSwap<HashMap<Key, i64>>,
    /// Set by the first publish. Before it, every classification is `Unknown`:
    /// the boot dial fills sockets before either loop has published, and a
    /// detector that read that window as "ghost" would redial healthy sockets.
    published_once: AtomicBool,
    /// Epoch seconds of the LAST publish by either pool. A ghost verdict is
    /// refused when the frame's clock has run more than
    /// [`GHOST_VERDICT_MAX_PUBLISH_AGE_SECS`] past it — see that constant.
    last_publish_secs: std::sync::atomic::AtomicI64,
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
        self.publish_depth20_at(held, chrono::Utc::now().timestamp());
    }

    /// [`Self::publish_depth20`] with an explicit receipt clock, so the dropped
    /// map's timestamps are testable without waiting on the wall clock.
    pub fn publish_depth20_at<I>(&self, held: I, now_secs: i64)
    where
        I: IntoIterator<Item = (u64, ExchangeSegment)>,
    {
        let next = Self::collect(held);
        let previous = self.depth20.load_full();
        let _refused = self.record_dropped(&previous, &next, now_secs);
        self.depth20.store(Arc::new(next));
    }

    /// Replaces the depth-200 slot with what that pool now holds.
    pub fn publish_depth200<I>(&self, held: I)
    where
        I: IntoIterator<Item = (u64, ExchangeSegment)>,
    {
        self.publish_depth200_at(held, chrono::Utc::now().timestamp());
    }

    /// [`Self::publish_depth200`] with an explicit receipt clock.
    pub fn publish_depth200_at<I>(&self, held: I, now_secs: i64)
    where
        I: IntoIterator<Item = (u64, ExchangeSegment)>,
    {
        let next = Self::collect(held);
        let previous = self.depth200.load_full();
        let _refused = self.record_dropped(&previous, &next, now_secs);
        self.depth200.store(Arc::new(next));
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
    /// Classifies one depth packet's instrument against what the pools hold,
    /// for the ghost-instrument detector on the drain.
    ///
    /// O(1): two lock-free loads, at most three hash probes, no allocation.
    /// Reads the segment as its wire byte so the drain never has to map it to
    /// an enum first. `now_secs` is the frame's receipt clock in epoch seconds.
    #[must_use]
    pub fn classify_raw(
        &self,
        security_id: u64,
        segment_code: u8,
        now_secs: i64,
    ) -> DepthFrameClass {
        if !self
            .published_once
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return DepthFrameClass::Unknown;
        }
        let key = (security_id, segment_code);
        if self.depth20.load().contains(&key) || self.depth200.load().contains(&key) {
            return DepthFrameClass::Held;
        }
        match self.dropped.load().get(&key) {
            Some(dropped_at) if now_secs.saturating_sub(*dropped_at) < GHOST_GRACE_SECS => {
                DepthFrameClass::RecentlyDropped
            }
            // A ghost verdict needs a TRUSTWORTHY "now". The steering loops
            // publish every minute, so a frame clock more than two graces
            // past the last publish means either the wall clock stepped
            // forward (an NTP correction makes every recent drop read as
            // ghost at once — a redial storm on healthy sockets) or the
            // steering task is dead (then no drop is fresh and the map is
            // stale). Both refuse, in the safe direction: no redial on a
            // clock nobody has confirmed. Found by the 2026-09-08 hostile
            // sweep (finding #21).
            Some(_)
                if now_secs.saturating_sub(
                    self.last_publish_secs
                        .load(std::sync::atomic::Ordering::Acquire),
                ) > GHOST_VERDICT_MAX_PUBLISH_AGE_SECS =>
            {
                DepthFrameClass::Unknown
            }
            Some(_) => DepthFrameClass::Ghost,
            None => DepthFrameClass::Unknown,
        }
    }

    /// Could a packet for this contract ALREADY be in flight on THIS pool?
    ///
    /// The gate on the first-packet instrument. Its key carries no socket
    /// identity, so an in-flight packet from the socket a contract was DROPPED
    /// from answers the stamp of a fresh subscribe on a DIFFERENT socket, at a
    /// fabricated ~0 ms.
    ///
    /// ⚠ This exists because using [`Self::classify_raw`] for the same job was
    /// MEASURED to disable one pool outright (2026-09-11, two independent
    /// agents, from different directions). `classify_raw` answers `Held` from
    /// the UNION of both pools — and both boards are cut from ONE
    /// volume-ordered slice, so depth-200's entry set (top 5 distinct
    /// underlyings) is by construction almost always inside depth-20's top
    /// 250. Roughly **19 of every 20 depth-200 stamps** were refused as
    /// "already held" while `arrived` and `silent_window` read healthy: the
    /// same false-OK shape as the fabricated zero it replaced, one level up.
    /// The docblock calling that over-refusal "harmless" was wrong — it was
    /// TOTAL for one pool.
    ///
    /// So the held check is PER POOL. The dropped check deliberately is not:
    /// a drop is recorded without the pool that made it, and a contract still
    /// streaming after an ignored unsubscribe is unsafe to measure whichever
    /// pool dropped it.
    ///
    /// **HONEST LIMIT — the ghost tail outlives this answer.** The dropped map
    /// evicts at [`DROPPED_RETENTION_SECS`], and a socket that reaches its
    /// ghost-redial session ceiling stands down and lets the contract stream
    /// for the REST OF THE SESSION (measured 2026-09-10 and 2026-09-11:
    /// redials exhausted ~10:21–10:46 IST, ghosts continuing ~5 h). Past that
    /// retention this returns `false` and the fabricated zero is reachable
    /// again. Closing it needs state this process does not keep — the socket
    /// index in the stamp key, or a longer-lived "known ghosting" set — and is
    /// RECORDED rather than guessed at a third time.
    ///
    /// O(1): one `Acquire` load, one `ArcSwap` load per side, one set probe
    /// and one map probe. No allocation. Steering task only, never the drain.
    #[must_use]
    pub fn may_already_be_streaming(
        &self,
        security_id: u64,
        segment_code: u8,
        pool: DepthFeedKind,
        now_secs: i64,
    ) -> bool {
        if !self
            .published_once
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return false;
        }
        let key = (security_id, segment_code);
        let held_here = match pool {
            DepthFeedKind::Twenty => self.depth20.load().contains(&key),
            DepthFeedKind::TwoHundred => self.depth200.load().contains(&key),
        };
        if held_here {
            return true;
        }
        matches!(
            self.dropped.load().get(&key),
            Some(dropped_at) if now_secs.saturating_sub(*dropped_at) < DROPPED_RETENTION_SECS
        )
    }

    /// How many instruments the dropped map currently remembers. For tests
    /// and the boot log; bounded by [`MAX_DROPPED_TRACKED`].
    #[must_use]
    pub fn dropped_count(&self) -> usize {
        self.dropped.load().len()
    }

    /// Records every instrument that LEFT a slot at this publish, stamped with
    /// `now_secs`, and evicts entries older than [`DROPPED_RETENTION_SECS`].
    ///
    /// O(old + dropped) per publish — the previous set and the retained map are
    /// both walked once. Cold: once a minute per pool on the steering task,
    /// bounded by the pool budgets and [`MAX_DROPPED_TRACKED`]. The READ side
    /// (`classify_raw`) stays O(1).
    ///
    /// Returns the number of keys REFUSED by the cap. That count is what the
    /// counter reports and what the tests assert on: a `metrics::counter!` is
    /// process-global and cannot be read back, so a return value is the only
    /// way to pin "one increment per KEY" rather than one per publish.
    // O(1) EXEMPT: cold per-minute steering path, bounded by the pool budgets
    // (≤ 250 + ≤ 5 held) and MAX_DROPPED_TRACKED; the hot-path reader is O(1).
    fn record_dropped(&self, previous: &HashSet<Key>, next: &HashSet<Key>, now_secs: i64) -> u64 {
        let current = self.dropped.load();
        let mut map: HashMap<Key, i64> = HashMap::with_capacity(
            current
                .len()
                .saturating_add(previous.len())
                .min(MAX_DROPPED_TRACKED),
        );
        for (key, dropped_at) in current.iter() {
            // Evict what has aged out, and forget a drop that a later publish
            // re-held (it is in `next`): `classify_raw` checks held first, so
            // this is hygiene rather than correctness, but it keeps the map's
            // size honest.
            if now_secs.saturating_sub(*dropped_at) < DROPPED_RETENTION_SECS && !next.contains(key)
            {
                map.insert(*key, *dropped_at);
            }
        }
        let mut refused: u64 = 0;
        for key in previous.iter() {
            if next.contains(key) {
                continue;
            }
            // The NEWER drop wins. A contract can sit in BOTH pools (the top
            // five are normally inside the top 250): depth-20 drops it at T,
            // depth-200 drops it at T+300. Keeping the T stamp made the first
            // frames after the SECOND unsubscribe read as ghosts — the grace
            // had long elapsed against a drop that was not the one the socket
            // was still honouring — and redialled a healthy socket. Found by
            // the 2026-09-08 hostile sweep. Overwriting never grows the map,
            // so the cap below is only consulted for a NEW key.
            if let Some(stamp) = map.get_mut(key) {
                *stamp = now_secs;
                continue;
            }
            if map.len() >= MAX_DROPPED_TRACKED {
                // Fail-closed: refuse to remember more rather than grow
                // without bound. A refused entry can never read as Ghost,
                // which is the safe direction (no redial on a guess).
                //
                // CORRECTED 2026-09-09, two defects in these four lines:
                //
                // (1) It counted the PUBLISH, not the KEYS -- one increment
                //     however many instruments were forgotten. The depth-20
                //     budget is DEPTH20_ENTRY_RANKS (250), and the retained
                //     map can already be AT the cap when the loop starts, so
                //     a single publish could forget 250 instruments and tell
                //     the operator `1`. The counter's own doc says "inserts
                //     refused", i.e. per-key was always the intended reading;
                //     the code was what disagreed.
                //
                // (2) It `break`ed. That abandoned the `get_mut` stamp
                //     refresh ABOVE for every remaining key -- silently
                //     defeating the newer-drop-wins rule documented there,
                //     and reviving the healthy-socket redial that rule was
                //     written to stop (2026-09-08 hostile sweep). Refreshing
                //     an existing stamp never grows the map, so the cap is no
                //     reason to stop walking.
                //
                // Count per key, keep walking, and report once at the end so
                // one publish still costs one log line.
                refused = refused.saturating_add(1);
                continue;
            }
            map.insert(*key, now_secs);
        }
        if refused > 0 {
            metrics::counter!(DROPPED_REFUSED_COUNTER).increment(refused);
            tracing::warn!(
                code =
                    tickvault_common::error_code::ErrorCode::WsGapSubscriptionBatching.code_str(),
                source = "dropped_map_full",
                refused,
                tracked = map.len(),
                cap = MAX_DROPPED_TRACKED,
                "depth view dropped map is full; these instruments are not remembered and can never read as ghost (fail-closed: no redial on a guess)"
            );
        }
        self.dropped.store(Arc::new(map));
        self.last_publish_secs
            .store(now_secs, std::sync::atomic::Ordering::Release);
        self.published_once
            .store(true, std::sync::atomic::Ordering::Release);
        refused
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
/// The ownership argument that makes the view safe is UNAFFECTED by this being
/// global: the single-writer-TASK invariant in the module header is what holds
/// (not "one writer per slot" — see the correction there), and the accessor
/// hands out a shared reference rather than a mutable one, so a global cannot
/// add a second writer without a second CALL SITE, which is the thing pinned by
/// `the_view_has_exactly_one_publishing_call_site`. `run_depth_rebalance`
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

    // ------------------------------------------------------------------
    // Ghost classification: only an instrument THIS process dropped, whose
    // grace has elapsed, reads as a ghost.
    // ------------------------------------------------------------------

    const T0: i64 = 1_800_000_000;
    const FNO: u8 = 2; // ExchangeSegment::NseFno.binary_code()

    #[test]
    fn before_the_first_publish_everything_is_unknown() {
        let view = DepthSubscriptionView::new();
        assert_eq!(view.classify_raw(1, FNO, T0), DepthFrameClass::Unknown);
        assert_eq!(view.dropped_count(), 0);
    }

    #[test]
    fn a_held_instrument_classifies_held_on_either_pool() {
        let view = DepthSubscriptionView::new();
        view.publish_depth20_at([(1, NSE_FNO)], T0);
        view.publish_depth200_at([(2, NSE_FNO)], T0);
        assert_eq!(view.classify_raw(1, FNO, T0), DepthFrameClass::Held);
        assert_eq!(view.classify_raw(2, FNO, T0), DepthFrameClass::Held);
        assert_eq!(
            NSE_FNO.binary_code(),
            FNO,
            "the test's raw code must match the enum"
        );
    }

    #[test]
    fn an_instrument_never_held_is_unknown_never_ghost() {
        // The swap-in window: a contract subscribed between two publishes is
        // exactly "not held, not dropped". Calling it a ghost would redial a
        // healthy socket on every swap.
        let view = DepthSubscriptionView::new();
        view.publish_depth20_at([(1, NSE_FNO)], T0);
        assert_eq!(
            view.classify_raw(99, FNO, T0 + 1_000),
            DepthFrameClass::Unknown
        );
    }

    #[test]
    fn publish_depth20_at_marks_a_dropped_instrument_recently_dropped_inside_the_grace_and_ghost_after_it()
     {
        let view = DepthSubscriptionView::new();
        view.publish_depth20_at([(1, NSE_FNO), (2, NSE_FNO)], T0);
        // Minute two: 1 was swapped out.
        view.publish_depth20_at([(2, NSE_FNO)], T0 + 60);
        assert_eq!(view.dropped_count(), 1);
        assert_eq!(
            view.classify_raw(1, FNO, T0 + 60 + GHOST_GRACE_SECS - 1),
            DepthFrameClass::RecentlyDropped
        );
        assert_eq!(
            view.classify_raw(1, FNO, T0 + 60 + GHOST_GRACE_SECS),
            DepthFrameClass::Ghost
        );
        // The survivor is unaffected.
        assert_eq!(view.classify_raw(2, FNO, T0 + 600), DepthFrameClass::Held);
    }

    #[test]
    fn a_dropped_instrument_re_held_by_a_later_publish_reads_held_and_leaves_the_map() {
        let view = DepthSubscriptionView::new();
        view.publish_depth20_at([(1, NSE_FNO)], T0);
        view.publish_depth20_at(std::iter::empty(), T0 + 60);
        assert_eq!(view.dropped_count(), 1);
        view.publish_depth20_at([(1, NSE_FNO)], T0 + 120);
        assert_eq!(view.classify_raw(1, FNO, T0 + 1_000), DepthFrameClass::Held);
        assert_eq!(
            view.dropped_count(),
            0,
            "a re-held key is forgotten as dropped"
        );
    }

    #[test]
    fn publish_depth200_at_carries_the_other_pools_drop_forward_instead_of_masking_it() {
        // The two publishers share the dropped map; a depth-200 publish must
        // carry the depth-20 drop forward, not erase it.
        let view = DepthSubscriptionView::new();
        view.publish_depth20_at([(1, NSE_FNO)], T0);
        view.publish_depth20_at(std::iter::empty(), T0 + 60);
        view.publish_depth200_at([(7, NSE_FNO)], T0 + 61);
        assert_eq!(
            view.classify_raw(1, FNO, T0 + 61 + GHOST_GRACE_SECS),
            DepthFrameClass::Ghost
        );
    }

    /// A contract held by BOTH pools (the top five are normally inside the
    /// top 250): depth-20 drops it at T, depth-200 drops it at T+300. The
    /// grace must run from the SECOND drop — the one the socket is still
    /// honouring — or the first frames after it read as a ghost and a healthy
    /// socket is redialled. Found by the 2026-09-08 hostile sweep; the older
    /// `!map.contains_key` guard kept the T stamp and this test failed on it.
    #[test]
    fn a_contract_dropped_by_both_pools_gets_its_grace_from_the_newer_drop() {
        let view = DepthSubscriptionView::new();
        view.publish_depth20_at([(1, NSE_FNO)], T0);
        view.publish_depth200_at([(1, NSE_FNO)], T0);
        // depth-20 lets it go first...
        view.publish_depth20_at(std::iter::empty(), T0 + 60);
        // ...it is still HELD by depth-200, so it is not even dropped yet.
        assert_eq!(view.classify_raw(1, FNO, T0 + 61), DepthFrameClass::Held);
        // depth-200 drops it 300 s later: the grace runs from HERE.
        view.publish_depth200_at(std::iter::empty(), T0 + 360);
        assert_eq!(
            view.classify_raw(1, FNO, T0 + 360 + GHOST_GRACE_SECS - 1),
            DepthFrameClass::RecentlyDropped,
            "inside the grace of the newer drop — never a ghost"
        );
        assert_eq!(
            view.classify_raw(1, FNO, T0 + 360 + GHOST_GRACE_SECS),
            DepthFrameClass::Ghost
        );
    }

    /// A frame clock more than two graces past the last publish — a forward
    /// clock step, or a dead steering loop — refuses the ghost verdict
    /// rather than redialling on a clock nobody has confirmed.
    #[test]
    fn a_ghost_verdict_is_refused_when_the_last_publish_is_too_old() {
        let view = DepthSubscriptionView::new();
        view.publish_depth20_at([(1, NSE_FNO)], T0);
        view.publish_depth20_at(std::iter::empty(), T0 + 60);
        // Inside the publish-age bound: a real ghost.
        assert_eq!(
            view.classify_raw(1, FNO, T0 + 60 + GHOST_VERDICT_MAX_PUBLISH_AGE_SECS),
            DepthFrameClass::Ghost
        );
        // One second past it (the clock stepped, or nobody has published in
        // three minutes): refused, never a redial.
        assert_eq!(
            view.classify_raw(1, FNO, T0 + 60 + GHOST_VERDICT_MAX_PUBLISH_AGE_SECS + 1),
            DepthFrameClass::Unknown
        );
        // A fresh publish restores the verdict.
        view.publish_depth200_at(
            std::iter::empty(),
            T0 + 60 + GHOST_VERDICT_MAX_PUBLISH_AGE_SECS,
        );
        assert_eq!(
            view.classify_raw(1, FNO, T0 + 60 + GHOST_VERDICT_MAX_PUBLISH_AGE_SECS + 1),
            DepthFrameClass::Ghost
        );
        assert_eq!(GHOST_VERDICT_MAX_PUBLISH_AGE_SECS, 2 * GHOST_GRACE_SECS);
        assert!(GHOST_VERDICT_MAX_PUBLISH_AGE_SECS < DROPPED_RETENTION_SECS);
    }

    #[test]
    fn dropped_count_falls_when_entries_are_evicted_after_the_retention_window() {
        let view = DepthSubscriptionView::new();
        view.publish_depth20_at([(1, NSE_FNO)], T0);
        view.publish_depth20_at(std::iter::empty(), T0 + 60);
        // A publish inside the retention keeps it...
        view.publish_depth200_at(std::iter::empty(), T0 + 60 + DROPPED_RETENTION_SECS - 1);
        assert_eq!(view.dropped_count(), 1);
        // ...and one at the boundary evicts it.
        view.publish_depth200_at(std::iter::empty(), T0 + 60 + DROPPED_RETENTION_SECS);
        assert_eq!(view.dropped_count(), 0);
        assert_eq!(
            view.classify_raw(1, FNO, T0 + 10_000),
            DepthFrameClass::Unknown
        );
    }

    #[test]
    fn the_dropped_map_counts_every_refused_key_not_one_per_publish() {
        // The counter increments PER KEY. Before 2026-09-09 it incremented
        // once per publish and then `break`ed, so an operator was told `1`
        // for up to a depth-20 budget's worth of forgotten instruments.
        let view = DepthSubscriptionView::new();
        let full: HashSet<Key> = (0..MAX_DROPPED_TRACKED as u64).map(|i| (i, FNO)).collect();
        let none: HashSet<Key> = HashSet::new();
        assert_eq!(
            view.record_dropped(&full, &none, T0),
            0,
            "filling exactly to the cap refuses nothing"
        );
        assert_eq!(view.dropped_count(), MAX_DROPPED_TRACKED);

        // The map is now AT the cap, so every one of these NEW keys is refused.
        let overflow: HashSet<Key> = (900_000..900_137).map(|i| (i, FNO)).collect();
        assert_eq!(
            view.record_dropped(&overflow, &none, T0 + 1),
            overflow.len() as u64,
            "one increment per refused KEY, not one per publish"
        );
    }

    #[test]
    fn a_refused_key_does_not_stop_the_newer_drop_wins_refresh() {
        // The cap arm used to `break`, which abandoned the `get_mut` stamp
        // refresh ABOVE it for every remaining key. That silently defeated
        // newer-drop-wins and redialled healthy sockets (2026-09-08 sweep).
        // Here EVERY already-tracked key must still be refreshed even though
        // the same publish refuses new ones -- and because a `HashSet` has no
        // iteration order, asserting on ALL of them is what makes the old
        // `break` fail deterministically wherever it landed.
        let view = DepthSubscriptionView::new();
        let tracked: HashSet<Key> = (0..MAX_DROPPED_TRACKED as u64).map(|i| (i, FNO)).collect();
        let none: HashSet<Key> = HashSet::new();
        view.record_dropped(&tracked, &none, T0);
        assert_eq!(view.dropped_count(), MAX_DROPPED_TRACKED);

        // Re-drop every tracked key (refreshes) alongside new keys (refused),
        // far enough past T0 that a STALE stamp reads Ghost and a refreshed
        // one reads RecentlyDropped.
        let later = T0 + 4 * GHOST_GRACE_SECS;
        let mut mixed = tracked.clone();
        mixed.extend((900_000..900_137).map(|i| (i, FNO)));
        let refused = view.record_dropped(&mixed, &none, later);
        assert!(refused > 0, "the new keys must actually hit the cap");

        // Probe inside the publish-freshness window.
        let probe_at = later + 1;
        let stale = tracked
            .iter()
            .filter(|(id, seg)| {
                view.classify_raw(*id, *seg, probe_at) != DepthFrameClass::RecentlyDropped
            })
            .count();
        assert_eq!(
            stale, 0,
            "{stale} tracked keys kept a stale stamp: the cap arm stopped the refresh"
        );
    }

    #[test]
    fn the_view_has_exactly_one_publishing_call_site() {
        // The module header's safety argument is "all publishing happens on
        // the single run_depth_rebalance task". That is only true while there
        // is ONE production caller: every write here is load-rebuild-store on
        // an `ArcSwap`, so a second concurrent publisher loses drops. Splitting
        // the pools onto two tasks needs `rcu` FIRST -- see the header.
        let rebalance = include_str!("depth_rebalance.rs");
        let stack = include_str!("dhan_feed_stack.rs");
        let calls = rebalance.matches("publish_depth_subscriptions(").count()
            - rebalance.matches("fn publish_depth_subscriptions(").count();
        assert_eq!(
            calls, 2,
            "publish_depth_subscriptions is called {calls} times; the header claims two \
             points in ONE loop body. A new call site may be a second WRITER TASK -- \
             read the concurrency section before changing this number."
        );
        assert!(
            stack.matches("run_depth_rebalance(").count()
                - stack.matches("fn run_depth_rebalance(").count()
                <= 1,
            "run_depth_rebalance is spawned more than once: the single-writer-task \
             invariant the view relies on would no longer hold"
        );
    }

    #[test]
    fn the_dropped_map_refuses_past_its_cap_rather_than_growing() {
        let view = DepthSubscriptionView::new();
        let big: Vec<(u64, ExchangeSegment)> = (0..(MAX_DROPPED_TRACKED as u64 + 50))
            .map(|i| (i, NSE_FNO))
            .collect();
        view.publish_depth20_at(big, T0);
        view.publish_depth20_at(std::iter::empty(), T0 + 60);
        assert_eq!(view.dropped_count(), MAX_DROPPED_TRACKED);
    }

    /// THE regression this method exists for. `classify_raw` answers `Held`
    /// from the UNION of both pools, and both depth boards are cut from ONE
    /// volume-ordered slice — so depth-200's top-5 entry set is almost always
    /// inside depth-20's top 250. Gating depth-200 stamps on `classify_raw`
    /// refused ~19 of every 20 of them (MEASURED 2026-09-11 by two
    /// independent agents) and disabled that half of the instrument while its
    /// counters read healthy.
    #[test]
    fn may_already_be_streaming_is_per_pool_so_the_other_pools_holding_never_blocks_a_stamp() {
        let view = DepthSubscriptionView::new();
        // The normal live shape: the depth-200 pick is also in depth-20's 250.
        view.publish_depth20_at([(42, NSE_FNO)], T0);
        view.publish_depth200_at(std::iter::empty(), T0);

        assert!(
            view.may_already_be_streaming(42, NSE_FNO.binary_code(), DepthFeedKind::Twenty, T0),
            "depth-20 holds it, so a depth-20 stamp must still be refused"
        );
        assert!(
            !view.may_already_be_streaming(
                42,
                NSE_FNO.binary_code(),
                DepthFeedKind::TwoHundred,
                T0
            ),
            "depth-20 holding it says NOTHING about a depth-200 subscribe — the \
             pool byte in the stamp key already separates them, and refusing \
             here is what silently disabled the depth-200 half"
        );
        // And the pool-blind answer it replaced would have refused both.
        assert_eq!(
            view.classify_raw(42, NSE_FNO.binary_code(), T0),
            DepthFrameClass::Held,
            "pinning WHY classify_raw is the wrong question for this gate"
        );
    }

    /// The defence that must survive the pool split: a contract this process
    /// DROPPED may still be streaming (Dhan ignores the unsubscribe), so it is
    /// unmeasurable for EITHER pool — a drop is recorded without the pool that
    /// made it.
    #[test]
    fn a_recently_dropped_contract_is_unmeasurable_for_both_pools() {
        let view = DepthSubscriptionView::new();
        view.publish_depth20_at([(7, NSE_FNO)], T0);
        view.publish_depth20_at(std::iter::empty(), T0 + 60); // drops 7

        for pool in [DepthFeedKind::Twenty, DepthFeedKind::TwoHundred] {
            assert!(
                view.may_already_be_streaming(7, NSE_FNO.binary_code(), pool, T0 + 61),
                "a dropped contract may still be on the wire, whichever pool re-takes it"
            );
        }
        // ...and the honest limit: past the retention this process has
        // genuinely forgotten, and the ghost tail outlives that.
        assert!(
            !view.may_already_be_streaming(
                7,
                NSE_FNO.binary_code(),
                DepthFeedKind::Twenty,
                T0 + 60 + DROPPED_RETENTION_SECS + 1
            ),
            "recorded, not hidden: the measured ghost tail outlives this window"
        );
    }

    #[test]
    fn a_never_seen_contract_is_always_measurable() {
        let view = DepthSubscriptionView::new();
        view.publish_depth20_at([(1, NSE_FNO)], T0);
        assert!(!view.may_already_be_streaming(
            999,
            NSE_FNO.binary_code(),
            DepthFeedKind::Twenty,
            T0
        ));
        // I-P1-11: the same number on another segment is another instrument.
        assert!(!view.may_already_be_streaming(1, IDX.binary_code(), DepthFeedKind::Twenty, T0));
    }

    #[test]
    fn classify_raw_keys_on_the_composite_so_a_segment_twin_is_not_a_ghost() {
        // I-P1-11: dropping id 27 on NSE_FNO says nothing about id 27 on IDX_I.
        // Probe inside the publish-freshness window (a verdict older than
        // GHOST_VERDICT_MAX_PUBLISH_AGE_SECS after the last publish is refused).
        let probe_at = T0 + 60 + GHOST_GRACE_SECS + 1;
        let view = DepthSubscriptionView::new();
        view.publish_depth20_at([(27, NSE_FNO)], T0);
        view.publish_depth20_at(std::iter::empty(), T0 + 60);
        assert_eq!(
            view.classify_raw(27, IDX.binary_code(), probe_at),
            DepthFrameClass::Unknown
        );
        assert_eq!(view.classify_raw(27, FNO, probe_at), DepthFrameClass::Ghost);
    }

    #[test]
    fn the_grace_is_shorter_than_the_retention_and_the_redial_cooldown_fits_inside_it() {
        // Otherwise a ghost is forgotten before it can be redialled twice.
        assert!(GHOST_GRACE_SECS < DROPPED_RETENTION_SECS);
        assert!(
            tickvault_core::websocket::pool_supervisor::GHOST_REDIAL_COOLDOWN_SECS * 2
                < DROPPED_RETENTION_SECS
        );
    }
}
