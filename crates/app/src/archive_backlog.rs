//! The archive copy work the drain hold counts (plan item PR AA step 6,
//! AA-5; owner Quote 51, 2026-11-07).
//!
//! # What it is
//!
//! After each daily archive pass, the daily leg publishes here the bytes of
//! old partitions the pass could have copied to S3 and dropped but left on
//! the box (`ArchiveRunSummary::archive_backlog_bytes`), whether free space
//! is still below the daily free-space target, and when it measured. The
//! drain hold reads it once a minute ([`held_backlog_bytes`]) and keeps the
//! box up while copy work is left.
//!
//! # When it holds the box
//!
//! Only when ALL of these are true ([`backlog_holds`]):
//! - the reading is fresh: measured under [`ARCHIVE_BACKLOG_FRESH_SECS`] ago,
//!   or a daily pass is running and started, or last dropped a partition
//!   (`partition_archive::last_archive_drop_unix`), under
//!   [`ARCHIVE_BACKLOG_RUNNING_MAX_SECS`] ago (a long pass does not let the
//!   reading go stale while it is still dropping);
//! - the bytes are above 0;
//! - free space was below the target when it was measured.
//!
//! The first pass of a process has no reading yet. Its start writes a
//! provisional one ([`PROVISIONAL_BACKLOG_BYTES`], free space from the
//! probe taken just before the pass), so the box stays up while that first
//! pass runs, for at most [`ARCHIVE_BACKLOG_RUNNING_MAX_SECS`]. Before, the
//! first evening pass ran with nothing holding the box, and the 16:30 stop
//! could cut it off. The pass's own result replaces it; a pass that did not
//! run its sweep removes it.
//!
//! A cross-verification hold is not counted. An hour whose rows are still in
//! flight is left out only when a pass REACHED it; an hour no pass reached
//! (not listed yet, or the pass was quiesce-busy, WAL-blocked or capped) is
//! counted. ⚠ CORRECTED 2026-11-09 (PR AA review finding 12): this line said
//! every in-flight hour was left out, which held for reached hours only. Since
//! then a depth hour no acknowledgement can clear before the next session
//! (depth rows stop at the graced end of the day's arrival window) is left out
//! whether or not a pass reached it
//! (`snapshot_gate::depth_hour_cannot_clear_tonight`), so the 15:00 depth hour
//! no longer holds the box until about 20:00. After midnight the previous
//! IST day's Muhurat status comes from the installed trading calendar, so the
//! hour stays out on an ordinary night (review of finding 12, same day: it
//! came back at 00:00 before); with no calendar installed the Muhurat end is
//! assumed and it is counted again (keeps more). Tick-sink hours still count.
//! `[partition_retention] drain_hold_counts_archive = false` means the daily
//! leg never publishes, so nothing here holds.
//!
//! # What it never does
//!
//! It never holds past 07:30 IST: the drain hold's own window and expiry
//! end there, and the daily leg stops passing then, so the reading goes
//! stale. A stuck backlog releases through the drain hold's stall rule: no
//! partition dropped and nothing else moving for an hour. ⚠ CHANGED
//! 2026-11-09 (PR AA-G, finding 10): this read "no bytes freed for an hour"
//! while the rule only saw the reading at the END of each pass, so a long
//! pass that kept dropping was judged stalled; an accepted DROP
//! (`partition_archive::archive_drops_total`) now counts as progress, and a
//! running pass stays fresh while it dropped one in the last two hours.
//! No page, no alarm, no EMF name: the gauge [`ARCHIVE_BACKLOG_GAUGE`] is
//! local `/metrics` only.
//!
//! # Cost
//!
//! One uncontended `Mutex` over a few words, written once per daily pass
//! and read once a minute. Cold, never on the frame drain.

use std::sync::Mutex;

/// Gauge: bytes of the last published archive backlog (0 when none). Local
/// `/metrics` only, registered at 0 by [`register_archive_backlog_gauge`].
pub const ARCHIVE_BACKLOG_GAUGE: &str = "tv_archive_backlog_bytes";

/// A reading older than this does not hold the box: three daily-leg polls
/// (300 s each), so one slow pass cannot let it lapse between two readings.
pub const ARCHIVE_BACKLOG_FRESH_SECS: i64 = 900;

/// While a daily pass runs, its start keeps the last reading fresh for at
/// most this long, so a pass that never returns cannot hold the box all
/// night through this alone. ⚠ CHANGED 2026-11-09 (PR AA-G, finding 10): its
/// latest accepted DROP restarts this, so the bound reads "running and no
/// partition dropped for two hours"; the 07:30 IST end of the hold is
/// unchanged.
pub const ARCHIVE_BACKLOG_RUNNING_MAX_SECS: i64 = 7_200;

/// The bytes of a provisional reading: the first pass of a process is
/// running and has not measured anything yet. No measured backlog reaches
/// this value.
pub const PROVISIONAL_BACKLOG_BYTES: u64 = u64::MAX;

/// How far a reading may sit ahead of the clock and still count (a small
/// step of the wall clock between the write and the read).
const ARCHIVE_BACKLOG_CLOCK_SKEW_SECS: i64 = 60;

/// One published reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BacklogReading {
    /// Bytes of partitions left on the box that a pass could act on.
    pub bytes: u64,
    /// Free space was below the daily free-space target at the measurement.
    pub below_target: bool,
    /// When it was measured (Unix seconds).
    pub at_unix: i64,
    /// When the daily pass now running started; `None` between passes.
    pub pass_running_since: Option<i64>,
}

/// The process-wide cell the daily leg writes and the drain hold reads.
#[derive(Debug, Default)]
pub struct ArchiveBacklog {
    inner: Mutex<Option<BacklogReading>>,
}

impl ArchiveBacklog {
    /// An empty cell. O(1).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Option<BacklogReading>) -> R) -> R {
        // A panic while the lock was held leaves only a few plain words
        // behind, so a poisoned lock is still safe to use.
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard)
    }

    /// Records that a daily pass started at `now_unix`, keeping the last
    /// reading. With no reading yet (the first pass of the process), it
    /// writes a provisional one: [`PROVISIONAL_BACKLOG_BYTES`], measured now,
    /// with `below_target` from the free-space probe taken before the pass.
    /// O(1).
    // TEST-EXEMPT: covered by test_regression_first_pass_on_an_empty_cell_holds_the_box
    pub fn note_pass_started(&self, now_unix: i64, below_target: bool) {
        self.with(|reading| match reading.as_mut() {
            Some(reading) => reading.pass_running_since = Some(now_unix),
            None => {
                *reading = Some(BacklogReading {
                    bytes: PROVISIONAL_BACKLOG_BYTES,
                    below_target,
                    at_unix: now_unix,
                    pass_running_since: Some(now_unix),
                });
            }
        });
    }

    /// Records the end of a daily pass that did not run its sweep (the
    /// other archive leg held the pass lock, or the archiver could not be
    /// built): the last reading stays, ageing from when it was measured. A
    /// provisional reading is removed: nothing was measured. O(1).
    // TEST-EXEMPT: covered by test_publish_note_and_reading_round_trip
    pub fn note_pass_ended(&self) {
        self.with(|slot| {
            if slot.is_some_and(|r| r.bytes == PROVISIONAL_BACKLOG_BYTES) {
                *slot = None;
            } else if let Some(reading) = slot.as_mut() {
                reading.pass_running_since = None;
            }
        });
    }

    /// Records the end of a daily pass. `bytes` is what the pass measured
    /// (`None` when it did not run or could not measure, which clears the
    /// reading so nothing holds on it); `below_target` whether free space is
    /// below the target. Sets [`ARCHIVE_BACKLOG_GAUGE`]. O(1).
    pub fn publish(&self, bytes: Option<u64>, below_target: bool, now_unix: i64) {
        self.with(|reading| {
            *reading = bytes.map(|bytes| BacklogReading {
                bytes,
                below_target,
                at_unix: now_unix,
                pass_running_since: None,
            });
        });
        #[allow(clippy::cast_precision_loss)]
        // APPROVED: a byte count shown as a gauge; a rounding above 2^53 bytes is harmless
        let shown = bytes.unwrap_or(0) as f64;
        metrics::gauge!(ARCHIVE_BACKLOG_GAUGE).set(shown);
    }

    /// The last reading, if any. O(1).
    #[must_use]
    pub fn reading(&self) -> Option<BacklogReading> {
        self.with(|reading| *reading)
    }
}

/// The one cell production uses.
pub static ARCHIVE_BACKLOG: ArchiveBacklog = ArchiveBacklog::new();

/// Registers [`ARCHIVE_BACKLOG_GAUGE`] at 0, so the series exists before
/// the first pass. O(1).
// TEST-EXEMPT: covered by test_held_backlog_bytes_reads_the_process_cell
pub fn register_archive_backlog_gauge() {
    metrics::gauge!(ARCHIVE_BACKLOG_GAUGE).set(0.0);
}

/// The bytes that keep the box up at `now_unix`, or `None` when the reading
/// does not hold it (absent, stale, 0 bytes, or free space at or above the
/// target). See the module docs. Pure, O(1).
#[must_use]
pub fn backlog_holds(
    reading: Option<BacklogReading>,
    last_drop_unix: Option<i64>,
    now_unix: i64,
) -> Option<u64> {
    let reading = reading?;
    let age_ok = |since: i64, max: i64| {
        let age = now_unix.saturating_sub(since);
        age < max && age > -ARCHIVE_BACKLOG_CLOCK_SKEW_SECS
    };
    let fresh = age_ok(reading.at_unix, ARCHIVE_BACKLOG_FRESH_SECS)
        || reading.pass_running_since.is_some_and(|since| {
            // PR AA-G (finding 10): a running pass also stays fresh from its
            // latest accepted DROP; a drop from before its start never counts.
            // ⚠ CHANGED 2026-11-11 (review): a drop stamped ahead of the clock
            // (recorded during a forward wall-clock step, then the clock
            // stepped back) is read as now, so a pass still dropping is not
            // judged stale for the size of the step; the next drop replaces
            // the stamp (`partition_archive` stores the latest, not the max).
            age_ok(since, ARCHIVE_BACKLOG_RUNNING_MAX_SECS)
                || last_drop_unix
                    .map(|at| at.min(now_unix))
                    .filter(|at| *at >= since)
                    .is_some_and(|at| age_ok(at, ARCHIVE_BACKLOG_RUNNING_MAX_SECS))
        });
    (fresh && reading.bytes > 0 && reading.below_target).then_some(reading.bytes)
}

/// [`backlog_holds`] over [`ARCHIVE_BACKLOG`] at `now_unix`. O(1).
#[must_use]
pub fn held_backlog_bytes(now_unix: i64) -> Option<u64> {
    backlog_holds(
        ARCHIVE_BACKLOG.reading(),
        tickvault_storage::partition_archive::last_archive_drop_unix(),
        now_unix,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_762_500_000;

    fn reading(bytes: u64, below_target: bool, at_unix: i64) -> BacklogReading {
        BacklogReading {
            bytes,
            below_target,
            at_unix,
            pass_running_since: None,
        }
    }

    /// [`backlog_holds`] with no drop recorded.
    fn holds(reading: Option<BacklogReading>, now_unix: i64) -> Option<u64> {
        backlog_holds(reading, None, now_unix)
    }

    #[test]
    fn test_hold_released_when_backlog_stale_or_zero_or_target_met() {
        // Fresh, above 0, below the target: holds with its bytes.
        assert_eq!(holds(Some(reading(5, true, NOW)), NOW), Some(5));
        assert_eq!(
            holds(
                Some(reading(5, true, NOW - ARCHIVE_BACKLOG_FRESH_SECS + 1)),
                NOW
            ),
            Some(5)
        );
        // Stale.
        assert_eq!(
            holds(
                Some(reading(5, true, NOW - ARCHIVE_BACKLOG_FRESH_SECS)),
                NOW
            ),
            None
        );
        // Zero bytes.
        assert_eq!(holds(Some(reading(0, true, NOW)), NOW), None);
        // Target met.
        assert_eq!(holds(Some(reading(5, false, NOW)), NOW), None);
        // Nothing published (the switch is off, or no pass measured).
        assert_eq!(holds(None, NOW), None);
        // A reading far ahead of the clock does not count.
        assert_eq!(holds(Some(reading(5, true, NOW + 3_600)), NOW), None);
        assert_eq!(holds(Some(reading(5, true, NOW + 30)), NOW), Some(5));
    }

    #[test]
    fn test_running_pass_keeps_the_reading_fresh_but_bounded() {
        let old = NOW - 2 * ARCHIVE_BACKLOG_FRESH_SECS;
        let running = |since: i64| BacklogReading {
            pass_running_since: Some(since),
            ..reading(9, true, old)
        };
        assert_eq!(holds(Some(running(NOW - 600)), NOW), Some(9));
        assert_eq!(
            holds(Some(running(NOW - ARCHIVE_BACKLOG_RUNNING_MAX_SECS)), NOW),
            None,
            "a pass that never returns stops holding"
        );
    }

    /// PR AA-G (finding 10): a pass still dropping partitions past two hours
    /// keeps its reading fresh; one that stopped dropping, or a drop from
    /// before the pass started, does not.
    #[test]
    fn test_regression_running_pass_that_keeps_dropping_stays_fresh_past_two_hours() {
        let old = NOW - 2 * ARCHIVE_BACKLOG_FRESH_SECS;
        let since = NOW - 3 * 3_600;
        let running = BacklogReading {
            pass_running_since: Some(since),
            ..reading(9, true, old)
        };
        assert_eq!(
            holds(Some(running), NOW),
            None,
            "no drop: stale after two hours"
        );
        assert_eq!(
            backlog_holds(Some(running), Some(NOW - 600), NOW),
            Some(9),
            "a drop ten minutes ago keeps it fresh"
        );
        assert_eq!(
            backlog_holds(
                Some(running),
                Some(NOW - ARCHIVE_BACKLOG_RUNNING_MAX_SECS),
                NOW
            ),
            None,
            "no drop for two hours: stale"
        );
        assert_eq!(
            backlog_holds(Some(running), Some(since - 1), NOW),
            None,
            "a drop before the pass started never counts"
        );
        // 2026-11-11 review: the case above is already refused by age alone.
        // This one is not: a pass start a little ahead of the clock (a step
        // back) with a recent drop from before it; only the start filter
        // refuses it.
        let started_ahead = BacklogReading {
            pass_running_since: Some(NOW + 120),
            ..running
        };
        assert_eq!(
            backlog_holds(Some(started_ahead), Some(NOW - 60), NOW),
            None,
            "a drop from before the pass started never counts"
        );
        let at_target = BacklogReading {
            below_target: false,
            ..running
        };
        assert_eq!(
            backlog_holds(Some(at_target), Some(NOW - 60), NOW),
            None,
            "free space at the target: nothing holds, whatever the drops say"
        );
        let between = BacklogReading {
            pass_running_since: None,
            ..running
        };
        assert_eq!(
            backlog_holds(Some(between), Some(NOW - 60), NOW),
            None,
            "a drop never refreshes an old reading between passes"
        );
    }

    // Regression: 2026-11-11 — PR AA-G review. A drop stamped during a
    // forward wall-clock step sat ahead of the clock once it stepped back,
    // and the age check refused it, so a long pass still dropping read stale.
    #[test]
    fn test_regression_backlog_holds_reads_a_future_drop_stamp_as_now() {
        let running = BacklogReading {
            pass_running_since: Some(NOW - 3 * 3_600),
            ..reading(9, true, NOW - 2 * ARCHIVE_BACKLOG_FRESH_SECS)
        };
        assert_eq!(
            backlog_holds(Some(running), Some(NOW + 600), NOW),
            Some(9),
            "a drop stamped ten minutes ahead still keeps the running pass fresh"
        );
        let between = BacklogReading {
            pass_running_since: None,
            ..running
        };
        assert_eq!(
            backlog_holds(Some(between), Some(NOW + 600), NOW),
            None,
            "it never refreshes a reading between passes"
        );
    }

    #[test]
    fn test_publish_note_and_reading_round_trip() {
        let cell = ArchiveBacklog::new();
        assert_eq!(cell.reading(), None);
        // A start with nothing published writes a provisional reading; a
        // pass that then did not run its sweep removes it.
        cell.note_pass_started(NOW, true);
        assert_eq!(
            cell.reading().map(|r| r.bytes),
            Some(PROVISIONAL_BACKLOG_BYTES)
        );
        cell.note_pass_ended();
        assert_eq!(cell.reading(), None);
        cell.publish(Some(42), true, NOW);
        assert_eq!(cell.reading(), Some(reading(42, true, NOW)));
        cell.note_pass_started(NOW + 300, false);
        assert_eq!(
            cell.reading().and_then(|r| r.pass_running_since),
            Some(NOW + 300)
        );
        cell.note_pass_ended();
        assert_eq!(cell.reading(), Some(reading(42, true, NOW)));
        cell.publish(Some(7), false, NOW + 600);
        assert_eq!(cell.reading(), Some(reading(7, false, NOW + 600)));
        // A pass that measured nothing clears the reading.
        cell.publish(None, true, NOW + 900);
        assert_eq!(cell.reading(), None);
    }

    /// PR AA finding 5: every process starts with the cell empty, so the
    /// first evening pass ran with nothing holding the box, and the 16:30
    /// stop could cut it off. The start now writes a provisional reading.
    #[test]
    fn test_regression_first_pass_on_an_empty_cell_holds_the_box() {
        let cell = ArchiveBacklog::new();
        cell.note_pass_started(NOW, true);
        assert_eq!(
            holds(cell.reading(), NOW + 600),
            Some(PROVISIONAL_BACKLOG_BYTES),
            "the first pass, below the target, holds the box while it runs"
        );
        assert_eq!(
            holds(cell.reading(), NOW + ARCHIVE_BACKLOG_RUNNING_MAX_SECS),
            None,
            "bounded like any running pass"
        );
        // The pass's result replaces the provisional reading.
        cell.publish(Some(3), true, NOW + 900);
        assert_eq!(holds(cell.reading(), NOW + 900), Some(3));

        // Free space at or above the target before the pass: nothing holds.
        let above = ArchiveBacklog::new();
        above.note_pass_started(NOW, false);
        assert_eq!(holds(above.reading(), NOW + 600), None);
    }

    #[test]
    fn test_held_backlog_bytes_reads_the_process_cell() {
        register_archive_backlog_gauge();
        // The process cell is shared by tests in this binary; only the
        // shape is asserted: a stale clock never holds.
        assert_eq!(held_backlog_bytes(i64::MIN / 2), None);
    }
}
