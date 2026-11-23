//! Model test of the frame-counter lead record (2026-11-23, PR24a).
//!
//! # The invariant
//!
//! Every frame's sequence lies in the frame span of its receipt hour
//! ([`super::hour_frame_seq_span`]), or inside a recorded step window that
//! overlaps that span. That is what the archive's in-flight check needs: an
//! hour is judged against its own span and every window overlapping it, so a
//! frame outside both could be dropped while still on its way.
//!
//! # What runs for real, and what is modelled
//!
//! REAL: the mint arithmetic and its lead hook (`ws_frame_spill::mint_base`,
//! `lead_exceeds`, `note_lead_began_in`, `end_counter_lead_episode_on` with
//! its re-check) on a private cell; the step log
//! (`ClockStepLog::record_and_persist_pending_lead`, which records and
//! writes, `follow_counter_lead`, `note_from`, `bind`, `persist_if_dirty`,
//! `note_boot_counter_lead`) on a private log bound to a temp folder, so a
//! restart reads the persisted file back.
//!
//! MODELLED: true time in 1 ms ticks; a wall clock that can step forward or
//! back by 0 to 2 h; the receipt anchor (refreshed every 30 s, a backward
//! move of 1 s or less refused, a move over 1 s noted as a step, exactly as
//! `refresh_noting_step`, with no read gap); frames at 0 to 20,000 a second;
//! the tick writer's per-batch sample at a random cadence or stalled for a
//! whole segment; a crash and restart (the frame counter seeded past the
//! newest frame, the log rebuilt from its file, the boot belt fed the newest
//! frame); the archive verdict's own record call at the end.
//!
//! # What is left out
//!
//! Two wall-clock steps less than one anchor refresh (30 s) apart: the
//! receipt anchor sees only their net move, a limit of the anchor that
//! predates PR24a, so the model spaces steps more than a refresh apart.
//!
//! # The one documented gap
//!
//! A crash while a lead episode is pending and no record call has run since
//! it began (or an anchor-noted step is not yet persisted; a record call
//! writes what it recorded itself, 2026-11-23 review), when the newest frame
//! no longer leads its receipt (the lead
//! decayed before the crash), leaves that episode unrecorded: the boot belt
//! sees only the newest frame. A frame whose only cover was such a window
//! (held in RAM at the crash, not persisted, not the belt's) is then EXEMPT
//! from the check; every other frame is checked. For a lead episode it
//! needs a stalled tick writer and a crash together; for an anchor-noted
//! step, a crash in the gap between the 30 s refresh and the next tick
//! batch (a limit that predates PR24a).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use proptest::prelude::*;

use super::{HourFrameSpan, RECEIPT_SLOP_NANOS, hour_frame_seq_span};
use crate::wal_applied_watermark::REPLAY_REORDER_SLACK_SEQ;
use crate::wal_clock_step::{CLOCK_STEP_MIN_NANOS, ClockStepLog, StepWindows, lead_window};
use crate::ws_frame_spill::{
    PACKET_INDEX_BITS, end_counter_lead_episode_on, lead_exceeds, mint_base, note_lead_began_in,
};

const SEC: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;
const REFRESH: u64 = 30 * SEC;
const IST: u64 = 19_800;
/// 2026-11-23 09:00:00 IST in UTC nanos: an IST hour boundary.
const BASE_HOUR: u64 = 1_795_404_600 * SEC;

#[derive(Debug, Clone, Copy)]
enum Event {
    None,
    /// The wall clock steps by `nanos`, forward or back.
    Step {
        forward: bool,
        nanos: u64,
    },
    /// The process crashes and restarts; the wall steps back by `back` as it
    /// does (a restart with the persisted high-water ahead of the wall).
    Restart {
        back: u64,
    },
}

#[derive(Debug, Clone, Copy)]
struct Segment {
    event: Event,
    dur_ms: u64,
    rate: u64,
    /// The tick writer's batch cadence, `None`: stalled for the segment.
    sample_every_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct Flags {
    mint_hook: bool,
    archive_record: bool,
}

const REAL: Flags = Flags {
    mint_hook: true,
    archive_record: true,
};

struct Process {
    log: ClockStepLog,
    since: AtomicU64,
    /// Index of this process's first frame in `frames`.
    first_frame: usize,
}

struct Sim {
    dir: PathBuf,
    flags: Flags,
    t: u64,
    off: i128,
    anchor: (u64, u64),
    next_refresh: u64,
    counter_base: u64,
    frames: Vec<(u64, u64, bool)>,
    p: Process,
    exempt: usize,
}

fn temp_dir() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("tv-lead-model-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// The frame span of the IST hour holding `receipt` (the arithmetic of
/// [`hour_frame_seq_span`], checked against it below).
fn hour_span_of(receipt: u64) -> HourFrameSpan {
    let ist = receipt / SEC + IST;
    let start_utc = ist - ist % 3_600 - IST;
    HourFrameSpan {
        lo: (start_utc * SEC).saturating_sub(REPLAY_REORDER_SLACK_SEQ),
        hi: (start_utc + 3_600) * SEC + RECEIPT_SLOP_NANOS + REPLAY_REORDER_SLACK_SEQ,
    }
}

/// Whether `seq` is in its receipt hour's span or inside a window of
/// `windows` (or `extra`) that overlaps the span: the invariant.
fn covered(windows: &StepWindows, extra: Option<(u64, u64)>, seq: u64, receipt: u64) -> bool {
    let span = hour_span_of(receipt);
    let inside = |(lo, hi): (u64, u64)| lo <= span.hi && span.lo <= hi && lo <= seq && seq <= hi;
    (span.lo <= seq && seq <= span.hi) || windows.iter().any(inside) || extra.is_some_and(inside)
}

impl Sim {
    fn new(start: u64, flags: Flags) -> Self {
        let dir = temp_dir();
        let log = ClockStepLog::new();
        log.bind(&dir, start);
        Self {
            dir,
            flags,
            t: start,
            off: 0,
            anchor: (start, start),
            next_refresh: start + REFRESH,
            counter_base: 0,
            frames: Vec::new(),
            p: Process {
                log,
                since: AtomicU64::new(0),
                first_frame: 0,
            },
            exempt: 0,
        }
    }

    fn wall(&self, t: u64) -> u64 {
        u64::try_from(i128::from(t) + self.off).unwrap_or(0)
    }

    fn receipt(&self, t: u64) -> u64 {
        self.anchor.1 + (t - self.anchor.0)
    }

    /// `refresh_noting_step` with no read gap.
    fn refresh(&mut self) {
        let (old_t, old_wall) = self.anchor;
        let projected = old_wall + (self.t - old_t);
        let new = self.wall(self.t);
        let (adopt, jump) = if new >= projected {
            (true, new - projected)
        } else {
            let rewind = projected - new;
            (rewind > SEC, rewind)
        };
        if adopt {
            self.anchor = (self.t, new);
            if jump > CLOCK_STEP_MIN_NANOS {
                self.p.log.note_from("receipt_anchor", old_wall, new, jump);
            }
        }
        self.next_refresh = self.t + REFRESH;
    }

    fn mint(&mut self, t: u64) {
        let now = self.wall(t);
        let now_base = now >> PACKET_INDEX_BITS;
        let next = mint_base(self.counter_base, now_base, u64::MAX >> PACKET_INDEX_BITS);
        self.counter_base = next;
        if self.flags.mint_hook && lead_exceeds(next, now_base) {
            note_lead_began_in(&self.p.since, now);
        }
        self.frames
            .push((next << PACKET_INDEX_BITS, self.receipt(t), false));
    }

    /// What `record_pending_counter_lead` does, on the private cell: the
    /// record and its own write, then the end (with its re-check of the
    /// counter against the wall) through the production functions.
    fn record(&self, now: u64) {
        let seq = self.counter_base << PACKET_INDEX_BITS;
        let since = self.p.since.load(Ordering::SeqCst);
        if self.p.log.record_and_persist_pending_lead(since, now, seq) {
            let counter = AtomicU64::new(seq);
            end_counter_lead_episode_on(&self.p.since, since, &counter, || now);
        }
    }

    /// The tick writer's per-batch call (`persist_clock_step_log_if_dirty`).
    fn sample(&mut self) {
        let now = self.wall(self.t);
        self.record(now);
        self.p
            .log
            .follow_counter_lead(now, self.counter_base << PACKET_INDEX_BITS);
        self.p.log.persist_if_dirty();
    }

    fn restart(&mut self, back: u64) {
        // What the crashed process held in RAM: its windows and its pending
        // lead episode, recorded as the next call would have.
        let crash_wall = self.wall(self.t);
        let since = self.p.since.load(Ordering::Acquire);
        let pending = (since != 0)
            .then(|| lead_window(since, crash_wall, self.counter_base << PACKET_INDEX_BITS));
        let in_ram = self.p.log.windows();
        self.off -= i128::from(back.min(crash_wall / 2));
        let boot = self.wall(self.t);
        let log = ClockStepLog::new();
        log.bind(&self.dir, boot);
        let newest = self
            .frames
            .last()
            .map(|&(seq, receipt, _)| (seq, i64::try_from(receipt).unwrap_or(0)));
        log.note_boot_counter_lead(boot, newest);
        // `bind` runs the belt itself, from the WAL on disk, and then writes
        // what it added at once. This model has no WAL on disk, so its bind's
        // belt finds nothing: the model runs the belt here and writes it
        // here, standing in for bind's own write (which this call therefore
        // masks; that write is pinned by
        // `wal_clock_step::tests::test_bind_runs_the_boot_belt_from_the_wal_and_writes_it_at_once`).
        let _written = log.persist_if_dirty();
        // The documented gap: a frame whose only cover was in RAM at the
        // crash (a lead episode or an anchor-noted step not yet persisted,
        // the belt not firing) is exempt; every other frame is checked.
        let kept = log.windows();
        for frame in &mut self.frames[self.p.first_frame..] {
            let (seq, receipt, _) = *frame;
            if !covered(&kept, None, seq, receipt) && covered(&in_ram, pending, seq, receipt) {
                frame.2 = true;
                self.exempt += 1;
            }
        }
        // The counter is seeded past the newest frame on disk.
        if let Some(&(seq, _, _)) = self.frames.last() {
            self.counter_base = self.counter_base.max((seq >> PACKET_INDEX_BITS) + 1);
        }
        self.anchor = (self.t, boot);
        self.next_refresh = self.t + REFRESH;
        self.p = Process {
            log,
            since: AtomicU64::new(0),
            first_frame: self.frames.len(),
        };
    }

    fn run_ms(&mut self, dur_ms: u64, rate: u64, sample_every_ms: Option<u64>) {
        for i in 0..dur_ms {
            if self.t >= self.next_refresh {
                self.refresh();
            }
            let n = rate * (i + 1) / 1_000 - rate * i / 1_000;
            for k in 0..n {
                self.mint(self.t + k * (MS / n));
            }
            if sample_every_ms.is_some_and(|e| (i + 1) % e == 0) {
                self.sample();
            }
            self.t += MS;
        }
    }

    fn run(mut self, segments: &[Segment]) -> Result<usize, String> {
        let mut cadence = None;
        let mut last_step: Option<u64> = None;
        for seg in segments {
            match seg.event {
                Event::None => {}
                // Two steps less than one anchor refresh apart are seen by the
                // receipt anchor as their net move (a pre-existing limit of
                // the anchor, not of the lead record), so the model spaces
                // steps more than one refresh apart.
                Event::Step { .. } if last_step.is_some_and(|at| self.t < at + REFRESH + SEC) => {}
                Event::Step { forward, nanos } => {
                    last_step = Some(self.t);
                    if forward {
                        self.off += i128::from(nanos);
                    } else {
                        self.off -= i128::from(nanos);
                    }
                }
                Event::Restart { back } => self.restart(back),
            }
            cadence = seg.sample_every_ms;
            self.run_ms(seg.dur_ms, seg.rate, seg.sample_every_ms);
        }
        // A quiet tail past one anchor refresh (a forward step is recorded
        // at the refresh), then the archive verdict's own record call.
        self.run_ms(REFRESH / MS + 1_000, 0, cadence);
        if self.flags.archive_record {
            self.record(self.wall(self.t));
        }
        let windows = self.p.log.windows();
        let result = self.frames.iter().find_map(|&(seq, receipt, exempt)| {
            (!exempt && !covered(&windows, None, seq, receipt)).then(|| {
                format!(
                    "frame seq {seq} receipt {receipt} lies outside its hour span {:?} \
                     and every overlapping window of {:?}",
                    hour_span_of(receipt),
                    windows.iter().collect::<Vec<_>>()
                )
            })
        });
        let _ = std::fs::remove_dir_all(&self.dir);
        result.map_or(Ok(self.exempt), Err)
    }
}

/// A 3 s burst at 20,000 frames a second ending just before an IST hour
/// boundary, with the tick writer stalled throughout: the counter leads the
/// wall by about 4.9 s at the boundary, past the hour span's 3 s of slop and
/// slack, and the lead has decayed before anything samples it.
fn stalled_burst_at_the_boundary() -> (u64, Vec<Segment>) {
    let start = BASE_HOUR + 3_600 * SEC - 3 * SEC;
    let segs = vec![
        Segment {
            event: Event::None,
            dur_ms: 3_000,
            rate: 20_000,
            sample_every_ms: None,
        },
        Segment {
            event: Event::None,
            dur_ms: 10_000,
            rate: 0,
            sample_every_ms: None,
        },
    ];
    (start, segs)
}

#[test]
fn test_lead_model_hour_span_matches_hour_frame_seq_span() {
    let span = hour_frame_seq_span("2026-11-23T09").expect("span");
    assert_eq!(hour_span_of(BASE_HOUR), span);
    assert_eq!(hour_span_of(BASE_HOUR + 3_599 * SEC), span);
    assert_ne!(hour_span_of(BASE_HOUR + 3_600 * SEC), span);
}

/// The stalled burst is held with both halves of PR24a, and is a
/// counterexample with either removed (the bites the plan asks for).
#[test]
fn test_lead_model_bites_without_the_mint_hook_or_the_archive_record() {
    let (start, segs) = stalled_burst_at_the_boundary();
    assert_eq!(Sim::new(start, REAL).run(&segs), Ok(0));
    let no_hook = Flags {
        mint_hook: false,
        ..REAL
    };
    assert!(
        Sim::new(start, no_hook).run(&segs).is_err(),
        "mint hook bite"
    );
    let no_archive_record = Flags {
        archive_record: false,
        ..REAL
    };
    assert!(
        Sim::new(start, no_archive_record).run(&segs).is_err(),
        "archive record bite"
    );
}

/// A backward step of 2 h at low rate with a sampling writer, a restart
/// while the counter still leads, and the boot belt over a crash inside a
/// burst: each held with no exemption.
#[test]
fn test_lead_model_backward_step_restart_and_boot_belt() {
    let start = BASE_HOUR + 3_600 * SEC - 20 * SEC;
    let segs = [
        Segment {
            event: Event::None,
            dur_ms: 5_000,
            rate: 500,
            sample_every_ms: Some(200),
        },
        Segment {
            event: Event::Step {
                forward: false,
                nanos: 7_200 * SEC,
            },
            dur_ms: 40_000,
            rate: 300,
            sample_every_ms: Some(500),
        },
        Segment {
            event: Event::Restart { back: 0 },
            dur_ms: 10_000,
            rate: 300,
            sample_every_ms: Some(500),
        },
    ];
    assert_eq!(Sim::new(start, REAL).run(&segs), Ok(0));
    // A crash inside a stalled burst: the newest frame leads its receipt,
    // so the belt records the last session.
    let segs = [
        Segment {
            event: Event::None,
            dur_ms: 3_000,
            rate: 20_000,
            sample_every_ms: None,
        },
        Segment {
            event: Event::Restart { back: 0 },
            dur_ms: 5_000,
            rate: 100,
            sample_every_ms: Some(100),
        },
    ];
    let (start, _) = stalled_burst_at_the_boundary();
    assert_eq!(Sim::new(start, REAL).run(&segs), Ok(0));
}

/// The shrunk counterexamples proptest saved while PR24a was built, as fixed
/// inputs (review finding, 2026-11-23: the seeds file replays only while the
/// strategy keeps its shape). The first three are held with the code as it
/// is now. The fourth puts a 15 s forward step 2 ms after a 30 s backward
/// one: the receipt anchor sees two steps less than one refresh apart only
/// as their net move (a limit that predates PR24a), so the model drops the
/// second step, and the case runs as the first step alone.
#[test]
fn test_lead_model_saved_counterexamples_replay_as_named_cases() {
    let seg = |event, dur_ms, rate, sample_every_ms| Segment {
        event,
        dur_ms,
        rate,
        sample_every_ms,
    };
    let back = |nanos| Event::Step {
        forward: false,
        nanos,
    };
    let cases: [(u64, Vec<Segment>); 3] = [
        (
            0,
            vec![
                seg(back(134_792_067_818), 1, 1_000, Some(1)),
                seg(Event::Restart { back: 0 }, 1, 0, Some(1)),
            ],
        ),
        (
            2_969,
            vec![
                seg(Event::None, 40_501, 0, Some(1)),
                seg(back(3_080_143_069_230), 1, 1_000, Some(1)),
                seg(Event::Restart { back: 200_635_347 }, 3, 334, Some(4)),
                seg(Event::Restart { back: 0 }, 1, 0, Some(1)),
            ],
        ),
        (
            0,
            vec![
                seg(back(91_447_246_465), 2_365, 8_000, Some(1)),
                seg(Event::None, 27_636, 603, Some(5)),
                seg(Event::Restart { back: 0 }, 1, 0, Some(1)),
            ],
        ),
    ];
    for (start_off, segs) in cases {
        let outcome = Sim::new(BASE_HOUR + start_off * SEC, REAL).run(&segs);
        assert!(outcome.is_ok(), "start_off {start_off}: {outcome:?}");
    }
    // The excluded shape: the second step lies inside one refresh of the
    // first, so the model drops it and the run equals the first step alone.
    const { assert!(2 * MS < REFRESH + SEC) };
    let both = [
        seg(back(30_000_902_721), 2, 500, Some(1)),
        seg(
            Event::Step {
                forward: true,
                nanos: 15_000_885_826,
            },
            1,
            0,
            Some(1),
        ),
    ];
    let first_alone = [
        seg(back(30_000_902_721), 2, 500, Some(1)),
        seg(Event::None, 1, 0, Some(1)),
    ];
    assert_eq!(
        Sim::new(BASE_HOUR, REAL).run(&both),
        Sim::new(BASE_HOUR, REAL).run(&first_alone)
    );
    assert!(Sim::new(BASE_HOUR, REAL).run(&both).is_ok());
}

fn segment() -> impl Strategy<Value = Segment> {
    let step_nanos = prop_oneof![0_u64..(3 * SEC), 0_u64..(7_200 * SEC)];
    let event = prop_oneof![
        6 => Just(Event::None),
        2 => (any::<bool>(), step_nanos.clone())
            .prop_map(|(forward, nanos)| Event::Step { forward, nanos }),
        1 => prop_oneof![Just(0_u64), step_nanos].prop_map(|back| Event::Restart { back }),
    ];
    let load = prop_oneof![
        3 => (1_u64..60_000, 0_u64..1_500),
        1 => (100_u64..3_000, 8_000_u64..=20_000),
    ];
    let cadence = prop_oneof![
        3 => (1_u64..2_000).prop_map(Some),
        1 => Just(None),
    ];
    (event, load, cadence).prop_map(|(event, (dur_ms, rate), sample_every_ms)| Segment {
        event,
        dur_ms,
        rate,
        sample_every_ms,
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Every frame's sequence is in its receipt hour's span or inside a
    /// recorded window overlapping it, through bursts up to 20,000 frames a
    /// second, steps of up to 2 h either way, restarts with the counter
    /// ahead of the wall, and a tick writer stalled for whole segments.
    #[test]
    fn frame_seq_lead_window_properties(
        start_off in 0_u64..3_600,
        segs in proptest::collection::vec(segment(), 1..6),
    ) {
        let start = BASE_HOUR + start_off * SEC;
        let outcome = Sim::new(start, REAL).run(&segs);
        prop_assert!(outcome.is_ok(), "{:?}", outcome);
    }
}
