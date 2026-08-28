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
//! [`bars_per_day`] for that TF (Σ over the 5 RAM-resident TFs = 601
//! bars/day/slot; ~17 MB at 8 slots × 35 days — test-asserted under a
//! 40 MB ceiling). The 16 GDF-gated second-scale frames (C3) are
//! allocated as capacity-1 placeholders (16 × 48 B = 768 B/slot) — ZERO
//! rows until the GDF 1s live feed lands; full-formula rings for them
//! (Σ 75_413 bars/day/slot) would be ~966 MB at 8 × 35, blowing the
//! t4g.medium envelope for frames no feed writes yet.
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
//! ## Slot lookup (2026-08-10)
//! Slots live in a `HashMap<SlotKey, Arc<Slot>>` keyed on the FULL I-P1-11
//! composite `(feed, security_id, exchange_segment_code)` — `security_id`
//! alone is NEVER a key. Lookup is therefore **O(1) average** (one hash +
//! one bucket probe), independent of `MAX_SPOT_BAR_SLOTS`.
//!
//! Before 2026-08-10 this was a `Vec<Arc<Slot>>` walked linearly on EVERY
//! read and EVERY write — O(#slots), i.e. up to 256 `SlotKey` comparisons
//! per bar. The module header still claimed "O(#slots ≤ 8)" from the era
//! when the cap was 8; the cap became 256 on 2026-08-07 and the scan cost
//! rose 32x with it. The structure, not the comment, is fixed here.
//!
//! Honest residual: the returned `Arc<Slot>` still costs one atomic refcount
//! bump per call. That is O(1) (a single `fetch_add`), not a scan, and it is
//! what lets the caller release the table lock BEFORE taking the slot lock —
//! the property that keeps `find_or_create_slot`'s read→write upgrade
//! deadlock-free. It is deliberately kept.
//!
//! ## Read path (HONEST envelope)
//! Guarded locks (`parking_lot::RwLock`, ~30 ns uncontended) + binary
//! search: O(1)-average slot lookup + O(log ring) bucket search — NOT
//! lock-free, and the RING half is NOT claimed O(1). Reads are COLD
//! today: NO strategy consumer exists (the
//! §28 boundary — this is the read contract only, the `chain_snapshot`
//! precedent; any future consumer is bound by the §38.8 decision-freshness
//! gate under its own dated operator scope).
//!
//! ## Durability (honest)
//! PROCESS-LOCAL. QuestDB (`candles_*`, DEDUP `ts, security_id, segment,
//! feed`) remains the durable truth; a restart re-fills the rings via
//! PR-1's boot catch-up. Depth is bounded by CAPTURED history — the depth
//! accessors/gauges show the honest fill level, never a fabricated month.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;

use tickvault_common::constants::{MARKET_CLOSE_IST_NANOS, MARKET_OPEN_IST_NANOS};
use tickvault_common::feed::Feed;
use tickvault_common::types::SecurityId;

use crate::candles::{TF_COUNT, TfIndex};

/// Trading-session length in seconds (09:15 → 15:40 IST), const-derived
/// from the canonical nanos constants so a session change cannot silently
/// diverge the ring capacity math.
///
/// 2026-08-07: 22_500 (375 min) -> 23_100 (385 min). NSE extended equity
/// derivatives to 15:40 on 2026-08-03 for the new Closing Auction Session —
/// see `MARKET_CLOSE_IST_NANOS`. This const-assert is the reason that change
/// could not land half-applied: the ring capacity math derives from the same
/// two constants, so a session move that missed this file would fail the
/// BUILD rather than silently under-size every per-timeframe ring.
/// 2026-08-28: the RAM ring must span the CANDLE session, which now opens at
/// 09:00 with the pre-open call auction - not the market open at 09:15. Sized
/// off `MARKET_OPEN_IST_NANOS` this ring was fifteen minutes short of the
/// grid feeding it, so the first fifteen pre-open 1m bars of every day would
/// have been evicted by the day's own later bars: a silent, capacity-shaped
/// data loss that no counter would have reported.
pub const CANDLE_SESSION_OPEN_IST_NANOS: i64 = 32_400 * 1_000_000_000;

const _: () = assert!(
    CANDLE_SESSION_OPEN_IST_NANOS < MARKET_OPEN_IST_NANOS,
    "the candle session must open BEFORE the market, never at or after it"
);

pub const SESSION_SECS: u32 =
    ((MARKET_CLOSE_IST_NANOS - CANDLE_SESSION_OPEN_IST_NANOS) / 1_000_000_000) as u32;

const _: () = assert!(
    SESSION_SECS == 24_000,
    "session length must be 400 minutes (09:00-15:40 IST): 15 pre-open + 385 regular"
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

/// Σ [`bars_per_day`] over the RAM-RESIDENT frames (the 5-frame live
/// minute/day set) — the per-slot per-day bar count (601; pinned by
/// `test_bars_per_day_session_math`). The 16 GDF-gated second-scale
/// frames are EXCLUDED: they are capacity-1 placeholders with ZERO rows
/// until the GDF 1s feed lands, never part of the resident-bar total.
#[must_use]
// TEST-EXEMPT: pure Σ over bars_per_day — pinned by test_bars_per_day_session_math + the capacity-envelope tests.
pub fn total_bars_per_day_all_tfs() -> u32 {
    let mut total = 0u32;
    for tf in TfIndex::ALL {
        if tf.is_second_scale() {
            continue; // GDF-gated placeholder — zero rows until the GDF feed.
        }
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
    /// No slot could be created: the store is at `MAX_SPOT_BAR_SLOTS`
    /// distinct instruments (2026-08-07). Distinct from `DroppedOverWindow`,
    /// which is correct age-based eviction — this is a capacity REFUSAL and
    /// means the instrument has no RAM retention at all.
    SlotCapacityExhausted,
}

/// Outcome of one day-block record (test/forensics surface).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockOutcome {
    /// Bars landed (prepended/appended/replaced).
    pub recorded: usize,
    /// Bars older than the retained window at capacity — dropped.
    pub dropped_over_window: usize,
}

/// Hard cap on distinct `(feed, security_id, exchange_segment)` slots.
///
/// 2026-08-07: before this, the slot table grew unbounded — one entry per
/// distinct instrument ever seen, never evicted. It was safe only by the
/// external convention that callers feed the small hardcoded index list; the
/// type enforced nothing. Capping it made the growth bounded BY CONSTRUCTION.
///
/// 2026-08-10: raised 256 → 25,000 to match [`AGGREGATOR_MAX_SLOTS`]. The 256
/// was sized as "~32x today's ~8 live slots" when the runtime was REST-only on
/// four indices. Under the authorized r8g.xlarge target (operator Quote 13,
/// 2026-08-08 — 13 timeframes at ~25,000 instruments) that cap **refuses
/// 24,744 of 25,000 instruments** with `SlotCapacityExhausted`, i.e. zero RAM
/// retention for all but the first 256 — a fail-closed refusal, so it would
/// have been loud rather than silent, but it would have made the upgrade
/// useless for its stated purpose.
///
/// Sized to the SAME ceiling as the aggregator so the two cannot disagree
/// about how many instruments the box admits. Memory is bounded by
/// construction: the slot table is `RwLock<HashMap<SlotKey, Arc<Slot>>>`, so
/// this is a ceiling on entries actually inserted, not a pre-allocation.
/// Raising it further is a deliberate, reviewable edit — which is the point.
pub const MAX_SPOT_BAR_SLOTS: usize = crate::candles::AGGREGATOR_MAX_SLOTS;

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
            // Empty-ring overflow keeps the NEWEST `capacity` bars and
            // drops the OLDEST prefix (PR-2 round-1 LOW fix): eviction
            // direction must match the ring's front-eviction semantics —
            // stale history never displaces newer bars.
            let skip = bars.len().saturating_sub(self.capacity);
            out.dropped_over_window = skip;
            for bar in &bars[skip..] {
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
///
/// `Hash` (2026-08-10) makes this the direct key of the slot map, so the
/// I-P1-11 composite IS the hash key — there is no narrower key anywhere in
/// the lookup path that a same-`security_id`-different-segment pair could
/// collide on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// O(1)-average slot index keyed on the FULL I-P1-11 composite
    /// (`SlotKey` = feed + security_id + exchange_segment_code). Replaced a
    /// `Vec` linear scan on 2026-08-10; capacity is still bounded by
    /// [`MAX_SPOT_BAR_SLOTS`] (the bound is enforced in
    /// `find_or_create_slot`, not by the container).
    slots: RwLock<HashMap<SlotKey, std::sync::Arc<Slot>>>,
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
            // Pre-sized to the ~8 live spot slots so the steady-state store
            // never rehashes; growth beyond that is bounded by
            // MAX_SPOT_BAR_SLOTS at the creation site.
            slots: RwLock::new(HashMap::with_capacity(8)),
            middle_inserts: AtomicU64::new(0),
            dropped_over_window: AtomicU64::new(0),
        }
    }

    /// The configured ring depth in trading days.
    #[must_use]
    // TEST-EXEMPT: trivial config accessor — exercised throughout the store tests (depth/capacity assertions).
    pub fn spot_days(&self) -> u32 {
        self.spot_days
    }

    /// O(1) average: one hash of the I-P1-11 composite + one bucket probe,
    /// plus one atomic refcount bump on the returned `Arc`. Independent of
    /// [`MAX_SPOT_BAR_SLOTS`] (was an O(#slots ≤ 256) scan before
    /// 2026-08-10).
    fn find_slot(&self, key: SlotKey) -> Option<std::sync::Arc<Slot>> {
        let slots = self.slots.read();
        slots.get(&key).map(std::sync::Arc::clone)
    }

    fn find_or_create_slot(&self, key: SlotKey) -> Option<std::sync::Arc<Slot>> {
        if let Some(slot) = self.find_slot(key) {
            return Some(slot);
        }
        let mut slots = self.slots.write();
        // Re-check under the write lock (another writer may have raced) —
        // O(1) average, same composite key.
        if let Some(slot) = slots.get(&key) {
            return Some(std::sync::Arc::clone(slot));
        }
        // 2026-08-07: HARD CAP. Before this, `slots` grew by one entry for
        // every distinct (feed, security_id, exchange_segment) ever seen and
        // NEVER evicted — unbounded in code. It was safe only because callers
        // happened to feed the hardcoded index list (~8 slots); the doc comment
        // said "8 slots" as a DESIGN ASSUMPTION, with nothing enforcing it.
        // Every feed-expansion plan in the repo widens that SID set, at which
        // point the growth becomes real and silent. Bounded by construction now,
        // and loud at the boundary rather than discovered as memory pressure.
        if slots.len() >= MAX_SPOT_BAR_SLOTS {
            metrics::counter!("tv_spot_bar_slots_exhausted_total").increment(1);
            tracing::error!(
                feed = key.feed.as_str(),
                security_id = key.security_id,
                cap = MAX_SPOT_BAR_SLOTS,
                live = slots.len(),
                "spot bar store slot capacity exhausted — this instrument's bars \
                 are NOT being retained in RAM; raise MAX_SPOT_BAR_SLOTS"
            );
            return None;
        }
        let mut rings = Vec::with_capacity(TF_COUNT);
        for tf in TfIndex::ALL {
            // GDF-gated second-scale frames (C3): capacity-1 placeholder —
            // ZERO rows until the GDF 1s live feed lands (separate lane).
            // The full session formula (e.g. S1 = 22_500 bars/day) would
            // pre-allocate ~966 MB of empty rings across 8 slots × 35 days.
            let capacity = if tf.is_second_scale() {
                1
            } else {
                (bars_per_day(tf) as usize) * (self.spot_days as usize)
            };
            rings.push(TfRing::new(capacity.max(1)));
        }
        let slot = std::sync::Arc::new(Slot {
            key,
            rings: RwLock::new(rings),
        });
        slots.insert(key, std::sync::Arc::clone(&slot));
        Some(slot)
    }

    fn note_outcome(&self, outcome: UpsertOutcome) {
        match outcome {
            UpsertOutcome::InsertedMiddle => {
                self.middle_inserts.fetch_add(1, Ordering::Relaxed);
            }
            UpsertOutcome::DroppedOverWindow => {
                self.dropped_over_window.fetch_add(1, Ordering::Relaxed);
            }
            // Counted at the refusal site (find_or_create_slot) with the
            // instrument named; nothing further to tally here.
            UpsertOutcome::SlotCapacityExhausted => {}
            UpsertOutcome::Appended | UpsertOutcome::Replaced => {}
        }
    }

    /// LIVE write path: upsert one sealed bar by bucket ts (refold
    /// re-emits REPLACE in place — idempotent by construction, mirroring
    /// the `candles_*` DEDUP UPSERT semantics).
    pub fn append_sealed(&self, key: SlotKey, tf: TfIndex, bar: RamBar) -> UpsertOutcome {
        let Some(slot) = self.find_or_create_slot(key) else {
            return UpsertOutcome::SlotCapacityExhausted;
        };
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
        let Some(slot) = self.find_or_create_slot(key) else {
            // Slot capacity refusal — already counted + logged loudly inside
            // find_or_create_slot. Report zero recorded rather than pretending.
            return BlockOutcome {
                recorded: 0,
                dropped_over_window: bars.len(),
            };
        };
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
            for slot in slots.values() {
                copies.push(std::sync::Arc::clone(slot));
            }
            copies
        };
        let slot_count = slot_arcs.len();
        for slot in &slot_arcs {
            let feed_idx = slot.key.feed.index();
            seen_feed[feed_idx] = true;
            let rings = slot.rings.read();
            for (ordinal, ring) in rings.iter().enumerate() {
                // Resident bars count over EVERY ring (a GDF-gated ring that
                // ever gains a row must show up here); the byte ESTIMATE
                // covers the RAM-resident live-frame rings only — the 16
                // second-scale placeholders are a pinned 768 B/slot
                // (test_second_scale_rings_are_capacity_one_placeholders).
                bars_resident_per_feed[feed_idx] += ring.bars.len() as u64;
                let second_scale = TfIndex::from_ordinal(ordinal)
                    .map(TfIndex::is_second_scale)
                    .unwrap_or(false);
                if !second_scale {
                    estimated_bytes +=
                        (ring.capacity as u64) * (core::mem::size_of::<RamBar>() as u64);
                }
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
        // ceil(24_000 / tf_secs), floored at 1 — spot-checked + summed.
        // 2026-08-07: session 375 -> 385 min (NSE CAS change 2026-08-03).
        // 2026-08-28: 385 -> 400 min — the candle session now opens at 09:00
        // with the NSE pre-open call auction, so every ring gained the 15
        // pre-open minutes it must hold. M1 385 -> 400, M3 129 -> 134,
        // M5 77 -> 80.
        assert_eq!(bars_per_day(TfIndex::M1), 400);
        assert_eq!(bars_per_day(TfIndex::M3), 134);
        assert_eq!(bars_per_day(TfIndex::M5), 80);
        assert_eq!(bars_per_day(TfIndex::M15), 27);
        assert_eq!(bars_per_day(TfIndex::D1), 1);
        // 2026-08-10: M2/M30/M60 appended (operator Quote 13's thirteen
        // frames). 2026-08-28 (400-min session): ceil(24_000/120)=200,
        // ceil(24_000/1800)=14, ceil(24_000/3600)=7 → 642 + 221 = 863.
        assert_eq!(bars_per_day(TfIndex::M2), 200);
        assert_eq!(bars_per_day(TfIndex::M30), 14);
        assert_eq!(bars_per_day(TfIndex::M60), 7);
        assert_eq!(total_bars_per_day_all_tfs(), 863);
        assert_eq!(SESSION_SECS, 24_000);
    }

    #[test]
    fn test_estimated_capacity_bytes_under_40mb_envelope() {
        // The design envelope: 8 slots (2 feeds × 4 spot SIDs) × 35 days.
        assert_eq!(core::mem::size_of::<RamBar>(), 48, "RamBar must stay 48 B");
        let bytes = estimated_capacity_bytes(35, 8);
        // 863 × 35 × 8 × 48 = 11_598_720 B ≈ 11.1 MiB
        // (2026-08-07: 601 -> 618 bars/day with the 385-minute session;
        //  2026-08-10: 618 -> 831 with M2/M30/M60, operator Quote 13;
        //  2026-08-28: 831 -> 863 with the 09:00 pre-open open — +430 KB
        //  total, i.e. the whole pre-open capture costs under half a
        //  megabyte of RAM at the design envelope.)
        assert_eq!(bytes, 11_598_720);
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
        // spot_days=1 → M15 capacity = 27 bars (400-min session, 2026-08-28
        // — was 26 on the 385-min one); the 28th append evicts the oldest,
        // and an over-window old bar is dropped (never displaces).
        let store = SpotBarStore::new(1);
        let cap = bars_per_day(TfIndex::M15) as usize;
        assert_eq!(cap, 27);
        for i in 0..(cap as u32 + 1) {
            store.append_sealed(key(), TfIndex::M15, bar(OPEN0 + i * 900, f64::from(i)));
        }
        let bars = store.latest_n(key(), TfIndex::M15, 100);
        assert_eq!(bars.len(), cap, "capacity must hold after wraparound");
        assert_eq!(
            store.bar_at(key(), TfIndex::M15, OPEN0),
            None,
            "the oldest bar must be evicted"
        );
        // An over-window (older-than-front) bar at capacity is dropped.
        assert_eq!(
            store.append_sealed(key(), TfIndex::M15, bar(OPEN0 - 900, 999.0)),
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
    fn test_spot_bar_store_record_day_block_empty_ring_overflow_keeps_newest() {
        // PR-2 round-1 LOW: a block LARGER than capacity into an EMPTY
        // ring must keep the NEWEST `capacity` bars and drop the OLDEST
        // prefix — never the old keep-oldest/drop-newest direction.
        let store = SpotBarStore::new(1);
        let cap = bars_per_day(TfIndex::M15) as usize; // 25
        let mut bars = Vec::with_capacity(cap + 2);
        for i in 0..(cap as u32 + 2) {
            bars.push(bar(OPEN0 + i * 900, f64::from(i)));
        }
        let out = store.record_day_block(key(), TfIndex::M15, &bars);
        assert_eq!(out.recorded, cap, "exactly capacity bars recorded");
        assert_eq!(out.dropped_over_window, 2, "the 2 OLDEST bars dropped");
        // The oldest two are absent; the newest survives.
        assert!(store.bar_at(key(), TfIndex::M15, OPEN0).is_none());
        assert!(store.bar_at(key(), TfIndex::M15, OPEN0 + 900).is_none());
        assert_eq!(
            store
                .bar_at(key(), TfIndex::M15, OPEN0 + (cap as u32 + 1) * 900)
                .map(|b| b.close),
            Some(f64::from(cap as u32 + 1)),
            "the NEWEST bar must be resident"
        );
        assert_eq!(store.stats().dropped_over_window, 2);
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
            feed: Feed::Truedata,
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
            feed: Feed::Truedata,
            security_id: 4_611_686_018_427_387_913,
            exchange_segment_code: 0,
        };
        store.append_sealed(groww_key, TfIndex::M1, bar(OPEN0, 2.0));
        store.append_sealed(groww_key, TfIndex::M1, bar(OPEN0 + 60, 3.0));
        let stats = store.stats();
        assert_eq!(stats.slots, 2);
        assert_eq!(stats.bars_resident_per_feed[Feed::Dhan.index()], 1);
        assert_eq!(stats.bars_resident_per_feed[Feed::Truedata.index()], 2);
        assert_eq!(stats.min_depth_days_per_feed[Feed::Dhan.index()], 1);
        assert_eq!(stats.min_depth_days_per_feed[Feed::Truedata.index()], 1);
        // Two slots × 1 day × 863 bars × 48 B of pre-allocated capacity
        // (400-min session since 2026-08-28; 618 -> 831 on 2026-08-10 with
        // M2/M30/M60, then 831 -> 863 with the 09:00 pre-open open).
        assert_eq!(stats.estimated_bytes, 2 * 863 * 48);
    }

    #[test]
    fn test_second_scale_rings_are_capacity_one_placeholders() {
        // C3: the 16 GDF-gated second-scale frames allocate capacity-1
        // placeholder rings (ZERO rows until the GDF 1s feed lands — a
        // pinned 16 × 48 B = 768 B/slot of actual heap) and are excluded
        // from the RAM-resident bar total + byte estimate; the session
        // formula stays honest for the future GDF capacity flip.
        assert_eq!(bars_per_day(TfIndex::S1), 24_000);
        assert_eq!(bars_per_day(TfIndex::S2), 12_000);
        assert_eq!(bars_per_day(TfIndex::S15), 1_600);
        assert_eq!(bars_per_day(TfIndex::S30), 800);
        let mut gated_formula_total = 0u32;
        for tf in TfIndex::ALL {
            if tf.is_second_scale() {
                gated_formula_total += bars_per_day(tf);
            }
        }
        // 2026-08-07: 75_413 -> 77_422 with the 385-minute session;
        // 2026-08-28: 77_422 -> 80_440 with the 400-minute one. These are
        // the GDF-gated second-scale frames — capacity-1 placeholders today,
        // so the number is the would-be formula cost, not allocated memory.
        assert_eq!(gated_formula_total, 80_440, "gated formula sum drifted");
        // The resident total + byte estimate exclude the gated frames.
        // 2026-08-10: 618 -> 831 with M2/M30/M60 (operator Quote 13). These
        // three are minute-scale, so unlike the GDF-gated second frames they
        // ARE resident and DO count toward the byte estimate.
        assert_eq!(total_bars_per_day_all_tfs(), 863);
        let store = SpotBarStore::new(35);
        store.append_sealed(key(), TfIndex::M1, bar(OPEN0, 1.0));
        let stats = store.stats();
        assert_eq!(stats.estimated_bytes, 863 * 35 * 48);
        let slot = store.find_slot(key()).expect("slot exists");
        let rings = slot.rings.read();
        assert_eq!(rings.len(), TF_COUNT, "one ring per TfIndex ordinal");
        for (ordinal, ring) in rings.iter().enumerate() {
            let tf = TfIndex::from_ordinal(ordinal).expect("ring ordinal");
            if tf.is_second_scale() {
                assert_eq!(ring.capacity, 1, "{tf:?} placeholder capacity drifted");
                assert!(ring.bars.is_empty(), "{tf:?} must hold ZERO rows pre-GDF");
            } else {
                assert_eq!(
                    ring.capacity,
                    (bars_per_day(tf) as usize) * 35,
                    "{tf:?} live-frame ring capacity drifted"
                );
            }
        }
    }

    // -----------------------------------------------------------------
    // 2026-08-10 — O(1) composite-key slot lookup (was an O(#slots ≤ 256)
    // linear scan on EVERY read and EVERY write).
    // -----------------------------------------------------------------

    #[test]
    fn test_find_slot_composite_key_isolates_same_security_id_across_segments() {
        // I-P1-11: `security_id` alone is NEVER the key. The SAME numeric id
        // under two segments (the live FINNIFTY id=27 IDX_I vs NSE_EQ class)
        // must resolve to two INDEPENDENT slots — a same-id collapse would be
        // exactly the silent data loss the hash-key change must not introduce.
        let store = SpotBarStore::new(2);
        let idx = SlotKey {
            feed: Feed::Dhan,
            security_id: 27,
            exchange_segment_code: 0, // IDX_I
        };
        let eq = SlotKey {
            feed: Feed::Dhan,
            security_id: 27,
            exchange_segment_code: 1, // NSE_EQ
        };
        // Same id, same segment, DIFFERENT feed — the third key axis.
        let groww_idx = SlotKey {
            feed: Feed::Truedata,
            security_id: 27,
            exchange_segment_code: 0,
        };
        assert_eq!(
            store.append_sealed(idx, TfIndex::M1, bar(OPEN0, 100.0)),
            UpsertOutcome::Appended
        );
        assert_eq!(
            store.append_sealed(eq, TfIndex::M1, bar(OPEN0, 200.0)),
            UpsertOutcome::Appended,
            "a different segment must open its OWN slot, not replace in an existing one"
        );
        assert_eq!(
            store.append_sealed(groww_idx, TfIndex::M1, bar(OPEN0, 300.0)),
            UpsertOutcome::Appended,
            "a different feed must open its OWN slot"
        );
        assert_eq!(store.stats().slots, 3, "three distinct composite keys");
        assert_eq!(
            store.bar_at(idx, TfIndex::M1, OPEN0).map(|b| b.close),
            Some(100.0)
        );
        assert_eq!(
            store.bar_at(eq, TfIndex::M1, OPEN0).map(|b| b.close),
            Some(200.0)
        );
        assert_eq!(
            store.bar_at(groww_idx, TfIndex::M1, OPEN0).map(|b| b.close),
            Some(300.0)
        );
        // And the hash lookup itself distinguishes them.
        assert!(store.find_slot(idx).is_some());
        assert!(store.find_slot(eq).is_some());
        assert!(
            store
                .find_slot(SlotKey {
                    feed: Feed::Dhan,
                    security_id: 27,
                    exchange_segment_code: 8, // never written
                })
                .is_none(),
            "an unwritten segment must MISS, not alias onto a sibling"
        );
    }

    #[test]
    fn test_find_or_create_slot_boundary_exactly_at_cap_then_one_past() {
        // Boundary permutation: fill EXACTLY MAX_SPOT_BAR_SLOTS distinct
        // composite keys (all must land), then one MORE (must be refused,
        // loudly, as SlotCapacityExhausted — not evicted, not silently
        // dropped), then prove an ALREADY-RESIDENT slot still works after
        // exhaustion (a cap refusal must never break live retention).
        let store = SpotBarStore::new(1);
        for i in 0..MAX_SPOT_BAR_SLOTS {
            let key = SlotKey {
                feed: Feed::Dhan,
                security_id: i as u64,
                exchange_segment_code: 0,
            };
            assert_eq!(
                store.append_sealed(key, TfIndex::M1, bar(OPEN0, i as f64)),
                UpsertOutcome::Appended,
                "slot {i} must land at/below the cap"
            );
        }
        assert_eq!(store.stats().slots, MAX_SPOT_BAR_SLOTS);

        let one_past = SlotKey {
            feed: Feed::Dhan,
            security_id: MAX_SPOT_BAR_SLOTS as u64,
            exchange_segment_code: 0,
        };
        assert_eq!(
            store.append_sealed(one_past, TfIndex::M1, bar(OPEN0, 1.0)),
            UpsertOutcome::SlotCapacityExhausted,
            "one past the cap must be REFUSED"
        );
        assert_eq!(
            store.stats().slots,
            MAX_SPOT_BAR_SLOTS,
            "a refusal must not grow the table"
        );
        assert!(
            store.bar_at(one_past, TfIndex::M1, OPEN0).is_none(),
            "the refused instrument has NO retention — never a silent partial"
        );
        // The block path reports the same refusal honestly.
        let block = store.record_day_block(
            one_past,
            TfIndex::M1,
            &[bar(OPEN0, 1.0), bar(OPEN0 + 60, 2.0)],
        );
        assert_eq!(block.recorded, 0);
        assert_eq!(block.dropped_over_window, 2, "every bar accounted for");

        // Resident slots are untouched by the refusal.
        let resident = SlotKey {
            feed: Feed::Dhan,
            security_id: 7,
            exchange_segment_code: 0,
        };
        assert_eq!(
            store.append_sealed(resident, TfIndex::M1, bar(OPEN0 + 60, 42.0)),
            UpsertOutcome::Appended
        );
        assert_eq!(
            store.bar_at(resident, TfIndex::M1, OPEN0).map(|b| b.close),
            Some(7.0),
            "the original bar survived"
        );
    }

    #[test]
    fn test_find_slot_serves_concurrent_readers_during_writes() {
        // Concurrency permutation: readers hammer the hash lookup while a
        // writer creates NEW slots (the write-lock path that rehashes). No
        // panic, no torn read — a reader either misses or sees a whole bar.
        let store = SpotBarStore::new(1);
        let seeded = SlotKey {
            feed: Feed::Dhan,
            security_id: 1_000_000,
            exchange_segment_code: 0,
        };
        store.append_sealed(seeded, TfIndex::M1, bar(OPEN0, 555.0));

        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..2_000 {
                        // The seeded slot is ALWAYS present and never rewritten.
                        assert_eq!(
                            store.bar_at(seeded, TfIndex::M1, OPEN0).map(|b| b.close),
                            Some(555.0),
                            "a resident slot must stay readable across concurrent inserts"
                        );
                        let churn = SlotKey {
                            feed: Feed::Truedata,
                            security_id: 42,
                            exchange_segment_code: 0,
                        };
                        // Either missing or complete — never a partial bar.
                        if let Some(b) = store.bar_at(churn, TfIndex::M1, OPEN0) {
                            assert_eq!(b.close, 9.0);
                        }
                    }
                });
            }
            scope.spawn(|| {
                for i in 0..200u64 {
                    let key = SlotKey {
                        feed: Feed::Dhan,
                        security_id: i,
                        exchange_segment_code: 0,
                    };
                    store.append_sealed(key, TfIndex::M1, bar(OPEN0, i as f64));
                }
                store.append_sealed(
                    SlotKey {
                        feed: Feed::Truedata,
                        security_id: 42,
                        exchange_segment_code: 0,
                    },
                    TfIndex::M1,
                    bar(OPEN0, 9.0),
                );
            });
        });

        assert_eq!(
            store.bar_at(seeded, TfIndex::M1, OPEN0).map(|b| b.close),
            Some(555.0)
        );
    }

    #[test]
    fn test_find_slot_empty_store_and_maximal_key_values_miss_cleanly() {
        // Empty + maximal inputs: an empty store misses on every accessor
        // without panicking, and u64::MAX / u8::MAX key components hash and
        // compare like any other (no width truncation in the key).
        let store = SpotBarStore::new(1);
        let maximal = SlotKey {
            feed: Feed::Truedata,
            security_id: u64::MAX,
            exchange_segment_code: u8::MAX,
        };
        assert!(store.find_slot(maximal).is_none());
        assert!(store.bar_at(maximal, TfIndex::M1, OPEN0).is_none());
        assert!(store.latest_n(maximal, TfIndex::M1, 5).is_empty());
        assert_eq!(store.depth_days(maximal), 0);
        assert_eq!(store.stats().slots, 0);

        assert_eq!(
            store.append_sealed(maximal, TfIndex::M1, bar(u32::MAX, 1.0)),
            UpsertOutcome::Appended
        );
        assert_eq!(
            store
                .bar_at(maximal, TfIndex::M1, u32::MAX)
                .map(|b| b.close),
            Some(1.0)
        );
        // The neighbouring id must NOT alias onto it.
        assert!(
            store
                .find_slot(SlotKey {
                    security_id: u64::MAX - 1,
                    ..maximal
                })
                .is_none()
        );
    }

    proptest::proptest! {
        /// Random composite keys: every distinct `(feed, sid, segment)` triple
        /// must round-trip to its OWN bar, and two triples differing in ANY
        /// single component must never share a slot. This is the property the
        /// linear scan gave for free and the hash key must preserve.
        #[test]
        fn test_find_slot_hash_key_roundtrips_arbitrary_composites(
            raw in proptest::collection::vec(
                (0usize..Feed::COUNT, 0u64..40, 0u8..4),
                1..40,
            )
        ) {
            let store = SpotBarStore::new(1);
            // Deduplicate by composite; the LAST write for a key wins (upsert).
            let mut expected: std::collections::HashMap<SlotKey, f64> =
                std::collections::HashMap::with_capacity(raw.len());
            for (idx, (feed_idx, sid, seg)) in raw.iter().enumerate() {
                let key = SlotKey {
                    feed: Feed::ALL[*feed_idx],
                    security_id: *sid,
                    exchange_segment_code: *seg,
                };
                let close = idx as f64 + 1.0;
                store.append_sealed(key, TfIndex::M1, bar(OPEN0, close));
                expected.insert(key, close);
            }
            proptest::prop_assert_eq!(store.stats().slots, expected.len());
            for (key, close) in &expected {
                proptest::prop_assert_eq!(
                    store.bar_at(*key, TfIndex::M1, OPEN0).map(|b| b.close),
                    Some(*close)
                );
            }
        }
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
