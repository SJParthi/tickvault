//! Audit PR41a, reworked for Z6 — a replayed candle never replaces a fuller
//! copy of the same bar, whatever order the copies are written in.
//!
//! ## The gap
//!
//! A candle can be written more than once. A tick that arrives one bucket
//! late re-folds the bar that just sealed and the aggregator sends the
//! amended bar down the same seal path (`ConsumeOutcome::AmendedLate`), and
//! the day-close seal can amend an intraday-sealed bar with carried volume.
//! Each later copy has at least as many ticks and at least as much volume as
//! the one before it, so `(tick_count, volume)`, compared in that order, is a
//! version number for the copies of one bar within one fold lineage.
//!
//! The candle tables' DEDUP key `(ts, security_id, segment, feed)` means the
//! last write wins. A copy that reaches the spill, the dead-letter file or
//! the escalation queue and is replayed AFTER a fuller copy was committed
//! puts the older bar back over the newer one, with nothing counting it.
//!
//! The first version (PR41a) kept one entry per `(security_id, segment, feed,
//! timeframe)` and relied on the order in which copies reached the disk. Four
//! paths broke that order (Z6): a later bucket spilled before the amend
//! committed and replaced the entry; the original was still in the
//! escalation queue when the amend committed; the original went to the
//! dead-letter file, which the ledger never saw; and a parked spill file
//! resumed after a later file had replayed the amend.
//!
//! ## How
//!
//! The ledger now keeps one entry per bar, `(slot, bucket)`, holding the
//! fullest copy on disk this process wrote (spill or dead-letter) and the
//! fullest copy committed to the database (live or by the mid-session
//! replay). Every rule is a max, so the result does not depend on order:
//!
//! * A spill append of a copy less full than the fullest one on disk or
//!   committed is not written ([`SpillLedger::verdict`]). This closes the
//!   escalation-queue path: the queued original is refused when it finally
//!   reaches the spill, because the amend was recorded as committed.
//! * A live commit fuller than the copy on disk is appended to the spill
//!   ([`SpillLedger::on_live_commit`] returns `Mirror`), so the boot drain,
//!   which keeps the fullest copy per bar across every file, ends on it.
//! * The mid-session replay skips a copy less full than the committed one
//!   ([`SpillLedger::replay_is_older`]) and records what it commits
//!   ([`SpillLedger::on_replay_commit`]), so a parked file resumed late
//!   cannot write an older copy over one a later file already replayed.
//! * A live commit with no entry while the escalation queue holds anything
//!   records a committed-only entry, so a queued original arriving later is
//!   refused.
//!
//! ## Bounds
//!
//! At most `capacity` entries, allocated once when the ledger is built and
//! never grown. Every per-seal operation is one hash probe: O(1), no
//! allocation. Entries are forgotten by [`SpillLedger::forget_replayed`],
//! which the mid-session replay calls when it has consumed every spill file
//! staged up to an epoch: an entry whose disk copies are all in those files,
//! that has no dead-letter copy, and whose escalation-queue horizon has
//! passed is dropped. That pass is O(capacity), on the seal writer task, at
//! most once per replay scan (30 s).
//!
//! **At capacity the ledger fails toward writing data.** A copy it cannot
//! track is still written (counted as `untracked`), and while any untracked
//! copy may be on disk, every live commit with no entry is mirrored to the
//! spill (counted as `mirrored_overflow`), so the fullest copy is always on
//! disk for the boot drain's max rule. Nothing is dropped.

use std::collections::HashMap;

use crate::seal_spill::SerializedSeal;

/// Identity of one bar: one aggregator slot's timeframe and one bucket.
/// Segment and feed are part of it (I-P1-11: a security id alone is not
/// unique, and two feeds' copies of the same bar are distinct rows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct BarKey {
    security_id: u64,
    bucket_start_ist_secs: u32,
    segment: u8,
    feed: u8,
    tf_ordinal: u8,
}

impl BarKey {
    fn of(seal: &SerializedSeal) -> Self {
        Self {
            security_id: seal.security_id,
            bucket_start_ist_secs: seal.bucket_start_ist_secs,
            segment: seal.exchange_segment_code,
            // `Feed::index()` is a small enum index; the cast cannot truncate.
            feed: u8::try_from(seal.feed.index()).unwrap_or(u8::MAX),
            tf_ordinal: seal.tf_ordinal,
        }
    }
}

/// `(tick_count, volume)`: how full a copy of a bar is. Both only grow
/// across the copies of one bar, so the tuple order is the copy order.
type Fullness = (u32, u64);

fn fullness_of(seal: &SerializedSeal) -> Fullness {
    (seal.tick_count, seal.volume)
}

const HAS_DISK: u8 = 1;
const HAS_COMMITTED: u8 = 1 << 1;
const DEAD_LETTERED: u8 = 1 << 2;

/// What the ledger knows about one bar. 40 bytes.
#[derive(Clone, Copy, Debug)]
struct BarEntry {
    disk_ticks: u32,
    committed_ticks: u32,
    /// Spill epoch of the newest disk copy (see the module docs).
    disk_epoch: u32,
    flags: u8,
    disk_volume: u64,
    committed_volume: u64,
    /// Escalation progress that must be reached before the entry may be
    /// forgotten: everything queued before its last update has been written.
    queue_mark: u64,
}

impl BarEntry {
    const EMPTY: Self = Self {
        disk_ticks: 0,
        committed_ticks: 0,
        disk_epoch: 0,
        flags: 0,
        disk_volume: 0,
        committed_volume: 0,
        queue_mark: 0,
    };

    fn disk(&self) -> Option<Fullness> {
        (self.flags & HAS_DISK != 0).then_some((self.disk_ticks, self.disk_volume))
    }

    fn committed(&self) -> Option<Fullness> {
        (self.flags & HAS_COMMITTED != 0).then_some((self.committed_ticks, self.committed_volume))
    }

    fn note_disk(&mut self, copy: Fullness, epoch: u32, dead_lettered: bool) {
        if self.disk().is_none_or(|held| copy > held) {
            self.disk_ticks = copy.0;
            self.disk_volume = copy.1;
        }
        self.disk_epoch = self.disk_epoch.max(epoch);
        self.flags |= HAS_DISK;
        if dead_lettered {
            self.flags |= DEAD_LETTERED;
        }
    }

    fn note_committed(&mut self, copy: Fullness) {
        if self.committed().is_none_or(|held| copy > held) {
            self.committed_ticks = copy.0;
            self.committed_volume = copy.1;
        }
        self.flags |= HAS_COMMITTED;
    }

    /// The fullest copy known on disk or in the database.
    fn fullest(&self) -> Option<Fullness> {
        match (self.disk(), self.committed()) {
            (Some(d), Some(c)) => Some(d.max(c)),
            (d, c) => d.or(c),
        }
    }
}

/// Copies the ledger could not track (it was full). Same forgetting rule as
/// an entry, so the ledger leaves overflow once those copies are consumed.
#[derive(Clone, Copy, Debug, Default)]
struct Overflow {
    active: bool,
    disk_epoch: u32,
    dead_lettered: bool,
    queue_mark: u64,
}

/// What the spill should do with a copy it is about to append.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpillVerdict {
    /// Write it, and record it once the write succeeds.
    Write,
    /// The spill or the database already holds a fuller copy of this bar.
    /// Do not write.
    OlderNotWritten,
    /// Write it, but the ledger is full and cannot track this bar.
    Untracked,
}

/// What a live commit asks of the spill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LiveCommit {
    /// Nothing: no older copy of this bar can be on disk.
    Nothing,
    /// Append the committed copy to the spill: an older copy is on disk.
    Mirror,
    /// Append the committed copy to the spill: the ledger overflowed and
    /// cannot tell whether an older copy is on disk.
    MirrorOverflow,
}

/// Escalation-queue progress at one moment (see
/// `SealSpillWriter::queue_progress`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct QueueProgress {
    /// Seals the escalation thread has finished with (written, sent to the
    /// DLQ, or reported lost) so far.
    pub(crate) written: u64,
    /// `true` when nothing was queued.
    pub(crate) idle: bool,
}

impl QueueProgress {
    /// `true` when everything queued before `mark` was taken is finished.
    const fn has_passed(self, mark: u64) -> bool {
        self.idle || self.written >= mark
    }
}

/// See the module docs.
#[derive(Debug)]
pub(crate) struct SpillLedger {
    bars: HashMap<BarKey, BarEntry>,
    capacity: usize,
    overflow: Overflow,
}

impl SpillLedger {
    /// A ledger for at most `capacity` bars, allocated now.
    ///
    /// # Complexity
    /// O(capacity) once, at construction.
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            bars: HashMap::with_capacity(capacity),
            capacity,
            overflow: Overflow::default(),
        }
    }

    /// `true` while the ledger holds anything or has overflowed.
    pub(crate) fn is_active(&self) -> bool {
        !self.bars.is_empty() || self.overflow.active
    }

    /// `true` while an untracked copy may be on disk.
    #[cfg(test)]
    pub(crate) const fn overflowed(&self) -> bool {
        self.overflow.active
    }

    /// The entry for `key`, inserted empty if there is room. `None` at
    /// capacity. One hash probe.
    fn entry(&mut self, key: BarKey) -> Option<&mut BarEntry> {
        if self.bars.len() < self.capacity {
            return Some(self.bars.entry(key).or_insert(BarEntry::EMPTY));
        }
        self.bars.get_mut(&key)
    }

    fn note_untracked(&mut self, epoch: u32, dead_lettered: bool, queue_mark: u64) {
        let o = &mut self.overflow;
        o.active = true;
        o.disk_epoch = o.disk_epoch.max(epoch);
        o.dead_lettered |= dead_lettered;
        o.queue_mark = o.queue_mark.max(queue_mark);
    }

    /// Decide what to do with a copy about to be appended to the spill.
    ///
    /// Refused only when a FULLER copy of the same bar is on disk or
    /// committed: the fuller one is what every replay must end on. An equal
    /// copy is written (it is the same bar).
    ///
    /// # Complexity
    /// O(1), one hash probe.
    pub(crate) fn verdict(&self, seal: &SerializedSeal) -> SpillVerdict {
        match self.bars.get(&BarKey::of(seal)) {
            Some(entry) if entry.fullest().is_some_and(|f| fullness_of(seal) < f) => {
                SpillVerdict::OlderNotWritten
            }
            Some(_) => SpillVerdict::Write,
            None if self.bars.len() >= self.capacity => SpillVerdict::Untracked,
            None => SpillVerdict::Write,
        }
    }

    /// Record a copy now on disk: in the spill at `epoch`, or in the
    /// dead-letter file (`dead_lettered`, which pins the entry until the next
    /// boot, since only the boot drain reads that file). `also_committed`
    /// for a mirrored live commit. Call only after the write succeeded.
    /// Returns `false` when the ledger was full and the copy is untracked.
    ///
    /// # Complexity
    /// O(1), one hash probe; never grows the map past its capacity.
    pub(crate) fn record_on_disk(
        &mut self,
        seal: &SerializedSeal,
        epoch: u32,
        dead_lettered: bool,
        also_committed: bool,
        queue_mark: u64,
    ) -> bool {
        let copy = fullness_of(seal);
        match self.entry(BarKey::of(seal)) {
            Some(entry) => {
                entry.note_disk(copy, epoch, dead_lettered);
                if also_committed {
                    entry.note_committed(copy);
                }
                entry.queue_mark = entry.queue_mark.max(queue_mark);
                true
            }
            None => {
                self.note_untracked(epoch, dead_lettered, queue_mark);
                false
            }
        }
    }

    /// A copy was committed by the live writer. Records it and says whether
    /// it must also be appended to the spill.
    ///
    /// `queue_busy`: the escalation queue holds something, which may be an
    /// older copy of this bar on its way to the spill. A committed-only entry
    /// is then recorded so that copy is refused when it arrives.
    ///
    /// # Complexity
    /// O(1), at most two hash probes.
    pub(crate) fn on_live_commit(
        &mut self,
        seal: &SerializedSeal,
        queue_busy: bool,
        epoch: u32,
        queue_mark: u64,
    ) -> LiveCommit {
        let key = BarKey::of(seal);
        let copy = fullness_of(seal);
        if let Some(entry) = self.bars.get_mut(&key) {
            let mirror = entry.disk().is_some_and(|held| copy > held);
            entry.note_committed(copy);
            entry.queue_mark = entry.queue_mark.max(queue_mark);
            return if mirror {
                LiveCommit::Mirror
            } else {
                LiveCommit::Nothing
            };
        }
        if !self.overflow.active && !queue_busy {
            // No copy of this bar is on disk this process (or every one has
            // been replayed and forgotten), and none can be in the queue.
            return LiveCommit::Nothing;
        }
        match self.entry(key) {
            Some(entry) => {
                entry.note_committed(copy);
                entry.queue_mark = entry.queue_mark.max(queue_mark);
            }
            // Full: the queued original, if any, may be written untracked.
            None => self.note_untracked(epoch, false, queue_mark),
        }
        if self.overflow.active {
            LiveCommit::MirrorOverflow
        } else {
            LiveCommit::Nothing
        }
    }

    /// A copy was committed by the mid-session replay. Updates an existing
    /// entry only: a bar with no entry has no other tracked copy.
    ///
    /// # Complexity
    /// O(1), one hash probe.
    pub(crate) fn on_replay_commit(&mut self, seal: &SerializedSeal) {
        if let Some(entry) = self.bars.get_mut(&BarKey::of(seal)) {
            entry.note_committed(fullness_of(seal));
        }
    }

    /// `true` when `replayed` is less full than a copy already committed.
    /// The replay drops it.
    ///
    /// # Complexity
    /// O(1), one hash probe.
    pub(crate) fn replay_is_older(&self, replayed: &SerializedSeal) -> bool {
        self.bars
            .get(&BarKey::of(replayed))
            .and_then(BarEntry::committed)
            .is_some_and(|held| fullness_of(replayed) < held)
    }

    /// Forget every bar whose copies can no longer be written: every disk
    /// copy is in a spill file staged at or before `consumed_through` (and
    /// the replay has consumed all of those), it has no dead-letter copy, and
    /// everything queued before its last update is finished. Returns how
    /// many entries were dropped.
    ///
    /// # Complexity
    /// O(capacity): one pass over the map, no allocation. Called by the
    /// mid-session replay at most once per scan, on the seal writer task.
    pub(crate) fn forget_replayed(&mut self, consumed_through: u32, queue: QueueProgress) -> usize {
        let before = self.bars.len();
        // O(1) EXEMPT: begin — the forgetting pass, at most once per replay scan
        self.bars.retain(|_, entry| {
            let disk_consumed = entry.disk().is_none() || entry.disk_epoch <= consumed_through;
            let forgettable = entry.flags & DEAD_LETTERED == 0
                && disk_consumed
                && queue.has_passed(entry.queue_mark);
            !forgettable
        });
        // O(1) EXEMPT: end
        let o = self.overflow;
        if o.active
            && !o.dead_lettered
            && o.disk_epoch <= consumed_through
            && queue.has_passed(o.queue_mark)
        {
            self.overflow = Overflow::default();
        }
        before - self.bars.len()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.bars.len()
    }
}

/// The boot drain's record of the fullest copy of each bar it has written
/// (Z6). Built per boot, only when files are staged, and dropped with it.
///
/// Unlike the live ledger it grows on demand (cold, once per boot) up to
/// `cap` bars. Past the cap a copy is written untracked and counted, so the
/// failure direction is "write the data".
#[derive(Debug)]
pub(crate) struct BootWritten {
    bars: HashMap<BarKey, Fullness>,
    cap: usize,
}

impl BootWritten {
    /// An empty record for at most `cap` bars. Allocates nothing yet.
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            bars: HashMap::new(),
            cap,
        }
    }

    /// `true` when a fuller copy of this bar was already written.
    ///
    /// # Complexity
    /// O(1), one hash probe.
    pub(crate) fn is_older(&self, seal: &SerializedSeal) -> bool {
        self.bars
            .get(&BarKey::of(seal))
            .is_some_and(|held| fullness_of(seal) < *held)
    }

    /// Record a copy appended to the database writer. Returns `false` when
    /// the record is full and this bar is untracked.
    ///
    /// # Complexity
    /// O(1) amortised, one hash probe (the map may grow: boot only).
    pub(crate) fn record(&mut self, seal: &SerializedSeal) -> bool {
        let key = BarKey::of(seal);
        let copy = fullness_of(seal);
        if let Some(held) = self.bars.get_mut(&key) {
            *held = (*held).max(copy);
            return true;
        }
        if self.bars.len() >= self.cap {
            return false;
        }
        self.bars.insert(key, copy);
        true
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

    const IDLE: QueueProgress = QueueProgress {
        written: 0,
        idle: true,
    };

    #[test]
    fn a_live_commit_fuller_than_the_disk_copy_is_mirrored() {
        let mut ledger = SpillLedger::with_capacity(8);
        let original = copy_of(13, 2, TfIndex::M1, 600, 4, 40);
        assert_eq!(ledger.verdict(&original), SpillVerdict::Write);
        assert!(ledger.record_on_disk(&original, 1, false, false, 0));

        let amended = copy_of(13, 2, TfIndex::M1, 600, 5, 40);
        assert_eq!(
            ledger.on_live_commit(&amended, false, 1, 0),
            LiveCommit::Mirror
        );
        // Once the mirror is on disk, committing it again asks for nothing.
        assert!(ledger.record_on_disk(&amended, 1, false, true, 0));
        assert_eq!(
            ledger.on_live_commit(&amended, false, 1, 0),
            LiveCommit::Nothing
        );
        // A bar never on disk asks for nothing and gets no entry.
        assert_eq!(
            ledger.on_live_commit(&copy_of(13, 2, TfIndex::M1, 660, 9, 90), false, 1, 0),
            LiveCommit::Nothing
        );
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn test_regression_z6_a_later_bucket_does_not_hide_the_earlier_bars_amend() {
        // Path A: the old ledger kept one entry per slot, so spilling B2
        // replaced B1 and the amend of B1 was never mirrored.
        let mut ledger = SpillLedger::with_capacity(8);
        ledger.record_on_disk(&copy_of(13, 2, TfIndex::M1, 600, 4, 40), 1, false, false, 0);
        ledger.record_on_disk(&copy_of(13, 2, TfIndex::M1, 660, 1, 10), 1, false, false, 0);
        assert_eq!(
            ledger.on_live_commit(&copy_of(13, 2, TfIndex::M1, 600, 5, 40), false, 1, 0),
            LiveCommit::Mirror
        );
    }

    #[test]
    fn a_carry_settlement_with_equal_ticks_and_more_volume_is_fuller() {
        let mut ledger = SpillLedger::with_capacity(8);
        ledger.record_on_disk(
            &copy_of(13, 2, TfIndex::M60, 3_600, 4, 40),
            1,
            false,
            false,
            0,
        );
        assert_eq!(
            ledger.on_live_commit(&copy_of(13, 2, TfIndex::M60, 3_600, 4, 45), false, 1, 0),
            LiveCommit::Mirror
        );
    }

    #[test]
    fn an_older_copy_is_refused_against_disk_or_committed() {
        let mut ledger = SpillLedger::with_capacity(8);
        ledger.record_on_disk(&copy_of(13, 2, TfIndex::M1, 600, 5, 40), 1, false, false, 0);
        let older = copy_of(13, 2, TfIndex::M1, 600, 4, 40);
        assert_eq!(ledger.verdict(&older), SpillVerdict::OlderNotWritten);
        // An equal copy is written.
        assert_eq!(
            ledger.verdict(&copy_of(13, 2, TfIndex::M1, 600, 5, 40)),
            SpillVerdict::Write
        );
        // An older bucket is a different bar.
        assert_eq!(
            ledger.verdict(&copy_of(13, 2, TfIndex::M1, 540, 1, 1)),
            SpillVerdict::Write
        );
    }

    #[test]
    fn test_regression_z6_a_queued_original_is_refused_after_its_amend_committed() {
        // Path B: nothing is on disk yet, the original waits in the
        // escalation queue, and the amend commits live.
        let mut ledger = SpillLedger::with_capacity(8);
        let original = copy_of(13, 2, TfIndex::M1, 600, 4, 40);
        let amended = copy_of(13, 2, TfIndex::M1, 600, 5, 40);
        assert_eq!(
            ledger.on_live_commit(&amended, true, 1, 7),
            LiveCommit::Nothing
        );
        assert_eq!(ledger.verdict(&original), SpillVerdict::OlderNotWritten);
        // With the queue empty, a commit records nothing.
        let mut idle = SpillLedger::with_capacity(8);
        assert_eq!(
            idle.on_live_commit(&amended, false, 1, 0),
            LiveCommit::Nothing
        );
        assert!(!idle.is_active());
    }

    #[test]
    fn replay_is_older_only_against_a_committed_copy() {
        let mut ledger = SpillLedger::with_capacity(8);
        let c1 = copy_of(13, 2, TfIndex::M1, 600, 4, 40);
        let c2 = copy_of(13, 2, TfIndex::M1, 600, 5, 40);
        ledger.record_on_disk(&c1, 1, false, false, 0);
        ledger.record_on_disk(&c2, 1, false, false, 0);
        // Both on disk, neither committed: the replay writes both in order.
        assert!(!ledger.replay_is_older(&c1));
        // Once the replay commits c2 (a later file first), c1 is dropped.
        ledger.on_replay_commit(&c2);
        assert!(ledger.replay_is_older(&c1));
        assert!(!ledger.replay_is_older(&c2));
    }

    #[test]
    fn segment_feed_timeframe_and_bucket_are_separate_bars() {
        let mut ledger = SpillLedger::with_capacity(8);
        let base = copy_of(27, 0, TfIndex::M1, 600, 5, 0);
        ledger.record_on_disk(&base, 1, false, true, 0);
        // Same id, another segment (I-P1-11): not the same bar.
        assert!(!ledger.replay_is_older(&copy_of(27, 1, TfIndex::M1, 600, 4, 0)));
        assert!(!ledger.replay_is_older(&copy_of(27, 0, TfIndex::M5, 600, 4, 0)));
        assert!(!ledger.replay_is_older(&copy_of(27, 0, TfIndex::M1, 660, 4, 0)));
        let mut other_feed = copy_of(27, 0, TfIndex::M1, 600, 4, 0);
        other_feed.feed = Feed::Truedata;
        assert!(!ledger.replay_is_older(&other_feed));
        assert!(ledger.replay_is_older(&copy_of(27, 0, TfIndex::M1, 600, 4, 0)));
    }

    #[test]
    fn test_regression_z6_ledger_at_capacity_fails_toward_writing() {
        let mut ledger = SpillLedger::with_capacity(2);
        ledger.record_on_disk(&copy_of(1, 2, TfIndex::M1, 600, 1, 1), 1, false, false, 0);
        ledger.record_on_disk(&copy_of(2, 2, TfIndex::M1, 600, 1, 1), 1, false, false, 0);
        let third = copy_of(3, 2, TfIndex::M1, 600, 1, 1);
        assert_eq!(ledger.verdict(&third), SpillVerdict::Untracked);
        assert!(!ledger.record_on_disk(&third, 1, false, false, 0));
        assert_eq!(ledger.len(), 2, "never grows past its capacity");
        assert!(ledger.overflowed());
        // Every commit of an untracked bar is mirrored while overflowed.
        assert_eq!(
            ledger.on_live_commit(&copy_of(3, 2, TfIndex::M1, 600, 2, 1), false, 1, 0),
            LiveCommit::MirrorOverflow
        );
        // A tracked bar still updates at the cap.
        assert_eq!(
            ledger.on_live_commit(&copy_of(1, 2, TfIndex::M1, 600, 2, 1), false, 1, 0),
            LiveCommit::Mirror
        );
        // Once the untracked copy's epoch is consumed, overflow ends.
        ledger.forget_replayed(1, IDLE);
        assert!(!ledger.overflowed());
        assert_eq!(ledger.len(), 0);
    }

    #[test]
    fn test_regression_z6_forget_replayed_keeps_what_can_still_be_written() {
        let mut ledger = SpillLedger::with_capacity(16);
        // Consumed spill copy: forgotten.
        ledger.record_on_disk(&copy_of(1, 2, TfIndex::M1, 600, 1, 1), 3, false, false, 0);
        // A spill copy appended after the consumed epoch: kept.
        ledger.record_on_disk(&copy_of(2, 2, TfIndex::M1, 600, 1, 1), 4, false, false, 0);
        // A dead-letter copy: kept until the next boot.
        ledger.record_on_disk(&copy_of(3, 2, TfIndex::M1, 600, 1, 1), 1, true, false, 0);
        // Committed while the queue held something not yet written: kept.
        ledger.on_live_commit(&copy_of(4, 2, TfIndex::M1, 600, 2, 1), true, 1, 10);
        let busy = QueueProgress {
            written: 9,
            idle: false,
        };
        assert_eq!(ledger.forget_replayed(3, busy), 1);
        assert_eq!(ledger.len(), 3);
        // The queue passes the mark: the committed-only entry goes.
        let passed = QueueProgress {
            written: 10,
            idle: false,
        };
        assert_eq!(ledger.forget_replayed(3, passed), 1);
        assert_eq!(ledger.len(), 2);
    }

    #[test]
    fn boot_written_keeps_the_fullest_and_is_bounded() {
        let mut written = BootWritten::new(1);
        let c2 = copy_of(13, 2, TfIndex::M1, 600, 5, 40);
        assert!(written.record(&c2));
        assert!(written.is_older(&copy_of(13, 2, TfIndex::M1, 600, 4, 40)));
        assert!(!written.is_older(&c2));
        // Past the cap a new bar is untracked, and the tracked one stays.
        assert!(!written.record(&copy_of(13, 2, TfIndex::M1, 660, 1, 1)));
        assert!(written.is_older(&copy_of(13, 2, TfIndex::M1, 600, 4, 40)));
    }
}
