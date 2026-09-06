//! The previous close, kept so the gainer filter has something to divide by.
//!
//! ## The gap this closes
//!
//! `volume_leaderboard::eligible_gain_pct(ltp, prev_close)` is the gainer
//! ELIGIBILITY filter the 2026-09-06 depth lock specifies. Nothing supplied
//! its second argument.
//!
//! Dhan sends the previous close on its OWN packet type (response code 6),
//! not on the tick. The parser decodes it into `ParsedFrame::PreviousClose`
//! — and the drain then drops it into the catch-all arm, whose own comment
//! says previous-close packets are *"deliberately not folded"*. That was
//! correct while nothing needed the value. It is not correct now.
//!
//! **Wiring the leaderboard without this would have been worse than leaving
//! it unwired.** Every gain would read `0.0`, `eligible_gain_pct` would
//! return `Some(0.0)` for everything, and the eligibility filter would
//! silently admit the entire universe while looking like it was filtering.
//! That is the false-OK class, and it would have been invisible: no counter
//! moves, no error fires, the ranking just quietly stops being a gainer
//! ranking.
//!
//! ## Why a refusal, not an eviction
//!
//! Past the cap a NEW instrument is REFUSED and counted; a tracked one is
//! never evicted. That is the `day_ohlc_tracker` and `tick_gap_tracker`
//! shape, and it is the right one here for a reason specific to this store:
//! a missing previous close makes an instrument INELIGIBLE as a gainer, so
//! refusing fails CLOSED — the contract simply does not qualify. Evicting
//! instead would make a contract's eligibility flap as the map churned, and
//! a filter whose answer changes for reasons unrelated to the market is
//! worse than one that says no.
//!
//! ## Why non-finite is refused at INGEST
//!
//! The quote parser carries a test asserting `day_close.is_nan()`, so a
//! non-finite close is a real wire value, not a hypothetical. `0.0` is also
//! a live sentinel — Ticker-mode packets and pre-open instruments carry it.
//! Storing either would push the problem into `eligible_gain_pct`, which
//! already refuses them; gating here means the map holds only values that
//! can produce an answer, and the refusal is COUNTED where it happens rather
//! than silently absorbed one layer down.
//!
//! ## Complexity
//!
//! O(1) average for `record` and `get` — one hash probe on the I-P1-11
//! composite key. Zero allocation after construction in the steady state:
//! the map is pre-sized to the cap and `record` on a tracked instrument
//! overwrites in place. Space is O(instruments), hard-bounded by the cap.

use std::collections::HashMap;

use tickvault_common::types::ExchangeSegment;

/// The I-P1-11 composite identity. `security_id` ALONE is not unique — Dhan
/// reuses one numeric id across segments, so keying on the bare id would let
/// an index's previous close answer a stock option's query.
pub type PrevCloseKey = (u64, ExchangeSegment);

/// Hard ceiling on tracked instruments. 25,000 deliberately matches
/// `DayOhlcTracker::MAX_TRACKED_INSTRUMENTS` and
/// `TickGapTracker::MAX_TRACKED_INSTRUMENTS`, so one figure covers every
/// per-instrument map on the same universe and there is no second number to
/// keep in step.
pub const MAX_TRACKED_INSTRUMENTS: usize = 25_000;

/// Counter for a NEW instrument refused at the cap.
pub const REFUSED_COUNTER: &str = "tv_prev_close_store_refused_total";

/// Counter for a value refused because it could not produce an answer.
pub const REJECTED_VALUE_COUNTER: &str = "tv_prev_close_store_rejected_value_total";

/// What `record` did with the offered previous close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    /// Stored — a new instrument, or a new value for a tracked one.
    Stored,
    /// The value was not finite, or was not above zero. Refused before it
    /// could reach the divide.
    RejectedValue,
    /// The map is at its ceiling and this instrument is not in it.
    RefusedAtCapacity,
}

/// Previous closes for the day, one per instrument.
///
/// Single-owner by construction — it lives on the drain's own state and is
/// reached through `&mut self`, which is why it is a plain `HashMap` and not
/// a concurrent map. `papaya` buys lock-free CONCURRENT reads and pays
/// epoch-reclamation cost that a single-owner path has no use for; the
/// aggregator's own header records the same decision for the same reason.
#[derive(Debug)]
pub struct PrevCloseStore {
    closes: HashMap<PrevCloseKey, f64>,
    refused_at_capacity: u64,
    rejected_values: u64,
}

impl Default for PrevCloseStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PrevCloseStore {
    /// Pre-sized to the cap so the steady state never reallocates.
    ///
    /// This is a PRE-SIZE, not the bound. The bound is the explicit check in
    /// [`Self::record`] — a distinction this repository has had to correct in
    /// its own rule files more than once, because `with_capacity` reads like
    /// a limit and is not one.
    #[must_use]
    pub fn new() -> Self {
        Self {
            closes: HashMap::with_capacity(MAX_TRACKED_INSTRUMENTS),
            refused_at_capacity: 0,
            rejected_values: 0,
        }
    }

    /// Records a previous close, or says why it would not.
    ///
    /// O(1) average. No allocation once the map is at size.
    pub fn record(
        &mut self,
        security_id: u64,
        segment: ExchangeSegment,
        previous_close: f64,
    ) -> RecordOutcome {
        // Gated HERE rather than at the divide: the map should hold only
        // values that can produce an answer, and the refusal should be
        // counted where the bad value arrived.
        if !previous_close.is_finite() || previous_close <= 0.0 {
            self.rejected_values = self.rejected_values.saturating_add(1);
            metrics::counter!(REJECTED_VALUE_COUNTER).increment(1);
            // Counted, deliberately NOT logged. `0.0` is the DOCUMENTED
            // Ticker-mode sentinel and the pre-open value, so this arm fires
            // legitimately and often; a line here would be noise that buries
            // the capacity warning below, which is the one that means
            // something is wrong. The capacity refusal is rare and permanent;
            // this one is common and expected.
            return RecordOutcome::RejectedValue;
        }
        let key = (security_id, segment);
        // A TRACKED instrument updates even at the ceiling — the cap bounds
        // how many instruments are held, never how fresh their values are.
        // Refusing an update would freeze a stale close in place, which is a
        // wrong answer rather than a missing one.
        // A refusal here means this instrument can never qualify as a gainer
        // for the rest of the session — a SILENT narrowing of the ranking,
        // which is why a counter alone is not enough and a line is warranted.
        //
        // The line is throttled to powers of two (the `tick_gap_tracker`
        // pattern): once the map is full EVERY new instrument refuses, so an
        // unthrottled `warn!` would be a log storm on the drain's own path —
        // the drain-stall shape that ends in upstream tick loss. Powers of two
        // keep the first refusal, the shape of the growth and the order of
        // magnitude, at a bounded number of lines.
        if self.closes.len() >= MAX_TRACKED_INSTRUMENTS && !self.closes.contains_key(&key) {
            self.refused_at_capacity = self.refused_at_capacity.saturating_add(1);
            metrics::counter!(REFUSED_COUNTER).increment(1);
            if self.refused_at_capacity.is_power_of_two() {
                tracing::warn!(
                    security_id,
                    segment = segment.as_str(),
                    tracked = MAX_TRACKED_INSTRUMENTS,
                    refused_total = self.refused_at_capacity,
                    "previous-close store is at its ceiling — this instrument has no \
                     previous close and can never qualify as a gainer this session; \
                     the ranking is narrower than the subscribed universe \
                     (logged at powers of two, counted every time)"
                );
            }
            return RecordOutcome::RefusedAtCapacity;
        }
        self.closes.insert(key, previous_close);
        RecordOutcome::Stored
    }

    /// The previous close, or `None` if this instrument has none.
    ///
    /// `None` is the honest answer and the caller must treat it as "not
    /// eligible", never as zero.
    #[must_use]
    pub fn get(&self, security_id: u64, segment: ExchangeSegment) -> Option<f64> {
        self.closes.get(&(security_id, segment)).copied()
    }

    /// How many instruments are held.
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.closes.len()
    }

    /// New instruments refused because the map was full.
    #[must_use]
    pub const fn refused_at_capacity(&self) -> u64 {
        self.refused_at_capacity
    }

    /// Values refused because they could not produce an answer.
    #[must_use]
    pub const fn rejected_values(&self) -> u64 {
        self.rejected_values
    }

    /// Clears every close for the new trading day.
    ///
    /// Mandatory, and the failure mode if it is missed is a QUIET one: every
    /// gain would be computed against YESTERDAY's close, so the whole
    /// ranking would be shifted by the overnight gap and nothing would say
    /// so. `retain` is not used — the whole map is stale, not part of it.
    ///
    /// The counters are deliberately NOT reset: they are cumulative session
    /// facts, and zeroing them would hide a capacity problem that happened
    /// before the reset.
    pub fn reset_daily(&mut self) {
        self.closes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FNO: ExchangeSegment = ExchangeSegment::NseFno;
    const IDX: ExchangeSegment = ExchangeSegment::IdxI;

    #[test]
    fn record_stores_a_value_that_get_returns() {
        let mut s = PrevCloseStore::new();
        assert_eq!(s.record(10, FNO, 250.5), RecordOutcome::Stored);
        assert_eq!(s.get(10, FNO), Some(250.5));
    }

    #[test]
    fn get_returns_none_for_an_untracked_instrument() {
        // None, never 0.0 — a zero here would make the gain read -100% and
        // look like a real, enormous move.
        let s = PrevCloseStore::new();
        assert_eq!(s.get(10, FNO), None);
    }

    #[test]
    fn record_keys_on_the_composite_so_two_segments_never_collide() {
        // I-P1-11: the bare id is reused. Without the segment in the key an
        // index's close would answer a stock option's query.
        let mut s = PrevCloseStore::new();
        s.record(13, IDX, 24_000.0);
        s.record(13, FNO, 120.0);
        assert_eq!(s.get(13, IDX), Some(24_000.0));
        assert_eq!(s.get(13, FNO), Some(120.0));
        assert_eq!(s.tracked(), 2);
    }

    #[test]
    fn record_rejects_a_non_finite_value() {
        // The quote parser carries a test asserting a NaN close on the wire,
        // so this is a real value, not a hypothetical.
        let mut s = PrevCloseStore::new();
        assert_eq!(s.record(10, FNO, f64::NAN), RecordOutcome::RejectedValue);
        assert_eq!(
            s.record(11, FNO, f64::INFINITY),
            RecordOutcome::RejectedValue
        );
        assert_eq!(
            s.record(12, FNO, f64::NEG_INFINITY),
            RecordOutcome::RejectedValue
        );
        assert_eq!(s.tracked(), 0);
        assert_eq!(s.rejected_values(), 3);
    }

    #[test]
    fn record_rejects_zero_and_negative_because_zero_is_a_live_sentinel() {
        // Ticker-mode packets and pre-open instruments carry 0.0. Storing it
        // would divide by zero one layer down.
        let mut s = PrevCloseStore::new();
        assert_eq!(s.record(10, FNO, 0.0), RecordOutcome::RejectedValue);
        assert_eq!(s.record(11, FNO, -5.0), RecordOutcome::RejectedValue);
        assert_eq!(s.tracked(), 0);
    }

    #[test]
    fn record_overwrites_a_tracked_instrument_in_place() {
        let mut s = PrevCloseStore::new();
        s.record(10, FNO, 100.0);
        assert_eq!(s.record(10, FNO, 110.0), RecordOutcome::Stored);
        assert_eq!(s.get(10, FNO), Some(110.0));
        assert_eq!(s.tracked(), 1);
    }

    #[test]
    fn record_refuses_a_new_instrument_at_the_ceiling() {
        let mut s = PrevCloseStore::new();
        for i in 0..MAX_TRACKED_INSTRUMENTS as u64 {
            assert_eq!(s.record(i, FNO, 100.0), RecordOutcome::Stored);
        }
        assert_eq!(s.tracked(), MAX_TRACKED_INSTRUMENTS);
        assert_eq!(
            s.record(999_999, FNO, 100.0),
            RecordOutcome::RefusedAtCapacity
        );
        assert_eq!(s.tracked(), MAX_TRACKED_INSTRUMENTS);
        assert_eq!(s.refused_at_capacity(), 1);
        assert_eq!(s.get(999_999, FNO), None);
    }

    #[test]
    fn record_still_updates_a_tracked_instrument_at_the_ceiling() {
        // The cap bounds how many instruments are held, NEVER how fresh
        // their values are. Refusing this update would freeze a stale close
        // in place — a wrong answer rather than a missing one.
        let mut s = PrevCloseStore::new();
        for i in 0..MAX_TRACKED_INSTRUMENTS as u64 {
            s.record(i, FNO, 100.0);
        }
        assert_eq!(s.record(7, FNO, 175.0), RecordOutcome::Stored);
        assert_eq!(s.get(7, FNO), Some(175.0));
        assert_eq!(s.refused_at_capacity(), 0);
    }

    #[test]
    fn reset_daily_clears_every_close_so_no_gain_uses_yesterdays_number() {
        // The failure mode if this is missed is QUIET: every gain would be
        // computed against yesterday's close and nothing would say so.
        let mut s = PrevCloseStore::new();
        s.record(10, FNO, 100.0);
        s.record(11, IDX, 200.0);
        s.reset_daily();
        assert_eq!(s.tracked(), 0);
        assert_eq!(s.get(10, FNO), None);
        assert_eq!(s.get(11, IDX), None);
    }

    #[test]
    fn reset_daily_keeps_the_counters_because_they_are_session_facts() {
        // Zeroing them would hide a capacity problem that happened before
        // the reset.
        let mut s = PrevCloseStore::new();
        s.record(10, FNO, f64::NAN);
        s.reset_daily();
        assert_eq!(s.rejected_values(), 1);
    }

    #[test]
    fn tracked_and_refused_at_capacity_start_at_zero() {
        let s = PrevCloseStore::default();
        assert_eq!(s.tracked(), 0);
        assert_eq!(s.refused_at_capacity(), 0);
        assert_eq!(s.rejected_values(), 0);
    }

    #[test]
    fn a_stored_close_produces_a_finite_gain_through_the_leaderboard_filter() {
        // The whole point of the store: `eligible_gain_pct` had no second
        // argument. This is the end-to-end shape it now has.
        use crate::volume_leaderboard::eligible_gain_pct;
        let mut s = PrevCloseStore::new();
        s.record(10, FNO, 100.0);
        let prev = s.get(10, FNO).expect("just stored");
        let gain = eligible_gain_pct(110.0, prev).expect("both finite and positive");
        assert!((gain - 10.0).abs() < 1e-9, "expected +10%, got {gain}");
    }

    #[test]
    fn an_untracked_instrument_is_ineligible_rather_than_wrongly_ranked() {
        // The false-OK this store exists to prevent: without it the caller
        // would have passed 0.0 and `eligible_gain_pct` would have refused
        // it — but a caller that DEFAULTED to 0.0 in the other direction
        // (treating a missing close as "no change") would have admitted the
        // entire universe. `None` forces the caller to decide.
        let s = PrevCloseStore::new();
        assert!(s.get(10, FNO).is_none());
    }
}
