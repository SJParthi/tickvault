//! Audit PR41a — which copy of a spilled candle is the fullest.
//!
//! ## The gap this closes
//!
//! A candle can be written more than once. A tick that arrives one bucket
//! late re-folds the bar that just sealed and the aggregator sends the
//! amended bar down the same seal path (`ConsumeOutcome::AmendedLate`), and
//! the day-close seal can amend an intraday-sealed bar with carried volume.
//! Each later copy has at least as many ticks and at least as much volume as
//! the one before it.
//!
//! If the FIRST copy went to the spill (the database was down, or the ring
//! was full) and the amended copy was then written live, the spill replay
//! wrote the first copy back over the amended row: last write wins under the
//! candle tables' DEDUP key, and nothing counted it. Measured shape of the
//! risk: the ring-cap saturation of 2026-08-20 spilled 61% of sealed candles
//! with the database UP, so amendments of spilled bars went live within
//! seconds while their originals waited on disk.
//!
//! ## How
//!
//! The spill writer keeps this ledger under its own append lock. It holds one
//! entry per `(security_id, segment, feed, timeframe)`: the newest bucket the
//! spill holds a copy of, and that copy's tick count and volume.
//!
//! One entry per slot is enough because a late tick can amend only the most
//! recently sealed bucket of a timeframe. Once the next bucket seals, no copy
//! of the older one can be superseded any more.
//!
//! * After every successful live flush the writer asks
//!   [`SpillLedger::live_supersedes`] for each committed seal. A committed
//!   copy of the same bucket that is fuller is appended to the spill as well,
//!   AFTER the older copy, so every replay (mid-session or the next boot's)
//!   writes the older copy first and the fuller one last.
//! * A spill append of a copy that is LESS full than the one the spill
//!   already holds for that bucket is not written: the fuller copy is already
//!   on disk, and writing the older one after it would invert the order.
//! * The mid-session replay drops a record whose bucket's ledger copy is
//!   fuller, so the database never holds the older copy even briefly.
//!
//! "Fuller" is `(tick_count, volume)` compared in that order. Both only grow
//! across the copies of one bar: an amend adds a tick, a carry settlement adds
//! volume.
//!
//! ## Bounds
//!
//! At most `capacity` entries, allocated once when the ledger is built, never
//! grown: the aggregator's slot ceiling times the timeframe count. A slot past
//! it is not tracked and the caller counts it. Every operation is one hash
//! probe: O(1), no allocation.

use std::collections::HashMap;

use crate::seal_spill::SerializedSeal;

/// Identity of one aggregator slot's timeframe. Segment and feed are part of
/// it (I-P1-11: a security id alone is not unique, and two feeds' copies of
/// the same bar are distinct rows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct LedgerKey {
    security_id: u64,
    segment: u8,
    feed: u8,
    tf_ordinal: u8,
}

impl LedgerKey {
    fn of(seal: &SerializedSeal) -> Self {
        Self {
            security_id: seal.security_id,
            segment: seal.exchange_segment_code,
            // `Feed::index()` is 0 or 1; the cast cannot truncate.
            feed: u8::try_from(seal.feed.index()).unwrap_or(u8::MAX),
            tf_ordinal: seal.tf_ordinal,
        }
    }
}

/// The copy of the newest spilled bucket of one slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LedgerCopy {
    bucket_start_ist_secs: u32,
    tick_count: u32,
    volume: u64,
}

impl LedgerCopy {
    fn of(seal: &SerializedSeal) -> Self {
        Self {
            bucket_start_ist_secs: seal.bucket_start_ist_secs,
            tick_count: seal.tick_count,
            volume: seal.volume,
        }
    }

    fn fullness(self) -> (u32, u64) {
        (self.tick_count, self.volume)
    }
}

/// What the spill should do with a copy it is about to append.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpillVerdict {
    /// Write it, and record it once the write succeeds.
    Write,
    /// The spill already holds a fuller copy of this bucket. Do not write.
    OlderNotWritten,
    /// Write it, but the ledger is full and cannot track this slot.
    Untracked,
}

/// See the module docs.
#[derive(Debug)]
pub(crate) struct SpillLedger {
    slots: HashMap<LedgerKey, LedgerCopy>,
    capacity: usize,
}

impl SpillLedger {
    /// A ledger for at most `capacity` slots, allocated now.
    ///
    /// # Complexity
    /// O(capacity) once, at construction.
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            slots: HashMap::with_capacity(capacity),
            capacity,
        }
    }

    /// Decide what to do with a copy about to be appended to the spill.
    ///
    /// Only a copy of the SAME bucket can be refused. A copy of an older
    /// bucket is written: that bucket can no longer be amended, so whichever
    /// copy of it is on disk is the last one.
    ///
    /// # Complexity
    /// O(1), one hash probe.
    pub(crate) fn verdict(&self, seal: &SerializedSeal) -> SpillVerdict {
        match self.slots.get(&LedgerKey::of(seal)) {
            Some(held)
                if held.bucket_start_ist_secs == seal.bucket_start_ist_secs
                    && LedgerCopy::of(seal).fullness() < held.fullness() =>
            {
                SpillVerdict::OlderNotWritten
            }
            Some(_) => SpillVerdict::Write,
            None if self.slots.len() >= self.capacity => SpillVerdict::Untracked,
            None => SpillVerdict::Write,
        }
    }

    /// Record a copy the spill now holds. Call only after the write
    /// succeeded, so the ledger never names a copy that is not on disk.
    ///
    /// # Complexity
    /// O(1), one hash probe; never grows the map past its capacity.
    pub(crate) fn record(&mut self, seal: &SerializedSeal) {
        let key = LedgerKey::of(seal);
        let copy = LedgerCopy::of(seal);
        if let Some(held) = self.slots.get_mut(&key) {
            let newer_bucket = copy.bucket_start_ist_secs > held.bucket_start_ist_secs;
            let same_bucket_fuller = copy.bucket_start_ist_secs == held.bucket_start_ist_secs
                && copy.fullness() > held.fullness();
            if newer_bucket || same_bucket_fuller {
                *held = copy;
            }
            return;
        }
        if self.slots.len() < self.capacity {
            self.slots.insert(key, copy);
        }
    }

    /// `true` when `committed`, just written live, is a fuller copy of a
    /// bucket the spill holds an older copy of. The caller appends it to the
    /// spill so every replay ends on it.
    ///
    /// # Complexity
    /// O(1), one hash probe.
    pub(crate) fn live_supersedes(&self, committed: &SerializedSeal) -> bool {
        self.slots
            .get(&LedgerKey::of(committed))
            .is_some_and(|held| {
                held.bucket_start_ist_secs == committed.bucket_start_ist_secs
                    && LedgerCopy::of(committed).fullness() > held.fullness()
            })
    }

    /// `true` when `replayed` is an older copy of a bucket the spill also
    /// holds a fuller copy of. The replay drops it.
    ///
    /// # Complexity
    /// O(1), one hash probe.
    pub(crate) fn replay_is_older(&self, replayed: &SerializedSeal) -> bool {
        self.slots
            .get(&LedgerKey::of(replayed))
            .is_some_and(|held| {
                held.bucket_start_ist_secs == replayed.bucket_start_ist_secs
                    && LedgerCopy::of(replayed).fullness() < held.fullness()
            })
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::feed::Feed;
    use tickvault_trading::candles::{BufferedSeal, LiveCandleState, TfIndex};

    fn copy_of(
        sid: u64,
        seg: u8,
        tf: TfIndex,
        bucket: u32,
        ticks: u32,
        volume: u64,
    ) -> SerializedSeal {
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = bucket;
        state.tick_count = ticks;
        state.volume = volume;
        state.open = 100.0;
        state.high = 100.0;
        state.low = 100.0;
        state.close = 100.0;
        SerializedSeal::from(&BufferedSeal::new(sid, seg, tf, state, Feed::Dhan))
    }

    #[test]
    fn live_supersedes_the_spilled_original_with_an_amended_copy() {
        let mut ledger = SpillLedger::with_capacity(8);
        let original = copy_of(13, 2, TfIndex::M1, 600, 4, 40);
        assert_eq!(ledger.verdict(&original), SpillVerdict::Write);
        ledger.record(&original);

        let amended = copy_of(13, 2, TfIndex::M1, 600, 5, 40);
        assert!(ledger.live_supersedes(&amended));
        // The same copy committed live supersedes nothing.
        assert!(!ledger.live_supersedes(&original));
        // A later bucket is a different bar.
        assert!(!ledger.live_supersedes(&copy_of(13, 2, TfIndex::M1, 660, 9, 90)));
    }

    #[test]
    fn a_carry_settlement_with_equal_ticks_and_more_volume_is_fuller() {
        let mut ledger = SpillLedger::with_capacity(8);
        ledger.record(&copy_of(13, 2, TfIndex::M60, 3_600, 4, 40));
        assert!(ledger.live_supersedes(&copy_of(13, 2, TfIndex::M60, 3_600, 4, 45)));
    }

    #[test]
    fn replay_is_older_and_not_written_for_an_older_copy_of_a_spilled_bucket() {
        let mut ledger = SpillLedger::with_capacity(8);
        ledger.record(&copy_of(13, 2, TfIndex::M1, 600, 5, 40));
        let older = copy_of(13, 2, TfIndex::M1, 600, 4, 40);
        assert_eq!(ledger.verdict(&older), SpillVerdict::OlderNotWritten);
        assert!(ledger.replay_is_older(&older));
        // An older BUCKET is always written: it cannot be amended any more.
        assert_eq!(
            ledger.verdict(&copy_of(13, 2, TfIndex::M1, 540, 1, 1)),
            SpillVerdict::Write
        );
        // An equal copy is written and replayed.
        let equal = copy_of(13, 2, TfIndex::M1, 600, 5, 40);
        assert_eq!(ledger.verdict(&equal), SpillVerdict::Write);
        assert!(!ledger.replay_is_older(&equal));
    }

    #[test]
    fn record_keeps_the_newest_bucket_and_the_fullest_copy() {
        let mut ledger = SpillLedger::with_capacity(8);
        ledger.record(&copy_of(13, 2, TfIndex::M1, 600, 5, 40));
        // Less full, same bucket: ignored.
        ledger.record(&copy_of(13, 2, TfIndex::M1, 600, 3, 40));
        assert!(ledger.replay_is_older(&copy_of(13, 2, TfIndex::M1, 600, 4, 40)));
        // Older bucket: ignored.
        ledger.record(&copy_of(13, 2, TfIndex::M1, 540, 9, 90));
        assert!(ledger.replay_is_older(&copy_of(13, 2, TfIndex::M1, 600, 4, 40)));
        // Newer bucket: replaces, and the older bucket is no longer judged.
        ledger.record(&copy_of(13, 2, TfIndex::M1, 660, 1, 10));
        assert!(!ledger.replay_is_older(&copy_of(13, 2, TfIndex::M1, 600, 4, 40)));
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn segment_feed_and_timeframe_are_separate_slots() {
        let mut ledger = SpillLedger::with_capacity(8);
        ledger.record(&copy_of(27, 0, TfIndex::M1, 600, 5, 0));
        // Same id, another segment (I-P1-11): not the same bar.
        assert!(!ledger.replay_is_older(&copy_of(27, 1, TfIndex::M1, 600, 4, 0)));
        // Same id and segment, another timeframe.
        assert!(!ledger.replay_is_older(&copy_of(27, 0, TfIndex::M5, 600, 4, 0)));
        let mut other_feed = copy_of(27, 0, TfIndex::M1, 600, 4, 0);
        other_feed.feed = Feed::Truedata;
        assert!(!ledger.replay_is_older(&other_feed));
    }

    #[test]
    fn a_full_ledger_tracks_no_new_slot_and_never_grows() {
        let mut ledger = SpillLedger::with_capacity(2);
        ledger.record(&copy_of(1, 2, TfIndex::M1, 600, 1, 1));
        ledger.record(&copy_of(2, 2, TfIndex::M1, 600, 1, 1));
        let third = copy_of(3, 2, TfIndex::M1, 600, 1, 1);
        assert_eq!(ledger.verdict(&third), SpillVerdict::Untracked);
        ledger.record(&third);
        assert_eq!(ledger.len(), 2);
        // A tracked slot still updates at the cap.
        ledger.record(&copy_of(1, 2, TfIndex::M1, 660, 1, 1));
        assert_eq!(ledger.len(), 2);
        assert!(ledger.replay_is_older(&copy_of(1, 2, TfIndex::M1, 660, 0, 0)));
    }
}
