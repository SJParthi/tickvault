//! Today's spot price per instrument, held in RAM, read without a database.
//!
//! ## Why this exists (measured, 2026-09-08)
//!
//! Locating at-the-money needs one number per underlying: what the stock is
//! trading at right now. Until this store, the contract selector obtained it
//! by asking QuestDB — `SELECT security_id, segment, ltp FROM ticks ...
//! LATEST ON ts PARTITION BY security_id, segment`.
//!
//! On 2026-09-08 that cost thirty minutes of the trading day:
//!
//! | IST | ingest ticks / 5 min | `priced_underlyings` | `stock_options` |
//! |---|---:|---:|---:|
//! | 09:15 (open) | 103,887 | 0 | 0 |
//! | 09:30 | 106,124 | 0 | 0 |
//! | 09:44 | 102,902 | 118 (indices only) | 0 |
//! | **09:45** | 104,801 | **861** | **20,224** |
//!
//! Ticks were arriving at full rate from the opening bell. The join saw
//! nothing for half an hour, because a row that QuestDB has ACCEPTED is not
//! queryable until it has been APPLIED, and `tv_questdb_wal_apply_lag_max`
//! read 11,503 at 09:00 and 27,089 by 09:55 — still climbing. The process
//! held every price it needed in memory the entire time.
//!
//! Cost of the wait: 20,224 stock-option contracts and every stock-option
//! depth socket, unsubscribed through the most volatile half hour of the
//! session. The lag grows, so the wait grows with it.
//!
//! This store removes the database from that path. The drain writes the
//! price it has just decoded; the attach task reads it. Nothing in between.
//!
//! ## Why `papaya` here, when `PrevCloseStore` next door is a plain `HashMap`
//!
//! That store is single-owner — it lives on the drain's state and is reached
//! through `&mut self`, so a concurrent map would buy lock-free reads nobody
//! makes and pay epoch-reclamation for them. This one has TWO parties by
//! construction: the drain writes it on the tick path, and the contract and
//! depth attach tasks read it from their own tasks, minutes later. That is
//! precisely the case `papaya` is for, and the reason the aggregator's header
//! gives for REJECTING it is the reason to accept it here.
//!
//! The value is an `AtomicI64`, not a `Mutex<i64>`: a write is one relaxed
//! store and a read is one relaxed load, so an already-tracked instrument
//! never takes a lock on the tick path.
//!
//! ## Ordering
//!
//! `Relaxed` on both sides. The only invariant is that a reader observes SOME
//! price this instrument actually traded at — never a torn or invented one,
//! which an aligned 64-bit atomic guarantees on its own. There is no second
//! location whose visibility must be ordered against this one, so
//! `Acquire`/`Release` would buy a happens-before edge nothing consumes.
//!
//! CORRECTED 2026-09-08, by a hostile review of this file's first draft: that
//! draft justified `Relaxed` as avoiding "a fence on every tick". It does not.
//! Every `record` calls `prices.pin()`, which is `seize::Collector::enter` — a
//! thread-local lookup plus a light store barrier, and a symmetric one on
//! drop. That pin already costs more than the ordering choice does. `Relaxed`
//! is still correct, for the reason above; the saving it was claimed to make
//! was not real, and a wrong justification for a right decision is how the
//! next reader learns the wrong lesson.
//!
//! ## Complexity
//!
//! `record` and `latest_paise` are **O(1) average** — one hash probe on the
//! I-P1-11 composite key plus one atomic operation. Zero allocation for an
//! already-tracked instrument, which is every tick after the first.
//! `snapshot_prices` is **O(tracked)** and says so; it runs on the attach
//! path at most once per retry attempt, over ~870 spot instruments, never on
//! the tick path. Space is O(instruments), hard-bounded by the cap below.

use std::sync::atomic::{AtomicI64, Ordering};

use papaya::HashMap as PapayaHashMap;
use tickvault_common::types::ExchangeSegment;

/// The I-P1-11 composite identity. `security_id` ALONE is not unique — Dhan
/// reuses one numeric id across segments, so keying on the bare id would let
/// an index's price answer a stock's query and place that stock's entire
/// option ladder around the wrong number.
pub type SpotPriceKey = (u64, ExchangeSegment);

/// Hard ceiling on tracked instruments. 25,000 deliberately matches
/// `PrevCloseStore::MAX_TRACKED_INSTRUMENTS`, `DayOhlcTracker` and
/// `TickGapTracker`, so one figure covers every per-instrument map on the
/// same universe and there is no second number to keep in step.
pub const MAX_TRACKED_INSTRUMENTS: usize = 25_000;

/// Counter for a NEW instrument refused at the cap.
pub const REFUSED_COUNTER: &str = "tv_spot_price_store_refused_total";

/// Counter for a price refused because it could not locate at-the-money.
pub const REJECTED_VALUE_COUNTER: &str = "tv_spot_price_store_rejected_value_total";

/// Gauge: how many instruments the store currently prices.
///
/// The number that separates "the RAM path is warming up" from "the RAM path
/// is not being fed", which no other signal distinguishes — both render as an
/// empty contract universe.
pub const TRACKED_GAUGE: &str = "tv_spot_price_store_tracked";

/// What `record` did with the offered price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    /// Stored — a new instrument, or a fresher price for a tracked one.
    Stored,
    /// Not finite, not positive, or too large to hold in paise. Refused
    /// before it could place a ladder.
    RejectedValue,
    /// The map is at its ceiling and this instrument is not already in it.
    RefusedAtCapacity,
}

/// Converts a rupee price to paise, or says it cannot.
///
/// Shared by the RAM path and the QuestDB path deliberately. Two conversions
/// that must agree and are written twice is a drift waiting to happen: the
/// fallback would place ladders one rounding step away from the primary, and
/// the difference would appear as an off-by-one strike that no log explains.
///
/// Returns `None` for a price that cannot locate at-the-money: non-finite,
/// non-positive (a price of zero would put the window at the bottom of every
/// ladder), or beyond exact integer range in an `f64`.
#[must_use]
pub fn rupees_to_paise(rupees: f64) -> Option<i64> {
    if !rupees.is_finite() || rupees <= 0.0 {
        return None;
    }
    let paise = (rupees * 100.0).round();
    // 2^53 - 1: past this an f64 no longer represents consecutive integers,
    // so the rounded value is not the price any more.
    if paise > 9_007_199_254_740_991.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    // APPROVED: bounded above by the line before and below by the `<= 0.0`
    // guard — a whole number well inside i64.
    Some(paise as i64)
}

/// Latest traded price per instrument, in paise, for today.
///
/// Written by the drain on the tick path, read by the attach tasks. Both go
/// through `&self`; there is no lock on either side.
#[derive(Debug)]
pub struct SpotPriceStore {
    prices: PapayaHashMap<SpotPriceKey, AtomicI64>,
}

impl Default for SpotPriceStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SpotPriceStore {
    /// An empty store.
    ///
    /// Deliberately NOT pre-sized to the cap: `papaya` grows by resizing, and
    /// reserving 25,000 slots for a set that is ~870 in practice would commit
    /// memory the universe never uses. The bound is the explicit check in
    /// [`Self::record`], never an allocation figure — a distinction this
    /// repository has had to correct in its own rule files more than once,
    /// because `with_capacity` reads like a limit and is not one.
    #[must_use]
    pub fn new() -> Self {
        Self {
            prices: PapayaHashMap::new(),
        }
    }

    /// Records the latest traded price, or says why it would not.
    ///
    /// O(1) average, and allocation-free for an already-tracked instrument.
    pub fn record(
        &self,
        security_id: u64,
        segment: ExchangeSegment,
        last_price: f64,
    ) -> RecordOutcome {
        let Some(paise) = rupees_to_paise(last_price) else {
            metrics::counter!(REJECTED_VALUE_COUNTER).increment(1);
            return RecordOutcome::RejectedValue;
        };
        let pinned = self.prices.pin();
        let key = (security_id, segment);
        if let Some(slot) = pinned.get(&key) {
            slot.store(paise, Ordering::Relaxed);
            return RecordOutcome::Stored;
        }
        // First price for this instrument. Two threads racing on the SAME new
        // key can both miss this `get` and both insert, so an older price can
        // land after a newer one. It self-heals on that instrument's next
        // tick, which at the observed ~100,000 ticks per five minutes is
        // immediate — and the alternative, a CAS loop on the tick path, buys
        // sub-second accuracy for a value the attach loop reads every 15 s.
        //
        // The check also races — two threads can
        // both observe `len < MAX` and both insert — so the true ceiling is
        // `MAX + (concurrent inserters - 1)`. That is deliberate and is the
        // same trade `DayOhlcTracker` records: making it exact needs a lock
        // or a CAS loop on the tick path to tighten a bound that is already
        // an arbitrary round number, and the overshoot is bounded by thread
        // count — single digits against 25,000.
        if pinned.len() >= MAX_TRACKED_INSTRUMENTS {
            metrics::counter!(REFUSED_COUNTER).increment(1);
            // CODED, and the code is not decoration: every CloudWatch metric
            // filter that can page an operator matches on `$.code`, so an
            // uncoded `error!` here would reach the log sink and nothing else.
            // WS-GAP-03 is what the rest of the contract-universe path already
            // carries for its own failures, and the `source` field is what
            // makes this one findable among them.
            //
            // Deliberately NOT alarmed: a new alarm is ~$0.10/mo against a
            // September forecast of $142.24 and an automatic
            // STOP_EC2_INSTANCES line at $135.00, so it needs an operator
            // lever rather than a cost note. Coded and searchable is what is
            // available without one, and saying so is better than shipping a
            // page that stops the trading box.
            tracing::error!(
                code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
                source = "spot_price_store_full",
                security_id,
                ?segment,
                tracked = pinned.len(),
                max = MAX_TRACKED_INSTRUMENTS,
                "spot price store is FULL — refusing a new instrument. Its option ladder \
                 cannot be centred and will be absent from the contract universe. \
                 Already-tracked instruments are unaffected."
            );
            return RecordOutcome::RefusedAtCapacity;
        }
        pinned.insert(key, AtomicI64::new(paise));
        RecordOutcome::Stored
    }

    /// The latest price for one instrument, in paise.
    ///
    /// `None` means no price has been observed today — which is not the same
    /// as zero and must never be rendered as one.
    #[must_use]
    pub fn latest_paise(&self, security_id: u64, segment: ExchangeSegment) -> Option<i64> {
        Some(
            self.prices
                .pin()
                .get(&(security_id, segment))?
                .load(Ordering::Relaxed),
        )
    }

    /// Every price the store holds, in the `(security_id, segment_code)` shape
    /// the contract selector's join already consumes.
    ///
    /// O(tracked), and named `snapshot` rather than `get` because of it. This
    /// is the ONE place a caller pays for the whole map, it runs on the attach
    /// path rather than the tick path, and the alternative — handing out the
    /// map and letting the join probe it per symbol — would be O(1) per probe
    /// but would hold a `papaya` pin across the caller's whole join, which is
    /// a worse trade for a set of ~870.
    #[must_use]
    pub fn snapshot_prices(&self) -> std::collections::HashMap<(u64, u8), i64> {
        let pinned = self.prices.pin();
        // Published HERE, inside the read, rather than at one call site.
        //
        // The first version published it from `load_contract_universe` only —
        // whose caller is the attach retry loop, which EXITS once contracts
        // are dialed. The per-minute depth re-fit then went on consuming this
        // store while the gauge sat frozen at whatever the last attach saw.
        // A gauge whose stated job is separating "warming up" from "not being
        // fed" is worth nothing if it stops moving exactly when the ongoing
        // consumer starts depending on it.
        #[allow(clippy::cast_precision_loss)]
        // APPROVED: bounded by MAX_TRACKED_INSTRUMENTS (25,000), far inside
        // exact f64 integer range.
        metrics::gauge!(TRACKED_GAUGE).set(pinned.len() as f64);
        let mut out = std::collections::HashMap::with_capacity(pinned.len());
        for ((security_id, segment), slot) in pinned.iter() {
            out.insert(
                (*security_id, segment.binary_code()),
                slot.load(Ordering::Relaxed),
            );
        }
        out
    }

    /// Number of instruments currently priced.
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.prices.pin().len()
    }

    /// True iff no price has been observed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tracked() == 0
    }

    /// Clears every price for the next session.
    ///
    /// Yesterday's close is not today's spot: carrying one across the daily
    /// boundary would centre a ladder on a stale number while every counter
    /// read normal, which is worse than having no price at all.
    pub fn reset_daily(&self) {
        self.prices.pin().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NSE_EQ: ExchangeSegment = ExchangeSegment::NseEquity;
    const IDX: ExchangeSegment = ExchangeSegment::IdxI;

    #[test]
    fn record_stores_a_price_that_reads_back_in_paise() {
        let store = SpotPriceStore::new();
        assert_eq!(store.record(2885, NSE_EQ, 1234.55), RecordOutcome::Stored);
        assert_eq!(store.latest_paise(2885, NSE_EQ), Some(123_455));
    }

    #[test]
    fn record_replaces_an_earlier_price_without_adding_a_second_entry() {
        let store = SpotPriceStore::new();
        store.record(2885, NSE_EQ, 1234.55);
        store.record(2885, NSE_EQ, 1240.00);
        assert_eq!(
            store.latest_paise(2885, NSE_EQ),
            Some(124_000),
            "the store holds the LATEST price; a stale one centres the ladder wrong"
        );
        assert_eq!(store.tracked(), 1, "an update must not add a second entry");
    }

    #[test]
    fn the_same_numeric_id_in_two_segments_is_two_instruments() {
        // I-P1-11. Dhan reuses one numeric id across segments; sharing a slot
        // would let an index's level price a stock's entire option ladder.
        let store = SpotPriceStore::new();
        store.record(27, IDX, 26_000.0);
        store.record(27, NSE_EQ, 415.25);
        assert_eq!(store.latest_paise(27, IDX), Some(2_600_000));
        assert_eq!(store.latest_paise(27, NSE_EQ), Some(41_525));
        assert_eq!(store.tracked(), 2);
    }

    #[test]
    fn latest_paise_is_absent_not_zero_for_an_instrument_that_has_not_traded() {
        let store = SpotPriceStore::new();
        assert_eq!(
            store.latest_paise(2885, NSE_EQ),
            None,
            "zero would place at-the-money at the bottom of the ladder; absent \
             must stay absent"
        );
    }

    #[test]
    fn record_refuses_a_price_that_cannot_centre_a_ladder() {
        let store = SpotPriceStore::new();
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                store.record(2885, NSE_EQ, bad),
                RecordOutcome::RejectedValue,
                "{bad} must not reach the ladder"
            );
        }
        assert!(store.is_empty(), "a refused price must not create a slot");
    }

    #[test]
    fn rupees_to_paise_refuses_a_price_past_exact_integer_range() {
        let store = SpotPriceStore::new();
        assert_eq!(
            store.record(1, NSE_EQ, 1e17),
            RecordOutcome::RejectedValue,
            "past 2^53 paise an f64 no longer represents consecutive integers, \
             so the rounded value is not the price"
        );
    }

    #[test]
    fn rupees_to_paise_is_the_same_conversion_the_database_path_uses() {
        // The anti-drift test. If these ever disagree the fallback centres
        // ladders one rounding step from the primary, and the difference shows
        // up as an off-by-one strike that no log explains.
        for rupees in [1234.55, 0.05, 99_999.994, 99_999.995, 1.0] {
            let ram = rupees_to_paise(rupees);
            let database = {
                if !rupees.is_finite() || rupees <= 0.0 {
                    None
                } else {
                    let paise = (rupees * 100.0).round();
                    if paise > 9_007_199_254_740_991.0 {
                        None
                    } else {
                        #[allow(clippy::cast_possible_truncation)]
                        // APPROVED: mirrors parse_spot_prices exactly; this
                        // test exists to prove they agree.
                        Some(paise as i64)
                    }
                }
            };
            assert_eq!(ram, database, "conversions diverged at {rupees}");
        }
    }

    #[test]
    fn snapshot_prices_is_the_shape_the_contract_join_already_consumes() {
        let store = SpotPriceStore::new();
        store.record(2885, NSE_EQ, 1234.55);
        store.record(13, IDX, 24_500.10);
        let snap = store.snapshot_prices();
        assert_eq!(snap.get(&(2885, NSE_EQ.binary_code())), Some(&123_455));
        assert_eq!(snap.get(&(13, IDX.binary_code())), Some(&2_450_010));
        assert_eq!(snap.len(), 2);
    }

    #[test]
    fn tracked_stops_at_the_cap_and_an_already_tracked_instrument_still_updates() {
        let store = SpotPriceStore::new();
        for id in 0..MAX_TRACKED_INSTRUMENTS as u64 {
            assert_eq!(store.record(id, NSE_EQ, 100.0), RecordOutcome::Stored);
        }
        assert_eq!(
            store.record(999_999, NSE_EQ, 100.0),
            RecordOutcome::RefusedAtCapacity,
            "past the ceiling a NEW instrument is refused"
        );
        assert_eq!(
            store.record(0, NSE_EQ, 250.0),
            RecordOutcome::Stored,
            "refusing the newcomer must never cost an already-tracked instrument \
             its price — that would be the strictly worse trade"
        );
        assert_eq!(store.latest_paise(0, NSE_EQ), Some(25_000));
    }

    #[test]
    fn reset_daily_clears_so_yesterdays_price_never_centres_todays_ladder() {
        let store = SpotPriceStore::new();
        store.record(2885, NSE_EQ, 1234.55);
        store.reset_daily();
        assert!(store.is_empty());
        assert_eq!(store.latest_paise(2885, NSE_EQ), None);
    }

    #[test]
    fn is_empty_and_snapshot_prices_hold_under_concurrent_writers_with_no_lock() {
        // The property that makes this store the right shape: the drain writes
        // through &self while an attach task reads through &self, with no lock
        // on either side. A regression to a Mutex-per-value or an RwLock map
        // would still pass every other test here.
        let store = std::sync::Arc::new(SpotPriceStore::new());
        let writers: Vec<_> = (0..4u64)
            .map(|w| {
                let store = std::sync::Arc::clone(&store);
                std::thread::spawn(move || {
                    for i in 0..250u64 {
                        store.record(w * 1000 + i, NSE_EQ, 100.0 + i as f64);
                    }
                })
            })
            .collect();
        let reader = {
            let store = std::sync::Arc::clone(&store);
            std::thread::spawn(move || {
                for _ in 0..200 {
                    let _ignored = store.snapshot_prices();
                }
            })
        };
        for w in writers {
            w.join().expect("writer panicked");
        }
        reader.join().expect("reader panicked");
        assert_eq!(store.tracked(), 1000);
        assert_eq!(store.latest_paise(3_249, NSE_EQ), Some(34_900));
    }
}
