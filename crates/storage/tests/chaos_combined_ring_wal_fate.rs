//! Combined-fault chaos: ring full + WAL writer failing, a reconnect storm into
//! one WAL, and the rescue-floor table saturated under watermark advances
//! (2026-10-30, track `ring-wal-fate`).
//!
//! Each test drives REAL public APIs of `tickvault-storage` with several faults
//! at once, and checks the honest envelope: no uncounted loss, no double count,
//! no duplicate frame, counters move with the returned outcomes.
//!
//! A process-wide test metrics recorder is installed first (a small in-file
//! `metrics::Recorder` over `AtomicU64` counters), so every counter assertion
//! reads the real series the production code increments. Tests that read the
//! frame-fate counters hold `FATE_LOCK`, because those handles are global.
//!
//! # Scenarios
//!
//! - **S6a ring shed + WAL loss, both orders, every shed kind.** Shed then
//!   lost, lost then shed, for a ring shed and a drain depth shed: each real
//!   loss is counted exactly once, on `tv_wal_shed_frames_lost_total{shed}`
//!   and (ring only) `tv_ticks_lost_total{source="wal_lost_after_shed"}`; a
//!   drain that finds the record already lost is refused and writes its rows
//!   (`tv_wal_drain_shed_refused_total{reason="wal_lost"}`), nothing lost.
//! - **S6b many threads, many frames, no collisions.** 8 shed threads and 8
//!   loss threads race over 2,048 frames per round, 12 rounds, mixed fates
//!   (shed + lost, lost only, shed only, drain shed + lost). Every frame whose
//!   shed and loss both happened is counted exactly once, whichever thread
//!   finished the pair; the counters equal the returned outcomes.
//! - **S6c colliding slots.** A table of 8 slots, 512 frames (64 per slot),
//!   sheds and losses in opposite orders on 16 threads: a loss is never
//!   counted twice, a recorded shed is never followed by a "not shed" loss,
//!   and every `LostFate::Unknown` moved `tv_wal_lost_fate_unknown_total`.
//! - **S6d shutdown scan racing the abandoned writer.** `count_unflushed_sheds`
//!   runs while 4 threads report the same frames lost: each shed frame above
//!   the flush is counted exactly once, by the scan or by the writer.
//! - **S6e a WAL loss blocks the rescue deferral.** After the writer loses a
//!   record, `rescue_may_defer_to_wal` refuses any batch at or below it, even
//!   when the flushed mark is past the batch.
//! - **S6f (ignored, DEFECT) lost, slot reused, then ring shed.** The one
//!   ordering in which a frame shed by the ring after its WAL loss is counted
//!   nowhere (neither as lost nor as fate unknown). Unreachable at the
//!   production table size; see the test.
//! - **S1 reconnect storm.** 16 producer threads append to one `WsFrameSpill`,
//!   each dropping and re-taking its handle every 250 frames (a socket
//!   redialling), while the writer drains. After shutdown, `replay_all`
//!   returns every `Spilled` frame exactly once, each producer's frames in
//!   append order, every sequence unique.
//! - **OOB rescue floors saturated under advancing acks.** 40 holders, 20 per sink
//!   (each sink has `RESCUE_FLOOR_SLOTS` = 16), hold floors while ackers advance both
//!   watermarks past them and holders persist and reload the file: the
//!   persisted watermark never reaches a held floor, every refused hold marks
//!   its range unapplied (still unapplied in the file after the acks pass it)
//!   and is counted on `tv_wal_rescue_floor_full_total`, and once every floor
//!   is released the persisted watermark passes the last batch.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};
use tickvault_storage::wal_applied_watermark::{
    AppliedSink, AppliedSnapshot, AppliedWatermark, RESCUE_FLOOR_FULL_COUNTER, RESCUE_FLOOR_SLOTS,
    WATERMARK_DURABILITY_LAG_SECS,
};
use tickvault_storage::wal_frame_fate::{
    DRAIN_SHED_REFUSED_COUNTER, FATE_UNKNOWN_COUNTER, FrameFate, LostFate, SHED_LOST_COUNTER,
    ShedKind, ShedMark, TICKS_LOST_SOURCE, note_flushed_through, rescue_may_defer_to_wal,
};
use tickvault_storage::ws_frame_spill::{
    AppendOutcome, PACKET_INDEX_BITS, WsFrameSpill, WsType, replay_all,
};

// ---------------------------------------------------------------------------
// Test metrics recorder
// ---------------------------------------------------------------------------

#[derive(Default)]
struct TestRecorder {
    counters: Mutex<HashMap<String, Arc<AtomicU64>>>,
}

fn key_string(name: &str, labels: &[(&str, &str)]) -> String {
    let mut pairs: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    pairs.sort();
    format!("{name}{{{}}}", pairs.join(","))
}

impl TestRecorder {
    fn handle(&self, key: &str) -> Arc<AtomicU64> {
        let mut map = self.counters.lock().unwrap();
        Arc::clone(map.entry(key.to_string()).or_default())
    }
}

impl Recorder for TestRecorder {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        let labels: Vec<(String, String)> = key
            .labels()
            .map(|l| (l.key().to_string(), l.value().to_string()))
            .collect();
        let borrowed: Vec<(&str, &str)> = labels
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        Counter::from_arc(self.handle(&key_string(key.name(), &borrowed)))
    }
    fn register_gauge(&self, _: &Key, _: &Metadata<'_>) -> Gauge {
        Gauge::noop()
    }
    fn register_histogram(&self, _: &Key, _: &Metadata<'_>) -> Histogram {
        Histogram::noop()
    }
}

static RECORDER: OnceLock<&'static TestRecorder> = OnceLock::new();

/// Installs the recorder once, before any production counter handle is
/// resolved (every test calls this first).
fn recorder() -> &'static TestRecorder {
    RECORDER.get_or_init(|| {
        let rec: &'static TestRecorder = Box::leak(Box::new(TestRecorder::default()));
        metrics::set_global_recorder(rec).expect("first and only recorder install");
        rec
    })
}

fn counter(name: &str, labels: &[(&str, &str)]) -> u64 {
    recorder()
        .handle(&key_string(name, labels))
        .load(Ordering::SeqCst)
}

/// The frame-fate counter handles are process-global; tests reading their
/// deltas run one at a time.
static FATE_LOCK: Mutex<()> = Mutex::new(());

fn fate_lock() -> std::sync::MutexGuard<'static, ()> {
    FATE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

const LIVE: WsType = WsType::LiveFeed;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FateCounters {
    ring_lost: u64,
    drain_lost: u64,
    ticks_lost: u64,
    unknown: u64,
    refused_wal_lost: u64,
    refused_unrecordable: u64,
}

fn fate_counters() -> FateCounters {
    FateCounters {
        ring_lost: counter(SHED_LOST_COUNTER, &[("shed", "ring")]),
        drain_lost: counter(SHED_LOST_COUNTER, &[("shed", "drain_depth")]),
        ticks_lost: counter(
            "tv_ticks_lost_total",
            &[("source", TICKS_LOST_SOURCE), ("ws_type", LIVE.as_str())],
        ),
        unknown: counter(FATE_UNKNOWN_COUNTER, &[]),
        refused_wal_lost: counter(DRAIN_SHED_REFUSED_COUNTER, &[("reason", "wal_lost")]),
        refused_unrecordable: counter(DRAIN_SHED_REFUSED_COUNTER, &[("reason", "unrecordable")]),
    }
}

fn delta(after: FateCounters, before: FateCounters) -> FateCounters {
    FateCounters {
        ring_lost: after.ring_lost - before.ring_lost,
        drain_lost: after.drain_lost - before.drain_lost,
        ticks_lost: after.ticks_lost - before.ticks_lost,
        unknown: after.unknown - before.unknown,
        refused_wal_lost: after.refused_wal_lost - before.refused_wal_lost,
        refused_unrecordable: after.refused_unrecordable - before.refused_unrecordable,
    }
}

/// A frame sequence for frame base `b` (packet index 0).
const fn seq(b: u64) -> u64 {
    b << PACKET_INDEX_BITS
}

fn tmp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!(
        "tv-chaos-ring-wal-fate-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create temp dir");
    p
}

// ---------------------------------------------------------------------------
// S6a — both orders, both shed kinds
// ---------------------------------------------------------------------------

#[test]
fn s6a_shed_and_wal_loss_in_both_orders_count_each_loss_once() {
    recorder();
    let _g = fate_lock();
    let t = FrameFate::<64>::new();
    let before = fate_counters();

    // Ring shed, then the writer fails the record.
    assert_eq!(t.note_shed(seq(10), ShedKind::Ring, LIVE), ShedMark::Marked);
    assert_eq!(
        t.note_lost(seq(10), LIVE),
        LostFate::WasShed(ShedKind::Ring)
    );
    // A second loss report of the same record counts nothing more.
    assert_eq!(t.note_lost(seq(10), LIVE), LostFate::NotShed);

    // Writer fails first, then the ring sheds the frame.
    assert_eq!(t.note_lost(seq(11), LIVE), LostFate::NotShed);
    assert_eq!(
        t.note_shed(seq(11), ShedKind::Ring, LIVE),
        ShedMark::AlreadyLost
    );

    // Drain depth shed, then the record is lost.
    assert!(t.allow_drain_shed(seq(12), LIVE));
    assert_eq!(
        t.note_lost(seq(12), LIVE),
        LostFate::WasShed(ShedKind::DrainDepth)
    );

    // Record lost first: the drain must not shed, it writes the rows.
    assert_eq!(t.note_lost(seq(13), LIVE), LostFate::NotShed);
    assert!(!t.allow_drain_shed(seq(13), LIVE));

    // Ring shed and drain shed of the same frame, then the loss: one loss,
    // attributed to the ring (the whole frame), never two.
    assert_eq!(t.note_shed(seq(14), ShedKind::Ring, LIVE), ShedMark::Marked);
    assert!(t.allow_drain_shed(seq(14), LIVE));
    assert_eq!(
        t.note_lost(seq(14), LIVE),
        LostFate::WasShed(ShedKind::Ring)
    );

    let d = delta(fate_counters(), before);
    assert_eq!(
        d,
        FateCounters {
            ring_lost: 3,
            drain_lost: 1,
            ticks_lost: 3,
            unknown: 0,
            refused_wal_lost: 1,
            refused_unrecordable: 0,
        },
        "frames 10, 11, 14 lost after a ring shed; 12 after a drain shed; 13 refused"
    );
}

// ---------------------------------------------------------------------------
// S6b — many threads x many frames, one slot per frame
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// Ring shed and WAL loss both happen.
    RingAndLost,
    /// Drain depth shed and WAL loss both happen.
    DrainAndLost,
    /// WAL loss only (its copy went on to the drain).
    LostOnly,
    /// Ring shed only (the WAL write succeeded).
    RingOnly,
}

fn plan_of(f: usize) -> Plan {
    match f % 7 {
        0..=2 => Plan::RingAndLost,
        3 | 4 => Plan::DrainAndLost,
        5 => Plan::LostOnly,
        _ => Plan::RingOnly,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShedOutcome {
    None,
    Ring(ShedMark),
    Drain(bool),
}

#[test]
fn s6b_threads_racing_shed_and_loss_over_many_frames_count_each_loss_once() {
    const SLOTS: usize = 4096;
    const FRAMES: usize = 2048;
    const THREADS: usize = 8;
    const ROUNDS: u64 = 12;
    recorder();
    let _g = fate_lock();

    for round in 0..ROUNDS {
        let table = Arc::new(FrameFate::<SLOTS>::new());
        let first_base = 1_000 + round * 10_000;
        let sheds: Arc<Vec<Mutex<ShedOutcome>>> =
            Arc::new((0..FRAMES).map(|_| Mutex::new(ShedOutcome::None)).collect());
        let losses: Arc<Vec<Mutex<Option<LostFate>>>> =
            Arc::new((0..FRAMES).map(|_| Mutex::new(None)).collect());
        let start = Arc::new(Barrier::new(2 * THREADS));
        let before = fate_counters();

        let mut handles = Vec::new();
        for th in 0..THREADS {
            {
                let (table, sheds, start) =
                    (Arc::clone(&table), Arc::clone(&sheds), Arc::clone(&start));
                handles.push(std::thread::spawn(move || {
                    start.wait();
                    // Shed threads walk forward.
                    for f in (th..FRAMES).step_by(THREADS) {
                        let s = seq(first_base + f as u64);
                        let out = match plan_of(f) {
                            Plan::RingAndLost | Plan::RingOnly => {
                                ShedOutcome::Ring(table.note_shed(s, ShedKind::Ring, LIVE))
                            }
                            Plan::DrainAndLost => {
                                ShedOutcome::Drain(table.allow_drain_shed(s, LIVE))
                            }
                            Plan::LostOnly => continue,
                        };
                        *sheds[f].lock().unwrap() = out;
                    }
                }));
            }
            let (table, losses, start) =
                (Arc::clone(&table), Arc::clone(&losses), Arc::clone(&start));
            handles.push(std::thread::spawn(move || {
                start.wait();
                // Loss threads walk backward over a different partition, so
                // each frame's two events meet in varying orders.
                let mine: Vec<usize> = (0..FRAMES)
                    .filter(|f| (f * 5 + 3) % THREADS == th)
                    .collect();
                for &f in mine.iter().rev() {
                    if plan_of(f) == Plan::RingOnly {
                        continue;
                    }
                    let s = seq(first_base + f as u64);
                    *losses[f].lock().unwrap() = Some(table.note_lost(s, LIVE));
                }
            }));
        }
        for h in handles {
            h.join().expect("race thread");
        }

        let mut expect_ring = 0u64;
        let mut expect_drain = 0u64;
        let mut expect_refused = 0u64;
        for f in 0..FRAMES {
            let shed = *sheds[f].lock().unwrap();
            let lost = *losses[f].lock().unwrap();
            match plan_of(f) {
                Plan::RingAndLost => {
                    let by_lost = lost == Some(LostFate::WasShed(ShedKind::Ring));
                    let by_shed = shed == ShedOutcome::Ring(ShedMark::AlreadyLost);
                    assert!(
                        by_lost ^ by_shed,
                        "round {round} frame {f}: shed={shed:?} lost={lost:?} must count once"
                    );
                    expect_ring += 1;
                }
                Plan::DrainAndLost => match (shed, lost) {
                    (ShedOutcome::Drain(true), Some(LostFate::WasShed(ShedKind::DrainDepth))) => {
                        expect_drain += 1;
                    }
                    (ShedOutcome::Drain(false), Some(LostFate::NotShed)) => expect_refused += 1,
                    other => panic!("round {round} frame {f}: drain race ended {other:?}"),
                },
                Plan::LostOnly => assert_eq!(lost, Some(LostFate::NotShed), "frame {f}"),
                Plan::RingOnly => {
                    assert_eq!(shed, ShedOutcome::Ring(ShedMark::Marked), "frame {f}");
                }
            }
        }
        let d = delta(fate_counters(), before);
        assert_eq!(d.ring_lost, expect_ring, "round {round}: ring counter");
        assert_eq!(
            d.ticks_lost, expect_ring,
            "round {round}: tick-loss counter"
        );
        assert_eq!(d.drain_lost, expect_drain, "round {round}: drain counter");
        assert_eq!(
            d.refused_wal_lost, expect_refused,
            "round {round}: refusals"
        );
        assert_eq!(
            d.unknown, 0,
            "round {round}: no collisions, no unknown fates"
        );
        assert_eq!(d.refused_unrecordable, 0, "round {round}");
        assert_eq!(
            expect_drain + expect_refused,
            (0..FRAMES)
                .filter(|f| plan_of(*f) == Plan::DrainAndLost)
                .count() as u64
        );
    }
}

// ---------------------------------------------------------------------------
// S6c — colliding slots (slot reuse)
// ---------------------------------------------------------------------------

#[test]
fn s6c_colliding_slots_never_double_count_and_unknown_fates_are_counted() {
    const SLOTS: usize = 8;
    const FRAMES: usize = 512;
    const THREADS: usize = 8;
    const ROUNDS: u64 = 20;
    recorder();
    let _g = fate_lock();
    let mut asymmetric_total = 0u64;
    let mut unknown_total = 0u64;

    for round in 0..ROUNDS {
        let table = Arc::new(FrameFate::<SLOTS>::new());
        let first_base = 7 + round * 100_000;
        let sheds: Arc<Vec<Mutex<Option<ShedMark>>>> =
            Arc::new((0..FRAMES).map(|_| Mutex::new(None)).collect());
        let losses: Arc<Vec<Mutex<Option<LostFate>>>> =
            Arc::new((0..FRAMES).map(|_| Mutex::new(None)).collect());
        let start = Arc::new(Barrier::new(2 * THREADS));
        let before = fate_counters();
        let mut handles = Vec::new();
        for th in 0..THREADS {
            {
                let (table, sheds, start) =
                    (Arc::clone(&table), Arc::clone(&sheds), Arc::clone(&start));
                handles.push(std::thread::spawn(move || {
                    start.wait();
                    for f in (th..FRAMES).step_by(THREADS) {
                        let m = table.note_shed(seq(first_base + f as u64), ShedKind::Ring, LIVE);
                        *sheds[f].lock().unwrap() = Some(m);
                    }
                }));
            }
            let (table, losses, start) =
                (Arc::clone(&table), Arc::clone(&losses), Arc::clone(&start));
            handles.push(std::thread::spawn(move || {
                start.wait();
                let mine: Vec<usize> = (th..FRAMES).step_by(THREADS).collect();
                for &f in mine.iter().rev() {
                    let l = table.note_lost(seq(first_base + f as u64), LIVE);
                    *losses[f].lock().unwrap() = Some(l);
                }
            }));
        }
        for h in handles {
            h.join().expect("collision thread");
        }

        let mut counted = 0u64;
        let mut unknown = 0u64;
        for f in 0..FRAMES {
            let shed = sheds[f].lock().unwrap().expect("shed ran");
            let lost = losses[f].lock().unwrap().expect("loss ran");
            let by_lost = lost == LostFate::WasShed(ShedKind::Ring);
            let by_shed = shed == ShedMark::AlreadyLost;
            assert!(
                !(by_lost && by_shed),
                "round {round} frame {f}: counted twice ({shed:?}, {lost:?})"
            );
            assert!(
                !(shed == ShedMark::Marked && lost == LostFate::NotShed),
                "round {round} frame {f}: a recorded shed then read as not shed = an uncounted loss"
            );
            if by_lost || by_shed {
                counted += 1;
            }
            if lost == LostFate::Unknown {
                unknown += 1;
            }
            if shed == ShedMark::Unrecordable && lost == LostFate::NotShed {
                // Counted nowhere: see s6f (the ignored DEFECT test).
                asymmetric_total += 1;
            }
        }
        let d = delta(fate_counters(), before);
        assert_eq!(
            d.ring_lost, counted,
            "round {round}: ring counter equals outcomes"
        );
        assert_eq!(d.ticks_lost, counted, "round {round}");
        assert_eq!(
            d.unknown, unknown,
            "round {round}: every Unknown moved the counter"
        );
        unknown_total += unknown;
    }
    // Non-vacuous: collisions really happened.
    assert!(
        unknown_total > 0,
        "no slot reuse reached a loss; the test did not collide"
    );
    eprintln!(
        "s6c: {unknown_total} unknown fates counted; {asymmetric_total} lost-then-reused-then-shed \
         frames counted nowhere (s6f)"
    );
}

// ---------------------------------------------------------------------------
// S6d — shutdown scan racing the abandoned writer
// ---------------------------------------------------------------------------

#[test]
fn s6d_shutdown_scan_racing_writer_losses_counts_each_shed_once() {
    const SLOTS: usize = 4096;
    const FRAMES: u64 = 2048;
    const BELOW: u64 = 256; // frames at or below the flushed mark
    const LOSERS: usize = 4;
    recorder();
    let _g = fate_lock();

    for round in 0..8u64 {
        let table = Arc::new(FrameFate::<SLOTS>::new());
        let first = 1 + round * 10_000;
        for f in 0..FRAMES {
            assert_eq!(
                table.note_shed(seq(first + f), ShedKind::Ring, LIVE),
                ShedMark::Marked
            );
        }
        let flushed = seq(first + BELOW - 1);
        let before = fate_counters();
        let start = Arc::new(Barrier::new(LOSERS + 1));
        let was_shed = Arc::new(AtomicU64::new(0));
        let mut handles = Vec::new();
        for th in 0..LOSERS {
            let (table, start, was_shed) = (
                Arc::clone(&table),
                Arc::clone(&start),
                Arc::clone(&was_shed),
            );
            handles.push(std::thread::spawn(move || {
                start.wait();
                // All four report every frame (the writer re-reporting is
                // allowed); walk from different ends.
                let order: Vec<u64> = if th % 2 == 0 {
                    (0..FRAMES).collect()
                } else {
                    (0..FRAMES).rev().collect()
                };
                for f in order {
                    if table.note_lost(seq(first + f), LIVE) == LostFate::WasShed(ShedKind::Ring) {
                        was_shed.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }));
        }
        start.wait();
        let (scan_ring, scan_drain) = table.count_unflushed_sheds(flushed, LIVE);
        for h in handles {
            h.join().expect("loser thread");
        }
        let by_writer = was_shed.load(Ordering::SeqCst);
        assert_eq!(scan_drain, 0);
        assert_eq!(
            scan_ring + by_writer,
            FRAMES,
            "round {round}: scan {scan_ring} + writer {by_writer} must count each shed once"
        );
        assert!(
            by_writer >= BELOW,
            "frames below the flush are the writer's alone"
        );
        let d = delta(fate_counters(), before);
        assert_eq!(d.ring_lost, FRAMES, "round {round}");
        assert_eq!(d.ticks_lost, FRAMES, "round {round}");
    }
}

// ---------------------------------------------------------------------------
// S6e — a WAL loss blocks the rescue deferral
// ---------------------------------------------------------------------------

#[test]
fn s6e_wal_loss_after_ring_shed_blocks_rescue_deferral_to_the_wal() {
    recorder();
    let _g = fate_lock();
    let t = FrameFate::<64>::new();
    // A base far above anything else this binary mints, so the global marks
    // this raises read only this test's frames.
    let lost_base = (u64::MAX >> PACKET_INDEX_BITS) - 1_000;
    let lost = seq(lost_base);
    assert_eq!(t.note_shed(lost, ShedKind::Ring, LIVE), ShedMark::Marked);
    assert_eq!(t.note_lost(lost, LIVE), LostFate::WasShed(ShedKind::Ring));
    // The writer then flushes past the batch (later records landed).
    note_flushed_through(seq(lost_base + 10));
    for (lo, hi) in [
        (seq(lost_base - 5), seq(lost_base + 5)),
        (lost, lost),
        (seq(lost_base - 50), seq(lost_base - 40)),
    ] {
        assert!(
            !rescue_may_defer_to_wal(lo, hi, false),
            "batch [{lo}, {hi}] at or below a lost record must spill inline, never defer"
        );
    }
}

// ---------------------------------------------------------------------------
// S6f — DEFECT: lost, slot reused, then shed = counted nowhere
// ---------------------------------------------------------------------------

/// The module contract: a frame whose shed state cannot be known because the
/// slot moved on is counted on `tv_wal_lost_fate_unknown_total`, never as
/// "not shed". In this one ordering it is counted nowhere: the writer loses
/// frame A first (`NotShed`, slot = A|LOST), a newer frame claims the slot,
/// then the ring sheds A, which reads `Unrecordable` and returns without a
/// counter. The ring shed caller (`WalRingSink::note_ring_shed`) discards the
/// mark. A was shed AND lost, so it is a real, uncounted loss.
///
/// Reachability: at the production size (2^22 slots, at least ~5.5 minutes of
/// sequences) this needs the reader to stall that long between queueing A for
/// the WAL and offering it to the ring, so it is not expected in production;
/// with a small table it is reached deterministically below (s6c counts any
/// it meets in its random races; in measured runs it met none).
#[test]
#[ignore = "DEFECT: a ring shed that finds its slot taken by a newer frame (Unrecordable) after the WAL lost it is counted nowhere"]
fn s6f_lost_then_slot_reused_then_ring_shed_is_counted_as_unknown() {
    recorder();
    let _g = fate_lock();
    let t = FrameFate::<8>::new();
    let before = fate_counters();
    let a = seq(16);
    let newer = seq(24); // same slot (24 & 7 == 16 & 7)
    assert_eq!(t.note_lost(a, LIVE), LostFate::NotShed);
    assert_eq!(t.note_shed(newer, ShedKind::Ring, LIVE), ShedMark::Marked);
    assert_eq!(t.note_shed(a, ShedKind::Ring, LIVE), ShedMark::Unrecordable);
    let d = delta(fate_counters(), before);
    assert!(
        d.ring_lost + d.unknown >= 1,
        "frame A was shed and lost; it must move the loss or the unknown counter, got {d:?}"
    );
}

// ---------------------------------------------------------------------------
// S1 — reconnect storm into one WAL
// ---------------------------------------------------------------------------

const PRODUCERS: usize = 16;
const PER_PRODUCER: u32 = 3_000;
const REDIAL_EVERY: u32 = 250;

fn storm_frame(producer: usize, n: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 16];
    buf[0] = 2; // ticker response code
    buf[1..3].copy_from_slice(&16u16.to_le_bytes());
    buf[3] = 2;
    buf[4..8].copy_from_slice(&(producer as u32).to_le_bytes());
    buf[8..12].copy_from_slice(&n.to_le_bytes());
    buf[12..16].copy_from_slice(&0xC0DE_u32.to_le_bytes());
    buf
}

#[test]
fn s1_reconnect_storm_every_spilled_frame_replays_once_in_producer_order() {
    recorder();
    let dir = tmp_dir("storm");
    let spilled = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicU64::new(0));
    let redials = Arc::new(AtomicUsize::new(0));
    // (producer, n) -> spilled? per producer.
    let accepted: Arc<Vec<Mutex<Vec<u32>>>> =
        Arc::new((0..PRODUCERS).map(|_| Mutex::new(Vec::new())).collect());
    {
        let root = Arc::new(WsFrameSpill::new(&dir).expect("spill"));
        let start = Arc::new(Barrier::new(PRODUCERS));
        let mut handles = Vec::new();
        for p in 0..PRODUCERS {
            let root = Arc::clone(&root);
            let (spilled, dropped, redials, accepted, start) = (
                Arc::clone(&spilled),
                Arc::clone(&dropped),
                Arc::clone(&redials),
                Arc::clone(&accepted),
                Arc::clone(&start),
            );
            handles.push(std::thread::spawn(move || {
                start.wait();
                let mut handle = Some(Arc::clone(&root));
                let mut mine = Vec::with_capacity(PER_PRODUCER as usize);
                for n in 0..PER_PRODUCER {
                    if n > 0 && n % REDIAL_EVERY == 0 {
                        // The socket drops: its handle goes, frames already
                        // queued stay in flight, then it redials.
                        drop(handle.take());
                        redials.fetch_add(1, Ordering::SeqCst);
                        std::thread::yield_now();
                        handle = Some(Arc::clone(&root));
                    }
                    let h = handle.as_ref().expect("handle held");
                    match h.append(WsType::LiveFeed, storm_frame(p, n)) {
                        AppendOutcome::Spilled => {
                            spilled.fetch_add(1, Ordering::SeqCst);
                            mine.push(n);
                        }
                        AppendOutcome::Dropped => {
                            dropped.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                }
                *accepted[p].lock().unwrap() = mine;
            }));
        }
        for h in handles {
            h.join().expect("producer");
        }
        let target = spilled.load(Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(15);
        while root.persisted_count() < target && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            root.drop_critical_count(),
            dropped.load(Ordering::SeqCst),
            "every refused append is counted on the drop counter"
        );
        let lost = root.shutdown(Duration::from_secs(10));
        assert_eq!(lost, 0, "a clean shutdown loses no queued record");
        drop(root);
    }
    assert_eq!(
        redials.load(Ordering::SeqCst),
        PRODUCERS * ((PER_PRODUCER as usize - 1) / REDIAL_EVERY as usize)
    );

    let replayed = replay_all(&dir).expect("replay_all");
    let total_spilled = spilled.load(Ordering::SeqCst);
    assert_eq!(
        replayed.len() as u64,
        total_spilled,
        "replay must return exactly the Spilled frames ({} dropped)",
        dropped.load(Ordering::SeqCst)
    );

    let mut seqs = std::collections::HashSet::with_capacity(replayed.len());
    let mut per_producer: Vec<Vec<(u32, u64)>> = vec![Vec::new(); PRODUCERS];
    for rec in &replayed {
        assert_eq!(rec.ws_type, WsType::LiveFeed);
        assert_eq!(rec.frame.len(), 16);
        assert!(
            seqs.insert(rec.frame_seq),
            "sequence {} replayed twice",
            rec.frame_seq
        );
        let p = u32::from_le_bytes(rec.frame[4..8].try_into().unwrap()) as usize;
        let n = u32::from_le_bytes(rec.frame[8..12].try_into().unwrap());
        assert!(p < PRODUCERS, "corrupt producer id {p}");
        per_producer[p].push((n, rec.frame_seq));
    }
    for (p, got) in per_producer.iter().enumerate() {
        let expected = accepted[p].lock().unwrap().clone();
        let ns: Vec<u32> = got.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            ns, expected,
            "producer {p}: replay must hold every Spilled frame once, in append order"
        );
        assert!(
            got.windows(2).all(|w| w[0].1 < w[1].1),
            "producer {p}: its sequences must strictly increase"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// OOB — rescue floors saturated while the watermarks advance
// ---------------------------------------------------------------------------

const HOLDERS: usize = 2 * RESCUE_FLOOR_SLOTS + 8;
const CHURN_BATCHES_PER_HOLDER: usize = 30;
/// One batch = 64 frame bases, the floor at its first.
const BATCH_STEP: u64 = 64 << PACKET_INDEX_BITS;
const FLOOR_BASE: u64 = 1_780_000_000_000_000_000 & !((1u64 << PACKET_INDEX_BITS) - 1);
const LAG_NANOS: u64 = (WATERMARK_DURABILITY_LAG_SECS + 1) * 1_000_000_000;

fn batch_range(id: u64) -> (u64, u64) {
    let lo = FLOOR_BASE + id * BATCH_STEP;
    (lo, lo + BATCH_STEP / 2)
}

fn sink_of(id: u64) -> AppliedSink {
    if id.is_multiple_of(2) {
        AppliedSink::Ticks
    } else {
        AppliedSink::Depth
    }
}

fn persisted_hwm(snap: &AppliedSnapshot, sink: AppliedSink) -> u64 {
    match sink {
        AppliedSink::Ticks => snap.hwm_ticks,
        AppliedSink::Depth => snap.hwm_depth,
    }
}

struct FloorRig {
    wm: AppliedWatermark,
    dir: PathBuf,
    clock: AtomicU64,
    /// Holds are taken in batch order under this lock, so an ack never
    /// passes a batch before its floor is held (the production order: the
    /// floor is pushed before the hand-off, every passing ack comes after).
    alloc: Mutex<u64>,
    handled_through: AtomicU64,
    overflowed: Mutex<Vec<u64>>,
    stop: std::sync::atomic::AtomicBool,
}

impl FloorRig {
    /// Holds the next batch's floor; returns its id and the floor (or `None`
    /// when the table was full, after checking the range was marked).
    fn hold_next(
        &self,
    ) -> (
        u64,
        Option<tickvault_storage::wal_applied_watermark::RescueFloor>,
    ) {
        let mut next = self.alloc.lock().unwrap();
        let id = *next;
        *next += 1;
        let (lo, hi) = batch_range(id);
        let floor = self.wm.hold_rescue_floor(sink_of(id), lo, hi);
        if floor.is_none() {
            assert!(
                self.wm.snapshot().range_has_unapplied(lo, hi),
                "batch {id}: a refused hold must mark its range unapplied at once"
            );
            self.overflowed.lock().unwrap().push(id);
        }
        self.handled_through.store(id + 1, Ordering::SeqCst);
        (id, floor)
    }

    fn persist_and_load(&self) -> Option<AppliedSnapshot> {
        let now = self.clock.fetch_add(LAG_NANOS, Ordering::SeqCst) + LAG_NANOS;
        self.wm.persist_at(now);
        AppliedSnapshot::load(&self.dir)
    }

    /// While `id`'s floor is held: the persisted watermark of its sink stays
    /// below its first sequence and the file does not call it applied.
    fn check_held(&self, id: u64) {
        let (lo, hi) = batch_range(id);
        if let Some(snap) = self.persist_and_load() {
            let hwm = persisted_hwm(&snap, sink_of(id));
            assert!(
                hwm < lo,
                "batch {id}: persisted watermark {hwm} reached the held floor {lo}"
            );
            assert!(!snap.range_is_applied(sink_of(id), lo, hi), "batch {id}");
        }
    }
}

#[test]
fn oob_rescue_floor_table_saturated_while_watermarks_advance() {
    recorder();
    const {
        assert!(
            HOLDERS > 2 * RESCUE_FLOOR_SLOTS,
            "the rig must outnumber the slots"
        )
    };
    let dir = tmp_dir("floors");
    let rig = Arc::new(FloorRig {
        wm: AppliedWatermark::new(),
        dir: dir.clone(),
        clock: AtomicU64::new(0),
        alloc: Mutex::new(0),
        handled_through: AtomicU64::new(0),
        overflowed: Mutex::new(Vec::new()),
        stop: std::sync::atomic::AtomicBool::new(false),
    });
    rig.wm.bind(&dir);
    let full_before = counter(RESCUE_FLOOR_FULL_COUNTER, &[]);

    // Two ackers: they advance both watermarks past every batch already held.
    let ackers: Vec<_> = (0..2)
        .map(|_| {
            let rig = Arc::clone(&rig);
            std::thread::spawn(move || {
                let mut acks = 0u64;
                while !rig.stop.load(Ordering::SeqCst) {
                    let through = rig.handled_through.load(Ordering::SeqCst);
                    if through > 0 {
                        let (_, hi) = batch_range(through - 1);
                        rig.wm.note_ticks_acked(hi + 1);
                        rig.wm.note_depth_acked(hi + 1);
                        acks += 1;
                    }
                    std::thread::yield_now();
                }
                acks
            })
        })
        .collect();

    // Phase 1: saturation. Every holder takes one floor and waits for all the
    // others before releasing, so at least HOLDERS - 2 x 16 holds are refused (sinks alternate).
    let barrier = Arc::new(Barrier::new(HOLDERS));
    let phase1: Vec<_> = (0..HOLDERS)
        .map(|_| {
            let (rig, barrier) = (Arc::clone(&rig), Arc::clone(&barrier));
            std::thread::spawn(move || {
                let (id, floor) = rig.hold_next();
                barrier.wait();
                if floor.is_some() {
                    rig.check_held(id);
                }
                barrier.wait();
                if let Some(f) = floor {
                    rig.wm.release_rescue_floor(f);
                }
            })
        })
        .collect();
    for h in phase1 {
        h.join().expect("phase 1 holder");
    }
    let saturated = rig.overflowed.lock().unwrap().len();
    assert!(
        saturated >= HOLDERS - 2 * RESCUE_FLOOR_SLOTS,
        "phase 1 must refuse at least {} holds, refused {saturated}",
        HOLDERS - 2 * RESCUE_FLOOR_SLOTS
    );

    // Phase 2: churn. Holders hold, persist and reload, release, repeat,
    // while the ackers keep moving the watermarks.
    let phase2: Vec<_> = (0..HOLDERS)
        .map(|_| {
            let rig = Arc::clone(&rig);
            std::thread::spawn(move || {
                for _ in 0..CHURN_BATCHES_PER_HOLDER {
                    let (id, floor) = rig.hold_next();
                    if let Some(f) = floor {
                        rig.check_held(id);
                        std::thread::yield_now();
                        rig.check_held(id);
                        rig.wm.release_rescue_floor(f);
                    }
                }
            })
        })
        .collect();
    for h in phase2 {
        h.join().expect("phase 2 holder");
    }
    rig.stop.store(true, Ordering::SeqCst);
    let total_acks: u64 = ackers.into_iter().map(|h| h.join().expect("acker")).sum();
    assert!(
        total_acks > 0,
        "the ackers must have advanced the watermarks"
    );

    let last = *rig.alloc.lock().unwrap() - 1;
    let (_, last_hi) = batch_range(last);
    rig.wm.note_ticks_acked(last_hi + 1);
    rig.wm.note_depth_acked(last_hi + 1);
    // Two persists a lag apart: the durability lag releases the final value.
    let _ = rig.persist_and_load();
    let snap = rig.persist_and_load().expect("watermark file");
    for sink in [AppliedSink::Ticks, AppliedSink::Depth] {
        assert!(
            persisted_hwm(&snap, sink) > last_hi,
            "{sink:?}: with every floor released the file must pass the last batch"
        );
    }

    let overflowed = rig.overflowed.lock().unwrap().clone();
    for &id in &overflowed {
        let (lo, hi) = batch_range(id);
        assert!(
            !snap.range_is_applied(sink_of(id), lo, hi),
            "refused batch {id}: still unapplied in the file after the acks passed it"
        );
    }
    let full_delta = counter(RESCUE_FLOOR_FULL_COUNTER, &[]) - full_before;
    assert_eq!(
        full_delta,
        overflowed.len() as u64,
        "every refused hold is counted on {RESCUE_FLOOR_FULL_COUNTER}"
    );
    eprintln!(
        "oob floors: {} batches, {} refused ({saturated} in the saturation phase), {total_acks} acks",
        last + 1,
        overflowed.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
