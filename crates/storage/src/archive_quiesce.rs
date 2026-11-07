//! The archive quiesce lock (plan item PR AA, step 1; owner Quote 51,
//! 2026-11-07).
//!
//! # What it is for
//!
//! The partition archiver copies an old partition to S3, checks the copy and
//! then drops the partition from the box. A row written into that partition
//! after the copy and before the drop would be dropped with it and exist
//! nowhere. The live drain only writes into the current hour, but four other
//! writers can post into an OLD hour:
//!
//! | Writer | Where | What it holds the lock around |
//! |---|---|---|
//! | Boot WAL refold | `dhan_feed_stack` (boot pass) | refold, flush, ack |
//! | WAL catch-up drain | `dhan_feed_stack` (each chunk) | refold, flush, ack |
//! | Text spill replay | `tick_spill_replay::replay_spill_dir` | one file, first POST to last ack |
//! | Framed depth spill replay | `depth_spill_frame::replay_framed_spill_dir` | one file, first POST to last ack |
//! | After-close depth pass | `deferred_depth_pass` (each bucket) | write, flush |
//!
//! Each writer takes the SHARED (read) side. The archiver takes the EXCLUSIVE
//! (write) side for one partition, from before its WAL snapshot to after its
//! drop (wired in PR AA step 2). While the archiver holds it, no writer that
//! can reach an old hour is posting. The live drain never takes this lock and
//! never waits on it.
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
//!   archive; outside it they wait at most [`WRITER_QUIESCE_WAIT`];
//! - the spill replays wait at most [`WRITER_QUIESCE_WAIT`] per file, then
//!   end the round with the files kept;
//! - the after-close depth pass runs on its own OS thread and waits for the
//!   one partition an archive pass may have in flight.
//!
//! # Complexity
//!
//! Every call is O(1): one semaphore operation, plus a timer when it waits.
//! Cold path only (once per replay file, per replay chunk, per depth bucket,
//! per archived partition); never per tick, never on the frame drain.

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
}

impl QuiesceWriter {
    /// Every writer that can be held, for the boot-time zero registration.
    pub const ALL: [Self; 4] = [
        Self::BootRefold,
        Self::CatchUp,
        Self::SpillReplay,
        Self::FramedSpillReplay,
    ];

    /// The stable `writer` label value.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::BootRefold => "boot_refold",
            Self::CatchUp => "catch_up",
            Self::SpillReplay => "spill_replay",
            Self::FramedSpillReplay => "framed_spill_replay",
        }
    }
}

/// One quiesce lock. The process uses ONE instance (see
/// [`hold_writers_for_archive`] and the shared accessors); tests build their
/// own so they never contend with the process lock.
#[derive(Debug)]
pub struct ArchiveQuiesce {
    lock: RwLock<()>,
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
        }
    }

    /// The archiver's side: waits at most `wait` for every writer to finish
    /// and holds them out until the guard is dropped. `None` on timeout; the
    /// queued request is then withdrawn, so writers behind it go ahead.
    pub async fn hold_writers(&self, wait: Duration) -> Option<RwLockWriteGuard<'_, ()>> {
        tokio::time::timeout(wait, self.lock.write()).await.ok()
    }

    /// A writer's side, waiting at most `wait`. `None` on timeout (an
    /// archiver holds or is queued for the lock).
    pub async fn shared(&self, wait: Duration) -> Option<RwLockReadGuard<'_, ()>> {
        tokio::time::timeout(wait, self.lock.read()).await.ok()
    }

    /// A writer's side without waiting. `None` while an archiver holds the
    /// lock or is queued for it.
    #[must_use]
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
}

/// The process's one quiesce lock.
static ARCHIVE_QUIESCE: ArchiveQuiesce = ArchiveQuiesce::new();

/// The archiver's side of the process lock: holds every writer that can post
/// into an old hour out, waiting at most `wait`. `None` on timeout.
pub async fn hold_writers_for_archive(wait: Duration) -> Option<RwLockWriteGuard<'static, ()>> {
    ARCHIVE_QUIESCE.hold_writers(wait).await
}

/// A writer's side of the process lock, waiting at most `wait`.
pub async fn archive_quiesce_shared(wait: Duration) -> Option<RwLockReadGuard<'static, ()>> {
    ARCHIVE_QUIESCE.shared(wait).await
}

/// A writer's side of the process lock, without waiting.
#[must_use]
pub fn try_archive_quiesce_shared() -> Option<RwLockReadGuard<'static, ()>> {
    ARCHIVE_QUIESCE.try_shared()
}

/// A writer's side of the process lock for a plain OS thread. See
/// [`ArchiveQuiesce::shared_blocking`].
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
pub fn note_quiesce_held(writer: QuiesceWriter) {
    metrics::counter!(QUIESCE_HELD_COUNTER, "writer" => writer.label()).increment(1);
}

/// Registers every `writer` series of [`QUIESCE_HELD_COUNTER`] at 0, so the
/// first real deferral is a visible change rather than a new series.
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
                "framed_spill_replay"
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
    fn test_counters_register_and_count_without_a_recorder() {
        register_quiesce_counters();
        for writer in QuiesceWriter::ALL {
            note_quiesce_held(writer);
        }
    }
}
