//! The live paper fill chain's tap on the frame drain (plan item LF-6b).
//!
//! The drain hands each live tick the candle fold accepted
//! (`IngestOutcome::Folded`) to the `tv-live-fill` thread through a bounded,
//! pre-allocated ring. The drain never waits: a full ring drops the tick and
//! counts it, and every tick that does go out carries the loss count so far
//! (`drop_seq`), so the chain thread can tell where in the stream ticks went
//! missing and keep orders off the candles they could have touched.
//!
//! `drop_seq` counts three kinds of loss, not only the tap's own ring:
//!
//! * a tick this tap's ring had no room for;
//! * a main-feed frame shed BEFORE the drain (the socket-to-drain ring was
//!   full, or the ring and the WAL both refused it): read from
//!   `pool_supervisor::live_shed::main_feed_live_shed_total` with one relaxed load per
//!   offer. A ring-shed frame is re-folded later from the WAL, but as a
//!   replayed frame, which never taps, so the live stream the chain builds
//!   candles from lacks it;
//! * a frame whose tail the drain abandoned (unknown code, partial packet),
//!   or a folded tick whose row append failed, noted by the drain through
//!   [`LiveFillTap::note_upstream_loss`];
//! * a main-feed socket that was delivering and is now down (closed, waiting
//!   out a redial, re-subscribing, parked): read from
//!   `pool_supervisor::live_shed::main_feed_socket_loss_state` with one relaxed load per
//!   offer. Its instruments get no ticks while the other sockets keep the
//!   stream busy, so no feed gap shows. The tap counts one loss on every
//!   change of the down-slot mask (the socket going down, and coming back),
//!   never once per second while it stays down (LF-9 finding 11: a socket
//!   parked for the rest of the day used to taint every instrument, on every
//!   socket, every second until the close). Each loss taints the stretch
//!   around that change (the chain's drop lag before it, its skew after), so
//!   a bar of a down socket's instrument that holds the close or the redial
//!   is refused, and a bar that lies wholly inside the downtime cannot exist
//!   (no tick reached it). **Honest limit:** for a downtime longer than the
//!   drop lag plus the skew, the stretch in between is not tainted for the
//!   OTHER sockets' instruments, which is the point; an instrument with a
//!   backup copy on a live socket keeps building bars from that copy there.
//!
//! The running loss total is also stored in a shared atomic
//! ([`LiveFillTap::with_loss_out`]) whenever it changes, so the chain thread
//! can taint BEFORE it releases anything even while ticks are still queued in
//! this ring (the in-band `drop_seq` arrives only behind them).
//!
//! Gates, both at the call site: only frames from a live socket
//! (`connection_index != u8::MAX`, a replayed WAL frame carries `u8::MAX`) and
//! never while the WAL backlog is being re-folded (`replaying_wal`). A
//! replayed tick is history the strategy already passed.
//!
//! # Complexity and allocation
//!
//! [`LiveFillTap::offer`]: O(1), two relaxed atomic loads (one more relaxed
//! store when the loss total moved), one `Copy` struct
//! built on the stack and one `try_send` into a crossbeam array slot; no
//! allocation, no lock, no log, no metric macro. The receiver busy-polls
//! ([`FrameRingReceiver::set_busy_polling`] is set before the sender is handed
//! out), so `try_send` never calls `notify_one`. Since 2026-11-13 (§28.6) a
//! queued tick also sets one plain `bool` (`wake_pending`), and the frame
//! drain calls [`LiveFillTap::flush_wake`] ONCE after the whole frame walk
//! (`LiveIngest::flush_live_fill_wake`, after `drain_main_feed_frame` returns
//! and after the frame's stage timings, so the wake is never timed as fold
//! work in `drain_main_frame`, `drain_main_tick` or `receipt_to_fold`): one
//! `SeqCst` load while the chain thread is awake, one swap and
//! one `unpark` on its idle-to-busy edge. So at most ONE wake per frame,
//! whatever the frame holds: before the 2026-11-13 review `offer` woke per
//! queued tick, and a thread that caught up and parked mid-frame was woken
//! again by the next offer, which on a 2-core host could preempt the drain
//! once per tick (`test_flush_wake_costs_at_most_one_unpark_per_frame_while_the_consumer_reparks`).
//! The edge is NOT rare: about one futex wake syscall per frame while the tap
//! is armed (MEASURED 8.2 to 8.7 µs per edge on the x86 dev container,
//! `live_fill/wake_edge`, 2026-11-13; not measured on the box). Cost of the
//! deferral: the chain sees a frame's ticks after the walk ends, a few µs
//! against the old 1 to 5 ms nap. The thread may not spin to avoid it (§28.6
//! REJECT). DHAT-gated by `crates/app/tests/dhat_live_fill.rs`.
//! [`LiveFillTap::publish_dropped`] runs on the drain's 30 s silence arm: one
//! counter add.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::Thread;

use tickvault_common::error_code::ErrorCode;
use tickvault_common::tick_types::{MarketDepthLevel, ParsedTick};
use tickvault_core::websocket::frame_ring::{
    FrameRingReceiver, FrameRingSender, FrameRingTrySendError, frame_ring,
};
use tickvault_core::websocket::pool_supervisor::live_shed::{
    main_feed_live_shed_total, main_feed_socket_loss_state,
};
use tickvault_trading::brutex_host::live_candles::IST_OFFSET_NS;

/// The counter the drain publishes tap drops on (local `/metrics` only).
pub const LIVE_FILL_TAP_DROPPED_COUNTER: &str = "tv_live_fill_tap_dropped_total";

/// One tick for the chain thread. `Copy`, 64 bytes, no heap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TapTick {
    pub security_id: u64,
    /// Receipt, IST epoch nanoseconds (UTC receipt + 5:30).
    pub receipt_ist_ns: i64,
    pub ltp: f32,
    /// Exchange trade time, IST epoch seconds.
    pub exchange_secs: u32,
    /// Vendor cumulative day volume.
    pub cum_volume: u32,
    /// Best bid / ask of the Full packet; 0.0 when the packet had no depth.
    pub best_bid: f32,
    pub best_ask: f32,
    pub best_bid_qty: u32,
    pub best_ask_qty: u32,
    /// Ticks lost before this one (wrapping): tap drops, frames shed before
    /// the drain, abandoned frame tails, failed row appends and main-feed
    /// socket downtime (see the module docs).
    pub drop_seq: u32,
    pub segment: u8,
    /// The packet carried a NEW trade (the fold's `advanced`). A repeat quote
    /// (the book moved, no trade) or the first packet after a subscribe
    /// carries an OLD trade time, price and cumulative volume: the chain only
    /// takes its best bid/ask (and the volume baseline), never a candle price.
    /// Also true for the first in-session packet of an instrument when its
    /// trade happened while this process listened (the fold's `fresh_first`):
    /// often the 09:15:00 opening trade.
    pub new_trade: bool,
    /// The packet carried a REAL trade older than the newest one the fold has
    /// seen for the instrument (a stale cumulative regression, a backup copy
    /// accepted late, an index tick at an older or the same second with a
    /// different price; the fold's `older_trade`). The chain folds its price
    /// into the bar it belongs to (or counts it late on a closed one) and
    /// never lets it reach the engine; it moves a close only when it is not
    /// `stale_packet`, as the fold's close does. Never true together with
    /// `new_trade`.
    pub older_trade: bool,
    /// The volume the candle fold credited this tick with (LF-9 findings 8,
    /// 9, 10): `ConsumeStats::volume_delta`, saturated to `u32`. The chain
    /// adds it to its bars instead of differencing `cum_volume`, so a chain
    /// bar carries the volume of the stored candle of the same bucket (the
    /// fold unwraps a `u32` wrap and re-anchors a counter restart).
    pub volume_delta: u32,
    /// The volume credited to a day's first session bucket (the one holding
    /// 09:15:00): `ConsumeStats::first_bucket_volume_delta`. Equal to
    /// `volume_delta` except on a pre-market boot's first trade.
    pub first_bucket_volume_delta: u32,
    /// The fold re-anchored a vendor counter restart on this tick
    /// (`ConsumeStats::cumulative_restarted`): each frame's next bar adds
    /// nothing for its opening tick, as the fold's does.
    pub cumulative_restarted: bool,
    /// The fold read this tick as a stale packet (`ConsumeStats::stale_packet`,
    /// a cumulative below the slot's high mark; LF-9 review). Only a stale
    /// packet is kept off a bar's close: an `older_trade` that is not stale
    /// (an index tick at the same second with another price) moves the close
    /// as the fold's does.
    pub stale_packet: bool,
    /// The fold SEEDED the instrument's volume baseline on this tick
    /// (`ConsumeStats::seeded`; LF-9 review, second pass): it credited
    /// nothing to an earlier tick of the instrument, so the chain cannot
    /// have missed one.
    pub fold_seed: bool,
    /// The fold ended a replay gap on this LIVE tick and credited it nothing
    /// (`ConsumeStats::gap_reseed`; LF-9 review, third pass): typically an
    /// instrument's first live tick after a mid-session boot re-folded the
    /// WAL tail. Like `fold_seed`, nothing earlier of this instrument can
    /// have been missed by the chain.
    pub fold_gap_reseed: bool,
}

/// The fold's per-tick volume for a [`TapTick`] (LF-9). O(1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TapVolume {
    pub delta: u64,
    pub first_bucket_delta: u64,
    pub restart: bool,
    /// `ConsumeStats::stale_packet` (LF-9 review).
    pub stale: bool,
    /// `ConsumeStats::seeded` (LF-9 review, second pass).
    pub seed: bool,
    /// `ConsumeStats::gap_reseed` (LF-9 review, third pass).
    pub gap_reseed: bool,
}

impl TapVolume {
    /// The fold's own per-tick volume, taken from its `ConsumeStats` (LF-9
    /// review, finding 8): the ONE mapping the frame drain, the offline
    /// replay and the tests share, so no copy can swap two fields. O(1).
    #[must_use]
    #[inline]
    pub fn from_stats(stats: &tickvault_trading::candles::ConsumeStats) -> Self {
        Self {
            delta: stats.volume_delta,
            first_bucket_delta: stats.first_bucket_volume_delta,
            restart: stats.cumulative_restarted,
            stale: stats.stale_packet,
            seed: stats.seeded,
            gap_reseed: stats.gap_reseed,
        }
    }
}

impl TapTick {
    /// Builds the chain's tick from the drain's decoded packet and, for a Full
    /// packet, its best depth level. O(1), no allocation.
    #[must_use]
    #[inline]
    pub fn from_parsed(
        tick: &ParsedTick,
        best: Option<&MarketDepthLevel>,
        drop_seq: u32,
        new_trade: bool,
        older_trade: bool,
        volume: TapVolume,
    ) -> Self {
        let (best_bid, best_ask, best_bid_qty, best_ask_qty) = match best {
            Some(l) => (l.bid_price, l.ask_price, l.bid_quantity, l.ask_quantity),
            None => (0.0, 0.0, 0, 0),
        };
        Self {
            security_id: tick.security_id,
            receipt_ist_ns: tick.received_at_nanos.saturating_add(IST_OFFSET_NS),
            ltp: tick.last_traded_price,
            exchange_secs: tick.exchange_timestamp,
            cum_volume: tick.volume,
            best_bid,
            best_ask,
            best_bid_qty,
            best_ask_qty,
            drop_seq,
            segment: tick.exchange_segment_code,
            new_trade,
            older_trade: older_trade && !new_trade,
            volume_delta: u32::try_from(volume.delta).unwrap_or(u32::MAX),
            first_bucket_volume_delta: u32::try_from(volume.first_bucket_delta).unwrap_or(u32::MAX),
            cumulative_restarted: volume.restart,
            stale_packet: volume.stale,
            fold_seed: volume.seed,
            fold_gap_reseed: volume.gap_reseed,
        }
    }
}

/// Whether a folded tick may go to the chain: a live socket's frame, and not
/// while the WAL backlog is re-folded. O(1).
#[must_use]
#[inline]
pub const fn tap_gate(connection_index: u8, replaying_wal: bool) -> bool {
    connection_index != u8::MAX && !replaying_wal
}

/// The drain-side half. Owned by `LiveIngest` (single owner, `&mut`), so the
/// counts are plain fields.
#[derive(Debug)]
pub struct LiveFillTap {
    tx: FrameRingSender<TapTick>,
    /// Ticks this tap's ring refused.
    dropped: u64,
    published: u64,
    /// Main-feed frames lost before the drain, plus abandoned frame tails.
    upstream_lost: u64,
    /// The shed total's source: `main_feed_live_shed_total` in production.
    shed_total: fn() -> u64,
    /// The shed total as last read.
    shed_seen: u64,
    /// The socket loss word's source: `main_feed_socket_loss_state` in
    /// production.
    socket_state: fn() -> u64,
    /// The socket loss word as last read.
    socket_seen: u64,
    /// Where the running loss total is published for the chain thread.
    loss_out: Option<Arc<AtomicU64>>,
    /// The loss total last published.
    loss_published: u64,
    /// The chain thread is gone (its receiver dropped, e.g. a refused day
    /// roll): every later offer returns at once, doing no work.
    closed: bool,
    /// The chain thread's wake (§28.6): woken once per frame, by
    /// [`Self::flush_wake`], when the frame queued a tick. `None` in tests
    /// that drive the ring by hand.
    wake: Option<Arc<ChainWake>>,
    /// A tick was queued since the last [`Self::flush_wake`].
    wake_pending: bool,
}

/// The tap's link from the boot to the frame drain: the ring's sender, the
/// shared loss total the chain thread reads out of band, and the chain
/// thread's wake (§28.6).
#[derive(Debug)]
pub struct TapLink {
    pub tx: FrameRingSender<TapTick>,
    pub loss: Arc<AtomicU64>,
    pub wake: Arc<ChainWake>,
}

/// The chain thread's park/unpark handshake with the tap (owner card
/// 2026-11-13, "Yes, instant wake (Recommended)", rule file §28.6). The
/// thread parks instead of napping when both its rings are empty; the tap
/// unparks it when it queues a tick.
///
/// # Why no tick is stranded
///
/// The tap pushes (crossbeam's tail compare-and-swap, `SeqCst`), then loads
/// `parked` (`SeqCst`). The thread stores `parked = true` (`SeqCst`), then
/// checks its rings (crossbeam-channel 0.5.17 `is_empty`: two `SeqCst`
/// loads) and parks only when both are empty. In the single total order of
/// `SeqCst` operations either the tap's load comes after the thread's store
/// (it sees the flag and unparks), or its push comes before the thread's
/// check (the thread sees the tick and does not park). An unpark that lands
/// before the park makes the park return at once. Same reasoning as
/// `FrameRingReceiver::set_busy_polling`. The park is bounded by the old nap,
/// so a missed wake can only cost what the old nap cost.
///
/// # Cost on the frame drain
///
/// While the thread is awake: one `SeqCst` load per queued tick (a plain load
/// on x86, `ldar` on ARM; not `Relaxed`, which would let the load pass the
/// push on ARM and lose a wake). On the idle-to-busy edge only: one swap and
/// one `unpark` (a futex wake when the thread is asleep). No allocation, no
/// lock. O(1). The tap calls [`Self::wake`] once per frame, after the frame
/// walk ([`LiveFillTap::flush_wake`]), never per tick, so the drain makes at
/// most one futex wake per frame while the tap is armed; see the module docs
/// and the `live_fill/wake_edge` bench.
#[derive(Debug, Default)]
pub struct ChainWake {
    parked: AtomicBool,
    thread: OnceLock<Thread>,
    /// Unparks issued (tests count wakes per edge).
    #[cfg(test)]
    unparks: AtomicU64,
    /// Waits that found the rings empty and called `park_timeout` (tests:
    /// a producer that queues only after this moved meets a real park).
    #[cfg(test)]
    parks_entered: AtomicU64,
    /// Test-only stall between declaring parked and the ring check, in ns.
    #[cfg(test)]
    check_stall_ns: AtomicU64,
}

impl ChainWake {
    /// A wake with no thread registered yet. Cold.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers the calling thread as the one to unpark. Once; a later call
    /// is ignored. Cold (thread start).
    pub fn register_current_thread(&self) {
        // A second registration keeps the first thread: `Err` is that case.
        if self.thread.set(std::thread::current()).is_err() {
            tracing::debug!("live paper fill chain: wake already registered");
        }
    }

    /// The tap's half, after a frame queued a tick: unparks the thread only
    /// on the edge where it declared itself parked. O(1).
    #[inline]
    pub fn wake(&self) {
        if self.parked.load(Ordering::SeqCst) && self.parked.swap(false, Ordering::AcqRel) {
            self.unpark_cold();
        }
    }

    #[cold]
    #[inline(never)]
    fn unpark_cold(&self) {
        #[cfg(test)]
        self.unparks.fetch_add(1, Ordering::Relaxed);
        if let Some(t) = self.thread.get() {
            t.unpark();
        }
    }

    /// The thread's half: declares itself parked, re-checks `empty`, and
    /// parks for at most `bound` while it is still true. Returns `true` when
    /// the tap ended the wait (it cleared the flag), `false` when the bound
    /// ran out or nothing was empty. O(1).
    pub fn park_unless(&self, empty: impl Fn() -> bool, bound: std::time::Duration) -> bool {
        self.parked.store(true, Ordering::SeqCst);
        #[cfg(test)]
        std::thread::sleep(std::time::Duration::from_nanos(
            self.check_stall_ns.load(Ordering::SeqCst),
        ));
        if empty() {
            #[cfg(test)]
            self.parks_entered.fetch_add(1, Ordering::SeqCst);
            std::thread::park_timeout(bound);
        }
        // Already `false` means the tap cleared it: it saw the thread parked.
        !self.parked.swap(false, Ordering::Relaxed)
    }

    /// Whether the thread has declared itself parked (the next queued tick
    /// pays the unpark). One `SeqCst` load. For tests and the wake-edge
    /// bench; O(1).
    #[must_use]
    pub fn is_parked(&self) -> bool {
        self.parked.load(Ordering::SeqCst)
    }

    /// Waits that entered `park_timeout` so far (tests). O(1).
    #[cfg(test)]
    #[must_use]
    // TEST-EXEMPT: test-only probe read by the park and wake tests in live_fill/wake_tests.rs.
    pub fn parks_entered(&self) -> u64 {
        self.parks_entered.load(Ordering::SeqCst)
    }

    /// Makes every later `park_unless` stall `d` between declaring itself
    /// parked and checking the rings (tests: a consumer the scheduler took
    /// off its core at the worst moment). O(1).
    #[cfg(test)]
    // TEST-EXEMPT: test-only fault injection used by the wake gate in live_fill/wake_tests.rs.
    pub fn set_check_stall(&self, d: std::time::Duration) {
        let ns = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        self.check_stall_ns.store(ns, Ordering::SeqCst);
    }

    /// Unparks issued so far (tests). O(1).
    #[cfg(test)]
    #[must_use]
    pub fn unparks(&self) -> u64 {
        self.unparks.load(Ordering::Relaxed)
    }
}

/// No socket loss (tests that do not exercise it). O(1).
fn no_socket_loss() -> u64 {
    0
}

impl LiveFillTap {
    /// Cold: registers the drop counter at 0 and takes the shed total so far
    /// as the baseline (sheds before the tap existed touched no chain candle).
    #[must_use]
    pub fn new(tx: FrameRingSender<TapTick>) -> Self {
        Self::with_sources(tx, main_feed_live_shed_total, main_feed_socket_loss_state)
    }

    /// [`Self::new`] with the shed total read from `shed_total` and no socket
    /// loss (tests use a private counter: the production one is
    /// process-wide). Cold.
    #[must_use]
    pub fn with_shed_source(tx: FrameRingSender<TapTick>, shed_total: fn() -> u64) -> Self {
        Self::with_sources(tx, shed_total, no_socket_loss)
    }

    /// [`Self::new`] with both upstream sources injected. The socket word
    /// read now is the baseline; a slot already down keeps counting. Cold.
    #[must_use]
    pub fn with_sources(
        tx: FrameRingSender<TapTick>,
        shed_total: fn() -> u64,
        socket_state: fn() -> u64,
    ) -> Self {
        metrics::counter!(LIVE_FILL_TAP_DROPPED_COUNTER).increment(0);
        Self {
            tx,
            dropped: 0,
            published: 0,
            upstream_lost: 0,
            shed_total,
            shed_seen: shed_total(),
            socket_state,
            socket_seen: socket_state(),
            loss_out: None,
            loss_published: 0,
            closed: false,
            wake: None,
            wake_pending: false,
        }
    }

    /// The production tap for a boot's [`TapLink`]: [`Self::new`] with the
    /// link's loss total and the chain thread's wake. Cold.
    #[must_use]
    pub fn from_link(link: TapLink) -> Self {
        Self::new(link.tx)
            .with_loss_out(link.loss)
            .with_wake(link.wake)
    }

    /// Wakes `wake`'s thread, once per frame that queued a tick, while it
    /// sleeps (builder, §28.6). Cold.
    #[must_use]
    pub fn with_wake(mut self, wake: Arc<ChainWake>) -> Self {
        self.wake = Some(wake);
        self
    }

    /// Publishes the running loss total to `loss` for the chain thread
    /// (builder). Cold.
    #[must_use]
    pub fn with_loss_out(mut self, loss: Arc<AtomicU64>) -> Self {
        loss.store(self.loss_total(), Ordering::Relaxed);
        self.loss_published = self.loss_total();
        self.loss_out = Some(loss);
        self
    }

    /// Offers one folded tick. `true` when queued; `false` when the ring was
    /// full or closed (counted). Frames shed before the drain and main-feed
    /// socket downtime since the last offer are folded into this tick's
    /// `drop_seq` first. O(1), no allocation.
    #[inline]
    pub fn offer(
        &mut self,
        tick: &ParsedTick,
        best: Option<&MarketDepthLevel>,
        new_trade: bool,
        older_trade: bool,
        volume: TapVolume,
    ) -> bool {
        if self.closed {
            return false;
        }
        let shed = (self.shed_total)();
        if shed != self.shed_seen {
            self.upstream_lost = self
                .upstream_lost
                .saturating_add(shed.wrapping_sub(self.shed_seen));
            self.shed_seen = shed;
        }
        // One loss per change of the down-slot mask (a socket went down or
        // came back), never per second while one stays down (LF-9 finding 11).
        let sock = (self.socket_state)();
        if sock != self.socket_seen {
            self.socket_seen = sock;
            self.upstream_lost = self.upstream_lost.saturating_add(1);
        }
        let t = TapTick::from_parsed(tick, best, self.drop_seq(), new_trade, older_trade, volume);
        match self.tx.try_send(t) {
            Ok(()) => {
                self.publish_loss();
                // The wake waits for the frame's end (`flush_wake`), so a
                // thread that catches up mid-frame is not woken per tick.
                self.wake_pending = true;
                true
            }
            Err(FrameRingTrySendError::Full(_)) => {
                self.dropped = self.dropped.saturating_add(1);
                self.publish_loss();
                false
            }
            Err(FrameRingTrySendError::Closed(_)) => {
                self.on_closed();
                false
            }
        }
    }

    /// Wakes the chain thread when a tick was queued since the last call.
    /// Called ONCE per frame, after the frame walk, so a frame costs at most
    /// one [`ChainWake::wake`] (one unpark on the idle-to-busy edge) however
    /// many ticks it queued. No tick is stranded: every push precedes this
    /// `SeqCst` load in program order (see [`ChainWake`]). O(1): one branch
    /// when nothing was queued.
    #[inline]
    pub fn flush_wake(&mut self) {
        if self.wake_pending {
            self.wake_pending = false;
            if let Some(w) = self.wake.as_deref() {
                w.wake();
            }
        }
    }

    /// The chain thread dropped its receiver: stop tapping for the rest of the
    /// process and say so once. Cold (once per process).
    #[cold]
    #[inline(never)]
    fn on_closed(&mut self) {
        self.closed = true;
        tracing::error!(
            code = ErrorCode::OrderReady01GateRefused.code_str(),
            source = "live_fill_tap_closed",
            "live paper fill tap: the chain thread has stopped, so the drain stops \
             handing it ticks for the rest of this process (paper chain only, no tick \
             is lost from storage)"
        );
    }

    /// Every loss so far (tap drops plus upstream losses). O(1).
    #[inline]
    fn loss_total(&self) -> u64 {
        self.dropped.wrapping_add(self.upstream_lost)
    }

    /// Stores the loss total for the chain thread when it moved. O(1): one
    /// compare, and one relaxed store on a loss.
    #[inline]
    fn publish_loss(&mut self) {
        let total = self.loss_total();
        if total != self.loss_published {
            self.loss_published = total;
            if let Some(out) = self.loss_out.as_ref() {
                out.store(total, Ordering::Relaxed);
            }
        }
    }

    /// The loss count the next tick carries: the low 32 bits of every loss
    /// so far. The chain only compares two consecutive values, so wrapping is
    /// harmless. O(1).
    #[inline]
    fn drop_seq(&self) -> u32 {
        // APPROVED: deliberate truncation to the low 32 bits (wrapping compare).
        #[allow(clippy::cast_possible_truncation)]
        let seq = self.loss_total() as u32;
        seq
    }

    /// The drain abandoned the tail of a live main-feed frame (an unknown
    /// packet code or a partial packet), or folded a live tick whose row
    /// append failed: ticks may be missing from the stream. O(1).
    #[inline]
    pub fn note_upstream_loss(&mut self) {
        self.upstream_lost = self.upstream_lost.saturating_add(1);
        self.publish_loss();
    }

    /// Ticks this tap's ring dropped so far.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Losses before the drain (shed frames, abandoned tails) seen so far.
    #[must_use]
    pub fn upstream_lost(&self) -> u64 {
        self.upstream_lost
    }

    /// Adds the drops since the last call to the counter, with one `warn!` when
    /// there were any. Cold (30 s arm).
    pub fn publish_dropped(&mut self) {
        let delta = self.dropped.saturating_sub(self.published);
        if delta > 0 {
            metrics::counter!(LIVE_FILL_TAP_DROPPED_COUNTER).increment(delta);
            self.published = self.dropped;
            tracing::warn!(
                dropped = delta,
                total = self.dropped,
                "live paper fill tap: the chain thread fell behind and ticks were dropped; \
                 the candles they touched refuse orders (paper chain only, no tick is lost \
                 from storage)"
            );
        }
    }
}

/// Builds the ring with the receiver in busy-poll mode BEFORE any sender
/// exists, so no `try_send` ever takes the wake path. Cold.
#[must_use]
pub fn live_fill_tap_ring(
    capacity: usize,
) -> (FrameRingSender<TapTick>, FrameRingReceiver<TapTick>) {
    let (tx, rx) = frame_ring::<TapTick>(capacity);
    rx.set_busy_polling(true);
    (tx, rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(sid: u64, ltp: f32, exch: u32, vol: u32) -> ParsedTick {
        ParsedTick {
            security_id: sid,
            exchange_segment_code: 2,
            last_traded_price: ltp,
            exchange_timestamp: exch,
            received_at_nanos: 1_000_000_000,
            volume: vol,
            ..ParsedTick::default()
        }
    }

    #[test]
    fn test_tap_gate_refuses_replayed_frames_and_wal_refold() {
        assert!(tap_gate(0, false));
        assert!(tap_gate(15, false));
        assert!(!tap_gate(u8::MAX, false), "a replayed WAL frame never taps");
        assert!(!tap_gate(0, true), "nothing taps while the WAL re-folds");
        assert!(!tap_gate(u8::MAX, true));
    }

    #[test]
    fn test_tap_tick_from_parsed_carries_best_level_and_ist_receipt() {
        let p = parsed(42, 101.5, 1_779_355_000, 900);
        let level = MarketDepthLevel {
            bid_quantity: 10,
            ask_quantity: 20,
            bid_orders: 1,
            ask_orders: 2,
            bid_price: 101.45,
            ask_price: 101.55,
        };
        let t = TapTick::from_parsed(&p, Some(&level), 7, true, true, TapVolume::default());
        assert_eq!(t.security_id, 42);
        assert_eq!(t.segment, 2);
        assert_eq!(t.receipt_ist_ns, 1_000_000_000 + IST_OFFSET_NS);
        assert_eq!(t.cum_volume, 900);
        assert_eq!(t.best_bid_qty, 10);
        assert_eq!(t.best_ask_qty, 20);
        assert!((t.best_ask - 101.55).abs() < 1e-6);
        assert_eq!(t.drop_seq, 7);
        assert!(
            t.new_trade && !t.older_trade,
            "a new trade is never also older"
        );
        let no_depth = TapTick::from_parsed(&p, None, 0, false, true, TapVolume::default());
        assert!(no_depth.older_trade);
        assert_eq!(no_depth.best_bid, 0.0);
        assert_eq!(no_depth.best_ask_qty, 0);
        assert!(std::mem::size_of::<TapTick>() <= 64);
    }

    #[test]
    fn test_live_fill_tap_offer_publish_dropped_and_live_fill_tap_ring() {
        let (tx, mut rx) = live_fill_tap_ring(2);
        assert!(rx.is_busy_polling());
        let mut tap = LiveFillTap::with_shed_source(tx, || 0);
        assert_eq!(tap.upstream_lost(), 0);
        let p = parsed(1, 10.0, 100, 1);
        assert!(tap.offer(&p, None, true, false, TapVolume::default()));
        assert!(tap.offer(&p, None, true, false, TapVolume::default()));
        assert!(
            !tap.offer(&p, None, true, false, TapVolume::default()),
            "full ring drops"
        );
        assert!(!tap.offer(&p, None, true, false, TapVolume::default()));
        assert_eq!(tap.dropped(), 2);
        assert_eq!(rx.try_recv().map(|t| t.drop_seq), Some(0));
        assert_eq!(rx.try_recv().map(|t| t.drop_seq), Some(0));
        assert!(tap.offer(&p, None, true, false, TapVolume::default()));
        assert_eq!(
            rx.try_recv().map(|t| t.drop_seq),
            Some(2),
            "the next tick names the drops before it"
        );
        tap.publish_dropped();
        tap.publish_dropped();
        assert_eq!(tap.published, 2);
        drop(rx);
        assert!(
            !tap.offer(&p, None, true, false, TapVolume::default()),
            "a closed ring queues nothing"
        );
        assert_eq!(tap.dropped(), 2, "a closed ring is not a fell-behind drop");
    }

    static CLOSED_SHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static CLOSED_SHED_READS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    fn closed_shed_now() -> u64 {
        CLOSED_SHED_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        CLOSED_SHED.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// LF-6b review round 2, finding 1: once the chain thread is gone (its
    /// receiver dropped after a refused day roll), the tap stops for good: a
    /// later offer reads no source, stores no loss total and counts no drop,
    /// so the 30 s arm never reports a fell-behind that did not happen.
    #[test]
    fn test_regression_closed_ring_stops_the_tap_without_dead_work_or_fell_behind_warn() {
        let (tx, rx) = live_fill_tap_ring(4);
        let loss = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut tap = LiveFillTap::with_shed_source(tx, closed_shed_now)
            .with_loss_out(std::sync::Arc::clone(&loss));
        let p = parsed(1, 10.0, 100, 1);
        drop(rx);
        assert!(!tap.offer(&p, None, true, false, TapVolume::default()));
        assert!(tap.closed, "the first closed send stops the tap");
        let reads = CLOSED_SHED_READS.load(std::sync::atomic::Ordering::Relaxed);
        let stored = loss.load(std::sync::atomic::Ordering::Relaxed);
        CLOSED_SHED.store(9, std::sync::atomic::Ordering::Relaxed);
        assert!(!tap.offer(&p, None, true, false, TapVolume::default()));
        assert!(!tap.offer(&p, None, true, false, TapVolume::default()));
        assert_eq!(
            CLOSED_SHED_READS.load(std::sync::atomic::Ordering::Relaxed),
            reads,
            "a stopped tap reads no upstream source"
        );
        assert_eq!(
            loss.load(std::sync::atomic::Ordering::Relaxed),
            stored,
            "a stopped tap stores no loss total"
        );
        assert_eq!(tap.dropped(), 0, "a gone thread is not a fell-behind drop");
        tap.publish_dropped();
        assert_eq!(tap.published, 0, "nothing to report as fell behind");
    }

    static SHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    fn shed_now() -> u64 {
        SHED.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A frame shed before the drain (or an abandoned frame tail) between two
    /// tapped ticks moves the next tick's `drop_seq`, so the chain taints the
    /// candles in between; `new` starts from the production total.
    #[test]
    fn test_upstream_sheds_and_note_upstream_loss_move_drop_seq_with_shed_source_upstream_lost() {
        let (tx, mut rx) = live_fill_tap_ring(8);
        SHED.store(5, std::sync::atomic::Ordering::Relaxed);
        let mut tap = LiveFillTap::with_shed_source(tx, shed_now);
        let p = parsed(1, 10.0, 100, 1);
        assert!(tap.offer(&p, None, true, false, TapVolume::default()));
        assert_eq!(
            rx.try_recv().map(|t| t.drop_seq),
            Some(0),
            "sheds before the tap do not count"
        );
        SHED.store(8, std::sync::atomic::Ordering::Relaxed);
        assert!(tap.offer(&p, None, true, false, TapVolume::default()));
        assert_eq!(rx.try_recv().map(|t| t.drop_seq), Some(3));
        assert_eq!(tap.upstream_lost(), 3);
        assert_eq!(tap.dropped(), 0, "the tap's own drop metric is unchanged");
        tap.note_upstream_loss();
        assert!(tap.offer(&p, None, false, false, TapVolume::default()));
        let t = rx.try_recv().expect("queued");
        assert_eq!(t.drop_seq, 4);
        assert!(!t.new_trade);
        let (tx2, _rx2) = live_fill_tap_ring(1);
        let prod = LiveFillTap::new(tx2);
        assert_eq!(prod.upstream_lost(), 0);
    }

    static SOCKETS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    fn sockets_now() -> u64 {
        SOCKETS.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// LF-6b review finding 1, as LF-9 finding 11 bounds it: one of two
    /// main-feed sockets closes and redials while the other keeps delivering.
    /// No global silence ever shows, yet the stretch around the close and the
    /// stretch around the redial read untrusted (a bar of the down socket's
    /// instrument holding either is refused), and the loss total reaches the
    /// chain thread out of band. The downtime counts ONE loss per change of
    /// the down mask, never one per second: the middle of a long downtime is
    /// not tainted for the healthy socket's instruments, and a socket parked
    /// for the rest of the day taints only around the park.
    #[test]
    fn test_one_main_feed_socket_down_taints_the_stretch_with_sources_with_loss_out() {
        use crate::live_fill::taint::{StreamWatch, TaintRanges, TaintReason};
        const NS: i64 = 1_000_000_000;
        // 10:00:00 IST of an arbitrary day; receipt in UTC nanos.
        let t0_ist: i64 = 20_000 * 86_400 + 36_000;
        let utc = |ist_secs: i64, frac_ns: i64| (ist_secs - IST_OFFSET_NS / NS) * NS + frac_ns;
        let run = |down_at: i64, up_at: Option<i64>, secs: i64| {
            let (tx, mut rx) = live_fill_tap_ring(4_096);
            let loss = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            SOCKETS.store(0, std::sync::atomic::Ordering::Relaxed);
            let mut tap = LiveFillTap::with_sources(tx, || 0, sockets_now)
                .with_loss_out(std::sync::Arc::clone(&loss));
            let mut watch = StreamWatch::new();
            let mut taints = TaintRanges::new();
            let mut cum = 0u32;
            // The sibling socket ticks every 100 ms from 10:00:00.
            for step in 0..secs * 10 {
                let ist = t0_ist + step / 10;
                let frac = (step % 10) * 100_000_000;
                if ist == t0_ist + down_at && frac == 0 {
                    SOCKETS.store((1u64 << 32) | 1, std::sync::atomic::Ordering::Relaxed);
                }
                if up_at.is_some_and(|u| ist == t0_ist + u) && frac == 0 {
                    SOCKETS.store(2u64 << 32, std::sync::atomic::Ordering::Relaxed);
                }
                cum += 1;
                let mut p = parsed(7, 100.0, u32::try_from(ist).expect("fits"), cum);
                p.received_at_nanos = utc(ist, frac);
                assert!(tap.offer(&p, None, true, false, TapVolume::default()));
                let t = rx.try_recv().expect("queued");
                watch.on_tick(
                    t.drop_seq,
                    i64::from(t.exchange_secs),
                    t.receipt_ist_ns,
                    true,
                    &mut taints,
                );
                let wall = t.receipt_ist_ns + 1_000_000;
                watch.on_pass(wall, wall, &mut taints);
            }
            assert_eq!(
                loss.load(std::sync::atomic::Ordering::Relaxed),
                tap.upstream_lost(),
                "the chain thread sees the same loss total out of band"
            );
            (taints, tap.upstream_lost())
        };
        // Socket 0 down from 10:00:20 to 10:08:00 (a long 805-style park).
        let (taints, lost) = run(20, Some(480), 600);
        assert_eq!(lost, 2, "one loss when it went down, one when it came back");
        for s in [t0_ist + 20, t0_ist + 60, t0_ist + 479, t0_ist + 480] {
            assert_eq!(
                taints.taint_of(s, s + 1),
                Some(TaintReason::TickDropped),
                "second {} next to the close or the redial must read untrusted",
                s - t0_ist
            );
        }
        assert_eq!(
            taints.taint_of(t0_ist + 120, t0_ist + 150),
            None,
            "the middle of a long downtime does not taint the healthy socket's instruments"
        );
        assert_eq!(
            taints.taint_of(t0_ist + 590, t0_ist + 600),
            None,
            "after the socket is back and the lag bound passes, candles are trusted"
        );
        // Parked at 10:00:20 for the rest of the run (a terminal 808 / 806 /
        // 810): one loss, and nothing after the stretch around the park.
        let (taints, lost) = run(20, None, 1_200);
        assert_eq!(lost, 1, "a parked socket counts once, not once a second");
        assert_eq!(
            taints.taint_of(t0_ist + 20, t0_ist + 21),
            Some(TaintReason::TickDropped)
        );
        assert_eq!(
            taints.taint_of(t0_ist + 300, t0_ist + 1_199),
            None,
            "a parked socket must not taint every instrument for the rest of the day"
        );
        // No socket ever down: nothing is tainted.
        let (tx, mut rx) = live_fill_tap_ring(16);
        let mut quiet = LiveFillTap::with_sources(tx, || 0, || 0);
        let mut p = parsed(7, 100.0, u32::try_from(t0_ist).expect("fits"), 1);
        for k in 0..5 {
            p.received_at_nanos = utc(t0_ist + k, 0);
            assert!(quiet.offer(&p, None, true, false, TapVolume::default()));
            assert_eq!(rx.try_recv().map(|t| t.drop_seq), Some(0));
        }
    }

    // ---------------- §28.6: the chain thread's instant wake ----------------

    /// A thread parked on an empty ring wakes as soon as the frame that
    /// queued a tick ends (`flush_wake`), not when its bound runs out.
    /// Without the wake in `flush_wake` the park lasts the full 10 s bound.
    #[test]
    fn test_tap_offer_wakes_a_parked_chain_thread_at_once_with_wake_from_link() {
        let (tx, rx) = live_fill_tap_ring(8);
        let wake = Arc::new(ChainWake::new());
        let link = TapLink {
            tx,
            loss: Arc::new(AtomicU64::new(0)),
            wake: Arc::clone(&wake),
        };
        let mut tap = LiveFillTap::from_link(link);
        let w = Arc::clone(&wake);
        let consumer = std::thread::spawn(move || {
            w.register_current_thread();
            let t0 = std::time::Instant::now();
            let woke = w.park_unless(|| rx.is_empty(), std::time::Duration::from_secs(10));
            (woke, t0.elapsed(), rx.len())
        });
        // Wait until the consumer has declared itself parked.
        while !wake.parked.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        let p = parsed(1, 10.0, 100, 1);
        assert!(tap.offer(&p, None, true, false, TapVolume::default()));
        tap.flush_wake();
        let (woke, waited, queued) = consumer.join().expect("consumer");
        assert!(woke, "the tap ended the park");
        assert!(
            waited < std::time::Duration::from_secs(2),
            "woken at once, not at the 10 s bound: waited {waited:?}"
        );
        assert_eq!(queued, 1, "the wake took nothing off the ring");
        assert_eq!(wake.unparks(), 1);
    }

    /// At most one unpark per idle-to-busy edge: a burst of ticks while the
    /// thread is parked unparks once, and ticks while it is awake unpark
    /// never. Fails if `wake` unparks on every tick.
    #[test]
    fn test_wake_unparks_once_per_idle_to_busy_edge_with_wake_none_while_awake() {
        let (tx, mut rx) = live_fill_tap_ring(4_096);
        let wake = Arc::new(ChainWake::new());
        wake.register_current_thread();
        let mut tap = LiveFillTap::with_shed_source(tx, || 0).with_wake(Arc::clone(&wake));
        let p = parsed(1, 10.0, 100, 1);
        // Awake: nothing to unpark.
        for _ in 0..1_000 {
            assert!(tap.offer(&p, None, true, false, TapVolume::default()));
        }
        assert_eq!(wake.unparks(), 0, "no unpark while the thread is awake");
        while rx.try_recv().is_some() {}
        tap.flush_wake();
        assert_eq!(wake.unparks(), 0, "no unpark while the thread is awake");
        // Declared parked: a frame of 1,000 ticks unparks once, at its end.
        wake.parked.store(true, Ordering::SeqCst);
        for _ in 0..1_000 {
            assert!(tap.offer(&p, None, true, false, TapVolume::default()));
        }
        assert_eq!(wake.unparks(), 0, "no wake inside the frame walk");
        tap.flush_wake();
        tap.flush_wake();
        assert_eq!(wake.unparks(), 1, "one unpark for the edge");
        assert!(!wake.parked.load(Ordering::SeqCst), "the tap cleared it");
        // The token it left makes a later park return at once (no tick lost
        // to a wake that arrived before the park).
        let t0 = std::time::Instant::now();
        assert!(!wake.park_unless(|| true, std::time::Duration::from_secs(10)));
        assert!(t0.elapsed() < std::time::Duration::from_secs(2));
        // A full ring queues nothing and unparks nothing.
        let (full_tx, _full_rx) = live_fill_tap_ring(1);
        let mut full = LiveFillTap::with_shed_source(full_tx, || 0).with_wake(Arc::clone(&wake));
        assert!(full.offer(&p, None, true, false, TapVolume::default()));
        full.flush_wake();
        wake.parked.store(true, Ordering::SeqCst);
        assert!(!full.offer(&p, None, true, false, TapVolume::default()));
        full.flush_wake();
        assert_eq!(wake.unparks(), 1, "a refused tick wakes nobody");
    }

    /// `park_unless` does not park when the ring is not empty, and reports a
    /// bound that ran out as not woken.
    #[test]
    fn test_park_unless_skips_the_park_when_work_is_queued_register_current_thread_and_new() {
        let wake = ChainWake::new();
        wake.register_current_thread();
        let t0 = std::time::Instant::now();
        assert!(!wake.park_unless(|| false, std::time::Duration::from_secs(10)));
        assert!(t0.elapsed() < std::time::Duration::from_secs(2));
        assert!(!wake.park_unless(|| true, std::time::Duration::from_millis(5)));
        assert!(!wake.parked.load(Ordering::SeqCst), "never left set");
    }

    /// Steady state with frames spaced apart (the shape of the live drain):
    /// the chain thread drains each frame and parks again before the next,
    /// so the tap pays about one unpark per frame and never more. Pins
    /// the cost the module docs state (§28.6 review: the edge is per frame,
    /// not rare). `is_parked` reports the declared park.
    #[test]
    fn test_steady_state_costs_one_unpark_per_frame_is_parked() {
        const FRAMES: u64 = 100;
        const PER_FRAME: u64 = 8;
        let (tx, mut rx) = live_fill_tap_ring(4_096);
        let wake = Arc::new(ChainWake::new());
        let mut tap = LiveFillTap::with_shed_source(tx, || 0).with_wake(Arc::clone(&wake));
        let w = Arc::clone(&wake);
        let consumer = std::thread::spawn(move || {
            w.register_current_thread();
            let mut got = 0u64;
            while got < FRAMES * PER_FRAME {
                while rx.try_recv().is_some() {
                    got += 1;
                }
                if got < FRAMES * PER_FRAME {
                    w.park_unless(|| rx.is_empty(), std::time::Duration::from_secs(10));
                }
            }
        });
        let p = parsed(1, 10.0, 100, 1);
        for _ in 0..FRAMES {
            // The next frame arrives after the thread has drained and parked.
            let t0 = std::time::Instant::now();
            while !wake.is_parked() && t0.elapsed() < std::time::Duration::from_millis(200) {
                std::thread::yield_now();
            }
            for _ in 0..PER_FRAME {
                assert!(tap.offer(&p, None, true, false, TapVolume::default()));
            }
            tap.flush_wake();
        }
        consumer.join().expect("consumer");
        let unparks = wake.unparks();
        // One wake per frame at most, whatever the thread does mid-frame.
        assert!(
            unparks <= FRAMES,
            "at most one unpark per frame, got {unparks} for {FRAMES} frames"
        );
        assert!(
            unparks >= FRAMES / 2,
            "a frame arriving at a parked thread pays the unpark: got {unparks} for {FRAMES} frames"
        );
    }

    /// One frame whose walk is slow enough for the chain thread to catch up
    /// and park again between two of its ticks (the 2-core preemption shape
    /// of the 2026-11-13 review) still costs at most ONE unpark: the wake
    /// waits for the frame's end. Before the fix `offer` woke per queued
    /// tick, so this consumer was unparked once per tick.
    #[test]
    fn test_flush_wake_costs_at_most_one_unpark_per_frame_while_the_consumer_reparks() {
        const FRAMES: u64 = 6;
        const PER_FRAME: u64 = 12;
        let (tx, mut rx) = live_fill_tap_ring(4_096);
        let wake = Arc::new(ChainWake::new());
        let mut tap = LiveFillTap::with_shed_source(tx, || 0).with_wake(Arc::clone(&wake));
        let w = Arc::clone(&wake);
        let consumer = std::thread::spawn(move || {
            w.register_current_thread();
            let mut got = 0u64;
            while got < FRAMES * PER_FRAME {
                while rx.try_recv().is_some() {
                    got += 1;
                }
                if got < FRAMES * PER_FRAME {
                    w.park_unless(|| rx.is_empty(), std::time::Duration::from_secs(10));
                }
            }
        });
        let p = parsed(1, 10.0, 100, 1);
        let settle = |wake: &ChainWake| {
            let t0 = std::time::Instant::now();
            while !wake.is_parked() && t0.elapsed() < std::time::Duration::from_millis(500) {
                std::thread::yield_now();
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        for _ in 0..FRAMES {
            settle(&wake);
            for _ in 0..PER_FRAME {
                assert!(tap.offer(&p, None, true, false, TapVolume::default()));
                // A slow walk: the thread, if woken, drains and re-parks here.
                settle(&wake);
            }
            tap.flush_wake();
        }
        consumer.join().expect("consumer");
        let unparks = wake.unparks();
        assert!(
            unparks <= FRAMES,
            "at most one unpark per frame: got {unparks} for {FRAMES} frames of {PER_FRAME} ticks"
        );
        assert!(unparks >= 1, "the frame end woke the parked thread");
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(12))]
        /// No tick is ever stranded by the park: a consumer parked with a
        /// 30 s bound takes every tick of a producer sending with random
        /// gaps, and the whole case ends long before that bound could have
        /// been the thing that woke it.
        #[test]
        fn proptest_park_never_strands_a_tick(
            gaps in proptest::collection::vec(0u32..400, 200..4_000),
        ) {
            let total = gaps.len() as u64;
            let (tx, mut rx) = live_fill_tap_ring(64);
            let wake = Arc::new(ChainWake::new());
            let mut tap = LiveFillTap::with_shed_source(tx, || 0).with_wake(Arc::clone(&wake));
            let w = Arc::clone(&wake);
            let consumer = std::thread::spawn(move || {
                w.register_current_thread();
                let (mut got, mut woken) = (0u64, 0u64);
                while got < total {
                    while rx.try_recv().is_some() {
                        got += 1;
                    }
                    if got < total
                        && w.park_unless(|| rx.is_empty(), std::time::Duration::from_secs(30))
                    {
                        woken += 1;
                    }
                }
                woken
            });
            let t0 = std::time::Instant::now();
            let p = parsed(1, 10.0, 100, 1);
            for g in &gaps {
                while !tap.offer(&p, None, true, false, TapVolume::default()) {
                    tap.flush_wake();
                    std::thread::yield_now();
                }
                tap.flush_wake();
                match g % 4 {
                    0 => {}
                    1 => std::thread::yield_now(),
                    2 => std::hint::spin_loop(),
                    _ => std::thread::sleep(std::time::Duration::from_micros(u64::from(*g))),
                }
            }
            let woken = consumer.join().expect("consumer");
            proptest::prop_assert!(
                t0.elapsed() < std::time::Duration::from_secs(20),
                "a tick waited for the 30 s bound: a lost wake"
            );
            proptest::prop_assert!(woken > 0, "the tap, not a timeout, woke the consumer");
        }
    }
}
