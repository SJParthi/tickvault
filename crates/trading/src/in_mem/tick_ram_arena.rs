//! Full-session raw-tick residency in RAM, so a trading decision never waits
//! for a database round-trip.
//!
//! # Why this exists
//!
//! Operator directive, 2026-09-01: *"we need to keep all the ticks of the
//! entire app in memory RAM, that too without even missing any ticks, because
//! we cannot hit DB and wait."*
//!
//! Today a tick is folded into candle state and dropped. `MultiTfAggregator`
//! keeps the current bucket and the last sealed one per timeframe — excellent
//! for candle-shaped decisions, and nothing at all for a decision that needs
//! the raw sequence: order-flow imbalance, tick-by-tick momentum, microstructure
//! signals. Those need the ticks themselves.
//!
//! # The sizing decision, which is the whole design
//!
//! `ParsedTick` is **112 bytes** (measured, `size_of`). At the measured session
//! volume of ~85M ticks that is **9.5 GB** — and on the 32 GiB box, beside
//! QuestDB's 12 GiB limit, it does not fit.
//!
//! Most of those 112 bytes do not vary per tick:
//!
//! | Field group | Why it is not stored here |
//! |---|---|
//! | `day_open` / `day_close` / `day_high` / `day_low` | day STATE, identical on every tick of the day; `DayOhlcTracker` owns it |
//! | `oi_day_high` / `oi_day_low` | same |
//! | `iv` / `delta` / `gamma` / `theta` / `vega` | COMPUTED downstream, `NaN` for non-F&O |
//! | `security_id` / `exchange_segment_code` | implied by the slot the tick is filed under |
//! | `average_traded_price` | a running mean of fields already stored |
//!
//! What remains is what actually moves tick to tick, and it fits in **32
//! bytes** — a 3.5x reduction that turns 9.5 GB into **~2.7 GB**, which fits
//! with room to spare. That is the difference between a feature that can ship
//! and one that cannot.
//!
//! # Shape: one arena, not 25,000 rings
//!
//! The obvious design — a fixed ring per instrument — is the wrong one, and
//! the distribution says so. 85M ticks across ~23,000 instruments averages
//! ~3,700 each, but NIFTY takes millions while 119 instruments were measured
//! taking *zero*. A fixed per-instrument ring therefore wastes most of the
//! budget on slots that never fill, and overflows first on exactly the
//! instruments a strategy actually trades.
//!
//! So this is ONE append-only arena in arrival order, with a per-slot index
//! threading each instrument's ticks backwards through it. A busy instrument
//! consumes what it needs; a silent one consumes nothing. Every tick the
//! session produced is retained, in order, until the arena is full — which is
//! the literal guarantee the directive asks for, stated with its bound.
//!
//! # Complexity
//!
//! | Operation | Cost |
//! |---|---|
//! | `append` | **O(1)** — one bounds check, one index write, one head swap |
//! | `latest` | **O(1)** — one index read |
//! | `walk_back(n)` | **O(n)** in ticks RETURNED, never in ticks stored |
//! | `len` / `capacity` / `utilization_pct` | **O(1)** |
//!
//! Zero heap allocation after construction: the arena and the head table are
//! allocated once, with `with_capacity`, and only ever written by index.
//!
//! # What happens when it fills
//!
//! It REFUSES, counts, and says so once. It does not evict.
//!
//! Eviction was considered and rejected for a specific reason: this store's
//! entire purpose is that every tick is present. A silent eviction turns it
//! into a store that *usually* has the tick you want, which is the worst of
//! both worlds — a strategy cannot tell a missing tick from a tick that never
//! happened. Refusing is loud, bounded, and leaves the operator a real signal.
//!
//! **Nothing is lost by a refusal.** Every tick is already durably written to
//! the `ticks` table and captured in the frame WAL before this store is ever
//! offered one. A full arena costs RAM RESIDENCY — the DB round-trip comes
//! back — never the data.

use std::collections::HashMap;

use tickvault_common::types::ExchangeSegment;

/// Sentinel for "no previous tick for this instrument" / "no head yet".
///
/// `u32::MAX` rather than `Option<u32>`: an `Option` would push the record to
/// 40 bytes through niche-less padding, and at 85M records that is 680 MB paid
/// for a value that never occurs in a full arena.
pub const NO_TICK: u32 = u32::MAX;

/// One retained tick. Exactly 32 bytes — const-asserted below.
///
/// Field order is chosen for packing, not for reading: the four-byte values
/// come first so there is no interior padding at all.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct CompactTick {
    /// Arena index of this instrument's PREVIOUS tick, or [`NO_TICK`].
    ///
    /// This is what makes one shared arena work: each instrument is a backward
    /// chain through arrival-ordered storage, so a busy instrument uses what it
    /// needs and a silent one uses nothing.
    pub prev: u32,
    /// Exchange timestamp, IST epoch seconds — stored verbatim, never shifted.
    /// The +5:30 rule in `data-integrity.md` applies to REST, not to this.
    pub exchange_timestamp: u32,
    /// Last traded price. `f32` because that is what the wire carries; widening
    /// happens at the persistence boundary via `f32_to_f64_clean`, not here.
    pub last_traded_price: f32,
    /// Cumulative day volume as of this tick.
    pub volume: u32,
    /// Open interest as of this tick (0 for non-derivatives).
    pub open_interest: u32,
    /// Total buy quantity — half of the book-pressure pair a flow signal needs.
    pub total_buy_quantity: u32,
    /// Total sell quantity — the other half.
    pub total_sell_quantity: u32,
    /// Last trade quantity.
    pub last_trade_quantity: u16,
    /// Dense slot this tick belongs to, so a scan over the arena can attribute
    /// a record without a second lookup. `u16` bounds the store at 65,535
    /// instruments, asserted against the slot ceiling below.
    pub slot: u16,
}

// The whole feasibility argument rests on this number. If a field is added and
// the record grows to 40 bytes, the session cost goes 2.7 GB -> 3.4 GB and the
// doc comment above becomes a lie, so the size is pinned rather than described.
const _: () = assert!(
    core::mem::size_of::<CompactTick>() == 32,
    "CompactTick must stay 32 bytes: the memory budget, the capacity \
     derivation and this module's header all quote that figure"
);

/// Slot ceiling. Deliberately the same 25,000 the aggregator, indicator engine
/// and day-OHLC tracker use — one figure for the same universe, so a reader
/// does not have to work out which cap binds first.
pub const MAX_TICK_ARENA_SLOTS: usize = 25_000;

// `slot` is a u16, so the ceiling must fit one. Checked rather than assumed:
// raising MAX_TICK_ARENA_SLOTS past 65,535 without widening the field would
// silently alias two instruments onto one slot number.
const _: () = assert!(
    MAX_TICK_ARENA_SLOTS <= u16::MAX as usize,
    "slot is a u16; a larger ceiling would alias instruments onto one slot"
);

/// Why an append did not land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendRefusal {
    /// The arena is full. Retention stops; the `ticks` table does not.
    ArenaFull,
    /// The instrument could not be given a slot — the 25,000 ceiling.
    SlotsExhausted,
}

/// The per-tick values an append carries, as ONE value rather than seven
/// positional arguments.
///
/// Not cosmetic. Seven bare numbers at a call site — four of them `u32` — is a
/// shape where transposing `total_buy_quantity` and `total_sell_quantity`, or
/// `volume` and `open_interest`, compiles perfectly and silently inverts a
/// book-pressure signal. Named fields make that transposition impossible to
/// write by accident, and the compiler rejects a forgotten one.
///
/// `Copy` and field-for-field identical to the stored record minus `prev` and
/// `slot`, which the arena owns.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TickSample {
    /// Exchange timestamp, IST epoch seconds, verbatim.
    pub exchange_timestamp: u32,
    /// Last traded price.
    pub last_traded_price: f32,
    /// Last trade quantity.
    pub last_trade_quantity: u16,
    /// Cumulative day volume as of this tick.
    pub volume: u32,
    /// Open interest as of this tick.
    pub open_interest: u32,
    /// Total buy quantity.
    pub total_buy_quantity: u32,
    /// Total sell quantity.
    pub total_sell_quantity: u32,
}

/// Composite instrument key, per I-P1-11.
///
/// `security_id` alone is NOT unique — Dhan reuses numeric ids across
/// segments, so a bare-id map silently folds two instruments into one. The
/// segment is part of the key here for the same reason it is part of every
/// DEDUP key in `storage`.
pub type ArenaKey = (u64, ExchangeSegment);

/// Full-session raw-tick residency, bounded and O(1).
#[derive(Debug)]
pub struct TickRamArena {
    /// Arrival-ordered storage. Pre-allocated; never grows.
    arena: Vec<CompactTick>,
    /// Arena capacity in records, fixed at construction.
    capacity: usize,
    /// Per-slot index of that instrument's most recent tick, or [`NO_TICK`].
    heads: Vec<u32>,
    /// Per-slot retained count, for observability without a walk.
    counts: Vec<u32>,
    /// Composite key -> dense slot. The same shape as the aggregator's
    /// allocator, so the two cannot disagree about what an instrument is.
    slot_of: HashMap<ArenaKey, u16>,
    /// Ticks refused because the arena was full.
    refused_full: u64,
    /// Ticks refused because no slot could be allocated.
    refused_slots: u64,
}

impl TickRamArena {
    /// Build an arena sized to `capacity` ticks.
    ///
    /// Allocates everything up front — the arena, the head table and the count
    /// table — so no append can ever trigger a reallocation. A 2.7 GB
    /// allocation happening lazily, mid-session, on the frame drain, is exactly
    /// the stall this store exists to avoid.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            arena: Vec::with_capacity(capacity),
            capacity,
            heads: Vec::with_capacity(MAX_TICK_ARENA_SLOTS),
            counts: Vec::with_capacity(MAX_TICK_ARENA_SLOTS),
            slot_of: HashMap::with_capacity(MAX_TICK_ARENA_SLOTS),
            refused_full: 0,
            refused_slots: 0,
        }
    }

    /// Resolve (or allocate) the dense slot for an instrument. O(1) average.
    ///
    /// Never evicts: a slot, once given, belongs to that instrument for the
    /// process lifetime. Reassigning one would re-point an existing backward
    /// chain at a different instrument, which is silent corruption rather than
    /// a bounded loss.
    fn slot_for(&mut self, key: ArenaKey) -> Option<u16> {
        if let Some(slot) = self.slot_of.get(&key) {
            return Some(*slot);
        }
        let next = self.heads.len();
        if next >= MAX_TICK_ARENA_SLOTS {
            return None;
        }
        self.heads.push(NO_TICK);
        self.counts.push(0);
        // `next` < MAX_TICK_ARENA_SLOTS <= u16::MAX, const-asserted above.
        let slot = next as u16;
        self.slot_of.insert(key, slot);
        Some(slot)
    }

    /// Append one tick. **O(1)**, no allocation.
    ///
    /// # Errors
    ///
    /// [`AppendRefusal::ArenaFull`] once retention capacity is reached, or
    /// [`AppendRefusal::SlotsExhausted`] for a new instrument past the slot
    /// ceiling. Both are refusals, never evictions — see the module header.
    pub fn append(&mut self, key: ArenaKey, sample: TickSample) -> Result<u32, AppendRefusal> {
        // Capacity is checked BEFORE a slot is allocated, so a full arena
        // cannot burn one of the 25,000 slots on an instrument it will never
        // store a tick for.
        if self.arena.len() >= self.capacity {
            self.refused_full = self.refused_full.saturating_add(1);
            return Err(AppendRefusal::ArenaFull);
        }
        let Some(slot) = self.slot_for(key) else {
            self.refused_slots = self.refused_slots.saturating_add(1);
            return Err(AppendRefusal::SlotsExhausted);
        };
        let idx = self.arena.len() as u32;
        let slot_idx = slot as usize;
        self.arena.push(CompactTick {
            prev: self.heads[slot_idx],
            exchange_timestamp: sample.exchange_timestamp,
            last_traded_price: sample.last_traded_price,
            volume: sample.volume,
            open_interest: sample.open_interest,
            total_buy_quantity: sample.total_buy_quantity,
            total_sell_quantity: sample.total_sell_quantity,
            last_trade_quantity: sample.last_trade_quantity,
            slot,
        });
        self.heads[slot_idx] = idx;
        self.counts[slot_idx] = self.counts[slot_idx].saturating_add(1);
        Ok(idx)
    }

    /// The most recent tick for an instrument. **O(1)**.
    #[must_use]
    pub fn latest(&self, key: ArenaKey) -> Option<&CompactTick> {
        let slot = *self.slot_of.get(&key)?;
        let head = *self.heads.get(slot as usize)?;
        if head == NO_TICK {
            return None;
        }
        self.arena.get(head as usize)
    }

    /// Walk an instrument's ticks newest-first. **O(n) in ticks RETURNED.**
    ///
    /// Lazy and allocation-free: the iterator holds one index, so a strategy
    /// that wants the last 20 ticks pays for 20, not for the millions stored.
    pub fn walk_back(&self, key: ArenaKey) -> TickWalk<'_> {
        let cursor = self
            .slot_of
            .get(&key)
            .and_then(|slot| self.heads.get(*slot as usize).copied())
            .unwrap_or(NO_TICK);
        TickWalk {
            arena: &self.arena,
            cursor,
        }
    }

    /// Ticks retained. O(1).
    #[must_use]
    pub fn len(&self) -> usize {
        self.arena.len()
    }

    /// Whether anything is retained. O(1).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.arena.is_empty()
    }

    /// Retention capacity in ticks. O(1).
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Instruments holding at least one slot. O(1).
    #[must_use]
    pub fn tracked_instruments(&self) -> usize {
        self.heads.len()
    }

    /// Ticks retained for one instrument, without walking. O(1).
    #[must_use]
    pub fn tick_count(&self, key: ArenaKey) -> u32 {
        self.slot_of
            .get(&key)
            .and_then(|slot| self.counts.get(*slot as usize).copied())
            .unwrap_or(0)
    }

    /// Percent of retention capacity used, 0-100. O(1).
    ///
    /// Integer arithmetic on purpose: this feeds a gauge, and a float here
    /// would be the only float on a path that has none.
    #[must_use]
    pub fn utilization_pct(&self) -> u64 {
        if self.capacity == 0 {
            return 100;
        }
        (self.arena.len() as u64).saturating_mul(100) / self.capacity as u64
    }

    /// Ticks refused because the arena was full.
    #[must_use]
    pub const fn refused_full(&self) -> u64 {
        self.refused_full
    }

    /// Ticks refused because no slot was available.
    #[must_use]
    pub const fn refused_slots(&self) -> u64 {
        self.refused_slots
    }

    /// Bytes this arena committed at construction.
    ///
    /// The records plus both per-slot tables. The `slot_of` map is excluded and
    /// that is stated rather than silently omitted: its per-entry overhead is
    /// allocator-dependent, and at 25,000 entries it is under a megabyte
    /// against a multi-gigabyte arena.
    #[must_use]
    pub fn committed_bytes(&self) -> u64 {
        let records =
            (self.capacity as u64).saturating_mul(core::mem::size_of::<CompactTick>() as u64);
        let tables = (MAX_TICK_ARENA_SLOTS as u64).saturating_mul(8);
        records.saturating_add(tables)
    }
}

/// Newest-first walk over one instrument's retained ticks.
#[derive(Debug)]
pub struct TickWalk<'a> {
    arena: &'a [CompactTick],
    cursor: u32,
}

impl<'a> Iterator for TickWalk<'a> {
    type Item = &'a CompactTick;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor == NO_TICK {
            return None;
        }
        let tick = self.arena.get(self.cursor as usize)?;
        self.cursor = tick.prev;
        Some(tick)
    }
}

/// Ticks that fit in `budget_bytes` of retention.
///
/// Pure so the sizing can be tested without a host: the boot path supplies the
/// budget, this decides what it buys.
#[must_use]
pub const fn capacity_for_budget(budget_bytes: u64) -> usize {
    (budget_bytes / core::mem::size_of::<CompactTick>() as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    const NSE_FNO: ExchangeSegment = ExchangeSegment::NseFno;
    const IDX: ExchangeSegment = ExchangeSegment::IdxI;

    fn key(id: u64) -> ArenaKey {
        (id, NSE_FNO)
    }

    fn push(
        arena: &mut TickRamArena,
        k: ArenaKey,
        ts: u32,
        ltp: f32,
    ) -> Result<u32, AppendRefusal> {
        arena.append(
            k,
            TickSample {
                exchange_timestamp: ts,
                last_traded_price: ltp,
                last_trade_quantity: 1,
                volume: 100,
                open_interest: 0,
                total_buy_quantity: 0,
                total_sell_quantity: 0,
            },
        )
    }

    /// The reason `append` takes a struct rather than seven positional
    /// numbers: named fields make a transposition impossible to write, where
    /// four bare `u32`s in a row would compile and silently invert a signal.
    #[test]
    fn tick_sample_fields_land_in_the_record_unswapped() {
        let mut a = TickRamArena::with_capacity(4);
        let k = key(11);
        a.append(
            k,
            TickSample {
                exchange_timestamp: 1_700_000_000,
                last_traded_price: 24_150.25,
                last_trade_quantity: 7,
                volume: 111,
                open_interest: 222,
                total_buy_quantity: 333,
                total_sell_quantity: 444,
            },
        )
        .expect("append");
        let t = a.latest(k).expect("stored");
        // Every value is distinct, so a swap between ANY pair fails here.
        assert_eq!(t.exchange_timestamp, 1_700_000_000);
        assert_eq!(t.last_traded_price, 24_150.25);
        assert_eq!(t.last_trade_quantity, 7);
        assert_eq!(t.volume, 111);
        assert_eq!(t.open_interest, 222);
        assert_eq!(
            t.total_buy_quantity, 333,
            "buy pressure must never carry the sell side's number"
        );
        assert_eq!(
            t.total_sell_quantity, 444,
            "sell pressure must never carry the buy side's number"
        );
    }

    #[test]
    fn compact_tick_is_thirty_two_bytes_and_the_budget_depends_on_it() {
        assert_eq!(
            core::mem::size_of::<CompactTick>(),
            32,
            "the 2.7 GB session figure is derived from this"
        );
        // The whole point of the compaction: measured against the full tick.
        assert!(
            core::mem::size_of::<CompactTick>() * 3 < 112,
            "a compact record must be a real reduction over ParsedTick's 112 \
             bytes, or the arena does not fit beside QuestDB"
        );
    }

    #[test]
    fn capacity_for_budget_converts_bytes_to_ticks() {
        assert_eq!(capacity_for_budget(3200), 100);
        assert_eq!(capacity_for_budget(0), 0);
        // The real shape: ~2.7 GB holds ~85M ticks, the measured session.
        let ticks = capacity_for_budget(2_720_000_000);
        assert!(
            (84_000_000..=86_000_000).contains(&ticks),
            "2.72 GB must hold about one session of 85M ticks, got {ticks}"
        );
    }

    #[test]
    fn append_then_latest_is_the_last_tick() {
        let mut a = TickRamArena::with_capacity(16);
        let k = key(7);
        assert!(a.latest(k).is_none(), "nothing retained yet");
        push(&mut a, k, 100, 1.0).expect("append");
        push(&mut a, k, 200, 2.0).expect("append");
        let last = a.latest(k).expect("a tick");
        assert_eq!(last.exchange_timestamp, 200);
        assert_eq!(last.last_traded_price, 2.0);
        assert_eq!(a.len(), 2);
        assert_eq!(a.tick_count(k), 2);
    }

    #[test]
    fn walk_back_returns_newest_first_and_only_this_instrument() {
        let mut a = TickRamArena::with_capacity(64);
        let (busy, quiet) = (key(1), key(2));
        // Interleaved, which is the real arrival pattern — the chain must not
        // be confused by another instrument's records sitting between two of
        // its own.
        push(&mut a, busy, 10, 1.0).expect("append");
        push(&mut a, quiet, 11, 9.0).expect("append");
        push(&mut a, busy, 12, 2.0).expect("append");
        push(&mut a, quiet, 13, 9.5).expect("append");
        push(&mut a, busy, 14, 3.0).expect("append");

        let seen: Vec<u32> = a.walk_back(busy).map(|t| t.exchange_timestamp).collect();
        assert_eq!(
            seen,
            [14, 12, 10],
            "newest-first, and NONE of the interleaved other instrument"
        );
        let seen_quiet: Vec<u32> = a.walk_back(quiet).map(|t| t.exchange_timestamp).collect();
        assert_eq!(seen_quiet, [13, 11]);
    }

    #[test]
    fn walk_back_of_an_unknown_instrument_is_empty_not_a_panic() {
        let a = TickRamArena::with_capacity(4);
        assert_eq!(a.walk_back(key(999)).count(), 0);
        assert_eq!(a.tick_count(key(999)), 0);
        assert!(a.latest(key(999)).is_none());
    }

    /// I-P1-11: the same numeric id in two segments is two instruments.
    #[test]
    fn the_same_security_id_in_two_segments_never_shares_a_chain() {
        let mut a = TickRamArena::with_capacity(16);
        let fno = (27_u64, NSE_FNO);
        let idx = (27_u64, IDX);
        push(&mut a, fno, 10, 1.0).expect("append");
        push(&mut a, idx, 20, 2.0).expect("append");

        assert_eq!(a.tracked_instruments(), 2, "two segments, two slots");
        assert_eq!(a.latest(fno).expect("fno").exchange_timestamp, 10);
        assert_eq!(a.latest(idx).expect("idx").exchange_timestamp, 20);
        assert_eq!(a.walk_back(fno).count(), 1);
        assert_eq!(a.walk_back(idx).count(), 1);
    }

    #[test]
    fn a_full_arena_refuses_and_never_evicts() {
        let mut a = TickRamArena::with_capacity(2);
        let k = key(3);
        push(&mut a, k, 1, 1.0).expect("first");
        push(&mut a, k, 2, 2.0).expect("second");
        assert_eq!(
            push(&mut a, k, 3, 3.0),
            Err(AppendRefusal::ArenaFull),
            "past capacity the append must refuse"
        );
        // The refusal must not have cost the tick that WAS retained.
        assert_eq!(a.len(), 2, "a refused append changes nothing");
        assert_eq!(
            a.latest(k).expect("still there").exchange_timestamp,
            2,
            "eviction would have dropped tick 1 and kept 3; this store refuses"
        );
        assert_eq!(a.walk_back(k).count(), 2);
        assert_eq!(a.refused_full(), 1);
    }

    #[test]
    fn a_full_arena_does_not_burn_a_slot_on_a_new_instrument() {
        let mut a = TickRamArena::with_capacity(1);
        push(&mut a, key(1), 1, 1.0).expect("first");
        assert_eq!(a.tracked_instruments(), 1);
        // A brand-new instrument arriving at a full arena must be refused for
        // FULLNESS, and must not consume one of the 25,000 slots on the way.
        assert_eq!(push(&mut a, key(2), 2, 2.0), Err(AppendRefusal::ArenaFull));
        assert_eq!(
            a.tracked_instruments(),
            1,
            "a refused append must not allocate a slot it will never fill"
        );
    }

    #[test]
    fn utilization_pct_is_o1_and_reads_true_at_the_edges() {
        let mut a = TickRamArena::with_capacity(4);
        assert_eq!(a.utilization_pct(), 0);
        push(&mut a, key(1), 1, 1.0).expect("append");
        push(&mut a, key(1), 2, 1.0).expect("append");
        assert_eq!(a.utilization_pct(), 50);
        push(&mut a, key(1), 3, 1.0).expect("append");
        push(&mut a, key(1), 4, 1.0).expect("append");
        assert_eq!(a.utilization_pct(), 100);
        // A zero-capacity arena reports FULL, never 0% — a store that can hold
        // nothing is not an empty store.
        let empty = TickRamArena::with_capacity(0);
        assert_eq!(empty.utilization_pct(), 100);
    }

    #[test]
    fn committed_bytes_matches_the_documented_session_figure() {
        // ~85M ticks, the measured session volume.
        let a = TickRamArena::with_capacity(85_000_000);
        let gb = a.committed_bytes() as f64 / 1e9;
        assert!(
            (2.6..=2.8).contains(&gb),
            "a session of retention must cost about 2.7 GB, got {gb:.2} GB — \
             if this moved, the module header and the memory budget are stale"
        );
    }

    #[test]
    fn append_never_reallocates_the_arena() {
        let mut a = TickRamArena::with_capacity(1_000);
        let before = a.arena.capacity();
        for i in 0..1_000_u32 {
            push(&mut a, key(u64::from(i % 50)), i, i as f32).expect("append");
        }
        assert_eq!(
            a.arena.capacity(),
            before,
            "the arena must never grow: a multi-GB realloc on the frame drain \
             is the stall this store exists to avoid"
        );
        assert_eq!(a.len(), 1_000);
    }

    #[test]
    fn slot_exhaustion_refuses_the_instrument_not_the_store() {
        // Capacity is generous; slots are what runs out.
        let mut a = TickRamArena::with_capacity(MAX_TICK_ARENA_SLOTS + 10);
        for i in 0..MAX_TICK_ARENA_SLOTS as u64 {
            push(&mut a, key(i), 1, 1.0).expect("within the slot ceiling");
        }
        assert_eq!(a.tracked_instruments(), MAX_TICK_ARENA_SLOTS);
        assert_eq!(
            push(&mut a, key(999_999), 1, 1.0),
            Err(AppendRefusal::SlotsExhausted)
        );
        assert_eq!(a.refused_slots(), 1);
        // An ALREADY-tracked instrument must keep working — slot exhaustion
        // stops new instruments, it does not stop the store.
        push(&mut a, key(0), 2, 2.0).expect("a tracked instrument still appends");
        assert_eq!(a.tick_count(key(0)), 2);
    }

    #[test]
    fn every_tick_of_a_burst_is_retained_in_order() {
        // The literal guarantee: nothing missing, nothing reordered.
        let mut a = TickRamArena::with_capacity(10_000);
        let k = key(42);
        for i in 0..10_000_u32 {
            push(&mut a, k, i, i as f32).expect("append");
        }
        assert_eq!(a.tick_count(k), 10_000);
        let mut expected = 9_999_i64;
        for tick in a.walk_back(k) {
            assert_eq!(
                i64::from(tick.exchange_timestamp),
                expected,
                "the backward chain must be strictly ordered with no gaps"
            );
            expected -= 1;
        }
        assert_eq!(expected, -1, "all 10,000 walked, none missing");
    }
}
