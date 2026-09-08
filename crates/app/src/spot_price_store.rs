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
//! ## What one slot holds, and why it is ONE atomic
//!
//! Each instrument owns a single `AtomicU64` packing the exchange's last
//! trade time (upper 32 bits, epoch seconds) and the raw `f32` bits of the
//! last traded price (lower 32 bits). One word, so a reader can never observe
//! a price from one tick beside a time from another — the torn-pair hazard
//! two separate atomics would carry — and so the write is one CAS rather than
//! a lock.
//!
//! ### Last EXCHANGE time wins, not last ARRIVAL
//!
//! ⚠ The first draft of this file (same day) stored last-arrival-wins, and a
//! hostile review of it found the defect within hours. The write-ahead log's
//! catch-up drain refolds DEFERRED segments through the same frame walk
//! while the live socket keeps delivering, so an old frame can be folded
//! AFTER a newer one; and Dhan itself sends a dormant instrument's snapshot
//! carrying whatever trade time it last printed at. Under last-arrival-wins
//! either of those overwrote a fresh price with an older one — and because
//! the contract selector lets RAM WIN over the database on every overlap, the
//! older price then centred that stock's entire option ladder. The database
//! path never had this problem: it is `LATEST ON ts`, ordered by the exchange
//! clock. This store now is too. A tick whose trade time is OLDER than the
//! one held is refused as [`RecordOutcome::OlderThanHeld`]; an equal time
//! (two ticks inside one second) takes the later arrival, which is the
//! later trade.
//!
//! ### The trading-day gate
//!
//! The database query bounds itself to `ts >= today`, and its own test says
//! why: an unbounded `LATEST ON` "returns yesterday's close and prices
//! at-the-money against it silently". Dhan sends the LAST TRADE TIME, so an
//! instrument that has not traded today arrives with yesterday's — measured
//! mean ~5 hours old, max 34 days (the aggregator's `stale_trading_day` gate
//! records the figures). RAM applies the SAME bound: a trade time from before
//! today's IST midnight is refused as [`RecordOutcome::StaleTradingDay`] and
//! never lands. The two sources therefore disagree only on freshness, never
//! on which day they describe — which is the whole point of overlaying them.
//!
//! The day is recomputed by [`SpotPriceStore::reset_daily`], which the drain
//! calls at its own IST-midnight rollover beside the leaderboard and
//! previous-close resets. Before the first reset the floor is the IST day of
//! construction.
//!
//! ## Why the raw `f32` bits and not paise
//!
//! The drain already widens the same `last_traded_price` for persistence
//! (`tick_persistence`); widening it again here, on every spot tick, would
//! be a second ryu round-trip on the hot path. The bits are stored as
//! decoded and the conversion — the house `f32_to_f64_clean` widener followed
//! by [`rupees_to_paise`] — runs on the READ side, which is ~870 probes a
//! minute against ~100,000 writes per five minutes. The conversion function
//! is shared with the database parser so the two sources can never drift by
//! a rounding step.
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
//! ## Ordering
//!
//! `Relaxed` everywhere. The only invariant is that a reader observes SOME
//! (time, price) pair this instrument actually printed — never a torn or
//! invented one, which the single aligned 64-bit word guarantees on its own.
//! There is no second location whose visibility must be ordered against this
//! one. (Every `record` already pays a `papaya` pin — a thread-local lookup
//! plus a light barrier — so `Relaxed` is correct here for the reason above,
//! not as a saving; the first draft claimed the saving and a review corrected
//! it.)
//!
//! ## Nothing on the tick path touches the metrics registry
//!
//! The refusal counters are plain atomics on this struct, folded into the
//! registry by [`SpotPriceStore::publish_metrics`], which the read side calls
//! once a minute. `metrics::counter!` on a per-tick arm is the `record_ws_lag`
//! class this repository has already removed once, and the refused population
//! is not small: every exact-`0.0` pre-open sentinel print lands on the
//! `RejectedValue` arm.
//!
//! ## Complexity
//!
//! `record` is **O(1) average** — one hash probe on the I-P1-11 composite key
//! plus one CAS, with a bounded retry only when a concurrent writer moved the
//! same slot (in production there is exactly one writer, the drain, so the
//! CAS succeeds first time). Zero allocation for an already-tracked
//! instrument, which is every tick after the first. `latest_paise` is one
//! probe plus one load plus the conversion. `snapshot_prices` is
//! **O(tracked)** and says so: it runs on the contract attach path (once per
//! retry) and on the depth re-fit (**once a minute, all session** — the first
//! draft said "at most once per retry" and was stale on arrival), never on
//! the tick path. Space is O(instruments), hard-bounded by the cap below.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use papaya::HashMap as PapayaHashMap;
use tickvault_common::constants::{IST_UTC_OFFSET_SECONDS_I64, SECONDS_PER_DAY};
use tickvault_common::price_precision::f32_to_f64_clean;
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

/// Counter for a tick refused because its exchange trade time is from before
/// today's IST midnight — a dormant instrument's stale snapshot.
pub const STALE_DAY_COUNTER: &str = "tv_spot_price_store_stale_day_total";

/// Counter for a tick refused because a NEWER exchange trade time was already
/// held — a replayed or re-ordered frame arriving after a fresher one.
pub const OLDER_THAN_HELD_COUNTER: &str = "tv_spot_price_store_older_than_held_total";

/// Gauge: how many instruments the store currently prices.
///
/// The number that separates "the RAM path is warming up" from "the RAM path
/// is not being fed", which no other signal distinguishes — both render as an
/// empty contract universe.
pub const TRACKED_GAUGE: &str = "tv_spot_price_store_tracked";

/// What `record` did with the offered tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    /// Stored — a new instrument, or a fresher trade for a tracked one.
    Stored,
    /// Not finite or not positive. Refused before it could place a ladder.
    RejectedValue,
    /// The exchange trade time is from before today's IST midnight. The
    /// database path bounds itself to today for the same reason; RAM must
    /// not be the source that silently centres a ladder on yesterday.
    StaleTradingDay,
    /// A tick whose trade time is OLDER than the one already held. Replayed
    /// and re-ordered frames land here; the fresher price is kept.
    OlderThanHeld,
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
    // The guard above is on RUPEES; this one is on PAISE, and they are not the
    // same test. Any price in (0, 0.005) rounds to ZERO paise — a sub-subnormal
    // f32, or a mis-framed packet whose bytes happen to decode small — and a
    // zero is not a small price, it is the ABSENCE of one.
    //
    // The overlay does `prices.extend(ram)`, so a zero here would OVERWRITE a
    // good database price and the underlying's entire ladder would disappear
    // into `underlyings_without_spot`. Both consumers guard `spot <= 0`, so
    // the failure is a silent absence rather than a wrong strike — the
    // harmless direction, and still not one to hand them.
    if paise <= 0.0 {
        return None;
    }
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

/// The IST day number of an exchange trade time (epoch seconds, UTC).
///
/// The same arithmetic the drain's midnight rollover uses
/// (`ist_day_number_now`): shift into IST, floor-divide by a day. Pure so the
/// boundary — 23:59:59 IST vs 00:00:00 IST — is a unit test.
#[must_use]
pub const fn ist_day_of(exchange_secs: u32) -> i64 {
    // APPROVED: `From` is not const-stable; a u32 -> i64 widening is lossless
    // by construction, and the test fixtures need this at compile time.
    (exchange_secs as i64)
        .saturating_add(IST_UTC_OFFSET_SECONDS_I64)
        .div_euclid(SECONDS_PER_DAY_I64)
}

// APPROVED: the same lossless widening, hoisted once so the two day-number
// helpers cannot disagree about the divisor.
const SECONDS_PER_DAY_I64: i64 = SECONDS_PER_DAY as i64;

/// Today's IST day number, from the wall clock.
fn ist_day_now() -> i64 {
    chrono::Utc::now()
        .timestamp()
        .saturating_add(IST_UTC_OFFSET_SECONDS_I64)
        .div_euclid(i64::from(SECONDS_PER_DAY))
}

/// Packs (exchange seconds, price bits) into the one word a slot holds.
#[inline]
const fn pack(exchange_secs: u32, price_bits: u32) -> u64 {
    ((exchange_secs as u64) << 32) | (price_bits as u64)
}

/// The exchange seconds half of a packed slot.
#[inline]
const fn packed_secs(word: u64) -> u32 {
    #[allow(clippy::cast_possible_truncation)]
    // APPROVED: the shift leaves exactly the upper 32 bits.
    let secs = (word >> 32) as u32;
    secs
}

/// The price half of a packed slot, as the `f32` the wire carried.
#[inline]
fn packed_price(word: u64) -> f32 {
    #[allow(clippy::cast_possible_truncation)]
    // APPROVED: the mask leaves exactly the lower 32 bits.
    let bits = (word & 0xFFFF_FFFF) as u32;
    f32::from_bits(bits)
}

/// Latest traded price per instrument for today.
///
/// Written by the drain on the tick path, read by the attach tasks. Both go
/// through `&self`; there is no lock on either side.
#[derive(Debug)]
pub struct SpotPriceStore {
    /// `(exchange_secs << 32) | f32 bits` per instrument — see the header.
    prices: PapayaHashMap<SpotPriceKey, AtomicU64>,
    /// The IST day a trade time must belong to. Ticks from an earlier day are
    /// refused. Set at construction, advanced by [`Self::reset_daily`].
    trading_day: AtomicI64,
    /// Refusal tallies, folded into the metrics registry off the tick path.
    rejected_value: AtomicU64,
    stale_day: AtomicU64,
    older_than_held: AtomicU64,
    refused_capacity: AtomicU64,
}

impl Default for SpotPriceStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SpotPriceStore {
    /// An empty store whose trading day is today (IST).
    ///
    /// Deliberately NOT pre-sized to the cap: `papaya` grows by resizing, and
    /// reserving 25,000 slots for a set that is ~870 in practice would commit
    /// memory the universe never uses. The bound is the explicit check in
    /// [`Self::record`], never an allocation figure — a distinction this
    /// repository has had to correct in its own rule files more than once,
    /// because `with_capacity` reads like a limit and is not one.
    #[must_use]
    pub fn new() -> Self {
        Self::for_trading_day(ist_day_now())
    }

    /// An empty store gated to the given IST day number.
    ///
    /// Production uses [`Self::new`]; this exists so a test can feed ticks
    /// stamped on a fixed historical day without the day gate refusing them.
    #[must_use]
    pub fn for_trading_day(ist_day: i64) -> Self {
        Self {
            prices: PapayaHashMap::new(),
            trading_day: AtomicI64::new(ist_day),
            rejected_value: AtomicU64::new(0),
            stale_day: AtomicU64::new(0),
            older_than_held: AtomicU64::new(0),
            refused_capacity: AtomicU64::new(0),
        }
    }

    /// The IST day number ticks must belong to.
    #[must_use]
    pub fn trading_day(&self) -> i64 {
        self.trading_day.load(Ordering::Relaxed)
    }

    /// Records the tick's last traded price, or says why it would not.
    ///
    /// `exchange_secs` is the exchange's last-trade time in epoch seconds —
    /// `ParsedTick::exchange_timestamp` as decoded, never the receipt clock.
    /// O(1) average, and allocation-free for an already-tracked instrument.
    pub fn record(
        &self,
        security_id: u64,
        segment: ExchangeSegment,
        last_price: f32,
        exchange_secs: u32,
    ) -> RecordOutcome {
        // Cheap f32 compares first, before the day arithmetic or the probe.
        // `is_finite()` FIRST: it refuses NaN and both infinities, which is
        // what lets the `<= 0.0` after it be an ordinary comparison — NaN
        // never reaches it. (`!(x > 0.0)` would also refuse NaN, but clippy
        // reads a negated partial-order compare as a refactor hazard.)
        if !last_price.is_finite() || last_price <= 0.0 {
            self.rejected_value.fetch_add(1, Ordering::Relaxed);
            return RecordOutcome::RejectedValue;
        }
        if ist_day_of(exchange_secs) < self.trading_day.load(Ordering::Relaxed) {
            self.stale_day.fetch_add(1, Ordering::Relaxed);
            return RecordOutcome::StaleTradingDay;
        }
        let word = pack(exchange_secs, last_price.to_bits());
        let pinned = self.prices.pin();
        let key = (security_id, segment);
        if let Some(slot) = pinned.get(&key) {
            let mut held = slot.load(Ordering::Relaxed);
            loop {
                if packed_secs(held) > exchange_secs {
                    self.older_than_held.fetch_add(1, Ordering::Relaxed);
                    return RecordOutcome::OlderThanHeld;
                }
                match slot.compare_exchange_weak(held, word, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => return RecordOutcome::Stored,
                    // Another writer moved the slot between our load and our
                    // CAS. Re-read and re-judge against what it wrote; in
                    // production there is exactly one writer, so this arm is
                    // a spurious-failure retry and nothing more.
                    Err(now) => held = now,
                }
            }
        }
        // First price for this instrument. Two threads racing on the SAME new
        // key can both miss this `get` and both insert, so an older price can
        // land after a newer one. It self-heals on that instrument's next
        // tick, which at the observed ~100,000 ticks per five minutes is
        // immediate — and the alternative, a CAS loop on the INSERT, buys
        // sub-second accuracy for a value the attach loop reads every 15 s.
        //
        // The cap check also races — two threads can both observe
        // `len < MAX` and both insert — so the true ceiling is
        // `MAX + (concurrent inserters - 1)`. That is deliberate and is the
        // same trade `DayOhlcTracker` records: making it exact needs a lock
        // on the tick path to tighten a bound that is already an arbitrary
        // round number, and the overshoot is bounded by thread count —
        // single digits against 25,000.
        if pinned.len() >= MAX_TRACKED_INSTRUMENTS {
            let n = self
                .refused_capacity
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            // Throttled to powers of two, the `TickGapTracker` /
            // `DayOhlcTracker` discipline. Unthrottled, a widened segment
            // filter or a garbage id stream would put a synchronous
            // formatting call on the drain for every one of ~100,000 ticks
            // per five minutes — the flood this line exists to warn about,
            // caused by the warning.
            //
            // CODED, and the code is not decoration: every CloudWatch metric
            // filter that can page an operator matches on `$.code`, so an
            // uncoded `error!` here would reach the log sink and nothing
            // else. WS-GAP-03 is what the rest of the contract-universe path
            // already carries for its own failures, and the `source` field is
            // what makes this one findable among them.
            //
            // Deliberately NOT alarmed: a new alarm is ~$0.10/mo against a
            // September forecast of $142.24 and an automatic
            // STOP_EC2_INSTANCES line at $135.00, so it needs an operator
            // lever rather than a cost note.
            if n.is_power_of_two() {
                tracing::error!(
                    code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
                    source = "spot_price_store_full",
                    security_id,
                    ?segment,
                    tracked = pinned.len(),
                    max = MAX_TRACKED_INSTRUMENTS,
                    refused_so_far = n,
                    "spot price store is FULL — refusing a new instrument. Its option ladder \
                     cannot be centred and will be absent from the contract universe. \
                     Already-tracked instruments are unaffected. Logged at powers of two."
                );
            }
            return RecordOutcome::RefusedAtCapacity;
        }
        pinned.insert(key, AtomicU64::new(word));
        RecordOutcome::Stored
    }

    /// The latest price for one instrument, in paise.
    ///
    /// `None` means no price has been observed today — which is not the same
    /// as zero and must never be rendered as one.
    #[must_use]
    pub fn latest_paise(&self, security_id: u64, segment: ExchangeSegment) -> Option<i64> {
        let word = self
            .prices
            .pin()
            .get(&(security_id, segment))?
            .load(Ordering::Relaxed);
        rupees_to_paise(f32_to_f64_clean(packed_price(word)))
    }

    /// The exchange trade time (epoch seconds) of the price held for one
    /// instrument, or `None` if none is held.
    #[must_use]
    pub fn latest_exchange_secs(&self, security_id: u64, segment: ExchangeSegment) -> Option<u32> {
        Some(packed_secs(
            self.prices
                .pin()
                .get(&(security_id, segment))?
                .load(Ordering::Relaxed),
        ))
    }

    /// Every price the store holds, in the `(security_id, segment_code)` shape
    /// the contract selector's join already consumes.
    ///
    /// O(tracked), and named `snapshot` rather than `get` because of it. This
    /// is the ONE place a caller pays for the whole map. It runs on the attach
    /// path once per retry and on the depth re-fit once a minute — never on
    /// the tick path — and the alternative, handing out the map and letting
    /// the join probe it per symbol, would hold a `papaya` pin across the
    /// caller's whole join, which is a worse trade for a set of ~870.
    ///
    /// Also the place the store's metrics reach the registry: a per-minute
    /// reader is the right cadence for that, and the tick path is not.
    #[must_use]
    pub fn snapshot_prices(&self) -> std::collections::HashMap<(u64, u8), i64> {
        self.publish_metrics();
        let pinned = self.prices.pin();
        let mut out = std::collections::HashMap::with_capacity(pinned.len());
        for ((security_id, segment), slot) in pinned.iter() {
            let word = slot.load(Ordering::Relaxed);
            // A held price that no longer converts — impossible by
            // construction, since `record` refused everything non-positive —
            // is skipped rather than rendered as zero, for the reason the
            // header gives: zero is the absence of a price.
            if let Some(paise) = rupees_to_paise(f32_to_f64_clean(packed_price(word))) {
                out.insert((*security_id, segment.binary_code()), paise);
            }
        }
        out
    }

    /// Folds the tick-path tallies into the metrics registry.
    ///
    /// Counters are published as ABSOLUTE values via `absolute`, so a reader
    /// that calls this more often than the tallies move re-states the same
    /// number rather than double-counting.
    pub fn publish_metrics(&self) {
        #[allow(clippy::cast_precision_loss)]
        // APPROVED: bounded by MAX_TRACKED_INSTRUMENTS (25,000), far inside
        // exact f64 integer range.
        metrics::gauge!(TRACKED_GAUGE).set(self.tracked() as f64);
        metrics::counter!(REJECTED_VALUE_COUNTER)
            .absolute(self.rejected_value.load(Ordering::Relaxed));
        metrics::counter!(STALE_DAY_COUNTER).absolute(self.stale_day.load(Ordering::Relaxed));
        metrics::counter!(OLDER_THAN_HELD_COUNTER)
            .absolute(self.older_than_held.load(Ordering::Relaxed));
        metrics::counter!(REFUSED_COUNTER).absolute(self.refused_capacity.load(Ordering::Relaxed));
    }

    /// Refusals so far, as `(rejected_value, stale_day, older_than_held,
    /// refused_capacity)`. For tests and the periodic drain log line.
    #[must_use]
    pub fn refusals(&self) -> (u64, u64, u64, u64) {
        (
            self.rejected_value.load(Ordering::Relaxed),
            self.stale_day.load(Ordering::Relaxed),
            self.older_than_held.load(Ordering::Relaxed),
            self.refused_capacity.load(Ordering::Relaxed),
        )
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

    /// Clears every price and moves the day gate to today for the next
    /// session.
    ///
    /// Yesterday's close is not today's spot: carrying one across the daily
    /// boundary would centre a ladder on a stale number while every counter
    /// read normal, which is worse than having no price at all. The gate
    /// moves in the same call so a tick from the day just ended — which the
    /// old gate admitted — is refused from here on.
    pub fn reset_daily(&self) {
        self.reset_for_trading_day(ist_day_now());
    }

    /// [`Self::reset_daily`] with an explicit day, for tests.
    pub fn reset_for_trading_day(&self, ist_day: i64) {
        self.prices.pin().clear();
        self.trading_day.store(ist_day, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NSE_EQ: ExchangeSegment = ExchangeSegment::NseEquity;
    const IDX: ExchangeSegment = ExchangeSegment::IdxI;

    /// 2026-08-14 10:00:00 UTC — a fixed in-session trade time.
    const T0: u32 = 1_755_165_600;
    const DAY: i64 = ist_day_of(T0);

    const _: () = assert!(DAY == 20_314, "2026-08-14 IST");

    fn store() -> SpotPriceStore {
        SpotPriceStore::for_trading_day(DAY)
    }

    #[test]
    fn record_stores_a_price_that_reads_back_in_paise() {
        let s = store();
        assert_eq!(s.record(2885, NSE_EQ, 1234.55, T0), RecordOutcome::Stored);
        assert_eq!(s.latest_paise(2885, NSE_EQ), Some(123_455));
        assert_eq!(s.latest_exchange_secs(2885, NSE_EQ), Some(T0));
    }

    #[test]
    fn a_later_trade_replaces_an_earlier_one_without_adding_a_second_entry() {
        let s = store();
        s.record(2885, NSE_EQ, 1234.55, T0);
        s.record(2885, NSE_EQ, 1240.00, T0 + 1);
        assert_eq!(
            s.latest_paise(2885, NSE_EQ),
            Some(124_000),
            "the store holds the LATEST trade; a stale one centres the ladder wrong"
        );
        assert_eq!(s.tracked(), 1, "an update must not add a second entry");
    }

    /// THE defect the first draft carried. The WAL catch-up drain refolds
    /// deferred segments through the same frame walk while the live socket
    /// keeps delivering, so an OLD frame can arrive AFTER a newer one — and
    /// RAM wins over the database on every overlap.
    #[test]
    fn an_older_trade_time_never_overwrites_a_fresher_price() {
        let s = store();
        assert_eq!(
            s.record(2885, NSE_EQ, 1240.00, T0 + 60),
            RecordOutcome::Stored
        );
        assert_eq!(
            s.record(2885, NSE_EQ, 1234.55, T0),
            RecordOutcome::OlderThanHeld,
            "a replayed frame from a minute ago must not replace the live price"
        );
        assert_eq!(s.latest_paise(2885, NSE_EQ), Some(124_000));
        assert_eq!(s.refusals().2, 1);
    }

    #[test]
    fn two_trades_in_the_same_second_take_the_later_arrival() {
        let s = store();
        s.record(2885, NSE_EQ, 1234.55, T0);
        assert_eq!(s.record(2885, NSE_EQ, 1234.60, T0), RecordOutcome::Stored);
        assert_eq!(
            s.latest_paise(2885, NSE_EQ),
            Some(123_460),
            "equal exchange seconds: the later arrival is the later trade"
        );
    }

    /// The database path bounds itself to today; RAM must too, or an
    /// instrument that has not traded today centres its ladder on yesterday.
    #[test]
    fn a_trade_time_from_before_today_is_refused_not_stored() {
        let s = store();
        let yesterday = T0 - u32::try_from(SECONDS_PER_DAY).unwrap_or(86_400);
        assert_eq!(
            s.record(2885, NSE_EQ, 1234.55, yesterday),
            RecordOutcome::StaleTradingDay
        );
        assert_eq!(s.latest_paise(2885, NSE_EQ), None);
        assert!(s.is_empty(), "a refused tick must not create a slot");
        assert_eq!(s.refusals().1, 1);
    }

    #[test]
    fn the_day_gate_admits_the_first_second_of_today_and_refuses_the_last_of_yesterday() {
        // IST midnight of DAY, as UTC epoch seconds.
        let midnight_ist_utc =
            u32::try_from(DAY * i64::from(SECONDS_PER_DAY) - IST_UTC_OFFSET_SECONDS_I64)
                .unwrap_or(0);
        let s = store();
        assert_eq!(
            s.record(1, NSE_EQ, 100.0, midnight_ist_utc - 1),
            RecordOutcome::StaleTradingDay,
            "23:59:59 IST yesterday"
        );
        assert_eq!(
            s.record(1, NSE_EQ, 100.0, midnight_ist_utc),
            RecordOutcome::Stored,
            "00:00:00 IST today"
        );
    }

    #[test]
    fn the_same_numeric_id_in_two_segments_is_two_instruments() {
        // I-P1-11. Dhan reuses one numeric id across segments; sharing a slot
        // would let an index's level price a stock's entire option ladder.
        let s = store();
        s.record(27, IDX, 26_000.0, T0);
        s.record(27, NSE_EQ, 415.25, T0);
        assert_eq!(s.latest_paise(27, IDX), Some(2_600_000));
        assert_eq!(s.latest_paise(27, NSE_EQ), Some(41_525));
        assert_eq!(s.tracked(), 2);
    }

    #[test]
    fn latest_paise_is_absent_not_zero_for_an_instrument_that_has_not_traded() {
        let s = store();
        assert_eq!(
            s.latest_paise(2885, NSE_EQ),
            None,
            "zero would place at-the-money at the bottom of the ladder; absent \
             must stay absent"
        );
    }

    #[test]
    fn record_refuses_a_price_that_cannot_centre_a_ladder() {
        let s = store();
        for bad in [0.0_f32, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                s.record(2885, NSE_EQ, bad, T0),
                RecordOutcome::RejectedValue,
                "{bad} must not reach the ladder"
            );
        }
        assert!(s.is_empty(), "a refused price must not create a slot");
        assert_eq!(s.refusals().0, 5);
    }

    /// A price too small to be ONE PAISE is an absence, not a small price —
    /// and the overlay lets RAM overwrite a good database price. The `f32`
    /// is admitted by `record` (it is positive and finite) and refused on the
    /// READ side by the shared conversion, so it can never reach a ladder.
    #[test]
    fn a_price_too_small_to_be_one_paise_never_reaches_a_reader() {
        let s = store();
        s.record(1, NSE_EQ, 0.004, T0);
        assert_eq!(s.latest_paise(1, NSE_EQ), None);
        assert!(
            s.snapshot_prices().is_empty(),
            "the snapshot must skip it rather than render zero"
        );
        // The smallest value that IS a paise still lands, so the guard has not
        // quietly become a floor on real prices. 0.005 rounds to 1.
        s.record(2, NSE_EQ, 0.005, T0);
        assert_eq!(s.latest_paise(2, NSE_EQ), Some(1));
    }

    #[test]
    fn rupees_to_paise_is_the_same_conversion_the_database_path_uses() {
        // The anti-drift test. If these ever disagree the fallback centres
        // ladders one rounding step from the primary, and the difference shows
        // up as an off-by-one strike that no log explains.
        for rupees in [1234.55, 0.05, 99_999.994, 99_999.995, 1.0, 0.004, 1e17] {
            let ram = rupees_to_paise(rupees);
            let database = {
                if !rupees.is_finite() || rupees <= 0.0 {
                    None
                } else {
                    let paise = (rupees * 100.0).round();
                    if paise <= 0.0 || paise > 9_007_199_254_740_991.0 {
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
    fn the_read_side_widens_through_the_house_cleaner_not_a_plain_cast() {
        // STORAGE-GAP-02: `f64::from(10.20_f32)` is 10.19999980926514, which
        // rounds to 1020 paise either way — so the fixture is a price where
        // the naive widening lands on the wrong side of a half-paise. Found by
        // exhaustive search over every 2-decimal price up to 2 lakh: 131072.1
        // (an MRF-class share price) widens naively to 131072.09375 →
        // 13107209 paise, while the shortest-decimal cleaner gives 13107210.
        let s = store();
        s.record(1, NSE_EQ, 131_072.1, T0);
        assert_eq!(
            s.latest_paise(1, NSE_EQ),
            Some(13_107_210),
            "131072.1 is 13107210 paise through the clean widener; the plain \
             cast gives 131072.09375 and 13107209"
        );
    }

    #[test]
    fn snapshot_prices_is_the_shape_the_contract_join_already_consumes() {
        let s = store();
        s.record(2885, NSE_EQ, 1234.55, T0);
        s.record(13, IDX, 24_500.10, T0);
        let snap = s.snapshot_prices();
        assert_eq!(snap.get(&(2885, NSE_EQ.binary_code())), Some(&123_455));
        assert_eq!(snap.get(&(13, IDX.binary_code())), Some(&2_450_010));
        assert_eq!(snap.len(), 2);
    }

    #[test]
    fn tracked_stops_at_the_cap_and_an_already_tracked_instrument_still_updates() {
        let s = store();
        for id in 0..MAX_TRACKED_INSTRUMENTS as u64 {
            assert_eq!(s.record(id, NSE_EQ, 100.0, T0), RecordOutcome::Stored);
        }
        assert_eq!(
            s.record(999_999, NSE_EQ, 100.0, T0),
            RecordOutcome::RefusedAtCapacity,
            "past the ceiling a NEW instrument is refused"
        );
        assert_eq!(
            s.record(0, NSE_EQ, 250.0, T0 + 1),
            RecordOutcome::Stored,
            "refusing the newcomer must never cost an already-tracked instrument \
             its price — that would be the strictly worse trade"
        );
        assert_eq!(s.latest_paise(0, NSE_EQ), Some(25_000));
        assert_eq!(s.refusals().3, 1);
    }

    #[test]
    fn reset_clears_prices_and_moves_the_day_gate_forward() {
        let s = store();
        s.record(2885, NSE_EQ, 1234.55, T0);
        s.reset_for_trading_day(DAY + 1);
        assert!(s.is_empty());
        assert_eq!(s.latest_paise(2885, NSE_EQ), None);
        assert_eq!(
            s.record(2885, NSE_EQ, 1234.55, T0),
            RecordOutcome::StaleTradingDay,
            "a tick from the day that just ended must be refused after the rollover"
        );
    }

    #[test]
    fn reset_daily_gates_to_the_wall_clock_day() {
        let s = SpotPriceStore::new();
        assert_eq!(s.trading_day(), ist_day_now());
        s.reset_daily();
        assert_eq!(s.trading_day(), ist_day_now());
    }

    #[test]
    fn ist_day_of_matches_the_drains_rollover_arithmetic() {
        // 2026-08-13 18:30:00 UTC == 2026-08-14 00:00:00 IST: the first second
        // of a new IST day.
        assert_eq!(ist_day_of(1_755_109_800), 20_314);
        assert_eq!(ist_day_of(1_755_109_799), 20_313);
        assert_eq!(ist_day_of(0), 0);
    }

    #[test]
    fn pack_and_unpack_round_trip_every_bit() {
        for (secs, price) in [
            (0_u32, 0.0_f32),
            (u32::MAX, f32::MAX),
            (T0, 1234.55),
            (7, -0.0),
        ] {
            let w = pack(secs, price.to_bits());
            assert_eq!(packed_secs(w), secs);
            assert_eq!(packed_price(w).to_bits(), price.to_bits());
        }
    }

    #[test]
    fn publish_metrics_restates_absolute_tallies_without_double_counting() {
        let s = store();
        s.record(1, NSE_EQ, -1.0, T0);
        s.record(1, NSE_EQ, -1.0, T0);
        s.publish_metrics();
        s.publish_metrics();
        assert_eq!(
            s.refusals(),
            (2, 0, 0, 0),
            "publishing must not move the tallies"
        );
    }

    #[test]
    fn is_empty_and_snapshot_prices_hold_under_concurrent_writers_with_no_lock() {
        // The property that makes this store the right shape: the drain writes
        // through &self while an attach task reads through &self, with no lock
        // on either side. A regression to a Mutex-per-value or an RwLock map
        // would still pass every other test here.
        let s = std::sync::Arc::new(store());
        let writers: Vec<_> = (0..4u64)
            .map(|w| {
                let s = std::sync::Arc::clone(&s);
                std::thread::spawn(move || {
                    for i in 0..250u64 {
                        s.record(w * 1000 + i, NSE_EQ, 100.0 + i as f32, T0);
                    }
                })
            })
            .collect();
        let reader = {
            let s = std::sync::Arc::clone(&s);
            std::thread::spawn(move || {
                for _ in 0..200 {
                    let _ignored = s.snapshot_prices();
                }
            })
        };
        for w in writers {
            w.join().expect("writer panicked");
        }
        reader.join().expect("reader panicked");
        assert_eq!(s.tracked(), 1000);
        assert_eq!(s.latest_paise(3_249, NSE_EQ), Some(34_900));
    }

    /// Concurrent writers on ONE slot with mixed trade times must leave the
    /// FRESHEST trade, whatever order the threads ran in.
    #[test]
    fn concurrent_writers_on_one_slot_leave_the_freshest_trade() {
        let s = std::sync::Arc::new(store());
        s.record(1, NSE_EQ, 1.0, T0);
        let writers: Vec<_> = (0..8u32)
            .map(|w| {
                let s = std::sync::Arc::clone(&s);
                std::thread::spawn(move || {
                    for i in 0..500u32 {
                        let offset = (i * 8 + w) % 400;
                        let secs = T0 + offset;
                        // The price is the OFFSET, not the epoch: an epoch
                        // second (~1.75e9) is beyond f32's 24-bit mantissa
                        // and would round, hiding which write actually won.
                        // APPROVED: offset < 400, exact in f32.
                        s.record(1, NSE_EQ, (offset + 1) as f32, secs);
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().expect("writer panicked");
        }
        assert_eq!(s.latest_exchange_secs(1, NSE_EQ), Some(T0 + 399));
        assert_eq!(s.latest_paise(1, NSE_EQ), Some(400 * 100));
    }
}
