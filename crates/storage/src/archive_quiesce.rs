//! The archive quiesce lock (plan item PR AA, step 1; owner Quote 51,
//! 2026-11-07).
//!
//! # What it is for
//!
//! The partition archiver copies an old partition to S3, checks the copy and
//! then drops the partition from the box. A row written into that partition
//! after the copy and before the drop would be dropped with it and exist
//! nowhere. The live drain only writes into the current hour, but six other
//! writers can post into an OLD hour:
//!
//! | Writer | Where | What it holds the lock around |
//! |---|---|---|
//! | Boot WAL refold | `dhan_feed_stack` (boot pass) | refold, flush, ack |
//! | WAL catch-up drain | `dhan_feed_stack` (each chunk) | refold, flush, ack |
//! | Text spill replay | `tick_spill_replay::replay_spill_dir` | one file, first POST to last ack |
//! | Framed depth spill replay | `depth_spill_frame::replay_framed_spill_dir` | one file, first POST to last ack |
//! | After-close depth pass | `deferred_depth_pass` (each bucket) | write, flush |
//! | After-close contract name repair | `contract_name_repair` (each table of each day) | re-count, `INSERT … SELECT` |
//!
//! Each writer takes the SHARED (read) side. The archiver takes the EXCLUSIVE
//! (write) side for one partition, from before its WAL snapshot to after its
//! drop (wired in PR AA step 2), but only for some partitions: every
//! partition that goes through the snapshot gate (the depth tables, and each
//! table on the hour path in that pass), and every partition of a table the
//! contract name repair can rewrite
//! ([`crate::contract_name_repair::repair_may_rewrite`]: `ticks`,
//! `feed_aux_packets` and the candle tables), on the day path too (recheck
//! of PR AA finding 1). Any other table's day-path partition is archived
//! without the lock, under the exact WAL gate and the re-count. While the
//! archiver holds it, no writer that can reach an old hour is posting. The
//! live drain never takes this lock and never waits on it.
//!
//! # Why tokio's `RwLock`
//!
//! It is FAIR (first come, first served). A writer that arrives after a
//! queued archiver waits behind it, so a steady stream of replay files cannot
//! starve the archiver. And a queued archiver that gives up (its wait timed
//! out, so its future is dropped) leaves the queue and hands back what it had
//! reserved, so the writers behind it go ahead. Both are pinned by tests
//! below.
//!
//! # Waits
//!
//! Every wait is bounded, and a writer that does not get the lock defers its
//! work (nothing is lost; the frames or files are offered again later):
//! - the boot refold and the catch-up drain only TRY inside the capture
//!   window (09:00 to 15:40 IST), so the morning boot never waits on an
//!   archive; outside it the boot refold waits at most
//!   [`WRITER_QUIESCE_WAIT`] and each catch-up chunk waits until the drain's
//!   own deadline (PR AA review: one partition can hold the lock longer
//!   than 60 s, and a chunk miss stops the drain). Both register a
//!   [`ReplayIntent`] for the whole replay first, so the archiver finishes
//!   the partition it holds and starts no other until the replay is done;
//!   the pass counts that as quiesce-busy and stands aside;
//! - the spill replays wait at most [`WRITER_QUIESCE_WAIT`] per file, then
//!   end the round with the files kept;
//! - the after-close depth pass runs on its own OS thread and waits for the
//!   one partition an archive pass may have in flight;
//! - the after-close contract name repair (from 15:45 IST) waits at most
//!   [`WRITER_QUIESCE_WAIT`] per table, then leaves that table, and so the
//!   day, for its next attempt.
//!
//! # Complexity
//!
//! Every call is O(1): one semaphore operation, plus a timer when it waits.
//! Cold path only (once per replay file, per replay chunk, per depth bucket,
//! per archived partition); never per tick, never on the frame drain.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use tickvault_common::constants::{
    TICK_PERSIST_END_SECS_OF_DAY_IST, TICK_PERSIST_START_SECS_OF_DAY_IST,
};

/// The longest a replay writer waits for an archiver to finish one partition,
/// outside the capture window. A miss defers the writer's work; nothing is
/// lost.
pub const WRITER_QUIESCE_WAIT: Duration = Duration::from_secs(WRITER_QUIESCE_WAIT_SECS);

/// [`WRITER_QUIESCE_WAIT`] in seconds.
pub const WRITER_QUIESCE_WAIT_SECS: u64 = 60;

/// Counter for a writer that did not get the lock and deferred its work.
/// Local `/metrics` only: no page, no alarm, no metric filter, no EMF name.
pub const QUIESCE_HELD_COUNTER: &str = "tv_partition_archive_quiesce_held_total";

/// A writer that can post rows into an old hour, as the `writer` label of
/// [`QUIESCE_HELD_COUNTER`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuiesceWriter {
    /// The boot WAL refold.
    BootRefold,
    /// One chunk of the WAL catch-up drain.
    CatchUp,
    /// One text spill file (`*.ilp`).
    SpillReplay,
    /// One framed depth spill file (`*.dspl`).
    FramedSpillReplay,
    /// The after-close contract name repair, one table of one day.
    ContractRepair,
}

impl QuiesceWriter {
    /// Every writer that can be held, for the boot-time zero registration.
    pub const ALL: [Self; 5] = [
        Self::BootRefold,
        Self::CatchUp,
        Self::SpillReplay,
        Self::FramedSpillReplay,
        Self::ContractRepair,
    ];

    /// The stable `writer` label value.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::BootRefold => "boot_refold",
            Self::CatchUp => "catch_up",
            Self::SpillReplay => "spill_replay",
            Self::FramedSpillReplay => "framed_spill_replay",
            Self::ContractRepair => "contract_repair",
        }
    }
}

/// One quiesce lock. The process uses ONE instance (see
/// [`hold_writers_for_archive`] and the shared accessors); tests build their
/// own so they never contend with the process lock.
#[derive(Debug)]
pub struct ArchiveQuiesce {
    lock: RwLock<()>,
    /// Old-hour replays that have said they are about to write (PR AA
    /// review): while any is registered the archiver does not START a new
    /// partition ([`Self::hold_writers`] refuses at once), so a replay
    /// waits behind at most the one partition already being copied.
    replay_intents: AtomicUsize,
}

/// A registered old-hour replay ([`ArchiveQuiesce::intend_replay`]). While
/// it lives the archiver starts no new partition; dropping it lets the
/// archiver in again. O(1).
#[derive(Debug)]
pub struct ReplayIntent<'q> {
    quiesce: &'q ArchiveQuiesce,
}

impl Drop for ReplayIntent<'_> {
    fn drop(&mut self) {
        self.quiesce.replay_intents.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Default for ArchiveQuiesce {
    fn default() -> Self {
        Self::new()
    }
}

impl ArchiveQuiesce {
    /// A free lock. `const` so the process instance needs no lazy setup.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            lock: RwLock::const_new(()),
            replay_intents: AtomicUsize::new(0),
        }
    }

    /// The archiver's side: waits at most `wait` for every writer to finish
    /// and holds them out until the guard is dropped. `None` on timeout; the
    /// queued request is then withdrawn, so writers behind it go ahead.
    ///
    /// PR AA review: also `None` AT ONCE while an old-hour replay is
    /// registered ([`Self::intend_replay`]). One archive partition can hold
    /// the lock for many minutes (two snapshot waits and an export), longer
    /// than a replay chunk can wait, so without this a replay that arrived
    /// during one partition missed it and found the archiver queued again
    /// for the next one. The pass counts it as quiesce-busy and stands aside.
    pub async fn hold_writers(&self, wait: Duration) -> Option<RwLockWriteGuard<'_, ()>> {
        if self.replay_intents.load(Ordering::Acquire) > 0 {
            return None;
        }
        tokio::time::timeout(wait, self.lock.write()).await.ok()
    }

    /// Registers an old-hour replay that is about to write (PR AA review).
    /// Hold the returned guard for the whole replay (the boot refold and the
    /// WAL catch-up drain): the archiver finishes the partition it may
    /// already hold and starts no other until the guard is dropped. O(1).
    #[must_use]
    // TEST-EXEMPT: covered by test_regression_archiver_stands_aside_while_a_replay_is_registered
    pub fn intend_replay(&self) -> ReplayIntent<'_> {
        self.replay_intents.fetch_add(1, Ordering::AcqRel);
        ReplayIntent { quiesce: self }
    }

    /// A writer's side, waiting at most `wait`. `None` on timeout (an
    /// archiver holds or is queued for the lock).
    pub async fn shared(&self, wait: Duration) -> Option<RwLockReadGuard<'_, ()>> {
        tokio::time::timeout(wait, self.lock.read()).await.ok()
    }

    /// A writer's side without waiting. `None` while an archiver holds the
    /// lock or is queued for it.
    #[must_use]
    // TEST-EXEMPT: covered by test_free_lock_lets_writers_share_and_the_archiver_in
    pub fn try_shared(&self) -> Option<RwLockReadGuard<'_, ()>> {
        self.lock.try_read().ok()
    }

    /// A writer's side for a plain OS thread (the after-close depth pass):
    /// waits in the queue, behind at most the one partition an archive pass
    /// has in flight. Must NOT be called inside an async context (tokio
    /// panics there). Cold path, once per after-close bucket.
    pub fn shared_blocking(&self) -> RwLockReadGuard<'_, ()> {
        self.lock.blocking_read()
    }

    /// The replay writers' rule: inside the capture window (09:00 to 15:40
    /// IST) TRY only, so the morning boot never waits on an archive; outside
    /// it wait at most [`WRITER_QUIESCE_WAIT`]. A miss is counted on
    /// [`QUIESCE_HELD_COUNTER`] with the writer's label, and the caller
    /// defers its work.
    pub async fn for_replay(
        &self,
        now_secs_of_day_ist: u64,
        writer: QuiesceWriter,
    ) -> Option<RwLockReadGuard<'_, ()>> {
        let guard = match replay_quiesce_wait(now_secs_of_day_ist) {
            None => self.try_shared(),
            Some(wait) => self.shared(wait).await,
        };
        if guard.is_none() {
            note_quiesce_held(writer);
        }
        guard
    }

    /// [`Self::for_replay`] for a writer with its own deadline (the WAL
    /// catch-up drain, PR AA review): inside the capture window TRY only, as
    /// before; outside it wait until `deadline` instead of 60 s. One
    /// archive partition can hold the lock for longer than 60 s, and a miss
    /// stops the drain for the process, so the drain now waits out the
    /// partition in progress, charged against its own clock. A miss is
    /// counted as before.
    // TEST-EXEMPT: covered by test_catchup_chunk_outlasts_a_long_archive_partition
    pub async fn for_replay_until(
        &self,
        now_secs_of_day_ist: u64,
        writer: QuiesceWriter,
        deadline: tokio::time::Instant,
    ) -> Option<RwLockReadGuard<'_, ()>> {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let guard = match replay_quiesce_wait_until(now_secs_of_day_ist, left) {
            None => self.try_shared(),
            Some(wait) => self.shared(wait).await,
        };
        if guard.is_none() {
            note_quiesce_held(writer);
        }
        guard
    }
}

/// How long a writer with `time_left` before its own deadline may wait at
/// `now_secs_of_day_ist` (PR AA review): `None` (try only) inside the
/// capture window, else all of `time_left`. Pure, O(1).
#[must_use]
// TEST-EXEMPT: covered by test_catchup_wait_runs_to_its_own_deadline_outside_the_window
pub fn replay_quiesce_wait_until(
    now_secs_of_day_ist: u64,
    time_left: Duration,
) -> Option<Duration> {
    replay_quiesce_wait(now_secs_of_day_ist).map(|_| time_left)
}

/// Registers an old-hour replay on the process lock. See
/// [`ArchiveQuiesce::intend_replay`].
#[must_use]
// TEST-EXEMPT: process-lock wrapper; intend_replay covered by test_regression_archiver_stands_aside_while_a_replay_is_registered
pub fn intend_old_hour_replay() -> ReplayIntent<'static> {
    ARCHIVE_QUIESCE.intend_replay()
}

/// The process lock under the catch-up drain's rule. See
/// [`ArchiveQuiesce::for_replay_until`].
// TEST-EXEMPT: process-lock wrapper; for_replay_until covered by test_catchup_chunk_outlasts_a_long_archive_partition
pub async fn archive_quiesce_for_replay_until(
    now_secs_of_day_ist: u64,
    writer: QuiesceWriter,
    deadline: tokio::time::Instant,
) -> Option<RwLockReadGuard<'static, ()>> {
    ARCHIVE_QUIESCE
        .for_replay_until(now_secs_of_day_ist, writer, deadline)
        .await
}

/// The process's one quiesce lock.
static ARCHIVE_QUIESCE: ArchiveQuiesce = ArchiveQuiesce::new();

/// The archiver's side of the process lock: holds every writer that can post
/// into an old hour out, waiting at most `wait`. `None` on timeout.
// TEST-EXEMPT: covered by test_process_accessors_take_and_release_the_one_lock
pub async fn hold_writers_for_archive(wait: Duration) -> Option<RwLockWriteGuard<'static, ()>> {
    ARCHIVE_QUIESCE.hold_writers(wait).await
}

/// A writer's side of the process lock, waiting at most `wait`.
pub async fn archive_quiesce_shared(wait: Duration) -> Option<RwLockReadGuard<'static, ()>> {
    ARCHIVE_QUIESCE.shared(wait).await
}

/// A writer's side of the process lock, without waiting. Test-only (PR AA
/// finding 18): no production writer takes the lock without the writers'
/// try-only-in-session rule and the held counter, which this skipped.
#[cfg(test)]
#[must_use]
fn try_archive_quiesce_shared() -> Option<RwLockReadGuard<'static, ()>> {
    ARCHIVE_QUIESCE.try_shared()
}

/// A writer's side of the process lock for a plain OS thread. See
/// [`ArchiveQuiesce::shared_blocking`].
// TEST-EXEMPT: covered by test_process_accessors_take_and_release_the_one_lock
pub fn archive_quiesce_shared_blocking() -> RwLockReadGuard<'static, ()> {
    ARCHIVE_QUIESCE.shared_blocking()
}

/// The process lock under the replay writers' rule. See
/// [`ArchiveQuiesce::for_replay`].
pub async fn archive_quiesce_for_replay(
    now_secs_of_day_ist: u64,
    writer: QuiesceWriter,
) -> Option<RwLockReadGuard<'static, ()>> {
    ARCHIVE_QUIESCE
        .for_replay(now_secs_of_day_ist, writer)
        .await
}

/// The process lock itself, for the spill replays (which take an
/// `&ArchiveQuiesce` so their tests can pass a private one).
// TEST-EXEMPT: covered by test_process_accessors_take_and_release_the_one_lock
pub(crate) fn process_archive_quiesce() -> &'static ArchiveQuiesce {
    &ARCHIVE_QUIESCE
}

/// How long a replay writer may wait at `now_secs_of_day_ist`: `None` (try
/// only) inside the capture window 09:00 to 15:40 IST, else
/// [`WRITER_QUIESCE_WAIT`]. Pure, O(1).
#[must_use]
pub fn replay_quiesce_wait(now_secs_of_day_ist: u64) -> Option<Duration> {
    let start = u64::from(TICK_PERSIST_START_SECS_OF_DAY_IST);
    let end = u64::from(TICK_PERSIST_END_SECS_OF_DAY_IST);
    if (start..end).contains(&now_secs_of_day_ist) {
        None
    } else {
        Some(WRITER_QUIESCE_WAIT)
    }
}

/// Counts one deferral on [`QUIESCE_HELD_COUNTER`]. O(1); the label values
/// are literals, so the counter stays on its non-allocating arm.
// TEST-EXEMPT: covered by test_counters_register_and_count_without_a_recorder_no_panic
pub fn note_quiesce_held(writer: QuiesceWriter) {
    metrics::counter!(QUIESCE_HELD_COUNTER, "writer" => writer.label()).increment(1);
}

/// Registers every `writer` series of [`QUIESCE_HELD_COUNTER`] at 0, so the
/// first real deferral is a visible change rather than a new series.
// TEST-EXEMPT: covered by test_counters_register_and_count_without_a_recorder_no_panic
pub fn register_quiesce_counters() {
    for writer in QuiesceWriter::ALL {
        metrics::counter!(QUIESCE_HELD_COUNTER, "writer" => writer.label()).increment(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IN_SESSION: u64 = 10 * 3600;
    const AFTER_CLOSE: u64 = 16 * 3600;

    #[test]
    fn test_writer_labels_are_stable_and_distinct() {
        let labels: Vec<&str> = QuiesceWriter::ALL.iter().map(|w| w.label()).collect();
        assert_eq!(
            labels,
            [
                "boot_refold",
                "catch_up",
                "spill_replay",
                "framed_spill_replay",
                "contract_repair"
            ]
        );
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len());
    }

    /// Inside 09:00 to 15:40 IST a replay writer only tries; at and after
    /// 15:40 and before 09:00 it may wait 60 s. Both edges are checked.
    #[test]
    fn test_replay_quiesce_wait_is_try_only_in_the_capture_window() {
        let start = u64::from(TICK_PERSIST_START_SECS_OF_DAY_IST);
        let end = u64::from(TICK_PERSIST_END_SECS_OF_DAY_IST);
        assert_eq!(WRITER_QUIESCE_WAIT.as_secs(), 60, "the design names 60 s");
        assert_eq!(replay_quiesce_wait(start), None);
        assert_eq!(replay_quiesce_wait(IN_SESSION), None);
        assert_eq!(replay_quiesce_wait(end - 1), None);
        assert_eq!(replay_quiesce_wait(end), Some(WRITER_QUIESCE_WAIT));
        assert_eq!(replay_quiesce_wait(start - 1), Some(WRITER_QUIESCE_WAIT));
        assert_eq!(replay_quiesce_wait(0), Some(WRITER_QUIESCE_WAIT));
        assert_eq!(replay_quiesce_wait(86_399), Some(WRITER_QUIESCE_WAIT));
        // A bad clock reading past the end of the day still waits, bounded.
        assert_eq!(replay_quiesce_wait(u64::MAX), Some(WRITER_QUIESCE_WAIT));
    }

    /// PR AA review: while an old-hour replay is registered the archiver
    /// starts no new partition, even on a free lock; once the replay drops
    /// its intent the archiver gets in again.
    #[tokio::test]
    async fn test_regression_archiver_stands_aside_while_a_replay_is_registered() {
        let q = ArchiveQuiesce::new();
        let intent = q.intend_replay();
        let started = tokio::time::Instant::now();
        assert!(
            q.hold_writers(Duration::from_secs(5)).await.is_none(),
            "a registered replay keeps the archiver from starting a partition"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "it refuses at once, without queueing"
        );
        let second = q.intend_replay();
        drop(intent);
        assert!(q.hold_writers(Duration::from_millis(10)).await.is_none());
        drop(second);
        assert!(q.hold_writers(Duration::from_millis(10)).await.is_some());
    }

    /// PR AA review: a catch-up chunk outside the capture window waits until
    /// its own deadline, longer than the old 60 s, so it outlasts one
    /// archive partition; inside the window it still only tries.
    #[test]
    fn test_catchup_wait_runs_to_its_own_deadline_outside_the_window() {
        let left = Duration::from_secs(900);
        assert_eq!(replay_quiesce_wait_until(AFTER_CLOSE, left), Some(left));
        assert!(replay_quiesce_wait_until(AFTER_CLOSE, left) > Some(WRITER_QUIESCE_WAIT));
        assert_eq!(replay_quiesce_wait_until(IN_SESSION, left), None);
        assert_eq!(
            replay_quiesce_wait_until(AFTER_CLOSE, Duration::ZERO),
            Some(Duration::ZERO)
        );
    }

    /// PR AA review: a writer queued while the archiver holds one partition
    /// gets in when it is released (it waits to its own deadline, not a
    /// fixed 60 s; see the pure test above), and the registered replay keeps
    /// the archiver from taking the next one.
    #[tokio::test]
    async fn test_catchup_chunk_outlasts_a_long_archive_partition() {
        let q: &'static ArchiveQuiesce = Box::leak(Box::new(ArchiveQuiesce::new()));
        let held = q.hold_writers(Duration::from_millis(10)).await;
        assert!(held.is_some());
        let intent = q.intend_replay();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let writer = tokio::spawn(async move {
            q.for_replay_until(AFTER_CLOSE, QuiesceWriter::CatchUp, deadline)
                .await
                .is_some()
        });
        // The partition in progress finishes a while later.
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(held);
        assert!(writer.await.expect("writer task"), "the chunk got in");
        assert!(
            q.hold_writers(Duration::from_millis(10)).await.is_none(),
            "no next partition while the replay is registered"
        );
        drop(intent);
    }

    #[tokio::test]
    async fn test_free_lock_lets_writers_share_and_the_archiver_in() {
        let q = ArchiveQuiesce::new();
        let a = q.try_shared();
        let b = q.shared(Duration::from_millis(10)).await;
        assert!(a.is_some() && b.is_some(), "writers share the free lock");
        drop((a, b));
        let held = q.hold_writers(Duration::from_millis(10)).await;
        assert!(held.is_some(), "a free lock lets the archiver in");
    }

    /// A writer holding the lock keeps the archiver out until the timeout,
    /// and the archiver's timed-out request leaves the queue: a writer that
    /// arrives after it still gets in at once.
    #[tokio::test]
    async fn test_archiver_write_times_out_against_held_reader() {
        let q = ArchiveQuiesce::new();
        let reader = q.try_shared();
        assert!(reader.is_some());
        let started = tokio::time::Instant::now();
        let archiver = q.hold_writers(Duration::from_millis(50)).await;
        assert!(archiver.is_none(), "a held reader keeps the archiver out");
        assert!(started.elapsed() >= Duration::from_millis(50));
        // The timed-out archiver left the queue: a new writer is not held.
        assert!(
            q.try_shared().is_some(),
            "a timed-out archiver left the queue"
        );
        drop(reader);
        assert!(q.hold_writers(Duration::from_millis(10)).await.is_some());
    }

    /// Fairness: once the archiver is queued (a writer still holds the lock),
    /// a writer arriving after it waits behind it, and gets in only after the
    /// archiver has had its turn.
    #[tokio::test]
    async fn test_quiesce_writer_waits_behind_queued_archiver() {
        let q: &'static ArchiveQuiesce = Box::leak(Box::new(ArchiveQuiesce::new()));
        let first_writer = q.try_shared();
        assert!(first_writer.is_some());
        let (got_tx, got_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let archiver = tokio::spawn(async move {
            let guard = q.hold_writers(Duration::from_secs(30)).await;
            assert!(
                guard.is_some(),
                "the archiver gets in once the writer leaves"
            );
            let _ = got_tx.send(());
            let _ = release_rx.await;
            drop(guard);
        });
        // Let the archiver queue.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(
            q.try_shared().is_none(),
            "a writer arriving after a queued archiver must not jump the queue"
        );
        assert!(
            q.shared(Duration::from_millis(20)).await.is_none(),
            "a waiting writer stays behind the queued archiver"
        );
        drop(first_writer);
        got_rx.await.expect("archiver got the lock");
        assert!(
            q.try_shared().is_none(),
            "the archiver holds the writers out"
        );
        let _ = release_tx.send(());
        archiver.await.expect("archiver task");
        assert!(
            q.try_shared().is_some(),
            "writers go ahead after the archiver"
        );
    }

    /// The boot refold in session never waits: with an archiver holding the
    /// lock, the in-session rule returns at once with `None`.
    #[tokio::test]
    async fn test_boot_refold_in_session_never_waits_on_quiesce() {
        let q = ArchiveQuiesce::new();
        let archiver = q.hold_writers(Duration::from_millis(10)).await;
        assert!(archiver.is_some());
        let started = std::time::Instant::now();
        let guard = q.for_replay(IN_SESSION, QuiesceWriter::BootRefold).await;
        assert!(guard.is_none(), "in session the refold only tries");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "in session the refold must not wait, took {:?}",
            started.elapsed()
        );
        drop(archiver);
        assert!(
            q.for_replay(IN_SESSION, QuiesceWriter::BootRefold)
                .await
                .is_some(),
            "a free lock is taken in session"
        );
    }

    /// Outside the capture window a replay writer waits for an archiver to
    /// finish, and gets the lock once it is released.
    #[tokio::test]
    async fn test_replay_after_close_waits_for_the_archiver_then_proceeds() {
        let q: &'static ArchiveQuiesce = Box::leak(Box::new(ArchiveQuiesce::new()));
        let archiver = q.hold_writers(Duration::from_millis(10)).await;
        assert!(archiver.is_some());
        let writer = tokio::spawn(async move {
            q.for_replay(AFTER_CLOSE, QuiesceWriter::CatchUp)
                .await
                .is_some()
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            !writer.is_finished(),
            "the writer waits while the archiver holds"
        );
        drop(archiver);
        assert!(
            writer.await.expect("writer task"),
            "the writer proceeds after"
        );
    }

    /// The blocking accessor works from a plain thread and waits for a held
    /// archiver.
    #[test]
    fn test_shared_blocking_waits_for_the_archiver_on_a_plain_thread() {
        let q: &'static ArchiveQuiesce = Box::leak(Box::new(ArchiveQuiesce::new()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        let archiver = runtime.block_on(q.hold_writers(Duration::from_millis(10)));
        assert!(archiver.is_some());
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _g = q.shared_blocking();
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "the depth pass waits while the archiver holds"
        );
        drop(archiver);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the depth pass proceeds after the archiver");
        thread.join().expect("thread");
    }

    /// Regression, PR AA finding 18: the bare no-wait shared accessor had no
    /// production caller and was public; it is test-only now.
    #[test]
    fn test_regression_no_public_try_shared_accessor() {
        let code = include_str!("archive_quiesce.rs");
        let needle = ["pub fn try_archive", "_quiesce_shared("].concat();
        assert!(!code.contains(&needle));
    }

    /// The process accessors work. Held only for an instant, so other tests
    /// in this binary that replay spill files are not delayed.
    #[tokio::test]
    async fn test_process_accessors_take_and_release_the_one_lock() {
        let shared = archive_quiesce_shared(WRITER_QUIESCE_WAIT).await;
        assert!(shared.is_some());
        drop(shared);
        assert!(try_archive_quiesce_shared().is_some());
        let replay = archive_quiesce_for_replay(AFTER_CLOSE, QuiesceWriter::SpillReplay).await;
        assert!(replay.is_some());
        drop(replay);
        let blocking = tokio::task::spawn_blocking(|| {
            drop(archive_quiesce_shared_blocking());
        })
        .await;
        assert!(blocking.is_ok());
        let held = hold_writers_for_archive(WRITER_QUIESCE_WAIT).await;
        assert!(held.is_some());
        drop(held);
        assert!(std::ptr::eq(process_archive_quiesce(), &ARCHIVE_QUIESCE));
    }

    /// With no recorder installed both calls are no-ops and must not panic.
    /// The rendered series (every label at 0, then one deferral counted) is
    /// asserted with a real Prometheus recorder by the app crate's
    /// `daily_archive_boot::test_archive_quiesce_counters_seed_at_zero_and_count`
    /// (storage has no exporter dev-dependency).
    #[test]
    fn test_counters_register_and_count_without_a_recorder_no_panic() {
        register_quiesce_counters();
        for writer in QuiesceWriter::ALL {
            note_quiesce_held(writer);
        }
    }
}
