//! Wave 6 Sub-PR #1 item 1.2 — bounded in-memory seal ring buffer.
//!
//! This module ships the **in-memory absorption tier** of the
//! ring → spill → DLQ pattern locked by decision **L-C1**, mirroring
//! `crates/storage/src/tick_persistence.rs::TickPersistenceWriter`'s
//! ring-buffer machinery. The next slice (in the storage crate) wires
//! this ring to ILP send + disk spill + NDJSON DLQ.
//!
//! ## Why a separate ring (vs reusing tick_persistence's machinery)
//!
//! Per locked decision L-C1: sealed candles are NOT ticks. The IST
//! midnight burst force-seals every open bucket across all 21 TFs in a
//! single tokio yield, and the persistence path differs (21 distinct
//! plain candle tables, one ILP `Sender` per TF). A dedicated ring
//! keeps the seal absorption budget independent of the (since-retired)
//! tick path's rescue ring.
//!
//! ## RAM budget
//!
//! `SEAL_BUFFER_CAPACITY = AGGREGATOR_MAX_SLOTS × TF_COUNT` (25,000 × 24
//! = 600,000) and `BufferedSeal` ≤ 144 bytes → **~86 MB worst-case**.
//! 0.26% of the r8g.xlarge 32 GiB host (operator Quote 13, 2026-08-08).
//! Was a hardcoded 200,000 (~29 MB) until 2026-08-10 — see the constant's
//! own doc for why that literal under-sized the midnight burst by 3×.
//!
//! **This paragraph restated the product as a literal and went stale**
//! (2026-08-14): it read "25,000 × 21 = 525,000 → ~76 MB" after `TF_COUNT`
//! moved 21 → 24 on 2026-08-10. The constant's own doc, forty lines below,
//! explicitly warns against doing exactly that — and this header did it
//! anyway. The numbers above are re-derived; if you are reading them long
//! after 2026-08-14, verify against `TF_COUNT` rather than trusting them.
//!
//! ## Drop semantics on overflow
//!
//! `try_buffer` returns `BufferOutcome::Buffered` on success.
//! `BufferOutcome::DroppedOldest(oldest)` on overflow — the OLDEST
//! seal is evicted to make room for the new one. The caller (a future
//! storage-crate writer task) is responsible for the spill-to-disk
//! escalation: when ring length exceeds the high-watermark, the
//! oldest-N entries spill to `data/spill/seals-YYYYMMDD.bin`. When
//! disk also fails, NDJSON DLQ catches every payload.
//!
//! Drop-OLDEST (vs drop-newest) preserves the most recent seals which
//! are closest to the failure root cause AND match the FIFO
//! `tick_persistence::TickPersistenceWriter` behavior.
//!
//! ## What this module does NOT yet ship
//!
//! - The ILP send adapter (storage-crate slice).
//! - Disk spill on watermark (storage-crate slice).
//! - NDJSON DLQ on dual failure (storage-crate slice).
//! - The async writer task that drains the ring (storage-crate slice).
//! - 7-layer observability hooks (Prom counters per outcome
//!   variant) — wired by the storage-crate slice.
//! - Boundary timer that pushes `force_seal_all` output into this
//!   ring (item 1.3, follow-up).

use std::collections::VecDeque;

use tickvault_common::feed::Feed;

use crate::candles::{LiveCandleState, TfIndex};

/// Maximum number of sealed bars buffered in RAM before overflow
/// triggers either a drop (OLDEST evicted, returned to caller) or
/// disk spill (handled by the storage-crate writer slice).
///
/// Sized to absorb the IST-midnight force-seal burst across ALL
/// instruments × ALL timeframes, plus headroom for downstream
/// backpressure spikes. Per locked decision L-C1.
///
/// DERIVED, not a literal, since 2026-08-10. The previous 200,000
/// carried the comment "generously sized to absorb the IST-midnight
/// force-seal burst across all instruments × 21 TFs" — which was
/// arithmetically FALSE at the configured ceiling:
/// `force_seal_all` emits `AGGREGATOR_MAX_SLOTS × TF_COUNT`
/// seals in one burst, so 200,000 would have force-evicted the
/// remainder down the spill/DLQ tiers in a single tokio yield, every
/// midnight, while the header claimed headroom. Deriving the constant
/// makes that class of drift impossible: change either input and this
/// follows.
///
/// **Do not restate the product as a literal in prose.** The original
/// 2026-08-10 note hardcoded "25,000 × 21 = 525,000 ≈ 76 MB"; `TF_COUNT`
/// then moved to 24, and the derived constant silently became 600,000
/// (≈86 MB) while every comment still said 525,000. The number was
/// re-derived correctly by the compiler and re-stated wrongly by the
/// docs — which is how the storage-side `SEAL_MPSC_CAPACITY` in front of
/// this ring was left at 200,000 through the whole 2026-08-10 repair.
/// Cost scales as `AGGREGATOR_MAX_SLOTS × TF_COUNT × size_of::<BufferedSeal>()`;
/// at the r8g.xlarge 32 GiB host (operator Quote 13, 2026-08-08) that is
/// a fraction of a percent either way. On the retired 4 GiB t4g.medium
/// the same correctness would have cost ~2% of the entire machine, which
/// is very likely why the literal was left low; the instance upgrade is
/// what makes the honest value affordable.
pub const SEAL_BUFFER_CAPACITY: usize =
    crate::candles::multi_tf_aggregator::AGGREGATOR_MAX_SLOTS * crate::candles::tf_index::TF_COUNT;

/// One sealed bar ready to flush to its `candles_*` plain table.
/// `Copy` so the ring's `VecDeque<BufferedSeal>` does not need
/// ref-counted entries. Sized ≤ 128 bytes per the const-assert below.
///
/// The `exchange_segment_code: u8` is the SAME byte as
/// `ParsedTick::exchange_segment_code`. The writer slice maps this
/// to the `exchange_segment` SYMBOL column via
/// `tickvault_common::segment::segment_code_to_str`.
///
/// `tf` is encoded as [`TfIndex`] (1 byte) so the writer slice can
/// dispatch to one of the 21 ILP `Sender`s by ordinal without lookup.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BufferedSeal {
    /// Composite-key part 1 (per I-P1-11).
    pub security_id: u64,
    /// Composite-key part 2 (per I-P1-11).
    pub exchange_segment_code: u8,
    /// Which timeframe sealed this bar.
    pub tf: TfIndex,
    /// Sealed candle state — open / high / low / close / volume / oi /
    /// tick_count + 3 pct fields (already stamped by the seal-time
    /// writer per L-H6). Bucket-open IST seconds = `state.bucket_start_ist_secs`.
    pub state: LiveCandleState,
    /// Broker-source provenance ([`Feed::Dhan`] / [`Feed::Groww`]). The seal
    /// writer stamps `feed.as_str()` into the `candles_<tf>` `feed` SYMBOL column
    /// (part of the DEDUP key `(ts, security_id, segment, feed)`), so a Dhan candle
    /// and a Groww candle for the SAME minute/instrument are BOTH kept — distinct
    /// feeds = distinct observations, never a collision (operator lock 2026-06-19
    /// "same tables + feed column"). ONE feed-parameterized writer serves both feeds.
    pub feed: Feed,
}

impl BufferedSeal {
    /// Constructs a buffered seal from the per-TF callback the
    /// `MultiTfAggregator::consume_tick` emits.
    ///
    /// `feed` is the source feed ([`Feed::Dhan`] / [`Feed::Groww`]); the writer
    /// stamps it as the `feed` SYMBOL so the two feeds never collide under the
    /// shared candle DEDUP key.
    ///
    /// Note: the `state.close_pct_from_prev_day` / `oi_pct_from_prev_day` /
    /// `volume_pct_from_prev_day` fields are 0.0 unless the caller
    /// stamps them before constructing. Per locked decision L-H6 the
    /// stamping is the storage-writer slice's job, NOT the cell
    /// type's — the cell type stays segment-agnostic and pct-cache-agnostic.
    #[inline]
    #[must_use]
    pub const fn new(
        security_id: u64,
        exchange_segment_code: u8,
        tf: TfIndex,
        state: LiveCandleState,
        feed: Feed,
    ) -> Self {
        Self {
            security_id,
            exchange_segment_code,
            tf,
            state,
            feed,
        }
    }
}

// Compile-time size assertion. `BufferedSeal` carries the entire
// `LiveCandleState` (128 bytes after the 2026-06-05 Option B `close_ts_ist_secs`)
// + the routing fields (security_id + segment + tf + padding). 144 bytes covers
// it. Ring RAM = SEAL_BUFFER_CAPACITY × this size — see that constant's note on
// why the product is deliberately NOT restated as a literal here.
const _: () = assert!(
    std::mem::size_of::<BufferedSeal>() <= 144,
    "BufferedSeal exceeded 144-byte budget — ring RAM = SEAL_BUFFER_CAPACITY × this size; bumping requires updating aws-budget.md."
);

/// Outcome of [`SealRing::try_buffer`].
///
/// `Buffered` is the happy path. `DroppedOldest` carries the evicted
/// payload so the caller (a storage-crate writer task) can route it
/// to disk spill or NDJSON DLQ before logging
/// `error!(code = AGGREGATOR-DROP-01)` on the truly-lost case (only
/// when ALL THREE absorbing tiers fail).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BufferOutcome {
    /// Seal accepted into the ring. Caller continues.
    Buffered,
    /// Ring was full; the OLDEST entry was evicted to make room for
    /// the new one. Caller MUST route the evicted entry to the next
    /// absorbing tier (disk spill / DLQ) per L-C1.
    DroppedOldest(BufferedSeal),
}

/// Bounded FIFO ring of sealed bars awaiting persistence.
///
/// `try_buffer` is the producer-side (called from the per-tick
/// callback in `MultiTfAggregator::consume_tick`). `pop_oldest` is
/// the consumer-side (called from the future writer task).
///
/// Single-threaded today — the writer task in the next slice will
/// own this ring and drain it in a tokio loop. The aggregator-side
/// producer pushes via a tokio `mpsc` channel ahead of the ring, OR
/// (TBD per next slice) via a wait-free SPSC if benchmarks show the
/// `mpsc` overhead is unacceptable. This module ships the
/// in-memory data structure; the channel choice + the writer task
/// land in the storage-crate slice.
#[derive(Debug)]
pub struct SealRing {
    inner: VecDeque<BufferedSeal>,
    capacity: usize,
}

impl SealRing {
    /// Constructs an empty ring with the locked
    /// [`SEAL_BUFFER_CAPACITY`] (per L-C1).
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(SEAL_BUFFER_CAPACITY)
    }

    /// Constructs an empty ring with a custom capacity. Used by
    /// tests; production callers should always go via [`Self::new`]
    /// to honor the locked capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        // A zero capacity makes `len() >= capacity` true while the deque is
        // EMPTY, so the eviction arm of `try_buffer` would call `pop_front` on
        // an empty deque and hit its invariant-violation `panic!` on the very
        // first push. Clamping to 1 keeps the ring degenerate-but-sound
        // (every push evicts the previous seal to the next tier) instead of
        // panicking on a hot-path push. `new()` is unaffected.
        Self {
            inner: VecDeque::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
        }
    }

    /// Configured capacity (does NOT shrink as items are pushed/popped).
    #[inline]
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Current number of buffered seals.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// `true` if the ring holds zero seals.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// `true` if the ring is at capacity and the next push will evict
    /// the oldest entry.
    #[inline]
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.inner.len() >= self.capacity
    }

    /// Push a seal onto the ring.
    ///
    /// On success returns [`BufferOutcome::Buffered`].
    /// On overflow returns [`BufferOutcome::DroppedOldest`] carrying
    /// the evicted entry so the caller can route to the next
    /// absorbing tier (disk spill / DLQ) per L-C1.
    pub fn try_buffer(&mut self, seal: BufferedSeal) -> BufferOutcome {
        if self.inner.len() >= self.capacity {
            // Drop-OLDEST per L-C1 (NOT drop-newest — preserve recent
            // seals near the failure root cause).
            let oldest = self.inner.pop_front().unwrap_or_else(|| {
                panic!(
                    "ring at capacity {} but pop_front returned None — invariant violated",
                    self.capacity
                )
            });
            self.inner.push_back(seal);
            BufferOutcome::DroppedOldest(oldest)
        } else {
            self.inner.push_back(seal);
            BufferOutcome::Buffered
        }
    }

    /// Pop the oldest buffered seal. Used by the writer task to drain
    /// in FIFO order. Returns `None` if the ring is empty.
    #[must_use]
    pub fn pop_oldest(&mut self) -> Option<BufferedSeal> {
        self.inner.pop_front()
    }

    /// Drain all buffered seals into the provided callback in FIFO
    /// order. After this call `self.is_empty() == true`. Used by
    /// graceful shutdown to flush remaining seals.
    pub fn drain_all<F: FnMut(BufferedSeal)>(&mut self, mut sink: F) {
        while let Some(seal) = self.inner.pop_front() {
            sink(seal);
        }
    }
}

impl Default for SealRing {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candles::multi_tf_aggregator::AGGREGATOR_MAX_SLOTS;
    use crate::candles::tf_index::TF_COUNT;
    use crate::candles::{LiveCandleState, TfIndex};

    fn mk_state(bucket_start: u32, close_price: f64) -> LiveCandleState {
        let mut s = LiveCandleState::empty();
        s.bucket_start_ist_secs = bucket_start;
        s.open = close_price;
        s.high = close_price;
        s.low = close_price;
        s.close = close_price;
        s
    }

    fn mk_seal(
        sid: u64,
        seg: u8,
        tf: TfIndex,
        bucket_start: u32,
        close_price: f64,
    ) -> BufferedSeal {
        BufferedSeal::new(
            sid,
            seg,
            tf,
            mk_state(bucket_start, close_price),
            Feed::Dhan,
        )
    }

    #[test]
    fn test_buffered_seal_size_within_budget() {
        // Pinned by `const _ = assert!` above. Runtime-mirrored here
        // so a future field bloat fails grep-able tests too.
        assert!(std::mem::size_of::<BufferedSeal>() <= 144);
    }

    #[test]
    fn test_buffered_seal_new_carries_routing_fields() {
        let seal = mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0);
        assert_eq!(seal.security_id, 13);
        assert_eq!(seal.exchange_segment_code, 0);
        assert_eq!(seal.tf, TfIndex::M1);
        assert_eq!(seal.state.bucket_start_ist_secs, 1_716_000_900);
        assert_eq!(seal.state.close, 100.0);
    }

    #[test]
    fn test_seal_buffer_capacity_absorbs_the_whole_midnight_burst() {
        // L-C1 design lock, RE-BLESSED 2026-08-10. The lock is no longer a
        // literal but the PROPERTY the literal was meant to guarantee: the
        // ring must hold an entire `force_seal_all` burst without evicting.
        //
        // The old form asserted `== 200_000` while `force_seal_all` emits
        // AGGREGATOR_MAX_SLOTS × TF_COUNT = 525,000 — so the ratchet was
        // actively PINNING a capacity 2.6× too small and reading as a safety
        // guarantee. Asserting the property instead of the number means
        // raising either input can never silently outgrow the ring again.
        assert_eq!(
            SEAL_BUFFER_CAPACITY,
            AGGREGATOR_MAX_SLOTS * TF_COUNT,
            "the ring must be sized to the full midnight force-seal burst"
        );
        assert!(
            SEAL_BUFFER_CAPACITY >= AGGREGATOR_MAX_SLOTS * TF_COUNT,
            "SEAL_BUFFER_CAPACITY {SEAL_BUFFER_CAPACITY} is smaller than one \
             force_seal_all burst ({AGGREGATOR_MAX_SLOTS} slots × {TF_COUNT} \
             timeframes) — the excess would be force-evicted to spill/DLQ in a \
             single yield every IST midnight"
        );
    }

    #[test]
    fn test_seal_ring_ram_budget_fits_the_locked_host() {
        // 32 GiB host (r8g.xlarge, operator Quote 13). The ring must stay a
        // rounding error against it, or the derivation above has quietly
        // become a memory problem rather than a correctness fix.
        const HOST_BYTES: usize = 32 * 1024 * 1024 * 1024;
        let ring_bytes = SEAL_BUFFER_CAPACITY * std::mem::size_of::<BufferedSeal>();
        assert!(
            ring_bytes * 100 < HOST_BYTES,
            "seal ring would take {ring_bytes} bytes, over 1% of the 32 GiB host"
        );
    }

    #[test]
    fn test_seal_ring_new_starts_empty() {
        let ring = SealRing::new();
        assert!(ring.is_empty());
        assert!(!ring.is_full());
        assert_eq!(ring.len(), 0);
        assert_eq!(ring.capacity(), SEAL_BUFFER_CAPACITY);
    }

    #[test]
    fn test_seal_ring_with_capacity_uses_provided_value() {
        let ring = SealRing::with_capacity(7);
        assert_eq!(ring.capacity(), 7);
        assert!(ring.is_empty());
    }

    #[test]
    fn test_seal_ring_try_buffer_returns_buffered_when_room() {
        let mut ring = SealRing::with_capacity(3);
        let seal = mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0);
        let outcome = ring.try_buffer(seal);
        assert_eq!(outcome, BufferOutcome::Buffered);
        assert_eq!(ring.len(), 1);
        assert!(!ring.is_full());
    }

    #[test]
    fn test_seal_ring_try_buffer_evicts_oldest_on_overflow() {
        let mut ring = SealRing::with_capacity(2);
        let s1 = mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0);
        let s2 = mk_seal(13, 0, TfIndex::M1, 1_716_000_960, 101.0);
        let s3 = mk_seal(13, 0, TfIndex::M1, 1_716_001_020, 102.0);
        assert_eq!(ring.try_buffer(s1), BufferOutcome::Buffered);
        assert_eq!(ring.try_buffer(s2), BufferOutcome::Buffered);
        assert!(ring.is_full());
        // s3 evicts s1 (drop-OLDEST per L-C1).
        let outcome = ring.try_buffer(s3);
        assert_eq!(outcome, BufferOutcome::DroppedOldest(s1));
        assert_eq!(ring.len(), 2);
        // FIFO order preserved: s2 is now oldest, s3 newest.
        assert_eq!(ring.pop_oldest(), Some(s2));
        assert_eq!(ring.pop_oldest(), Some(s3));
        assert_eq!(ring.pop_oldest(), None);
    }

    #[test]
    fn test_seal_ring_pop_oldest_returns_in_fifo_order() {
        let mut ring = SealRing::with_capacity(5);
        for i in 0..5 {
            let seal = mk_seal(
                13,
                0,
                TfIndex::M1,
                1_716_000_900 + i * 60,
                100.0 + f64::from(i),
            );
            assert_eq!(ring.try_buffer(seal), BufferOutcome::Buffered);
        }
        for i in 0..5 {
            let popped = ring.pop_oldest().expect("non-empty");
            assert_eq!(popped.state.bucket_start_ist_secs, 1_716_000_900 + i * 60);
        }
        assert!(ring.is_empty());
    }

    #[test]
    fn test_seal_ring_drain_all_emits_in_fifo_order_and_empties() {
        let mut ring = SealRing::with_capacity(3);
        for i in 0..3 {
            ring.try_buffer(mk_seal(13, 0, TfIndex::M1, 1_716_000_900 + i * 60, 100.0));
        }
        let mut emitted: Vec<u32> = Vec::new();
        ring.drain_all(|s| emitted.push(s.state.bucket_start_ist_secs));
        assert_eq!(emitted, vec![1_716_000_900, 1_716_000_960, 1_716_001_020]);
        assert!(ring.is_empty());
    }

    #[test]
    fn test_seal_ring_drain_all_on_empty_is_noop() {
        let mut ring = SealRing::with_capacity(3);
        let mut count = 0;
        ring.drain_all(|_| count += 1);
        assert_eq!(count, 0);
        assert!(ring.is_empty());
    }

    #[test]
    fn test_seal_ring_is_full_threshold_correct() {
        let mut ring = SealRing::with_capacity(2);
        assert!(!ring.is_full());
        ring.try_buffer(mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0));
        assert!(!ring.is_full());
        ring.try_buffer(mk_seal(13, 0, TfIndex::M1, 1_716_000_960, 101.0));
        assert!(ring.is_full());
    }

    #[test]
    fn test_seal_ring_capacity_one_correctly_evicts_each_push() {
        // Edge case: capacity 1. Every new push evicts the previous.
        let mut ring = SealRing::with_capacity(1);
        let s1 = mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0);
        let s2 = mk_seal(13, 0, TfIndex::M1, 1_716_000_960, 101.0);
        assert_eq!(ring.try_buffer(s1), BufferOutcome::Buffered);
        assert_eq!(ring.try_buffer(s2), BufferOutcome::DroppedOldest(s1));
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.pop_oldest(), Some(s2));
    }

    #[test]
    fn test_seal_ring_default_uses_locked_capacity() {
        let ring = SealRing::default();
        assert_eq!(ring.capacity(), SEAL_BUFFER_CAPACITY);
    }

    #[test]
    fn test_buffer_outcome_is_copy_for_zero_alloc_callers() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<BufferOutcome>();
        assert_copy::<BufferedSeal>();
    }

    #[test]
    fn test_seal_ring_distinguishes_segments_for_i_p1_11() {
        // Composite-key safety: same security_id on two different
        // segments produces two independent ring entries; they never
        // collapse via the BufferedSeal Eq.
        let mut ring = SealRing::with_capacity(4);
        let seg0 = mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0);
        let seg1 = mk_seal(13, 1, TfIndex::M1, 1_716_000_900, 200.0);
        ring.try_buffer(seg0);
        ring.try_buffer(seg1);
        assert_eq!(ring.len(), 2);
        let popped0 = ring.pop_oldest().expect("non-empty");
        let popped1 = ring.pop_oldest().expect("non-empty");
        assert_ne!(popped0, popped1);
        assert_eq!(popped0.exchange_segment_code, 0);
        assert_eq!(popped1.exchange_segment_code, 1);
        assert_eq!(popped0.state.close, 100.0);
        assert_eq!(popped1.state.close, 200.0);
    }

    #[test]
    fn test_seal_ring_handles_every_tf_distinctly() {
        // Push one seal per TF, drain in FIFO, verify TfIndex preserved.
        // Capacity is TF_COUNT, not a literal: at a hardcoded 21 the three
        // frames appended on 2026-08-10 would have been silently EVICTED
        // and this test would have asserted against a truncated drain.
        let mut ring = SealRing::with_capacity(TF_COUNT);
        for tf in TfIndex::ALL {
            ring.try_buffer(mk_seal(13, 0, tf, 1_716_000_900, 100.0));
        }
        let mut drained_tfs: Vec<TfIndex> = Vec::new();
        ring.drain_all(|s| drained_tfs.push(s.tf));
        assert_eq!(drained_tfs, TfIndex::ALL.to_vec());
    }
}
