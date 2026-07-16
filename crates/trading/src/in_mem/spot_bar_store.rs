//! Month-deep SPOT bar RAM residency store (operator directive 2026-07-16,
//! PR-2 of the data-completeness build — RAMSTORE-01 runbook:
//! `.claude/rules/project/ram-store-error-codes.md`).
//!
//! Operator authority (verbatim): *"how can i believe you that you have all
//! these already available in our in-memory app RAM — especially for the
//! current day and even in the future last one month data should be
//! entirely in memory app RAM, especially for trading decisions of entry
//! and exit"* + *"for only spots we will have minimum one month data"*.
//!
//! ## Shape
//! Per `(feed, security_id, exchange_segment)` slot (I-P1-11 composite —
//! `security_id` alone is never a key) × per [`TfIndex`] ring of SEALED
//! bars. Ring capacity = `spot_days` (config, default 35) ×
//! [`bars_per_day`] for that TF (Σ over the 21 TFs = 1,279 bars/day/slot;
//! ~17 MB at 8 slots × 35 days — test-asserted under a 40 MB ceiling).
//! [`RamBar`] is a 48-byte `Copy` struct; rings are `VecDeque<RamBar>`
//! pre-allocated at slot creation — the steady-state live write is an O(1)
//! `push_back` with front eviction, no per-append allocation.
//!
//! ## Write paths (the rest_candle_fold emit choke points)
//! - LIVE seals + current-day refold re-emits → [`SpotBarStore::append_sealed`]
//!   (upsert-by-ts: refolds re-emit existing buckets, so a matching ts
//!   REPLACES in place — never a duplicate bar, never unordered growth).
//! - Boot catch-up / past-day repair refolds → [`SpotBarStore::record_day_block`]
//!   (the catch-up iterates days NEWEST→OLDEST, so an all-older day block
//!   PREPENDS in one O(day) pass; a day already resident upserts in place).
//!
//! ## Read path (HONEST envelope)
//! Guarded locks (`parking_lot::RwLock`, ~30 ns uncontended) + binary
//! search: O(#slots ≤ 8) slot scan + O(log ring) — NOT lock-free and NOT
//! claimed O(1). Reads are COLD today: NO strategy consumer exists (the
//! §28 boundary — this is the read contract only, the `chain_snapshot`
//! precedent; any future consumer is bound by the §38.8 decision-freshness
//! gate under its own dated operator scope).
//!
//! ## Durability (honest)
//! PROCESS-LOCAL. QuestDB (`candles_*`, DEDUP `ts, security_id, segment,
//! feed`) remains the durable truth; a restart re-fills the rings via
//! PR-1's boot catch-up. Depth is bounded by CAPTURED history — the depth
//! accessors/gauges show the honest fill level, never a fabricated month.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;

use tickvault_common::constants::{MARKET_CLOSE_IST_NANOS, MARKET_OPEN_IST_NANOS};
use tickvault_common::feed::Feed;
use tickvault_common::types::SecurityId;

use crate::candles::{TF_COUNT, TfIndex};

/// Trading-session length in seconds (09:15 → 15:30 IST), const-derived
/// from the canonical nanos constants so a session change cannot silently
/// diverge the ring capacity math.
pub const SESSION_SECS: u32 =
    ((MARKET_CLOSE_IST_NANOS - MARKET_OPEN_IST_NANOS) / 1_000_000_000) as u32;

const _: () = assert!(
    SESSION_SECS == 22_500,
    "session length must be 375 minutes (09:15-15:30 IST)"
);

/// One sealed bar resident in RAM — 48 bytes, `Copy`, no heap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RamBar {
    /// Bucket-open IST epoch seconds (the `TfIndex::bucket_start` grid
    /// value — the same identity as the `candles_*` designated `ts`).
    pub bucket_start_ist_secs: u32,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    /// Official vendor volume Σ for the bucket (0 for indices — honest).
    pub volume: i64,
}

/// Session bars/day for a TF: `ceil(SESSION_SECS / tf_secs)`, floored at 1
/// (D1's 86_400 s bucket still yields one session bar).
#[must_use]
pub const fn bars_per_day(tf: TfIndex) -> u32 {
    let secs = tf.seconds_per_bucket();
    let bars = SESSION_SECS.div_ceil(secs);
    if bars == 0 { 1 } else { bars }
}

/// Σ [`bars_per_day`] over all 21 TFs — the per-slot per-day bar count
/// (1,279; pinned by `test_bars_per_day_session_math`).
#[must_use]
pub fn total_bars_per_day_all_tfs() -> u32 {
    let mut total = 0u32;
    for tf in TfIndex::ALL {
        total += bars_per_day(tf);
    }
    total
}

/// Pure capacity estimate: bytes of pre-allocated ring storage for
/// `slot_count` slots at `spot_days` depth (test-asserted < 40 MB at the
/// 8-slot × 35-day envelope).
#[must_use]
pub fn estimated_capacity_bytes(spot_days: u32, slot_count: u32) -> u64 {
    let bars =
        u64::from(total_bars_per_day_all_tfs()) * u64::from(spot_days) * u64::from(slot_count);
    bars * (core::mem::size_of::<RamBar>() as u64)
}

/// Outcome of one live upsert (test/forensics surface).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    /// New newest bar appended (oldest evicted if at capacity).
    Appended,
    /// Existing bucket ts replaced in place (refold re-emit — idempotent).
    Replaced,
    /// Inserted mid-ring (a bucket never emitted before, older than the
    /// tail — structurally rare; bounded O(ring) memmove, counted).
    InsertedMiddle,
    /// Older than the ring's retained window at capacity — dropped
    /// (correct eviction semantics; counted).
    DroppedOverWindow,
}

/// Outcome of one day-block record (test/forensics surface).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockOutcome {
    /// Bars landed (prepended/appended/replaced).
    pub recorded: usize,
    /// Bars older than the retained window at capacity — dropped.
    pub dropped_over_window: usize,
}

/// One per-TF sorted ring of sealed bars.
#[derive(Debug)]
struct TfRing {
    bars: VecDeque<RamBar>,
    capacity: usize,
}

impl TfRing {
    fn new(capacity: usize) -> Self {
        Self {
            bars: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Upsert one bar by bucket ts, keeping the ring sorted ascending.
    fn upsert(&mut self, bar: RamBar) -> UpsertOutcome {
        let ts = bar.bucket_start_ist_secs;
        match self
            .bars
            .binary_search_by(|b| b.bucket_start_ist_secs.cmp(&ts))
        {
            Ok(idx) => {
                self.bars[idx] = bar;
                UpsertOutcome::Replaced
            }
            Err(idx) if idx == self.bars.len() => {
                if self.bars.len() == self.capacity {
                    self.bars.pop_front();
                }
                self.bars.push_back(bar);
                UpsertOutcome::Appended
            }
            Err(0) => {
                if self.bars.len() == self.capacity {
                    // Older than everything retained at capacity — the
                    // correct eviction answer is to drop the incoming bar.
                    UpsertOutcome::DroppedOverWindow
                } else {
                    self.bars.push_front(bar);
                    UpsertOutcome::InsertedMiddle
                }
            }
            Err(idx) => {
                if self.bars.len() == self.capacity {
                    // Evict the oldest to make room; positions shift by -1.
                    self.bars.pop_front();
                    self.bars.insert(idx - 1, bar);
                } else {
                    self.bars.insert(idx, bar);
                }
                UpsertOutcome::InsertedMiddle
            }
        }
    }

    /// Record one chronological day block. The boot catch-up iterates days
    /// NEWEST→OLDEST, so the common shape is "every bar older than the
    /// current front" — block-PREPEND in one O(day) pass (reverse
    /// push_front keeps ascending order; capacity stops the prepend, the
    /// remainder being older-than-window). Any other shape (empty ring,
    /// or a repair refold of a resident day) upserts per bar.
    fn record_block(&mut self, bars: &[RamBar]) -> BlockOutcome {
        let mut out = BlockOutcome {
            recorded: 0,
            dropped_over_window: 0,
        };
        if bars.is_empty() {
            return out;
        }
        let all_older_than_front = match (self.bars.front(), bars.last()) {
            (Some(front), Some(last)) => last.bucket_start_ist_secs < front.bucket_start_ist_secs,
            _ => false,
        };
        if self.bars.is_empty() {
            for bar in bars {
                if self.bars.len() == self.capacity {
                    out.dropped_over_window += bars.len() - out.recorded;
                    return out;
                }
                self.bars.push_back(*bar);
                out.recorded += 1;
            }
            return out;
        }
        if all_older_than_front {
            for bar in bars.iter().rev() {
                if self.bars.len() == self.capacity {
                    out.dropped_over_window = bars.len() - out.recorded;
                    return out;
                }
                self.bars.push_front(*bar);
                out.recorded += 1;
            }
            return out;
        }
        for bar in bars {
            match self.upsert(*bar) {
                UpsertOutcome::DroppedOverWindow => out.dropped_over_window += 1,
                _ => out.recorded += 1,
            }
        }
        out
    }
}

/// Composite slot identity (I-P1-11: `security_id` alone is never unique —
/// the segment is mandatory; `feed` keeps Dhan/Groww bars distinct, the
/// candles feed-in-key mirror).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotKey {
    pub feed: Feed,
    pub security_id: SecurityId,
    pub exchange_segment_code: u8,
}

/// One (feed, sid, segment) slot: 21 rings behind one RwLock (writes are
/// per-emit-batch cold-path; reads cold — §28).
struct Slot {
    key: SlotKey,
    rings: RwLock<Vec<TfRing>>,
}

/// Store-wide observability snapshot (consumed by the 60 s stats task).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpotStoreStats {
    /// Total resident bars per feed (index = `Feed::index()`).
    pub bars_resident_per_feed: [u64; Feed::COUNT],
    /// MINIMUM distinct-day depth across the feed's slots (the GUARANTEED
    /// depth — never the best-case slot; 0 when the feed has no slots).
    pub min_depth_days_per_feed: [u32; Feed::COUNT],
    /// Pre-allocated ring capacity in bytes (the resident allocation).
    pub estimated_bytes: u64,
    /// Live slot count.
    pub slots: usize,
    /// Lifetime mid-ring inserts (structurally-rare path — forensics).
    pub middle_inserts: u64,
    /// Lifetime over-window drops (correct eviction — forensics).
    pub dropped_over_window: u64,
}

/// The month-deep spot bar RAM store. See the module doc for the full
/// contract + honest envelope.
pub struct SpotBarStore {
    spot_days: u32,
    slots: RwLock<Vec<std::sync::Arc<Slot>>>,
    middle_inserts: AtomicU64,
    dropped_over_window: AtomicU64,
}

impl SpotBarStore {
    /// Builds an empty store with the configured ring depth. Slots (and
    /// their pre-allocated rings) materialize on first write — at boot
    /// that is PR-1's catch-up, so the rings exist before market open.
    #[must_use]
    pub fn new(spot_days: u32) -> Self {
        Self {
            spot_days,
            slots: RwLock::new(Vec::with_capacity(8)),
            middle_inserts: AtomicU64::new(0),
            dropped_over_window: AtomicU64::new(0),
        }
    }

    /// The configured ring depth in trading days.
    #[must_use]
    pub fn spot_days(&self) -> u32 {
        self.spot_days
    }

    fn find_slot(&self, key: SlotKey) -> Option<std::sync::Arc<Slot>> {
        let slots = self.slots.read();
        for slot in slots.iter() {
            if slot.key == key {
                return Some(std::sync::Arc::clone(slot));
            }
        }
        None
    }

    fn find_or_create_slot(&self, key: SlotKey) -> std::sync::Arc<Slot> {
        if let Some(slot) = self.find_slot(key) {
            return slot;
        }
        let mut slots = self.slots.write();
        // Re-check under the write lock (another writer may have raced).
        for slot in slots.iter() {
            if slot.key == key {
                return std::sync::Arc::clone(slot);
            }
        }
        let mut rings = Vec::with_capacity(TF_COUNT);
        for tf in TfIndex::ALL {
            let capacity = (bars_per_day(tf) as usize) * (self.spot_days as usize);
            rings.push(TfRing::new(capacity.max(1)));
        }
        let slot = std::sync::Arc::new(Slot {
            key,
            rings: RwLock::new(rings),
        });
        slots.push(std::sync::Arc::clone(&slot));
        slot
    }

    fn note_outcome(&self, outcome: UpsertOutcome) {
        match outcome {
            UpsertOutcome::InsertedMiddle => {
                self.middle_inserts.fetch_add(1, Ordering::Relaxed);
            }
            UpsertOutcome::DroppedOverWindow => {
                self.dropped_over_window.fetch_add(1, Ordering::Relaxed);
            }
            UpsertOutcome::Appended | UpsertOutcome::Replaced => {}
        }
    }

    /// LIVE write path: upsert one sealed bar by bucket ts (refold
    /// re-emits REPLACE in place — idempotent by construction, mirroring
    /// the `candles_*` DEDUP UPSERT semantics).
    pub fn append_sealed(&self, key: SlotKey, tf: TfIndex, bar: RamBar) -> UpsertOutcome {
        let slot = self.find_or_create_slot(key);
        let mut rings = slot.rings.write();
        let outcome = rings[tf.as_ordinal()].upsert(bar);
        drop(rings);
        self.note_outcome(outcome);
        outcome
    }

    /// CATCH-UP / repair write path: record one chronological day block
    /// (ascending within the day). Newest→oldest catch-up days PREPEND in
    /// one O(day) pass; a resident day upserts in place.
    pub fn record_day_block(&self, key: SlotKey, tf: TfIndex, bars: &[RamBar]) -> BlockOutcome {
        let slot = self.find_or_create_slot(key);
        let mut rings = slot.rings.write();
        let outcome = rings[tf.as_ordinal()].record_block(bars);
        drop(rings);
        if outcome.dropped_over_window > 0 {
            self.dropped_over_window
                .fetch_add(outcome.dropped_over_window as u64, Ordering::Relaxed);
        }
        outcome
    }

    /// Newest-first read of up to `n` bars for one (slot, tf). COLD read
    /// contract (§28) — allocates the result Vec; never on a hot path.
    #[must_use]
    pub fn latest_n(&self, key: SlotKey, tf: TfIndex, n: usize) -> Vec<RamBar> {
        let mut out = Vec::with_capacity(n);
        let Some(slot) = self.find_slot(key) else {
            return out;
        };
        let rings = slot.rings.read();
        let ring = &rings[tf.as_ordinal()];
        for bar in ring.bars.iter().rev().take(n) {
            out.push(*bar);
        }
        out
    }

    /// Exact-bucket read by bucket-open ts (binary search).
    #[must_use]
    pub fn bar_at(&self, key: SlotKey, tf: TfIndex, bucket_start_ist_secs: u32) -> Option<RamBar> {
        let slot = self.find_slot(key)?;
        let rings = slot.rings.read();
        let ring = &rings[tf.as_ordinal()];
        match ring
            .bars
            .binary_search_by(|b| b.bucket_start_ist_secs.cmp(&bucket_start_ist_secs))
        {
            Ok(idx) => Some(ring.bars[idx]),
            Err(_) => None,
        }
    }

    /// Distinct IST calendar days resident in the slot's M1 ring — the
    /// honest depth answer to the operator's "is the month in RAM?".
    /// O(ring) cold scan (sorted ring → counts day transitions).
    #[must_use]
    pub fn depth_days(&self, key: SlotKey) -> u32 {
        let Some(slot) = self.find_slot(key) else {
            return 0;
        };
        let rings = slot.rings.read();
        let ring = &rings[TfIndex::M1.as_ordinal()];
        let mut days = 0u32;
        let mut last_day = u32::MAX;
        for bar in ring.bars.iter() {
            let day = bar.bucket_start_ist_secs / 86_400;
            if day != last_day {
                days += 1;
                last_day = day;
            }
        }
        days
    }

    /// Store-wide observability snapshot (the 60 s stats task's source).
    /// O(slots × rings) cold scan.
    #[must_use]
    pub fn stats(&self) -> SpotStoreStats {
        let mut bars_resident_per_feed = [0u64; Feed::COUNT];
        let mut min_depth_days_per_feed = [u32::MAX; Feed::COUNT];
        let mut seen_feed = [false; Feed::COUNT];
        let mut estimated_bytes = 0u64;
        let slot_arcs: Vec<std::sync::Arc<Slot>> = {
            let slots = self.slots.read();
            let mut copies = Vec::with_capacity(slots.len());
            for slot in slots.iter() {
                copies.push(std::sync::Arc::clone(slot));
            }
            copies
        };
        let slot_count = slot_arcs.len();
        for slot in &slot_arcs {
            let feed_idx = slot.key.feed.index();
            seen_feed[feed_idx] = true;
            let rings = slot.rings.read();
            for ring in rings.iter() {
                bars_resident_per_feed[feed_idx] += ring.bars.len() as u64;
                estimated_bytes += (ring.capacity as u64) * (core::mem::size_of::<RamBar>() as u64);
            }
            drop(rings);
            let depth = self.depth_days(slot.key);
            if depth < min_depth_days_per_feed[feed_idx] {
                min_depth_days_per_feed[feed_idx] = depth;
            }
        }
        for (idx, seen) in seen_feed.iter().enumerate() {
            if !seen {
                min_depth_days_per_feed[idx] = 0;
            }
        }
        SpotStoreStats {
            bars_resident_per_feed,
            min_depth_days_per_feed,
            estimated_bytes,
            slots: slot_count,
            middle_inserts: self.middle_inserts.load(Ordering::Relaxed),
            dropped_over_window: self.dropped_over_window.load(Ordering::Relaxed),
        }
    }
}

/// Process-global store (house `OnceLock` first-wins pattern — the
/// `set_global_fold_bar_sender` precedent). Absent = the feature is
/// config-disabled and every hook is a checked no-op.
static SPOT_BAR_STORE: OnceLock<SpotBarStore> = OnceLock::new();

/// Installs the process-global spot bar store (first-wins, idempotent).
/// Returns `false` when a store was already installed.
pub fn install_spot_bar_store(spot_days: u32) -> bool {
    SPOT_BAR_STORE.set(SpotBarStore::new(spot_days)).is_ok()
}

/// Read-only accessor for the process-global spot bar store.
#[must_use]
pub fn spot_bar_store() -> Option<&'static SpotBarStore> {
    SPOT_BAR_STORE.get()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const DAY0: u32 = 20_000 * 86_400;
    const OPEN0: u32 = DAY0 + 33_300; // 09:15 IST of day 0

    fn key() -> SlotKey {
        SlotKey {
            feed: Feed::Dhan,
            security_id: 13,
            exchange_segment_code: 0,
        }
    }

    fn bar(ts: u32, close: f64) -> RamBar {
        RamBar {
            bucket_start_ist_secs: ts,
            open: close - 1.0,
            high: close + 1.0,
            low: close - 2.0,
            close,
            volume: 7,
        }
    }

    #[test]
    fn test_bars_per_day_session_math() {
        // ceil(22_500 / tf_secs), floored at 1 — spot-checked + summed.
        assert_eq!(bars_per_day(TfIndex::M1), 375);
        assert_eq!(bars_per_day(TfIndex::M2), 188);
        assert_eq!(bars_per_day(TfIndex::M15), 25);
        assert_eq!(bars_per_day(TfIndex::M30), 13);
        assert_eq!(bars_per_day(TfIndex::H1), 7);
        assert_eq!(bars_per_day(TfIndex::H4), 2);
        assert_eq!(bars_per_day(TfIndex::D1), 1);
        assert_eq!(total_bars_per_day_all_tfs(), 1_279);
        assert_eq!(SESSION_SECS, 22_500);
    }

    #[test]
    fn test_estimated_capacity_bytes_under_40mb_envelope() {
        // The design envelope: 8 slots (2 feeds × 4 spot SIDs) × 35 days.
        assert_eq!(core::mem::size_of::<RamBar>(), 48, "RamBar must stay 48 B");
        let bytes = estimated_capacity_bytes(35, 8);
        // 1_279 × 35 × 8 × 48 = 17_189_760 B ≈ 16.4 MiB.
        assert_eq!(bytes, 17_189_760);
        assert!(
            bytes < 40 * 1024 * 1024,
            "spot ring envelope must stay under 40 MB (got {bytes})"
        );
    }

    #[test]
    fn test_spot_bar_store_append_sealed_and_bar_at_roundtrip() {
        let store = SpotBarStore::new(2);
        assert_eq!(store.spot_days(), 2);
        let b = bar(OPEN0, 100.0);
        assert_eq!(
            store.append_sealed(key(), TfIndex::M1, b),
            UpsertOutcome::Appended
        );
        assert_eq!(store.bar_at(key(), TfIndex::M1, OPEN0), Some(b));
        assert_eq!(store.bar_at(key(), TfIndex::M1, OPEN0 + 60), None);
        // A different TF ring is untouched.
        assert_eq!(store.bar_at(key(), TfIndex::M5, OPEN0), None);
    }

    #[test]
    fn test_spot_bar_store_upsert_by_ts_replaces_in_place() {
        // Refold re-emits re-deliver existing bucket ts values — the ring
        // must REPLACE, never duplicate (the DEDUP UPSERT RAM mirror).
        let store = SpotBarStore::new(2);
        store.append_sealed(key(), TfIndex::M1, bar(OPEN0, 100.0));
        store.append_sealed(key(), TfIndex::M1, bar(OPEN0 + 60, 101.0));
        assert_eq!(
            store.append_sealed(key(), TfIndex::M1, bar(OPEN0, 200.0)),
            UpsertOutcome::Replaced
        );
        let bars = store.latest_n(key(), TfIndex::M1, 10);
        assert_eq!(bars.len(), 2, "replace must never grow the ring");
        assert_eq!(bars[1].close, 200.0, "the repaired value must win");
        // A never-before-seen OLDER bucket lands mid-ring (rare, bounded).
        assert_eq!(
            store.append_sealed(key(), TfIndex::M1, bar(OPEN0 + 30, 150.0)),
            UpsertOutcome::InsertedMiddle
        );
        assert_eq!(store.stats().middle_inserts, 1);
    }

    #[test]
    fn test_spot_bar_store_wraparound_evicts_oldest() {
        // spot_days=1 → M30 capacity = 13 bars; the 14th append evicts the
        // oldest, and an over-window old bar is dropped (never displaces).
        let store = SpotBarStore::new(1);
        let cap = bars_per_day(TfIndex::M30) as usize;
        assert_eq!(cap, 13);
        for i in 0..(cap as u32 + 1) {
            store.append_sealed(key(), TfIndex::M30, bar(OPEN0 + i * 1_800, f64::from(i)));
        }
        let bars = store.latest_n(key(), TfIndex::M30, 100);
        assert_eq!(bars.len(), cap, "capacity must hold after wraparound");
        assert_eq!(
            store.bar_at(key(), TfIndex::M30, OPEN0),
            None,
            "the oldest bar must be evicted"
        );
        // An over-window (older-than-front) bar at capacity is dropped.
        assert_eq!(
            store.append_sealed(key(), TfIndex::M30, bar(OPEN0 - 1_800, 999.0)),
            UpsertOutcome::DroppedOverWindow
        );
        assert_eq!(store.stats().dropped_over_window, 1);
    }

    #[test]
    fn test_spot_bar_store_record_day_block_prepends_newest_to_oldest() {
        // The boot catch-up shape: today's block first, then OLDER days —
        // each older block must PREPEND keeping ascending order.
        let store = SpotBarStore::new(3);
        let day1_open = OPEN0 + 86_400;
        let mut today = Vec::with_capacity(3);
        let mut yesterday = Vec::with_capacity(3);
        for i in 0..3u32 {
            today.push(bar(day1_open + i * 60, 200.0 + f64::from(i)));
            yesterday.push(bar(OPEN0 + i * 60, 100.0 + f64::from(i)));
        }
        let first = store.record_day_block(key(), TfIndex::M1, &today);
        assert_eq!(first.recorded, 3);
        let second = store.record_day_block(key(), TfIndex::M1, &yesterday);
        assert_eq!(second.recorded, 3);
        assert_eq!(second.dropped_over_window, 0);
        let bars = store.latest_n(key(), TfIndex::M1, 10);
        assert_eq!(bars.len(), 6);
        // Newest-first: today's last bar leads; ascending order held.
        assert_eq!(bars[0].close, 202.0);
        assert_eq!(bars[5].close, 100.0);
        assert_eq!(store.depth_days(key()), 2);
        // Re-recording a resident day upserts in place (repair refold).
        let mut repaired = yesterday;
        repaired[1].close = 555.0;
        let third = store.record_day_block(key(), TfIndex::M1, &repaired);
        assert_eq!(third.recorded, 3);
        assert_eq!(
            store.latest_n(key(), TfIndex::M1, 10).len(),
            6,
            "repair must never duplicate"
        );
        assert_eq!(
            store
                .bar_at(key(), TfIndex::M1, OPEN0 + 60)
                .map(|b| b.close),
            Some(555.0)
        );
    }

    #[test]
    fn test_spot_bar_store_latest_n_returns_newest_first() {
        let store = SpotBarStore::new(1);
        for i in 0..5u32 {
            store.append_sealed(key(), TfIndex::M1, bar(OPEN0 + i * 60, f64::from(i)));
        }
        let bars = store.latest_n(key(), TfIndex::M1, 3);
        assert_eq!(bars.len(), 3);
        assert_eq!(bars[0].close, 4.0);
        assert_eq!(bars[2].close, 2.0);
        // Unknown slot → empty, never a panic.
        let other = SlotKey {
            feed: Feed::Groww,
            security_id: 99,
            exchange_segment_code: 0,
        };
        assert!(store.latest_n(other, TfIndex::M1, 3).is_empty());
    }

    #[test]
    fn test_spot_bar_store_depth_days_counts_distinct_days() {
        let store = SpotBarStore::new(5);
        assert_eq!(store.depth_days(key()), 0, "empty slot reads 0 depth");
        for day in 0..3u32 {
            store.append_sealed(key(), TfIndex::M1, bar(OPEN0 + day * 86_400, 1.0));
        }
        assert_eq!(store.depth_days(key()), 3);
    }

    #[test]
    fn test_spot_bar_store_stats_and_estimated_bytes() {
        let store = SpotBarStore::new(1);
        store.append_sealed(key(), TfIndex::M1, bar(OPEN0, 1.0));
        let groww_key = SlotKey {
            feed: Feed::Groww,
            security_id: 4_611_686_018_427_387_913,
            exchange_segment_code: 0,
        };
        store.append_sealed(groww_key, TfIndex::M1, bar(OPEN0, 2.0));
        store.append_sealed(groww_key, TfIndex::M1, bar(OPEN0 + 60, 3.0));
        let stats = store.stats();
        assert_eq!(stats.slots, 2);
        assert_eq!(stats.bars_resident_per_feed[Feed::Dhan.index()], 1);
        assert_eq!(stats.bars_resident_per_feed[Feed::Groww.index()], 2);
        assert_eq!(stats.min_depth_days_per_feed[Feed::Dhan.index()], 1);
        assert_eq!(stats.min_depth_days_per_feed[Feed::Groww.index()], 1);
        // Two slots × 1 day × 1,279 bars × 48 B of pre-allocated capacity.
        assert_eq!(stats.estimated_bytes, 2 * 1_279 * 48);
    }

    #[test]
    fn test_install_spot_bar_store_first_wins() {
        // Process-global: the first install wins; the second is refused
        // (idempotent — the fold hooks read whatever was installed first).
        // NOTE: OnceLock is process-wide, so this test is the ONLY one
        // touching the global (unit tests share one process).
        let first = install_spot_bar_store(35);
        let second = install_spot_bar_store(7);
        assert!(first, "first install must succeed");
        assert!(!second, "second install must be refused (first-wins)");
        let store = spot_bar_store().expect("global store must be readable");
        assert_eq!(store.spot_days(), 35, "the FIRST install's depth wins");
    }
}
