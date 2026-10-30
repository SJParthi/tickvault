//! Combined-fault permutations for the WAL frame-fate table (2026-10-30).
//!
//! Two frames, A and B, can each be SHED (by the ring, or their depth rows by
//! the drain) and LOST (the WAL writer failed their record), and the process
//! can shut down mid-fault (the shutdown scan `count_unflushed_sheds`, with the
//! writer abandoned and still running detached). This file drives EVERY
//! ordering of every subset of those five events, for every pair of shed
//! kinds, at several distances between A and B (the same slot exactly one
//! window apart, two windows apart, B older than A, and no collision), and
//! with the shutdown floor below both frames, at A, and between them. Each
//! case runs on a fresh 4-slot table, so slot reuse happens on purpose.
//!
//! Each case compares the counters with a physical model:
//! - a frame's only in-memory copy is dropped by a ring shed (any outcome),
//!   or by a drain shed the table allowed;
//! - a frame's WAL record is lost when the writer reported it lost, or when
//!   the shutdown scan ran and the frame is above the flushed floor (the
//!   abandoned writer never wrote it);
//! - the frame is really lost when both hold.
//!
//! Checked per case:
//! 1. **No false real loss:** `tv_wal_shed_frames_lost_total` never moves by
//!    more than the frames really lost.
//! 2. **Nothing silent:** every really lost frame moves the loss counter or
//!    `tv_wal_lost_fate_unknown_total`. One documented exemption: a frame
//!    lost only through the shutdown scan whose slot a NEWER frame of the
//!    case held when the scan ran (a frame holding its own slot is never
//!    exempt, and the test asserts the exemption stays narrower). That is
//!    the table's window limit; in production the WAL
//!    queue holds at most 524,288 records against 2^22 slots, so two
//!    unflushed frames never share a slot.
//! 3. **No runaway unknown:** the unknown counter moves at most once per
//!    `LostFate::Unknown` return and once per unrecordable ring shed.
//! 4. **Counters match the returned outcomes** where they can be attributed
//!    (cases without a shutdown scan): per frame at most one real count.
//! 5. **Per-label attribution, every case:** the ring and drain loss labels
//!    move exactly by the scan counts plus the `WasShed` and ring
//!    `AlreadyLost` returns of their kind; `tv_ticks_lost_total` moves with
//!    every ring loss; each drain-shed refusal is counted under the reason a
//!    slot model predicts (`wal_lost` when the frame holds its slot already
//!    lost, `unrecordable` when a newer frame holds it), and the verdict
//!    matches the model.
//!
//! Plus the boundaries the permutations do not reach: the shutdown floor
//! exactly at a frame and one below it, sequence zero, the highest base, a
//! frame partly shed when the writer's loss lands mid-frame (packet bits in
//! the sequence), a shutdown floor carrying packet bits, and an unrecordable
//! ring shed with no loss reported anywhere at or above it
//! (counted nowhere, correctly).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};
use tickvault_storage::wal_frame_fate::{
    DRAIN_SHED_REFUSED_COUNTER, FATE_UNKNOWN_COUNTER, FrameFate, LostFate, SHED_LOST_COUNTER,
    ShedKind, ShedMark, TICKS_LOST_SOURCE, lost_high_seq,
};
use tickvault_storage::ws_frame_spill::{PACKET_INDEX_BITS, WsType};

// ---------------------------------------------------------------------------
// Test metrics recorder (same shape as chaos_combined_ring_wal_fate.rs)
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

/// The fate counters and the lost high-water mark are process-wide; every
/// test in this file runs under this lock.
static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

const LIVE: WsType = WsType::LiveFeed;
const SLOTS: u64 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Snap {
    /// `tv_wal_shed_frames_lost_total{shed="ring"}`.
    ring: u64,
    /// `tv_wal_shed_frames_lost_total{shed="drain_depth"}`.
    drain: u64,
    /// `tv_ticks_lost_total{source="wal_lost_after_shed", ws_type="live_feed"}`.
    ticks: u64,
    unknown: u64,
    refused_wal_lost: u64,
    refused_unrecordable: u64,
}

impl Snap {
    const fn real(&self) -> u64 {
        self.ring + self.drain
    }
}

fn snap() -> Snap {
    Snap {
        ring: counter(SHED_LOST_COUNTER, &[("shed", "ring")]),
        drain: counter(SHED_LOST_COUNTER, &[("shed", "drain_depth")]),
        ticks: counter(
            "tv_ticks_lost_total",
            &[("source", TICKS_LOST_SOURCE), ("ws_type", LIVE.as_str())],
        ),
        unknown: counter(FATE_UNKNOWN_COUNTER, &[]),
        refused_wal_lost: counter(DRAIN_SHED_REFUSED_COUNTER, &[("reason", "wal_lost")]),
        refused_unrecordable: counter(DRAIN_SHED_REFUSED_COUNTER, &[("reason", "unrecordable")]),
    }
}

const fn seq(base: u64) -> u64 {
    base << PACKET_INDEX_BITS
}

/// Bases strictly increase across cases, so the process-wide lost mark that
/// earlier cases raised always sits below the current case's frames.
static NEXT_BASE: AtomicU64 = AtomicU64::new(1 << 20);

fn fresh_base() -> u64 {
    NEXT_BASE.fetch_add(1_000, Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// The permutation model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ev {
    Shed(usize),
    Lost(usize),
    Scan,
}

const EVENTS: [Ev; 5] = [Ev::Shed(0), Ev::Lost(0), Ev::Shed(1), Ev::Lost(1), Ev::Scan];

/// Where the shutdown scan's flushed mark sits.
#[derive(Debug, Clone, Copy)]
enum Floor {
    BelowBoth,
    AtLower,
    AtHigher,
}

fn permutations(items: &[Ev]) -> Vec<Vec<Ev>> {
    if items.is_empty() {
        return vec![Vec::new()];
    }
    let mut out = Vec::new();
    for i in 0..items.len() {
        let mut rest = items.to_vec();
        let head = rest.remove(i);
        for mut tail in permutations(&rest) {
            tail.insert(0, head);
            out.push(tail);
        }
    }
    out
}

/// Every ordering of every subset of the five events in which the shutdown
/// scan, when present, comes after every shed (the readers stop before the
/// scan runs). Losses may come after it: the abandoned writer keeps running.
fn orderings() -> Vec<Vec<Ev>> {
    let mut out = Vec::new();
    for mask in 0u32..(1 << EVENTS.len()) {
        let subset: Vec<Ev> = EVENTS
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, e)| *e)
            .collect();
        for p in permutations(&subset) {
            if let Some(scan_at) = p.iter().position(|e| *e == Ev::Scan)
                && p[scan_at..].iter().any(|e| matches!(e, Ev::Shed(_)))
            {
                continue;
            }
            out.push(p);
        }
    }
    out
}

#[derive(Default, Debug, Clone, Copy)]
struct FrameOutcome {
    shed: Option<ShedMark>,
    lost: Option<LostFate>,
    /// The process-wide lost mark read just before this frame's shed.
    lost_high_before_shed: u64,
}

#[derive(Default)]
struct Tally {
    cases: u64,
    really_lost: u64,
    exempt: u64,
    /// Frames lost only through the scan, in a colliding case.
    window_candidates: u64,
    unknown_moves: u64,
    refused: u64,
}

/// A reference model of one table slot: the frame base that holds it and its
/// flags, updated by the same rules as `FrameFate` (claim when older or empty,
/// refuse when newer, flags only for the holder).
#[derive(Default, Debug, Clone, Copy)]
struct ModelSlot {
    tag: u64,
    ring: bool,
    drain: bool,
    lost: bool,
}

impl ModelSlot {
    fn claim(&mut self, base: u64) {
        *self = Self {
            tag: base,
            ..Self::default()
        };
    }
}

fn run_case(order: &[Ev], kinds: [ShedKind; 2], b_offset: i64, floor: Floor, tally: &mut Tally) {
    let t = FrameFate::<{ SLOTS as usize }>::new();
    let a = fresh_base() + 100;
    let b = a
        .checked_add_signed(b_offset)
        .expect("offset stays positive");
    let bases = [a, b];
    let lower = a.min(b);
    let higher = a.max(b);
    let flushed = match floor {
        Floor::BelowBoth => seq(lower - 1),
        Floor::AtLower => seq(lower),
        Floor::AtHigher => seq(higher),
    };
    let floor_base = flushed >> PACKET_INDEX_BITS;
    let collide = (a % SLOTS) == (b % SLOTS);
    let slot_of = |x: usize| usize::try_from(bases[x] % SLOTS).unwrap();
    let mut model = [ModelSlot::default(); SLOTS as usize];

    let before = snap();
    let mut out = [FrameOutcome::default(); 2];
    let mut scan = None;
    let mut holder_at_scan = [0u64; 2];
    let mut unrecordable_ring = 0u64;
    let mut unknown_returns = 0u64;
    // Expected per-label counts, built from the returned outcomes.
    let mut want_ring = 0u64;
    let mut want_drain = 0u64;
    let mut want_refused_wal_lost = 0u64;
    let mut want_refused_unrecordable = 0u64;
    for ev in order {
        match *ev {
            Ev::Shed(x) => {
                out[x].lost_high_before_shed = lost_high_seq();
                let m = &mut model[slot_of(x)];
                let bx = bases[x];
                let mark = match kinds[x] {
                    ShedKind::Ring => {
                        let mark = t.note_shed(seq(bx), ShedKind::Ring, LIVE);
                        if mark == ShedMark::AlreadyLost {
                            want_ring += 1;
                        }
                        mark
                    }
                    ShedKind::DrainDepth => {
                        // The reason the model expects a refusal, if any.
                        let expect_wal_lost = m.tag == bx && m.lost;
                        let expect_unrecordable = m.tag > bx;
                        let allowed = t.allow_drain_shed(seq(bx), LIVE);
                        assert_eq!(
                            allowed,
                            !(expect_wal_lost || expect_unrecordable),
                            "drain shed verdict vs model: order {order:?} kinds {kinds:?} \
                             b_offset {b_offset}"
                        );
                        if expect_wal_lost {
                            want_refused_wal_lost += 1;
                            ShedMark::AlreadyLost
                        } else if expect_unrecordable {
                            want_refused_unrecordable += 1;
                            ShedMark::Unrecordable
                        } else {
                            ShedMark::Marked
                        }
                    }
                };
                // Model update, by the table's rules.
                if m.tag == bx {
                    if !m.lost {
                        match kinds[x] {
                            ShedKind::Ring => m.ring = true,
                            ShedKind::DrainDepth => m.drain = true,
                        }
                    }
                } else if m.tag < bx {
                    m.claim(bx);
                    match kinds[x] {
                        ShedKind::Ring => m.ring = true,
                        ShedKind::DrainDepth => m.drain = true,
                    }
                }
                if kinds[x] == ShedKind::Ring && mark == ShedMark::Unrecordable {
                    unrecordable_ring += 1;
                }
                out[x].shed = Some(mark);
            }
            Ev::Lost(x) => {
                let l = t.note_lost(seq(bases[x]), LIVE);
                match l {
                    LostFate::Unknown => unknown_returns += 1,
                    LostFate::WasShed(ShedKind::Ring) => want_ring += 1,
                    LostFate::WasShed(ShedKind::DrainDepth) => want_drain += 1,
                    LostFate::NotShed => {}
                }
                let m = &mut model[slot_of(x)];
                let bx = bases[x];
                if m.tag < bx {
                    m.claim(bx);
                }
                if m.tag == bx {
                    m.lost = true;
                }
                out[x].lost = Some(l);
            }
            Ev::Scan => {
                let s = t.count_unflushed_sheds(flushed, LIVE);
                want_ring += s.0;
                want_drain += s.1;
                for x in 0..2 {
                    holder_at_scan[x] = model[slot_of(x)].tag;
                }
                for m in &mut model {
                    if m.tag > floor_base && !m.lost && (m.ring || m.drain) {
                        m.lost = true;
                    }
                }
                scan = Some(s);
            }
        }
    }
    let after = snap();
    let real = after.real() - before.real();
    let unknown = after.unknown - before.unknown;
    let label = format!(
        "order {order:?} kinds {kinds:?} b_offset {b_offset} floor {floor:?} -> {out:?} scan {scan:?} \
         real {real} unknown {unknown}"
    );

    // Per-label attribution: each counter moved exactly by the outcomes that
    // report it, and every ring loss moved the tick-loss counter too.
    assert_eq!(
        after.ring - before.ring,
        want_ring,
        "ring loss counter: {label}"
    );
    assert_eq!(
        after.drain - before.drain,
        want_drain,
        "drain loss counter: {label}"
    );
    assert_eq!(
        after.ticks - before.ticks,
        after.ring - before.ring,
        "tv_ticks_lost_total must move with every ring loss: {label}"
    );
    assert_eq!(
        after.refused_wal_lost - before.refused_wal_lost,
        want_refused_wal_lost,
        "refused{{wal_lost}}: {label}"
    );
    assert_eq!(
        after.refused_unrecordable - before.refused_unrecordable,
        want_refused_unrecordable,
        "refused{{unrecordable}}: {label}"
    );

    // The physical model.
    let mut truth = [false; 2];
    let mut exempt = [false; 2];
    let mut window_candidates = 0u64;
    for x in 0..2 {
        let dropped = match (kinds[x], out[x].shed) {
            (_, None) => false,
            (ShedKind::Ring, Some(_)) => true,
            (ShedKind::DrainDepth, Some(m)) => m == ShedMark::Marked,
        };
        let reported = out[x].lost.is_some();
        let by_scan = scan.is_some() && bases[x] > floor_base;
        truth[x] = dropped && (reported || by_scan);
        let scan_only = truth[x] && !reported && collide;
        window_candidates += u64::from(scan_only);
        // The window limit only: a NEWER frame of the case held the slot when
        // the scan ran. A frame that holds its own slot is never exempt.
        exempt[x] = scan_only && holder_at_scan[x] > bases[x];
    }
    let truth_n = truth.iter().filter(|v| **v).count() as u64;
    let exempt_n = exempt.iter().filter(|v| **v).count() as u64;

    // 1. No false real loss.
    assert!(real <= truth_n, "false real loss: {label}");
    // 2. Nothing silent (bar the documented window exemption).
    assert!(
        real + unknown >= truth_n - exempt_n,
        "a really lost frame moved no counter: {label}"
    );
    // 3. No runaway unknown.
    assert!(
        unknown <= unknown_returns + unrecordable_ring,
        "unknown counter ran past its sources: {label}"
    );
    // 4. Attribution, where the returned outcomes say who counted.
    if scan.is_none() {
        let mut attributed_real = 0u64;
        let mut attributed_unknown = 0u64;
        for x in 0..2 {
            let by_lost = matches!(out[x].lost, Some(LostFate::WasShed(_)));
            let by_shed = kinds[x] == ShedKind::Ring && out[x].shed == Some(ShedMark::AlreadyLost);
            assert!(!(by_lost && by_shed), "frame {x} counted twice: {label}");
            let real_x = u64::from(by_lost || by_shed);
            assert!(
                real_x <= u64::from(truth[x]),
                "frame {x} false real loss: {label}"
            );
            let unknown_x = u64::from(out[x].lost == Some(LostFate::Unknown))
                + u64::from(
                    kinds[x] == ShedKind::Ring
                        && out[x].shed == Some(ShedMark::Unrecordable)
                        && (out[x].lost_high_before_shed >> PACKET_INDEX_BITS) >= bases[x],
                );
            if truth[x] {
                assert!(real_x + unknown_x >= 1, "frame {x} lost silently: {label}");
            }
            attributed_real += real_x;
            attributed_unknown += unknown_x;
        }
        assert_eq!(real, attributed_real, "real counter vs outcomes: {label}");
        assert_eq!(
            unknown, attributed_unknown,
            "unknown counter vs outcomes: {label}"
        );
    } else {
        // Exact with a scan too: the scan's counts plus the loss reports.
        assert_eq!(
            real,
            want_ring + want_drain,
            "real counter vs outcomes: {label}"
        );
    }

    tally.cases += 1;
    tally.really_lost += truth_n;
    tally.exempt += exempt_n;
    tally.window_candidates += window_candidates;
    tally.unknown_moves += unknown;
    tally.refused += want_refused_wal_lost + want_refused_unrecordable;
}
#[test]
fn every_ordering_of_shed_lost_and_shutdown_counts_each_lost_frame() {
    recorder();
    let _g = lock();
    let orders = orderings();
    let kinds = [ShedKind::Ring, ShedKind::DrainDepth];
    // Same slot one window apart (the exact reuse boundary), two windows
    // apart, B older than A in the same slot, and two non-colliding offsets.
    let offsets: [i64; 5] = [SLOTS as i64, 2 * SLOTS as i64, -(SLOTS as i64), 1, 3];
    let mut tally = Tally::default();
    for order in &orders {
        for ka in kinds {
            for kb in kinds {
                for off in offsets {
                    for floor in [Floor::BelowBoth, Floor::AtLower, Floor::AtHigher] {
                        if !order.contains(&Ev::Scan) && !matches!(floor, Floor::BelowBoth) {
                            continue; // the floor only matters to the scan
                        }
                        run_case(order, [ka, kb], off, floor, &mut tally);
                    }
                }
            }
        }
    }
    // Non-vacuous: the matrix really produced losses, unknowns and the
    // exempt window cases.
    // 65 orderings without the scan, 107 with it after every shed.
    assert_eq!(orders.len(), 172, "orderings");
    assert!(
        tally.really_lost > 1_000,
        "really lost: {}",
        tally.really_lost
    );
    assert!(
        tally.unknown_moves > 100,
        "unknown moves: {}",
        tally.unknown_moves
    );
    assert!(tally.exempt > 0, "the window exemption was never exercised");
    // The exemption is narrower than "lost only through the scan in a
    // colliding case": a frame that holds its own slot at the scan is checked.
    assert!(
        tally.exempt < tally.window_candidates,
        "exempt {} vs scan-only colliding {}: the exemption covers every candidate",
        tally.exempt,
        tally.window_candidates
    );
    assert!(tally.refused > 0, "no drain shed was ever refused");
    eprintln!(
        "fate permutations: {} cases, {} really lost frames, {} unknown counts, {} window-exempt \
         of {} scan-only colliding, {} drain refusals",
        tally.cases,
        tally.really_lost,
        tally.unknown_moves,
        tally.exempt,
        tally.window_candidates,
        tally.refused
    );
}

/// The ordering the 2026-10-30 fix closes, at the exact reuse boundary: A is
/// lost, the frame exactly one window later takes the slot, then A is shed.
#[test]
fn test_regression_lost_then_slot_reused_at_boundary_then_ring_shed_is_counted() {
    recorder();
    let _g = lock();
    let t = FrameFate::<{ SLOTS as usize }>::new();
    let a = fresh_base() + 1;
    let before = snap();
    assert_eq!(t.note_lost(seq(a), LIVE), LostFate::NotShed);
    assert_eq!(
        t.note_shed(seq(a + SLOTS), ShedKind::Ring, LIVE),
        ShedMark::Marked
    );
    assert_eq!(
        t.note_shed(seq(a), ShedKind::Ring, LIVE),
        ShedMark::Unrecordable
    );
    let d = snap();
    assert_eq!(d.real() - before.real(), 0);
    assert_eq!(
        d.unknown - before.unknown,
        1,
        "the lost-and-shed frame must count"
    );
}

/// With no loss reported at or above the frame, an unrecordable ring shed is
/// not a loss (its record is still on its way to disk; if the writer later
/// loses it, `note_lost` counts it as unknown). Nothing is counted.
#[test]
fn unrecordable_ring_shed_with_no_loss_anywhere_above_counts_nothing_until_lost() {
    recorder();
    let _g = lock();
    let t = FrameFate::<{ SLOTS as usize }>::new();
    let a = fresh_base() + 1;
    assert!(
        (lost_high_seq() >> PACKET_INDEX_BITS) < a,
        "an earlier test raised the lost mark above this frame"
    );
    let before = snap();
    assert_eq!(
        t.note_shed(seq(a + SLOTS), ShedKind::Ring, LIVE),
        ShedMark::Marked
    );
    assert_eq!(
        t.note_shed(seq(a), ShedKind::Ring, LIVE),
        ShedMark::Unrecordable
    );
    assert_eq!(snap(), before, "no loss yet, nothing to count");
    // Now the writer loses A: the slot is B's, so the loss reads unknown.
    assert_eq!(t.note_lost(seq(a), LIVE), LostFate::Unknown);
    let d = snap();
    assert_eq!(d.unknown - before.unknown, 1);
    assert_eq!(d.real() - before.real(), 0);
}

/// The shutdown scan's floor: a frame AT the flushed mark reached the kernel
/// and is not counted; the frame one base above it is.
#[test]
fn shutdown_scan_floor_boundary_counts_only_frames_above_the_flush() {
    recorder();
    let _g = lock();
    let t = FrameFate::<64>::new();
    let a = fresh_base() + 1;
    for f in [a, a + 1] {
        assert_eq!(t.note_shed(seq(f), ShedKind::Ring, LIVE), ShedMark::Marked);
    }
    let before = snap();
    // Flushed through A (packet index 0): A is at the floor, A+1 above it.
    assert_eq!(t.count_unflushed_sheds(seq(a), LIVE), (1, 0));
    // A second scan finds nothing new: A+1 is now marked lost.
    assert_eq!(t.count_unflushed_sheds(seq(a), LIVE), (0, 0));
    // The detached writer then reports A+1 lost: already counted, not again.
    assert_eq!(t.note_lost(seq(a + 1), LIVE), LostFate::NotShed);
    // A's record was flushed; a late loss report of it is still counted once.
    assert_eq!(t.note_lost(seq(a), LIVE), LostFate::WasShed(ShedKind::Ring));
    assert_eq!(snap().real() - before.real(), 2);

    // One base lower: the flushed mark just below A counts A as well.
    let t2 = FrameFate::<64>::new();
    let c = fresh_base() + 1;
    assert_eq!(t2.note_shed(seq(c), ShedKind::Ring, LIVE), ShedMark::Marked);
    assert_eq!(t2.count_unflushed_sheds(seq(c) - 1, LIVE), (1, 0));
}

/// Sequence zero is untracked on both sides, and a loss of it is counted as
/// unknown, never as "not shed". The highest representable base works.
#[test]
fn sequence_zero_and_the_highest_base_are_handled() {
    recorder();
    let _g = lock();
    let t = FrameFate::<{ SLOTS as usize }>::new();
    let before = snap();
    assert_eq!(t.note_shed(0, ShedKind::Ring, LIVE), ShedMark::Untracked);
    assert_eq!(t.note_lost(0, LIVE), LostFate::Unknown);
    assert_eq!(snap().unknown - before.unknown, 1);

    // The highest base, through the shutdown scan (a loss report would raise
    // the process-wide lost mark to the top and change the other tests).
    let top_table = FrameFate::<{ SLOTS as usize }>::new();
    let top = u64::MAX;
    let before = snap();
    assert_eq!(
        top_table.note_shed(top, ShedKind::Ring, LIVE),
        ShedMark::Marked
    );
    assert_eq!(top_table.count_unflushed_sheds(seq(1), LIVE), (1, 0));
    assert_eq!(top_table.count_unflushed_sheds(seq(1), LIVE), (0, 0));
    assert_eq!(snap().real() - before.real(), 1);
}

/// Inline depth asks per packet, so a frame can be partly shed: the first
/// packets' rows are shed, the writer's loss lands, and the later packets
/// are refused and written. The frame is counted lost once (as a drain
/// loss) and each later packet is one `wal_lost` refusal. A ring shed of a
/// lost frame carrying packet bits is `AlreadyLost` and counted once, and a
/// second loss report of it counts nothing. A shutdown floor carrying packet
/// bits treats its whole base as flushed.
#[test]
fn partial_frame_drain_shed_then_loss_mid_frame_counts_once_and_refuses_the_rest() {
    recorder();
    let _g = lock();
    let t = FrameFate::<64>::new();
    let a = fresh_base() + 1;
    let before = snap();
    assert!(t.allow_drain_shed(seq(a) | 1, LIVE), "packet 1 is shed");
    assert_eq!(
        t.note_lost(seq(a), LIVE),
        LostFate::WasShed(ShedKind::DrainDepth),
        "the writer's loss lands mid-frame"
    );
    assert!(
        !t.allow_drain_shed(seq(a) | 2, LIVE),
        "packet 2 is refused and written"
    );
    assert!(!t.allow_drain_shed(seq(a) | 3, LIVE), "packet 3 too");
    let d = snap();
    assert_eq!(d.drain - before.drain, 1, "the frame is lost once");
    assert_eq!(d.ring - before.ring, 0);
    assert_eq!(d.ticks - before.ticks, 0, "depth rows are not ticks");
    assert_eq!(
        d.refused_wal_lost - before.refused_wal_lost,
        2,
        "one refusal per later packet"
    );
    assert_eq!(d.refused_unrecordable - before.refused_unrecordable, 0);
    assert_eq!(d.unknown - before.unknown, 0);

    // A ring shed of a lost frame, carrying packet bits.
    let c = fresh_base() + 1;
    let before = snap();
    assert_eq!(t.note_lost(seq(c), LIVE), LostFate::NotShed);
    assert_eq!(
        t.note_shed(seq(c) | 3, ShedKind::Ring, LIVE),
        ShedMark::AlreadyLost
    );
    assert_eq!(t.note_lost(seq(c) | 7, LIVE), LostFate::NotShed);
    let d = snap();
    assert_eq!(d.ring - before.ring, 1, "counted once");
    assert_eq!(d.ticks - before.ticks, 1, "and once as a lost tick");
    assert_eq!(d.unknown - before.unknown, 0);

    // A floor with packet bits: base e is flushed, e + 1 is not.
    let t2 = FrameFate::<64>::new();
    let e = fresh_base() + 1;
    for f in [e, e + 1] {
        assert_eq!(t2.note_shed(seq(f), ShedKind::Ring, LIVE), ShedMark::Marked);
    }
    let packet_mask = (1u64 << PACKET_INDEX_BITS) - 1;
    assert_eq!(
        t2.count_unflushed_sheds(seq(e) | packet_mask, LIVE),
        (1, 0),
        "only e + 1 is above the floor"
    );
}
