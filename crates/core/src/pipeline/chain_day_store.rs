//! In-RAM CURRENT-DAY option-chain minute ring — the "options for the
//! current day" half of the RAM residency answer (operator directive
//! 2026-07-16, verbatim: *"how can i believe you that you have all these
//! already available in our in-memory app RAM — especially for the current
//! day and even in the future last one month data should be entirely in
//! memory app RAM, especially for trading decisions of entry and exit"*,
//! refined by *"for only spots we will have minimum one month data …
//! but option only for the current day"*).
//!
//! ## Shape
//! A fixed 6-slot store (`Feed::COUNT` 2 × [`ChainUnderlying::COUNT`] 3),
//! each slot a guarded `BTreeMap<minute_ts_ist_nanos, Arc<ChainMoneynessSnapshot>>`
//! for the CURRENT IST day only. The existing latest-minute registry
//! (`chain_snapshot.rs`) is UNTOUCHED — this store ADDS minute history so
//! a decision consumer can read the whole day's chain evolution, not just
//! the latest minute.
//!
//! ## Write vs read (the honest envelope)
//! - WRITE ([`ChainDayStore::record_live`] / [`ChainDayStore::record_rehydrated`]):
//!   cold path — once per (feed, underlying) per minute from the chain
//!   leg's scheduled fire (live), or the bounded boot rehydrate. Writes
//!   MAY allocate (one `Arc::new` per minute; the rows `Vec` was built by
//!   the caller) — the chain_snapshot "write may allocate" split.
//! - READ ([`ChainDayStore::minute_snapshot`] / [`ChainDayStore::latest_minutes`]):
//!   guarded `RwLock` + `BTreeMap` lookup — O(log minutes ≤ ~405) — NOT
//!   lock-free and NOT claimed O(1) (stated plainly; the reads are COLD
//!   today: NO strategy consumer exists — §28 boundary; this is the read
//!   contract only, bound by the §38.8 decision-freshness gate whenever a
//!   consumer ships with its own dated operator scope).
//!
//! ## Bounds (a hostile/runaway publisher can never grow RAM unbounded)
//! - Minutes per slot: [`MAX_MINUTES_PER_SLOT`] (405 — the 375-minute
//!   session + margin for the legs' boundary fires).
//! - Rows per minute: `chain_row_cap` (config `[market_ram_store]`,
//!   default 1_000) — an over-cap publish is TRUNCATED loudly
//!   (RAMSTORE-01 `stage="chain_truncated"`), never grown.
//! - Worst case ≈ 375 min × 6 slots × 1_000 rows × 24 B ≈ 54 MB —
//!   test-asserted < 120 MB.
//!
//! ## Day roll (pre-open honesty)
//! The store holds the LATEST PUBLISHED IST day: a publish for a NEWER
//! IST day clears the slot (options are current-day per the operator
//! scope); a STALE older-day publish is DROPPED loudly (RAMSTORE-01
//! `stage="day_drop"`) — it can never clear the live day. There is
//! deliberately NO clock-based clearing (simple + deterministic), so an
//! overnight-surviving process serves the PRIOR session pre-open until
//! the first ~09:16 publish rolls the day — any future consumer MUST
//! check [`ChainDayStore::day`] against today before trusting a read
//! (the §38.8 decision-freshness gate); every returned snapshot also
//! carries its own `minute_ts_ist_nanos` (day-derivable via
//! [`epoch_day_of_ist_nanos`]).
//!
//! ## Durability (audit Rule 11 honesty)
//! PROCESS-LOCAL. QuestDB (`option_chain_1m`) is and remains the durable
//! truth — a restart rehydrates today's minutes from it (the bounded
//! `/exec` windows in `crates/app/src/market_ram_store_boot.rs`), and
//! rehydrated minutes NEVER overwrite live-published ones. The store
//! deliberately holds ONLY the published decision rows (strike / leg /
//! ltp / moneyness + the snapshot header) — oi / volume / greeks stay in
//! the DB table, honestly NOT RAM-resident.
//!
//! Runbook: `.claude/rules/project/ram-store-error-codes.md` (RAMSTORE-01).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::warn;

use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;

use super::chain_snapshot::{ChainMoneynessSnapshot, ChainUnderlying, SnapshotRow};

/// Nanoseconds per IST day (day-roll bucketing of minute timestamps —
/// the stamps are IST-epoch nanos, so a plain div_euclid buckets days).
const NANOS_PER_DAY: i64 = 86_400_000_000_000;

/// Hard per-slot minute bound: the 375-minute session + margin for the
/// chain legs' boundary/warmup fires (mirrors the option_chain_1m legs'
/// own bounded-minutes discipline). An over-cap NEW minute is dropped
/// loudly — never unbounded growth.
pub const MAX_MINUTES_PER_SLOT: usize = 405;

/// Fixed slot count: 2 feeds × 3 chain underlyings.
const SLOT_COUNT: usize = Feed::COUNT * ChainUnderlying::COUNT;

/// Dense slot index for a (feed, underlying) pair — two integer ops.
const fn slot_index(feed: Feed, underlying: ChainUnderlying) -> usize {
    feed.index() * ChainUnderlying::COUNT + underlying.index()
}

/// IST epoch-day ordinal of an IST-nanos timestamp (day-roll key).
#[must_use]
pub const fn epoch_day_of_ist_nanos(ist_nanos: i64) -> i64 {
    ist_nanos.div_euclid(NANOS_PER_DAY)
}

/// One slot's interior: the current IST day ordinal + its minute map.
struct SlotInner {
    /// Current IST epoch-day ordinal (0 = unset — before the first
    /// record for this slot).
    day: i64,
    /// minute-open IST nanos → the published snapshot for that minute.
    minutes: BTreeMap<i64, Arc<ChainMoneynessSnapshot>>,
}

/// Outcome of a record call (returned so the wiring tests + callers can
/// assert behaviour without scraping logs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainRecordOutcome {
    /// Stored (fresh minute, or live overwrite of the same minute).
    Recorded,
    /// The rows were truncated to `chain_row_cap` and then stored.
    RecordedTruncated,
    /// Rehydrate: the minute was already live-resident — kept the live one.
    KeptExistingLive,
    /// The empty boot sentinel (minute ts 0) — never stored.
    SkippedSentinel,
    /// A STALE older-day publish — dropped (never clears the live day).
    DroppedOlderDay,
    /// The per-slot minute cap refused a NEW minute — dropped loudly.
    DroppedMinuteCap,
}

/// Per-feed + total stats snapshot (for the gauges/heartbeat task).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainDayStoreStats {
    /// Resident minutes per feed (summed over the feed's underlyings).
    pub minutes_resident_per_feed: [u64; Feed::COUNT],
    /// Total resident classified rows across all slots.
    pub rows_resident: u64,
    /// Estimated resident bytes (headers + rows; capacity-based rows).
    pub estimated_bytes: u64,
    /// Lifetime truncated publishes (row-cap enforcement fired).
    pub truncated_publishes: u64,
    /// Lifetime stale older-day drops.
    pub day_drops: u64,
    /// Lifetime minute-cap drops.
    pub minute_cap_drops: u64,
}

/// The current-day chain minute store (see the module docs).
pub struct ChainDayStore {
    /// Rows kept per stored minute (config `chain_row_cap`).
    chain_row_cap: usize,
    /// The 6 (feed, underlying) slots.
    slots: [RwLock<SlotInner>; SLOT_COUNT],
    /// Lifetime truncated publishes.
    truncated_publishes: AtomicU64,
    /// Lifetime stale older-day drops.
    day_drops: AtomicU64,
    /// Lifetime minute-cap drops.
    minute_cap_drops: AtomicU64,
}

impl ChainDayStore {
    /// Build an empty store with the configured per-minute row cap
    /// (cold path — boot only).
    #[must_use]
    pub fn new(chain_row_cap: usize) -> Self {
        Self {
            chain_row_cap: chain_row_cap.max(1),
            slots: std::array::from_fn(|_| {
                RwLock::new(SlotInner {
                    day: 0,
                    minutes: BTreeMap::new(),
                })
            }),
            truncated_publishes: AtomicU64::new(0),
            day_drops: AtomicU64::new(0),
            minute_cap_drops: AtomicU64::new(0),
        }
    }

    /// The configured per-minute row cap.
    #[must_use]
    pub const fn chain_row_cap(&self) -> usize {
        self.chain_row_cap
    }

    /// Record a LIVE-published minute snapshot (last-write-wins for the
    /// same minute — the live leg may legitimately re-fire a minute).
    pub fn record_live(&self, snapshot: ChainMoneynessSnapshot) -> ChainRecordOutcome {
        self.record(snapshot, true)
    }

    /// Record a boot-REHYDRATED minute snapshot — insert ONLY if the
    /// minute is absent (a live publish always outranks rehydration).
    pub fn record_rehydrated(&self, snapshot: ChainMoneynessSnapshot) -> ChainRecordOutcome {
        self.record(snapshot, false)
    }

    /// Shared record core (cold path — once per minute per slot).
    fn record(&self, snapshot: ChainMoneynessSnapshot, live: bool) -> ChainRecordOutcome {
        if snapshot.is_empty_sentinel() {
            return ChainRecordOutcome::SkippedSentinel;
        }
        let mut snapshot = snapshot;
        let mut truncated = false;
        if snapshot.rows.len() > self.chain_row_cap {
            let over = snapshot.rows.len();
            snapshot.rows.truncate(self.chain_row_cap);
            // PR-2 round-1 LOW: truncate keeps the ORIGINAL over-cap Vec
            // capacity resident — release it (cold, loud-path only; the
            // whole point of the cap is bounding resident bytes).
            snapshot.rows.shrink_to_fit();
            truncated = true;
            self.truncated_publishes.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("tv_ram_store_dropped_total", "reason" => "row_cap").increment(1);
            warn!(
                code = ErrorCode::RamStore01Degraded.code_str(),
                stage = "chain_truncated",
                feed = snapshot.feed.as_str(),
                underlying = snapshot.underlying.as_str(),
                minute_ts_ist_nanos = snapshot.minute_ts_ist_nanos,
                rows_published = over,
                rows_kept = self.chain_row_cap,
                "RAMSTORE-01: chain minute snapshot exceeded chain_row_cap — truncated (kept prefix; the option_chain_1m table keeps the full rows)"
            );
        }
        let minute = snapshot.minute_ts_ist_nanos;
        let day = epoch_day_of_ist_nanos(minute);
        let slot = &self.slots[slot_index(snapshot.feed, snapshot.underlying)];
        let mut inner = slot.write().unwrap_or_else(PoisonError::into_inner);
        if inner.day == 0 {
            inner.day = day;
        } else if day > inner.day {
            // Day rolled — options are CURRENT-DAY only per the operator
            // scope; clear yesterday and start the new day.
            inner.minutes.clear();
            inner.day = day;
        } else if day < inner.day {
            drop(inner);
            self.day_drops.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("tv_ram_store_dropped_total", "reason" => "day_drop").increment(1);
            warn!(
                code = ErrorCode::RamStore01Degraded.code_str(),
                stage = "day_drop",
                feed = snapshot.feed.as_str(),
                underlying = snapshot.underlying.as_str(),
                minute_ts_ist_nanos = minute,
                "RAMSTORE-01: stale older-day chain publish dropped — it can never clear the live day"
            );
            return ChainRecordOutcome::DroppedOlderDay;
        }
        let is_new_minute = !inner.minutes.contains_key(&minute);
        if is_new_minute && inner.minutes.len() >= MAX_MINUTES_PER_SLOT {
            drop(inner);
            self.minute_cap_drops.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("tv_ram_store_dropped_total", "reason" => "minute_cap").increment(1);
            warn!(
                code = ErrorCode::RamStore01Degraded.code_str(),
                stage = "chain_truncated",
                feed = snapshot.feed.as_str(),
                underlying = snapshot.underlying.as_str(),
                minute_ts_ist_nanos = minute,
                minute_cap = MAX_MINUTES_PER_SLOT,
                "RAMSTORE-01: per-slot minute cap refused a new chain minute — dropped (bounded growth defense)"
            );
            return ChainRecordOutcome::DroppedMinuteCap;
        }
        if !live && !is_new_minute {
            // Rehydration never overwrites a live-published minute.
            return ChainRecordOutcome::KeptExistingLive;
        }
        inner.minutes.insert(minute, Arc::new(snapshot));
        if truncated {
            ChainRecordOutcome::RecordedTruncated
        } else {
            ChainRecordOutcome::Recorded
        }
    }

    /// The stored snapshot for an exact minute-open IST-nanos stamp
    /// (guarded read + BTreeMap lookup — O(log minutes), NOT lock-free).
    #[must_use]
    pub fn minute_snapshot(
        &self,
        feed: Feed,
        underlying: ChainUnderlying,
        minute_ts_ist_nanos: i64,
    ) -> Option<Arc<ChainMoneynessSnapshot>> {
        let inner = self.slots[slot_index(feed, underlying)]
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        // Arc refcount bump on a cold read path, not a data copy.
        inner.minutes.get(&minute_ts_ist_nanos).map(Arc::clone)
    }

    /// The newest `n` stored minutes for a slot, NEWEST FIRST (cold read;
    /// allocates the result Vec — never called on a hot path).
    #[must_use]
    pub fn latest_minutes(
        &self,
        feed: Feed,
        underlying: ChainUnderlying,
        n: usize,
    ) -> Vec<Arc<ChainMoneynessSnapshot>> {
        let inner = self.slots[slot_index(feed, underlying)]
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let take = n.min(inner.minutes.len());
        let mut out = Vec::with_capacity(take);
        for (_, snap) in inner.minutes.iter().rev().take(take) {
            out.push(Arc::clone(snap)); // O(1) EXEMPT: Arc refcount bump on a cold read path, not a data copy
        }
        out
    }

    /// The slot's resident IST epoch-day ordinal (`0` = nothing recorded
    /// yet). PRE-OPEN HONESTY (PR-2 round-1): the store holds the LATEST
    /// PUBLISHED day — before the first ~09:16 publish of a new session,
    /// an overnight-surviving process still serves the PRIOR session's
    /// minutes, so any future consumer MUST compare this against today's
    /// IST epoch-day (the §38.8 decision-freshness gate) before trusting
    /// `minute_snapshot` / `latest_minutes` reads. Each returned snapshot
    /// additionally carries its own `minute_ts_ist_nanos`, day-derivable
    /// via [`epoch_day_of_ist_nanos`] — the read surface exposes the day
    /// both ways.
    #[must_use]
    pub fn day(&self, feed: Feed, underlying: ChainUnderlying) -> i64 {
        self.slots[slot_index(feed, underlying)]
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .day
    }

    /// Resident minutes for one feed (summed over its underlyings).
    #[must_use]
    pub fn minutes_resident(&self, feed: Feed) -> u64 {
        let mut total = 0u64;
        for &u in ChainUnderlying::ALL {
            let inner = self.slots[slot_index(feed, u)]
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            total += inner.minutes.len() as u64;
        }
        total
    }

    /// One stats snapshot (for the 60s gauges/heartbeat task — cold path).
    #[must_use]
    pub fn stats(&self) -> ChainDayStoreStats {
        let mut minutes_per_feed = [0u64; Feed::COUNT];
        let mut rows_resident = 0u64;
        let mut estimated_bytes = 0u64;
        let header_bytes = std::mem::size_of::<ChainMoneynessSnapshot>() as u64
            + std::mem::size_of::<Arc<ChainMoneynessSnapshot>>() as u64;
        let row_bytes = std::mem::size_of::<SnapshotRow>() as u64;
        for &feed in Feed::ALL {
            for &u in ChainUnderlying::ALL {
                let inner = self.slots[slot_index(feed, u)]
                    .read()
                    .unwrap_or_else(PoisonError::into_inner);
                let minutes = inner.minutes.len() as u64;
                minutes_per_feed[feed.index()] += minutes;
                estimated_bytes += minutes * header_bytes;
                for snap in inner.minutes.values() {
                    let rows = snap.rows.len() as u64;
                    rows_resident += rows;
                    estimated_bytes += rows * row_bytes;
                }
            }
        }
        ChainDayStoreStats {
            minutes_resident_per_feed: minutes_per_feed,
            rows_resident,
            estimated_bytes,
            truncated_publishes: self.truncated_publishes.load(Ordering::Relaxed),
            day_drops: self.day_drops.load(Ordering::Relaxed),
            minute_cap_drops: self.minute_cap_drops.load(Ordering::Relaxed),
        }
    }
}

/// Process-global store handle (house `OnceLock` pattern — the
/// chain_snapshot / spot_bar_store precedent). Installed once at boot by
/// `crates/app/src/market_ram_store_boot.rs` when `[market_ram_store]`
/// is enabled; absent otherwise (readers get `None`, never a panic).
static CHAIN_DAY_STORE: OnceLock<ChainDayStore> = OnceLock::new();

/// Install the process-global store (first-wins; a duplicate install is
/// refused and reported `false` so the caller can log RAMSTORE-01
/// `stage="install"` loudly).
pub fn install_chain_day_store(chain_row_cap: usize) -> bool {
    CHAIN_DAY_STORE
        .set(ChainDayStore::new(chain_row_cap))
        .is_ok()
}

/// The installed process-global store, if any.
#[must_use]
pub fn chain_day_store() -> Option<&'static ChainDayStore> {
    CHAIN_DAY_STORE.get()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::moneyness::{Moneyness, OptionLeg};

    /// 2026-07-16 09:16:00 IST as IST-epoch nanos (a plausible minute-open).
    const DAY_NANOS: i64 = NANOS_PER_DAY;
    const MINUTE_NANOS: i64 = 60 * 1_000_000_000;

    fn minute_on_day(day: i64, minute_of_day: i64) -> i64 {
        day * DAY_NANOS + minute_of_day * MINUTE_NANOS
    }

    fn snap(
        feed: Feed,
        underlying: ChainUnderlying,
        minute_ts: i64,
        rows: usize,
    ) -> ChainMoneynessSnapshot {
        let mut row_vec = Vec::with_capacity(rows);
        for i in 0..rows {
            row_vec.push(SnapshotRow {
                strike_paise: 2_450_000 + (i as i64) * 5_000,
                ltp_paise: 10_000 + (i as i64),
                leg: if i % 2 == 0 {
                    OptionLeg::Ce
                } else {
                    OptionLeg::Pe
                },
                moneyness: Moneyness::Otm,
            });
        }
        ChainMoneynessSnapshot {
            feed,
            underlying,
            minute_ts_ist_nanos: minute_ts,
            fetched_at_ist_nanos: minute_ts + 1_500_000_000,
            underlying_spot: 24_536.40,
            underlying_spot_paise: 2_453_640,
            atm_strike_paise: 2_455_000,
            expiry_ist_nanos: minute_ts + 3 * DAY_NANOS,
            spot_missing: false,
            rows: row_vec,
        }
    }

    #[test]
    fn test_chain_day_store_record_live_and_minute_snapshot_roundtrip() {
        let store = ChainDayStore::new(1_000);
        let day = 20_650; // arbitrary epoch-day ordinal
        let m1 = minute_on_day(day, 556); // 09:16 IST
        let m2 = minute_on_day(day, 557);
        assert_eq!(
            store.record_live(snap(Feed::Dhan, ChainUnderlying::Nifty, m1, 3)),
            ChainRecordOutcome::Recorded
        );
        assert_eq!(
            store.record_live(snap(Feed::Dhan, ChainUnderlying::Nifty, m2, 4)),
            ChainRecordOutcome::Recorded
        );
        let got = store
            .minute_snapshot(Feed::Dhan, ChainUnderlying::Nifty, m1)
            .expect("m1 resident");
        assert_eq!(got.rows.len(), 3);
        assert_eq!(got.minute_ts_ist_nanos, m1);
        // Live re-publish of the same minute is last-write-wins.
        assert_eq!(
            store.record_live(snap(Feed::Dhan, ChainUnderlying::Nifty, m1, 5)),
            ChainRecordOutcome::Recorded
        );
        let replaced = store
            .minute_snapshot(Feed::Dhan, ChainUnderlying::Nifty, m1)
            .expect("m1 still resident");
        assert_eq!(replaced.rows.len(), 5, "live overwrite wins");
        // Slot isolation: the Groww NIFTY slot is untouched.
        assert!(
            store
                .minute_snapshot(Feed::Groww, ChainUnderlying::Nifty, m1)
                .is_none()
        );
        assert_eq!(store.minutes_resident(Feed::Dhan), 2);
        assert_eq!(store.minutes_resident(Feed::Groww), 0);
    }

    #[test]
    fn test_chain_day_store_day_roll_clears_previous_day() {
        let store = ChainDayStore::new(1_000);
        let day = 20_650;
        let m_yday = minute_on_day(day, 700);
        let m_today = minute_on_day(day + 1, 556);
        assert_eq!(
            store.record_live(snap(Feed::Groww, ChainUnderlying::Banknifty, m_yday, 2)),
            ChainRecordOutcome::Recorded
        );
        assert_eq!(store.minutes_resident(Feed::Groww), 1);
        // A NEWER-day publish rolls the slot: yesterday cleared.
        assert_eq!(
            store.record_live(snap(Feed::Groww, ChainUnderlying::Banknifty, m_today, 2)),
            ChainRecordOutcome::Recorded
        );
        assert!(
            store
                .minute_snapshot(Feed::Groww, ChainUnderlying::Banknifty, m_yday)
                .is_none()
        );
        assert!(
            store
                .minute_snapshot(Feed::Groww, ChainUnderlying::Banknifty, m_today)
                .is_some()
        );
        assert_eq!(store.minutes_resident(Feed::Groww), 1);
    }

    #[test]
    fn test_chain_day_store_stale_older_day_publish_dropped() {
        let store = ChainDayStore::new(1_000);
        let day = 20_650;
        let m_today = minute_on_day(day, 556);
        let m_stale = minute_on_day(day - 1, 700);
        assert_eq!(
            store.record_live(snap(Feed::Dhan, ChainUnderlying::Sensex, m_today, 2)),
            ChainRecordOutcome::Recorded
        );
        // A stale OLDER-day publish is dropped and never clears the day.
        assert_eq!(
            store.record_live(snap(Feed::Dhan, ChainUnderlying::Sensex, m_stale, 2)),
            ChainRecordOutcome::DroppedOlderDay
        );
        assert!(
            store
                .minute_snapshot(Feed::Dhan, ChainUnderlying::Sensex, m_today)
                .is_some()
        );
        assert!(
            store
                .minute_snapshot(Feed::Dhan, ChainUnderlying::Sensex, m_stale)
                .is_none()
        );
        assert_eq!(store.stats().day_drops, 1);
    }

    #[test]
    fn test_chain_day_store_row_cap_truncates_loudly() {
        let store = ChainDayStore::new(10);
        assert_eq!(store.chain_row_cap(), 10);
        let day = 20_650;
        let m = minute_on_day(day, 556);
        assert_eq!(
            store.record_live(snap(Feed::Dhan, ChainUnderlying::Nifty, m, 25)),
            ChainRecordOutcome::RecordedTruncated
        );
        let got = store
            .minute_snapshot(Feed::Dhan, ChainUnderlying::Nifty, m)
            .expect("resident");
        assert_eq!(got.rows.len(), 10, "kept prefix only");
        assert_eq!(got.rows[0].strike_paise, 2_450_000, "prefix order intact");
        assert_eq!(store.stats().truncated_publishes, 1);
        // Minute-cap: fill the slot to the cap, then a NEW minute drops.
        let store2 = ChainDayStore::new(4);
        for i in 0..MAX_MINUTES_PER_SLOT {
            assert_eq!(
                store2.record_live(snap(
                    Feed::Groww,
                    ChainUnderlying::Nifty,
                    minute_on_day(day, i as i64),
                    1
                )),
                ChainRecordOutcome::Recorded
            );
        }
        assert_eq!(
            store2.record_live(snap(
                Feed::Groww,
                ChainUnderlying::Nifty,
                minute_on_day(day, MAX_MINUTES_PER_SLOT as i64),
                1
            )),
            ChainRecordOutcome::DroppedMinuteCap
        );
        // An EXISTING minute still updates at the cap (not a new key).
        assert_eq!(
            store2.record_live(snap(
                Feed::Groww,
                ChainUnderlying::Nifty,
                minute_on_day(day, 0),
                2
            )),
            ChainRecordOutcome::Recorded
        );
        assert_eq!(store2.stats().minute_cap_drops, 1);
    }

    #[test]
    fn test_chain_day_store_record_rehydrated_never_overwrites_live() {
        let store = ChainDayStore::new(1_000);
        let day = 20_650;
        let m_live = minute_on_day(day, 556);
        let m_gap = minute_on_day(day, 557);
        assert_eq!(
            store.record_live(snap(Feed::Dhan, ChainUnderlying::Nifty, m_live, 7)),
            ChainRecordOutcome::Recorded
        );
        // Rehydrate the SAME minute — the live one is kept.
        assert_eq!(
            store.record_rehydrated(snap(Feed::Dhan, ChainUnderlying::Nifty, m_live, 3)),
            ChainRecordOutcome::KeptExistingLive
        );
        assert_eq!(
            store
                .minute_snapshot(Feed::Dhan, ChainUnderlying::Nifty, m_live)
                .expect("resident")
                .rows
                .len(),
            7,
            "live minute untouched by rehydration"
        );
        // Rehydrate an ABSENT minute — inserted.
        assert_eq!(
            store.record_rehydrated(snap(Feed::Dhan, ChainUnderlying::Nifty, m_gap, 3)),
            ChainRecordOutcome::Recorded
        );
        assert_eq!(
            store
                .minute_snapshot(Feed::Dhan, ChainUnderlying::Nifty, m_gap)
                .expect("resident")
                .rows
                .len(),
            3
        );
    }

    #[test]
    fn test_chain_day_store_latest_minutes_and_minutes_resident() {
        let store = ChainDayStore::new(1_000);
        let day = 20_650;
        for i in 0..5 {
            assert_eq!(
                store.record_live(snap(
                    Feed::Groww,
                    ChainUnderlying::Sensex,
                    minute_on_day(day, 556 + i),
                    1
                )),
                ChainRecordOutcome::Recorded
            );
        }
        let latest = store.latest_minutes(Feed::Groww, ChainUnderlying::Sensex, 3);
        assert_eq!(latest.len(), 3);
        assert_eq!(latest[0].minute_ts_ist_nanos, minute_on_day(day, 560));
        assert_eq!(latest[1].minute_ts_ist_nanos, minute_on_day(day, 559));
        assert_eq!(latest[2].minute_ts_ist_nanos, minute_on_day(day, 558));
        // Asking for more than resident returns all, newest first.
        let all = store.latest_minutes(Feed::Groww, ChainUnderlying::Sensex, 99);
        assert_eq!(all.len(), 5);
        assert_eq!(store.minutes_resident(Feed::Groww), 5);
        assert_eq!(store.minutes_resident(Feed::Dhan), 0);
        assert!(
            store
                .latest_minutes(Feed::Dhan, ChainUnderlying::Nifty, 3)
                .is_empty()
        );
    }

    #[test]
    fn test_chain_day_store_stats_estimated_bytes_under_120mb() {
        // The structural worst case: 375 session minutes × 6 slots ×
        // chain_row_cap (1_000) rows — asserted under the 120 MB envelope.
        let header = std::mem::size_of::<ChainMoneynessSnapshot>() as u64
            + std::mem::size_of::<Arc<ChainMoneynessSnapshot>>() as u64;
        let worst = 375u64 * 6 * (header + 1_000 * std::mem::size_of::<SnapshotRow>() as u64);
        assert!(
            worst < 120 * 1024 * 1024,
            "chain day store worst case {worst} bytes must stay under 120 MB"
        );
        // Measured stats on a small store agree with the row/minute math.
        let store = ChainDayStore::new(1_000);
        let day = 20_650;
        store.record_live(snap(
            Feed::Dhan,
            ChainUnderlying::Nifty,
            minute_on_day(day, 556),
            4,
        ));
        store.record_live(snap(
            Feed::Dhan,
            ChainUnderlying::Nifty,
            minute_on_day(day, 557),
            6,
        ));
        let stats = store.stats();
        assert_eq!(stats.minutes_resident_per_feed[Feed::Dhan.index()], 2);
        assert_eq!(stats.minutes_resident_per_feed[Feed::Groww.index()], 0);
        assert_eq!(stats.rows_resident, 10);
        let expected = 2 * header + 10 * std::mem::size_of::<SnapshotRow>() as u64;
        assert_eq!(stats.estimated_bytes, expected);
        assert_eq!(stats.truncated_publishes, 0);
        assert_eq!(stats.day_drops, 0);
        assert_eq!(stats.minute_cap_drops, 0);
    }

    #[test]
    fn test_chain_day_store_day_exposes_resident_day() {
        // PR-2 round-1: the read surface exposes the slot's resident IST
        // epoch-day so a pre-open consumer can detect the PRIOR session
        // (the §38.8 freshness gate input). 0 = nothing recorded yet.
        let store = ChainDayStore::new(1_000);
        let day = 20_650;
        assert_eq!(store.day(Feed::Dhan, ChainUnderlying::Nifty), 0);
        store.record_live(snap(
            Feed::Dhan,
            ChainUnderlying::Nifty,
            minute_on_day(day, 556),
            2,
        ));
        assert_eq!(store.day(Feed::Dhan, ChainUnderlying::Nifty), day);
        // Slot isolation: other slots still read unset.
        assert_eq!(store.day(Feed::Groww, ChainUnderlying::Nifty), 0);
        // Day roll advances the resident day; a stale drop never regresses it.
        store.record_live(snap(
            Feed::Dhan,
            ChainUnderlying::Nifty,
            minute_on_day(day + 1, 556),
            2,
        ));
        assert_eq!(store.day(Feed::Dhan, ChainUnderlying::Nifty), day + 1);
        assert_eq!(
            store.record_live(snap(
                Feed::Dhan,
                ChainUnderlying::Nifty,
                minute_on_day(day, 700),
                2
            )),
            ChainRecordOutcome::DroppedOlderDay
        );
        assert_eq!(store.day(Feed::Dhan, ChainUnderlying::Nifty), day + 1);
    }

    #[test]
    fn test_chain_day_store_sentinel_never_recorded() {
        let store = ChainDayStore::new(1_000);
        let sentinel = ChainMoneynessSnapshot::empty_sentinel(Feed::Dhan, ChainUnderlying::Nifty);
        assert_eq!(
            store.record_live(sentinel),
            ChainRecordOutcome::SkippedSentinel
        );
        let sentinel2 = ChainMoneynessSnapshot::empty_sentinel(Feed::Dhan, ChainUnderlying::Nifty);
        assert_eq!(
            store.record_rehydrated(sentinel2),
            ChainRecordOutcome::SkippedSentinel
        );
        assert_eq!(store.minutes_resident(Feed::Dhan), 0);
        assert!(
            store
                .minute_snapshot(Feed::Dhan, ChainUnderlying::Nifty, 0)
                .is_none()
        );
    }

    #[test]
    fn test_install_chain_day_store_first_wins_and_chain_day_store_accessor() {
        // The process-global is empty until installed in THIS test binary
        // (no other test in this crate installs it).
        assert!(chain_day_store().is_none() || chain_day_store().is_some());
        let first = install_chain_day_store(800);
        if first {
            assert!(chain_day_store().is_some());
            assert_eq!(chain_day_store().expect("installed").chain_row_cap(), 800);
        }
        // A duplicate install is refused (first-wins) and reported false.
        assert!(!install_chain_day_store(999));
        assert_ne!(
            chain_day_store().expect("still installed").chain_row_cap(),
            999
        );
        // epoch-day helper sanity (div_euclid semantics incl. negatives).
        assert_eq!(epoch_day_of_ist_nanos(0), 0);
        assert_eq!(epoch_day_of_ist_nanos(NANOS_PER_DAY - 1), 0);
        assert_eq!(epoch_day_of_ist_nanos(NANOS_PER_DAY), 1);
        assert_eq!(epoch_day_of_ist_nanos(-1), -1);
    }
}
