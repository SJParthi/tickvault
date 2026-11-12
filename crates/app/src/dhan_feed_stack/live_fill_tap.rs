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
//!   `pool_supervisor::main_feed_live_shed_total` with one relaxed load per
//!   offer. A ring-shed frame is re-folded later from the WAL, but as a
//!   replayed frame, which never taps, so the live stream the chain builds
//!   candles from lacks it;
//! * a frame whose tail the drain abandoned (unknown code, partial packet),
//!   noted by the drain through [`LiveFillTap::note_upstream_loss`].
//!
//! Gates, both at the call site: only frames from a live socket
//! (`connection_index != u8::MAX`, a replayed WAL frame carries `u8::MAX`) and
//! never while the WAL backlog is being re-folded (`replaying_wal`). A
//! replayed tick is history the strategy already passed.
//!
//! # Complexity and allocation
//!
//! [`LiveFillTap::offer`]: O(1), one relaxed atomic load, one `Copy` struct
//! built on the stack and one `try_send` into a crossbeam array slot; no
//! allocation, no lock, no log, no metric macro. The receiver busy-polls
//! ([`FrameRingReceiver::set_busy_polling`] is set before the sender is handed
//! out), so `try_send` never calls `notify_one`. DHAT-gated by
//! `crates/app/tests/dhat_live_fill.rs`. [`LiveFillTap::publish_dropped`] runs
//! on the drain's 30 s silence arm: one counter add.

use tickvault_common::tick_types::{MarketDepthLevel, ParsedTick};
use tickvault_core::websocket::frame_ring::{FrameRingReceiver, FrameRingSender, frame_ring};
use tickvault_core::websocket::pool_supervisor::main_feed_live_shed_total;
use tickvault_trading::brutex_host::live_candles::IST_OFFSET_NS;

/// The counter the drain publishes tap drops on (local `/metrics` only).
pub const LIVE_FILL_TAP_DROPPED_COUNTER: &str = "tv_live_fill_tap_dropped_total";

/// One tick for the chain thread. `Copy`, 56 bytes, no heap.
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
    /// the drain, and abandoned frame tails (see the module docs).
    pub drop_seq: u32,
    pub segment: u8,
    /// The packet carried a NEW trade (the fold's `advanced`). A repeat quote
    /// (the book moved, no trade) or the first packet after a subscribe
    /// carries an OLD trade time, price and cumulative volume: the chain only
    /// takes its best bid/ask (and the volume baseline), never a candle price.
    pub new_trade: bool,
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
}

impl LiveFillTap {
    /// Cold: registers the drop counter at 0 and takes the shed total so far
    /// as the baseline (sheds before the tap existed touched no chain candle).
    #[must_use]
    pub fn new(tx: FrameRingSender<TapTick>) -> Self {
        Self::with_shed_source(tx, main_feed_live_shed_total)
    }

    /// [`Self::new`] with the shed total read from `shed_total` (tests use a
    /// private counter: the production one is process-wide). Cold.
    #[must_use]
    pub fn with_shed_source(tx: FrameRingSender<TapTick>, shed_total: fn() -> u64) -> Self {
        metrics::counter!(LIVE_FILL_TAP_DROPPED_COUNTER).increment(0);
        Self {
            tx,
            dropped: 0,
            published: 0,
            upstream_lost: 0,
            shed_total,
            shed_seen: shed_total(),
        }
    }

    /// Offers one folded tick. `true` when queued; `false` when the ring was
    /// full or closed (counted). Frames shed before the drain since the last
    /// offer are folded into this tick's `drop_seq` first. O(1), no
    /// allocation.
    #[inline]
    pub fn offer(
        &mut self,
        tick: &ParsedTick,
        best: Option<&MarketDepthLevel>,
        new_trade: bool,
    ) -> bool {
        let shed = (self.shed_total)();
        if shed != self.shed_seen {
            self.upstream_lost = self
                .upstream_lost
                .saturating_add(shed.wrapping_sub(self.shed_seen));
            self.shed_seen = shed;
        }
        let t = TapTick::from_parsed(tick, best, self.drop_seq(), new_trade);
        if self.tx.try_send(t).is_ok() {
            true
        } else {
            self.dropped = self.dropped.saturating_add(1);
            false
        }
    }

    /// The loss count the next tick carries: the low 32 bits of every loss
    /// so far. The chain only compares two consecutive values, so wrapping is
    /// harmless. O(1).
    #[inline]
    fn drop_seq(&self) -> u32 {
        // APPROVED: deliberate truncation to the low 32 bits (wrapping compare).
        #[allow(clippy::cast_possible_truncation)]
        let seq = self.dropped.wrapping_add(self.upstream_lost) as u32;
        seq
    }

    /// The drain abandoned the tail of a live main-feed frame (an unknown
    /// packet code or a partial packet): ticks may be missing. O(1).
    #[inline]
    pub fn note_upstream_loss(&mut self) {
        self.upstream_lost = self.upstream_lost.saturating_add(1);
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
        let t = TapTick::from_parsed(&p, Some(&level), 7, true);
        assert_eq!(t.security_id, 42);
        assert_eq!(t.segment, 2);
        assert_eq!(t.receipt_ist_ns, 1_000_000_000 + IST_OFFSET_NS);
        assert_eq!(t.cum_volume, 900);
        assert_eq!(t.best_bid_qty, 10);
        assert_eq!(t.best_ask_qty, 20);
        assert!((t.best_ask - 101.55).abs() < 1e-6);
        assert_eq!(t.drop_seq, 7);
        let no_depth = TapTick::from_parsed(&p, None, 0, false);
        assert_eq!(no_depth.best_bid, 0.0);
        assert_eq!(no_depth.best_ask_qty, 0);
        assert!(std::mem::size_of::<TapTick>() <= 56);
    }

    #[test]
    fn test_live_fill_tap_offer_publish_dropped_and_live_fill_tap_ring() {
        let (tx, mut rx) = live_fill_tap_ring(2);
        assert!(rx.is_busy_polling());
        let mut tap = LiveFillTap::with_shed_source(tx, || 0);
        assert_eq!(tap.upstream_lost(), 0);
        let p = parsed(1, 10.0, 100, 1);
        assert!(tap.offer(&p, None, true));
        assert!(tap.offer(&p, None, true));
        assert!(!tap.offer(&p, None, true), "full ring drops");
        assert!(!tap.offer(&p, None, true));
        assert_eq!(tap.dropped(), 2);
        assert_eq!(rx.try_recv().map(|t| t.drop_seq), Some(0));
        assert_eq!(rx.try_recv().map(|t| t.drop_seq), Some(0));
        assert!(tap.offer(&p, None, true));
        assert_eq!(
            rx.try_recv().map(|t| t.drop_seq),
            Some(2),
            "the next tick names the drops before it"
        );
        tap.publish_dropped();
        tap.publish_dropped();
        assert_eq!(tap.published, 2);
        drop(rx);
        assert!(!tap.offer(&p, None, true), "closed ring counts as a drop");
        assert_eq!(tap.dropped(), 3);
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
        assert!(tap.offer(&p, None, true));
        assert_eq!(
            rx.try_recv().map(|t| t.drop_seq),
            Some(0),
            "sheds before the tap do not count"
        );
        SHED.store(8, std::sync::atomic::Ordering::Relaxed);
        assert!(tap.offer(&p, None, true));
        assert_eq!(rx.try_recv().map(|t| t.drop_seq), Some(3));
        assert_eq!(tap.upstream_lost(), 3);
        assert_eq!(tap.dropped(), 0, "the tap's own drop metric is unchanged");
        tap.note_upstream_loss();
        assert!(tap.offer(&p, None, false));
        let t = rx.try_recv().expect("queued");
        assert_eq!(t.drop_seq, 4);
        assert!(!t.new_trade);
        let (tx2, _rx2) = live_fill_tap_ring(1);
        let prod = LiveFillTap::new(tx2);
        assert_eq!(prod.upstream_lost(), 0);
    }
}
