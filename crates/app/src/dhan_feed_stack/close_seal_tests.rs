//! Plan item PR23 (2026-11-23): the day's last bars are sealed at the close,
//! on the drain's sliced catch-up sweep, not at shutdown
//! (`close_seal::close_seal_cutoff`, `LiveIngest::catch_up_cutoff`). A
//! sibling file, to keep `dhan_feed_stack.rs` under its size pin.

use super::close_seal::{ClosePlan, LONGEST_BUCKET_SECS};
use super::*;
use std::collections::BTreeMap;
use tickvault_common::feed::Feed;

/// An IST day (IST epoch seconds / 86,400) and its midnight.
const DAY: i64 = 20_594;
const DAY_START: u32 = 20_594 * 86_400;

const fn at(h: u32, m: u32, s: u32) -> u32 {
    DAY_START + h * 3_600 + m * 60 + s
}

const fn nanos(secs: u32) -> i64 {
    secs as i64 * 1_000_000_000
}

/// More instruments than one idle step visits, so a sweep takes several.
const INSTRUMENTS: u32 = (CATCHUP_SEAL_STEP_SLOTS * 2 + 9) as u32;

/// One IDX_I ticker packet.
fn ticker_packet(security_id: u32, ltp: f32, ltt: u32) -> [u8; 16] {
    let mut p = [0u8; 16];
    p[0] = 2;
    p[1] = 16;
    p[3] = 0;
    p[4..8].copy_from_slice(&security_id.to_le_bytes());
    p[8..12].copy_from_slice(&ltp.to_le_bytes());
    p[12..16].copy_from_slice(&ltt.to_le_bytes());
    p
}

fn fold(ingest: &mut LiveIngest, security_id: u32, ltp: f32, ltt: u32, received: u32) {
    let packet = ticker_packet(security_id, ltp, ltt);
    let parsed = dispatch_frame(&packet, nanos(received)).expect("parse");
    let ParsedFrame::Tick(tick) = parsed else {
        panic!("expected a tick");
    };
    let _ = ingest.ingest_tick(&tick, u64::from(received), u64::from(received) * 1_000);
}

/// Every instrument trades at 15:15:10 and 15:39:50 (received on time), on
/// a regular day's plan, the wall at the last receipt.
fn session_book() -> LiveIngest {
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), INSTRUMENTS as usize);
    ingest.close_plan = ClosePlan::build(DAY, true, None);
    for sid in 1..=INSTRUMENTS {
        fold(
            &mut ingest,
            1_000 + sid,
            100.0,
            at(15, 15, 10),
            at(15, 15, 10),
        );
    }
    for sid in 1..=INSTRUMENTS {
        fold(
            &mut ingest,
            1_000 + sid,
            101.0,
            at(15, 39, 50),
            at(15, 39, 51),
        );
    }
    ingest.note_wall_clock(nanos(at(15, 39, 51)));
    ingest
}

/// Bars handed to the seal path so far (emitted, rescued or dropped: a
/// test has no seal writer).
fn sealed(ingest: &LiveIngest) -> u64 {
    ingest.seals_emitted() + ingest.seals_dropped() + ingest.seals_rescued()
}

/// Runs the sliced sweep to its end, asserting each step visits at most
/// `CATCHUP_SEAL_STEP_SLOTS` slots. Returns the number of steps.
fn run_sweep(ingest: &mut LiveIngest) -> usize {
    ingest.begin_catch_up_seal();
    let mut steps = 0;
    while ingest.catch_up.is_some() {
        let from = ingest.catch_up.map_or(0, |s| s.next_slot);
        let _ = ingest.step_idle_work();
        let to = ingest
            .catch_up
            .map_or(ingest.aggregator.len(), |s| s.next_slot);
        assert!(
            to - from <= CATCHUP_SEAL_STEP_SLOTS,
            "a step visits at most 256 slots"
        );
        steps += 1;
        assert!(steps < 1_000, "the sweep must finish");
    }
    steps
}

/// `(timeframe, bucket start) -> bars` the one-call sweep at `cutoff` seals.
fn bars_at(ingest: &mut LiveIngest, cutoff: u32) -> BTreeMap<(usize, u32), u32> {
    let mut out = BTreeMap::new();
    let _ = ingest
        .aggregator
        .catch_up_seal_all(cutoff, |_, _, _, tf, bar| {
            *out.entry((tf.as_ordinal(), bar.bucket_start_ist_secs))
                .or_insert(0) += 1;
        });
    out
}

#[test]
fn test_close_seal_seals_every_open_bar_without_shutdown() {
    let mut twin = session_book();
    twin.note_wall_clock(nanos(at(15, 44, 5)));
    let cutoff = twin.catch_up_cutoff();
    assert_eq!(
        cutoff,
        at(16, 40, 0),
        "the close term: 15:40 + {LONGEST_BUCKET_SECS} s"
    );
    let bars = bars_at(&mut twin, cutoff);
    for (tf, start) in [
        (TfIndex::M1, at(15, 39, 0)),
        (TfIndex::M15, at(15, 30, 0)),
        (TfIndex::M30, at(15, 15, 0)),
        (TfIndex::M60, at(15, 15, 0)),
    ] {
        assert_eq!(
            bars.get(&(tf.as_ordinal(), start)),
            Some(&INSTRUMENTS),
            "{tf:?} bar at {start} sealed for every instrument"
        );
    }

    // The same through the drain's sliced sweep: first the normal sweep at
    // the last receipt (it leaves the last bars open), then the close sweep.
    let mut ingest = session_book();
    run_sweep(&mut ingest);
    let before = sealed(&ingest);
    ingest.note_wall_clock(nanos(at(15, 44, 5)));
    let steps = run_sweep(&mut ingest);
    assert!(
        steps >= 3,
        "the close sweep takes several bounded steps, got {steps}"
    );
    let mut twin2 = session_book();
    run_sweep(&mut twin2);
    twin2.note_wall_clock(nanos(at(15, 44, 5)));
    let close_cutoff = twin2.catch_up_cutoff();
    let after_close: u32 = bars_at(&mut twin2, close_cutoff).values().sum();
    assert!(
        after_close > 0,
        "the last bars were still open before the close sweep"
    );
    assert_eq!(
        sealed(&ingest) - before,
        u64::from(after_close),
        "the sliced close sweep seals what the one-call sweep does"
    );
    assert_eq!(
        ingest.aggregator.force_seal_all(|_, _, _, _, _| {}),
        0,
        "nothing is left open for the shutdown seal"
    );
}

#[test]
fn test_crash_after_close_seal_loses_no_bar() {
    static PROGRESS: CatchUpProgress = CatchUpProgress::new();
    let mut ingest = session_book();
    ingest.catch_up_progress = &PROGRESS;
    run_sweep(&mut ingest);
    ingest.note_wall_clock(nanos(at(15, 44, 5)));
    run_sweep(&mut ingest);
    let crashed = sealed(&ingest);
    let (floor, _) = PROGRESS.load().expect("a completed sweep published");
    assert!(
        floor >= at(15, 40, 0),
        "the published floor reached the close: {floor}"
    );
    // A crash now: no shutdown seal runs.
    drop(ingest);

    // The twin ends in the shutdown seal.
    let mut twin = session_book();
    run_sweep(&mut twin);
    let (e, d) = twin.seal_open_buckets_at_close();
    let shutdown = sealed(&twin);
    assert!(e + d > 0, "the twin's shutdown seal had bars to write");
    assert_eq!(
        crashed, shutdown,
        "the crash after the close seal loses no bar"
    );
}

#[test]
fn test_crash_after_close_before_seal_boot_replay_rebuilds() {
    // The uninterrupted twin's bars, sealed at the close.
    let mut twin = session_book();
    twin.note_wall_clock(nanos(at(15, 44, 5)));
    let mut truth = BTreeMap::new();
    let cutoff = twin.catch_up_cutoff();
    let _ = twin
        .aggregator
        .catch_up_seal_all(cutoff, |_, sid, _, tf, bar| {
            truth.insert((sid, tf.as_ordinal(), bar.bucket_start_ist_secs), bar);
        });

    // Boot B at 16:00 after a crash before the close seal: the WAL replay
    // re-folds the session's frames, then hands over.
    let mut boot = LiveIngest::new(TickWriter::for_test(Feed::Dhan), INSTRUMENTS as usize);
    boot.close_plan = ClosePlan::build(DAY, true, None);
    boot.replaying_wal = true;
    boot.aggregator.set_replay_mode(true);
    for sid in 1..=INSTRUMENTS {
        fold(
            &mut boot,
            1_000 + sid,
            100.0,
            at(15, 15, 10),
            at(15, 15, 10),
        );
    }
    for sid in 1..=INSTRUMENTS {
        fold(
            &mut boot,
            1_000 + sid,
            101.0,
            at(15, 39, 50),
            at(15, 39, 51),
        );
    }
    // The hand-over (`finish_wal_replay`, at a 16:00 capture start).
    boot.replaying_wal = false;
    boot.aggregator.set_replay_mode(false);
    boot.aggregator.set_live_capture_start(at(16, 0, 0));
    let mut written = 0usize;
    let _ = boot.aggregator.finish_replay(false, |_, sid, _, tf, bar| {
        written += 1;
        let key = (sid, tf.as_ordinal(), bar.bucket_start_ist_secs);
        match truth.get(&key) {
            Some(t) => assert_eq!(t, &bar, "a hand-over bar is the truth"),
            // A bar the twin sealed on its own next tick (15:39:50), not in
            // the close sweep: the 15:15:10 trade alone.
            None => assert!(
                (bar.close - 100.0).abs() < f64::EPSILON
                    && bar.bucket_start_ist_secs < at(15, 39, 0),
                "a hand-over bar is the 15:15 trade alone"
            ),
        }
    });
    boot.note_wall_clock(nanos(at(16, 0, 0)));
    let cutoff = boot.catch_up_cutoff();
    assert_eq!(
        cutoff,
        at(16, 40, 0),
        "the first live sweep takes the close term"
    );
    let _ = boot
        .aggregator
        .catch_up_seal_all(cutoff, |_, sid, _, tf, bar| {
            written += 1;
            let key = (sid, tf.as_ordinal(), bar.bucket_start_ist_secs);
            assert_eq!(truth.get(&key), Some(&bar), "a rebuilt bar is never wrong");
        });
    assert_eq!(
        boot.aggregator.len(),
        INSTRUMENTS as usize,
        "the replay re-folded every instrument"
    );
    assert_eq!(
        boot.aggregator.force_seal_all(|_, _, _, _, _| {}),
        0,
        "no bar is left open: each was written ({written}) or withheld and counted"
    );
}

#[test]
fn test_close_seal_then_shutdown_and_midnight_roll_emit_nothing_more() {
    let mut ingest = session_book();
    ingest.note_wall_clock(nanos(at(15, 44, 5)));
    run_sweep(&mut ingest);
    assert_eq!(ingest.seal_open_buckets_at_close(), (0, 0));
    assert_eq!(ingest.roll_trading_day(), (0, 0));
    assert_eq!(
        ingest.close_plan,
        ClosePlan::UNSET,
        "the roll resets the plan"
    );
    // The next reading rebuilds it for the new day.
    ingest.note_wall_clock(nanos(at(0, 0, 5) + 86_400));
    assert_eq!(ingest.close_plan.ist_day, DAY + 1);
}

#[test]
fn test_late_trade_inside_margin_lands_in_open_bucket() {
    let mut ingest = session_book();
    // A trade stamped 15:39:30, for an instrument quiet until then, arrives at
    // 15:43:50: the close term has not started, so its bucket is still open.
    ingest.note_wall_clock(nanos(at(15, 43, 50)));
    run_sweep(&mut ingest);
    fold(&mut ingest, 9_999, 250.0, at(15, 39, 30), at(15, 43, 50));
    ingest.note_wall_clock(nanos(at(15, 44, 5)));
    let cutoff = ingest.catch_up_cutoff();
    let mut found = false;
    let _ = ingest
        .aggregator
        .catch_up_seal_all(cutoff, |_, sid, _, tf, bar| {
            if sid == 9_999 && tf == TfIndex::M1 {
                assert_eq!(bar.bucket_start_ist_secs, at(15, 39, 0));
                assert!((bar.close - 250.0).abs() < f64::EPSILON);
                found = true;
            }
        });
    assert!(
        found,
        "the late trade's minute is sealed with it, at the close"
    );
}

#[test]
fn test_close_seal_never_starts_during_wal_replay() {
    let mut ingest = session_book();
    ingest.note_wall_clock(nanos(at(15, 50, 0)));
    let live = ingest.catch_up_cutoff();
    assert_eq!(live, at(16, 40, 0));
    ingest.replaying_wal = true;
    assert_eq!(
        ingest.catch_up_cutoff(),
        at(15, 39, 50) - CATCHUP_LATENESS_MARGIN_SECS,
        "a WAL re-fold keeps the plain watermark cutoff"
    );
}

#[test]
fn test_catch_up_progress_floor_monotone_after_close_seal() {
    static PROGRESS: CatchUpProgress = CatchUpProgress::new();
    let mut ingest = session_book();
    ingest.catch_up_progress = &PROGRESS;
    ingest.note_wall_clock(nanos(at(15, 43, 0)));
    ingest.begin_catch_up_seal();
    let low = ingest.catch_up.map(|s| s.floor_cutoff).expect("a sweep");
    let _ = ingest.step_idle_work();
    assert!(
        ingest.catch_up.is_some(),
        "one step does not finish the sweep"
    );
    // The close term arrives while the sweep runs: an overrun.
    ingest.note_wall_clock(nanos(at(15, 44, 5)));
    ingest.begin_catch_up_seal();
    while ingest.catch_up.is_some() {
        let _ = ingest.step_idle_work();
    }
    assert_eq!(
        PROGRESS.load().map(|p| p.0),
        Some(low),
        "the overrun keeps the low floor"
    );
    for minute in [44, 46, 50, 59] {
        ingest.note_wall_clock(nanos(at(15, minute, 5)));
        run_sweep(&mut ingest);
        let (floor, _) = PROGRESS.load().expect("published");
        assert_eq!(
            floor,
            at(16, 40, 0),
            "later sweeps never publish below 16:40"
        );
    }
}

#[test]
fn test_empty_boot_after_close_publishes_no_floor() {
    static PROGRESS: CatchUpProgress = CatchUpProgress::new();
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
    ingest.catch_up_progress = &PROGRESS;
    ingest.close_plan = ClosePlan::build(DAY, true, None);
    ingest.note_wall_clock(nanos(at(16, 0, 0)));
    assert_eq!(
        ingest.catch_up_cutoff(),
        0,
        "no trade folded: no false floor"
    );
    ingest.begin_catch_up_seal();
    assert!(ingest.catch_up.is_none());
    assert_eq!(PROGRESS.load(), None);
}

/// The catch-up progress the drain publishes after one complete sweep at
/// each `walls` reading (IST epoch seconds) on [`session_book`], for the
/// cross-check readiness test in `dhan_live_crossverify_boot`.
pub(crate) fn published_after_sweeps(walls: &[u32]) -> Vec<Option<(u32, u32)>> {
    let progress: &'static CatchUpProgress = Box::leak(Box::new(CatchUpProgress::new()));
    let mut ingest = session_book();
    ingest.catch_up_progress = progress;
    walls
        .iter()
        .map(|&wall| {
            ingest.note_wall_clock(nanos(wall));
            run_sweep(&mut ingest);
            progress.load()
        })
        .collect()
}

/// [`DAY`]'s midnight as IST nanoseconds, for that test.
pub(crate) const fn day_start_ist_nanos() -> i64 {
    nanos(DAY_START)
}

/// [`at`], for that test.
pub(crate) const fn ist_at(h: u32, m: u32, s: u32) -> u32 {
    at(h, m, s)
}

/// PR23 source scan: the close term allocates nothing and loops only over
/// the session slice; the calendar is probed only on a day change; no drain
/// arm force-seals; no new metric, EMF or alarm name.
#[test]
fn test_close_seal_source_scan() {
    let module = include_str!("close_seal.rs");
    let prod = module.split("#[cfg(test)]").next().unwrap_or("");
    let body = prod
        .split("pub fn close_seal_cutoff(")
        .nth(1)
        .and_then(|s| s.split("\n}\n").next())
        .expect("close_seal_cutoff body");
    for banned in [
        "Vec", "format!", "String", "Box", "collect", "while ", "loop ",
    ] {
        assert!(
            !body.contains(banned),
            "close_seal_cutoff must not use `{banned}`"
        );
    }
    assert_eq!(
        body.matches("for ").count(),
        2,
        "two loops, both over `sessions`"
    );
    assert_eq!(body.matches(" in sessions").count(), 2);
    for banned in ["counter!", "gauge!", "histogram!", "tv_"] {
        assert!(
            !prod.contains(banned),
            "no new metric in close_seal.rs: `{banned}`"
        );
    }

    let src = include_str!("../dhan_feed_stack.rs");
    let prod = src.split("#[cfg(test)]\nmod tests").next().unwrap_or("");
    let fn_body = |name: &str| {
        prod.split(name)
            .nth(1)
            .and_then(|s| s.split("\n    }\n").next())
            .unwrap_or("")
            .to_string()
    };
    for name in ["fn catch_up_cutoff(", "pub(crate) fn note_wall_clock("] {
        let body = fn_body(name);
        assert!(!body.is_empty(), "{name} exists");
        assert!(
            !body.contains("installed_") && !body.contains("for_installed_calendar"),
            "{name} must not probe the calendar"
        );
    }
    assert!(fn_body("pub(crate) fn note_wall_clock(").contains("self.rebuild_close_plan(day)"));
    assert!(fn_body("fn rebuild_close_plan(").contains("for_installed_calendar"));
    let drain = prod
        .split_once("async fn run_frame_drain")
        .expect("the drain")
        .1
        .split("\n}\n")
        .next()
        .unwrap_or("");
    assert!(drain.contains("ingest.begin_catch_up_seal()"));
    assert!(
        !drain.contains("force_seal_all"),
        "no drain arm calls force_seal_all"
    );
}
