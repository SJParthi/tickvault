//! Chaos: the process is killed during the evening drain restart.
//!
//! The evening drain (`crates/app/src/evening_drain.rs`, operator Quote 31)
//! keeps one small state file across a planned restart. This file kills the
//! process at every point around that file and drives the decision across a
//! whole evening, asserting the module's own bounds:
//!
//! - never more than `EVENING_DRAIN_RESTARTS_MAX` restarts an evening;
//! - `EVENING_DRAIN_SPACING_SECS` between two recorded restarts;
//! - a torn, unreadable or garbage state file stops the restarts (fails
//!   closed) and is never overwritten by the decision path;
//! - a restart after which the backlog did not shrink is never retried;
//! - a persisted give-up never flips back;
//! - no restart before the day's cross-verification is settled (its marker,
//!   or `EVENING_DRAIN_XVERIFY_CLEAR_SECS_IST`, 16:25 IST since 2026-10-22),
//!   before the day's own work is done, or from 05:00 to 15:50 IST.
//!
//! The supervisor's per-minute composition (load, decide, latch, record) is
//! modelled by [`supervise`]; `the_supervisor_model_matches_drain_hold_rs`
//! pins every assumption of that model to `drain_hold.rs`'s source, so the
//! model cannot drift from the code it stands for. The model omits the
//! supervisor's stall and hold-write gates, which can only REMOVE restarts,
//! so it is a superset of what production can do.
//!
//! Hermetic: temp directories only, no QuestDB, no network, bounded loops.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tickvault_app::evening_drain::{
    EVENING_DRAIN_CATCHUP_BUDGET_SECS, EVENING_DRAIN_FIRST_SECS_IST,
    EVENING_DRAIN_LAST_RESTART_SECS_IST, EVENING_DRAIN_PENDING_MAX_AGE_SECS,
    EVENING_DRAIN_RESTARTS_MAX, EVENING_DRAIN_SPACING_SECS, EVENING_DRAIN_STATE_PATH,
    EVENING_DRAIN_XVERIFY_CLEAR_SECS_IST, EveningState, EveningStep, budget_or, catchup_budget_for,
    crossverify_settled, evening_step, load_state, state_for_give_up, state_for_restart,
    store_state,
};

const DAY: i32 = 739_910;
const IST_OFFSET_SECS: i64 = 19_800;
const SECS_PER_DAY: i64 = 86_400;
/// 15:40 IST, when the drain-hold supervisor starts looking.
const SUPERVISOR_START_SECS_IST: i64 = 15 * 3_600 + 40 * 60;
/// 07:30 IST, the drain-hold ceiling (supervisor stops looking).
const SUPERVISOR_END_SECS_IST: i64 = 7 * 3_600 + 30 * 60;
const MINUTE: i64 = 60;

// ---------------------------------------------------------------- helpers

static DIR_SEQ: AtomicU64 = AtomicU64::new(0);
/// `budget_or` reads a cwd-relative path; the one test that moves the cwd
/// holds this, and every other test here uses absolute paths only.
static CWD_LOCK: Mutex<()> = Mutex::new(());

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tv-chaos-evening-{}-{}-{}",
            std::process::id(),
            tag,
            n
        ));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn state_path(&self) -> PathBuf {
        self.0
            .join("data")
            .join("state")
            .join("evening-drain.state")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.0));
    }
}

/// Unix seconds of 15:40 IST on the evening `offset_days` after the base.
fn evening_start_unix(offset_days: i64) -> i64 {
    let base: i64 = 1_792_000_000;
    let secs = (base + IST_OFFSET_SECS).rem_euclid(SECS_PER_DAY);
    let shift = (SUPERVISOR_START_SECS_IST - secs).rem_euclid(SECS_PER_DAY);
    base + shift + offset_days * SECS_PER_DAY
}

fn secs_ist_of(unix: i64) -> u64 {
    u64::try_from((unix + IST_OFFSET_SECS).rem_euclid(SECS_PER_DAY)).unwrap_or(0)
}

/// Minutes from 15:40 to 07:29 IST the next morning.
fn evening_minutes() -> i64 {
    (SECS_PER_DAY - SUPERVISOR_START_SECS_IST + SUPERVISOR_END_SECS_IST) / MINUTE
}

fn in_no_restart_band(secs: u64) -> bool {
    (EVENING_DRAIN_LAST_RESTART_SECS_IST..EVENING_DRAIN_FIRST_SECS_IST).contains(&secs)
}

struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn sample_states() -> Vec<EveningState> {
    vec![
        EveningState::default(),
        EveningState {
            day: DAY,
            restarts: 2,
            last_unix: 1_792_000_000,
            backlog_before: 400,
            pending_unix: 1_792_000_000,
            gave_up: false,
        },
        EveningState {
            day: DAY,
            restarts: EVENING_DRAIN_RESTARTS_MAX,
            last_unix: 1_792_003_600,
            backlog_before: 1_696,
            pending_unix: 0,
            gave_up: true,
        },
        EveningState {
            day: i32::MAX,
            restarts: u32::MAX,
            last_unix: i64::MAX,
            backlog_before: u64::MAX,
            pending_unix: i64::MIN,
            gave_up: true,
        },
    ]
}

// ------------------------------------------------------ supervisor model

/// One minute of input to the supervisor.
#[derive(Clone, Copy)]
struct Minute {
    now: i64,
    secs: u64,
    other_done: bool,
    marker: bool,
    backlog: Option<u64>,
}

/// Where the process dies inside one supervisor minute.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kill {
    /// No kill.
    None,
    /// Restart decided, killed before the record was written.
    BeforeRestartStore,
    /// Record written, killed (or the request lost) before the restart.
    AfterRestartStoreRequestLost,
    /// Give-up decided, killed before the latch was written.
    BeforeGiveUpStore,
}

struct Outcome {
    step: EveningStep,
    /// The restart record written this minute, if any.
    recorded: Option<EveningState>,
}

/// The drain-hold supervisor's per-minute composition (drain_hold.rs, the
/// evening block; pinned by `the_supervisor_model_matches_drain_hold_rs`).
fn supervise(path: &Path, day: i32, m: &Minute, kill: Kill) -> Outcome {
    let loaded = load_state(path);
    let step = match loaded {
        Some(e) => evening_step(
            &e,
            day,
            m.other_done,
            crossverify_settled(m.marker, m.secs),
            m.backlog,
            m.now,
            m.secs,
        ),
        None => EveningStep::Idle,
    };
    let mut out = Outcome {
        step,
        recorded: None,
    };
    if let Some(e) = loaded {
        if step == EveningStep::GaveUp
            && !(e.day == day && e.gave_up)
            && kill != Kill::BeforeGiveUpStore
        {
            store_state(path, &state_for_give_up(&e, day)).expect("latch store");
        }
        if step == EveningStep::Restart && kill != Kill::BeforeRestartStore {
            let next = state_for_restart(&e, day, m.backlog.unwrap_or(0), m.now);
            store_state(path, &next).expect("restart store");
            out.recorded = Some(next);
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Model {
    Shrinks,
    Flat,
    Grows,
    Mixed,
}

struct EveningReport {
    recorded: Vec<EveningState>,
    restart_decisions: u32,
    gave_up_persisted: bool,
}

/// Drives one evening from 15:40 to 07:29 IST, one supervisor minute at a
/// time, injecting kills, scan races and (optionally) a torn state file.
/// Every invariant is asserted inline, minute by minute.
fn drive_evening(
    path: &Path,
    day: i32,
    start_unix: i64,
    model: Model,
    rng: &mut SplitMix,
    tear_at_minute: Option<i64>,
) -> EveningReport {
    let mut backlog: u64 = 50 + rng.below(1_700);
    // (done_at, value) of the replay a (re)started boot is running.
    let mut replay: Option<(i64, u64)> = None;
    let other_done_at = start_unix + MINUTE * i64::try_from(rng.below(180)).unwrap_or(0);
    let marker_at = match rng.below(3) {
        0 => None,
        _ => Some(start_unix + MINUTE * i64::try_from(rng.below(60)).unwrap_or(0)),
    };
    let mut crashes = 0u32;
    let mut torn_bytes: Option<Vec<u8>> = None;
    let mut report = EveningReport {
        recorded: Vec::new(),
        restart_decisions: 0,
        gave_up_persisted: false,
    };
    let replay_value = |b: u64, rng: &mut SplitMix| -> u64 {
        let m = if model == Model::Mixed {
            match rng.below(3) {
                0 => Model::Shrinks,
                1 => Model::Flat,
                _ => Model::Grows,
            }
        } else {
            model
        };
        match m {
            Model::Shrinks => b * 3 / 5,
            Model::Flat | Model::Mixed => b,
            Model::Grows => b + 3,
        }
    };

    for k in 0..evening_minutes() {
        let now = start_unix + k * MINUTE;
        let secs = secs_ist_of(now);
        if tear_at_minute == Some(k) && torn_bytes.is_none() {
            let full = std::fs::read(path).unwrap_or_default();
            assert!(full.len() >= 2, "nothing to tear");
            // Any cut short of the last field (never the same state).
            let cut_max = u64::try_from(full.len() - 1).unwrap_or(1);
            let cut = usize::try_from(rng.below(cut_max)).unwrap_or(0);
            let torn = full[..cut].to_vec();
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(path, &torn).expect("tear");
            torn_bytes = Some(torn);
        }
        if let Some((done_at, value)) = replay
            && now >= done_at
        {
            backlog = value;
            replay = None;
        }
        // A scan that races the catch-up's segment moves can read nothing.
        let scan = if (replay.is_some() && rng.below(4) == 0) || rng.below(97) == 0 {
            None
        } else {
            Some(backlog)
        };
        let m = Minute {
            now,
            secs,
            other_done: now >= other_done_at,
            marker: marker_at.is_some_and(|t| now >= t),
            backlog: scan,
        };
        let kill = if crashes >= 2 {
            Kill::None
        } else {
            match rng.below(40) {
                0 => Kill::BeforeRestartStore,
                1 => Kill::AfterRestartStoreRequestLost,
                2 => Kill::BeforeGiveUpStore,
                _ => Kill::None,
            }
        };
        let persisted_before = load_state(path);
        let latched_before = persisted_before.is_some_and(|s| s.day == day && s.gave_up);
        let out = supervise(path, day, &m, kill);

        // Torn file: fails closed and is never overwritten.
        if let Some(torn) = &torn_bytes {
            assert_eq!(
                out.step,
                EveningStep::Idle,
                "torn state must idle at minute {k}"
            );
            assert_eq!(
                std::fs::read(path).expect("read torn"),
                *torn,
                "the decision path must never overwrite a torn state file"
            );
        }
        // A persisted give-up never flips back.
        if latched_before {
            assert_eq!(
                out.step,
                EveningStep::GaveUp,
                "latched give-up flipped at {k}"
            );
        }
        if out.step == EveningStep::Restart {
            report.restart_decisions += 1;
            assert!(!in_no_restart_band(secs), "restart at {secs} s IST");
            assert!(m.other_done, "restart before the day's own work was done");
            assert!(
                m.marker || !(12 * 3_600..EVENING_DRAIN_XVERIFY_CLEAR_SECS_IST).contains(&secs),
                "restart before the cross-verification settled ({secs} s IST)"
            );
            assert!(m.backlog.is_some_and(|b| b > 0), "restart with no backlog");
        }
        match (out.step, kill) {
            (EveningStep::Restart, Kill::BeforeRestartStore) => {
                // A crash restart: its catch-up replays, uncounted.
                crashes += 1;
                replay = Some((now + 10 * MINUTE, replay_value(backlog, rng)));
            }
            (EveningStep::Restart, Kill::AfterRestartStoreRequestLost) => {
                // Recorded, but no restart came: nothing replayed.
            }
            (EveningStep::Restart, _) => {
                let catch_up = MINUTE * (5 + i64::try_from(rng.below(21)).unwrap_or(0));
                replay = Some((now + catch_up, replay_value(backlog, rng)));
            }
            (EveningStep::GaveUp, Kill::BeforeGiveUpStore) => {
                crashes += 1;
                replay = Some((now + 10 * MINUTE, replay_value(backlog, rng)));
            }
            _ => {}
        }
        if let Some(rec) = out.recorded {
            report.recorded.push(rec);
        }
        if load_state(path).is_some_and(|s| s.day == day && s.gave_up) {
            report.gave_up_persisted = true;
        }
    }

    // Evening-wide bounds.
    let n = u32::try_from(report.recorded.len()).unwrap_or(u32::MAX);
    assert!(n <= EVENING_DRAIN_RESTARTS_MAX, "{n} restarts recorded");
    for rec in &report.recorded {
        assert_eq!(rec.day, day);
        assert!(
            rec.restarts <= EVENING_DRAIN_RESTARTS_MAX,
            "restart #{} recorded",
            rec.restarts
        );
    }
    if let Some(first) = report.recorded.first() {
        // A fresh evening starts at 1 (a seeded file may already hold some).
        assert!(first.restarts >= 1);
    }
    for pair in report.recorded.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        assert_eq!(b.restarts, a.restarts + 1, "restart count skipped");
        assert!(
            b.last_unix - a.last_unix >= EVENING_DRAIN_SPACING_SECS,
            "spacing {} s < {}",
            b.last_unix - a.last_unix,
            EVENING_DRAIN_SPACING_SECS
        );
        assert!(
            b.backlog_before < a.backlog_before,
            "a restart that did not shrink the backlog was retried ({} -> {})",
            a.backlog_before,
            b.backlog_before
        );
    }
    if matches!(model, Model::Flat | Model::Grows) {
        assert!(
            n <= 1,
            "{model:?}: {n} restarts, a no-progress restart was retried"
        );
    }
    report
}

// --------------------------------------------------------------- tests

#[test]
fn the_supervisor_model_matches_drain_hold_rs() {
    let src = include_str!("../src/drain_hold.rs");
    for needle in [
        "crate::evening_drain::load_state(evening_path)",
        "Some(evening) if on_aws => crate::evening_drain::evening_step(",
        "crate::evening_drain::crossverify_settled(xverify_marker, secs_ist)",
        "_ => crate::evening_drain::EveningStep::Idle,",
        "evening_step == crate::evening_drain::EveningStep::GaveUp",
        "&& !(e.day == evening_day && e.gave_up)",
        "crate::evening_drain::state_for_give_up(&evening, evening_day)",
        "evening_step == crate::evening_drain::EveningStep::Restart && !stalled",
        "let backlog = work.wal_backlog.unwrap_or(0);",
        "crate::evening_drain::state_for_restart(evening, day, backlog, now_unix)",
    ] {
        assert!(
            src.contains(needle),
            "drain_hold.rs no longer contains: {needle}"
        );
    }
    // The restart is asked only after the record landed.
    let fn_at = src
        .find("async fn request_evening_restart(")
        .expect("request_evening_restart");
    let body = &src[fn_at..];
    let store = body.find("store_state(path, &next)").expect("store");
    let ok_arm = body.find("Ok(Ok(())) =>").expect("ok arm");
    let request = body.find("request_self_restart();").expect("request");
    assert!(
        store < ok_arm && ok_arm < request,
        "restart asked before the record landed"
    );
    let err_arm = body.find("Ok(Err(err)) =>").expect("err arm");
    assert!(
        request < err_arm,
        "the request must sit inside the Ok(Ok(())) arm only"
    );
}

#[test]
fn torn_state_file_at_every_byte_offset_fails_closed_or_reads_identical() {
    let dir = TempDir::new("torn-offsets");
    let path = dir.state_path();
    let mut checked = 0u32;
    for state in sample_states() {
        let full = state.to_line().into_bytes();
        for cut in 0..=full.len() {
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(&path, &full[..cut]).expect("write torn");
            let loaded = load_state(&path);
            // Dropping only the trailing newline is the same state; any
            // other cut loses at least the last field.
            if cut + 1 >= full.len() {
                assert_eq!(loaded, Some(state), "cut {cut} of {}", full.len());
            } else {
                assert_eq!(loaded, None, "cut {cut} of {:?} parsed", state.to_line());
            }
            checked += 1;
        }
    }
    assert!(checked > 100, "only {checked} offsets checked");
}

#[test]
fn a_torn_state_file_blocks_every_restart_for_the_whole_evening_and_is_never_rewritten() {
    let dir = TempDir::new("torn-evening");
    let path = dir.state_path();
    let start = evening_start_unix(0);
    let state = state_for_restart(&EveningState::default(), DAY, 900, start - SECS_PER_DAY);
    let full = state.to_line().into_bytes();
    // Every torn offset that does not read as the same state.
    for cut in 0..full.len() - 1 {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, &full[..cut]).expect("write torn");
        // Sampled every 7th minute to keep the file fast (~ 136 x 40 reads).
        for k in (0..evening_minutes()).step_by(7) {
            let now = start + k * MINUTE;
            let m = Minute {
                now,
                secs: secs_ist_of(now),
                other_done: true,
                marker: true,
                backlog: Some(1_000),
            };
            let out = supervise(&path, DAY, &m, Kill::None);
            assert_eq!(out.step, EveningStep::Idle, "cut {cut}, minute {k}");
            assert!(out.recorded.is_none());
        }
        assert_eq!(std::fs::read(&path).expect("read"), &full[..cut]);
    }
}

#[test]
fn missing_file_reads_as_a_fresh_evening_and_store_creates_the_directories() {
    let dir = TempDir::new("missing");
    let path = dir.state_path();
    assert!(!path.exists() && !path.parent().expect("parent").exists());
    assert_eq!(load_state(&path), Some(EveningState::default()));
    let start = evening_start_unix(0);
    let m = Minute {
        now: start + 30 * MINUTE,
        secs: secs_ist_of(start + 30 * MINUTE),
        other_done: true,
        marker: true,
        backlog: Some(77),
    };
    let out = supervise(&path, DAY, &m, Kill::None);
    assert_eq!(
        out.step,
        EveningStep::Restart,
        "16:10 with the marker written"
    );
    let written = load_state(&path).expect("written");
    assert_eq!(
        (written.day, written.restarts, written.backlog_before),
        (DAY, 1, 77)
    );
}

#[test]
fn a_directory_or_a_file_in_the_way_is_unreadable_and_fails_closed() {
    // A directory where the state file should be.
    let dir = TempDir::new("dir-in-way");
    let path = dir.state_path();
    std::fs::create_dir_all(&path).expect("dir at the file path");
    assert_eq!(
        load_state(&path),
        None,
        "a directory must not read as no restart yet"
    );
    assert!(store_state(&path, &EveningState::default()).is_err());
    let now = evening_start_unix(0) + 60 * MINUTE;
    let m = Minute {
        now,
        secs: secs_ist_of(now),
        other_done: true,
        marker: true,
        backlog: Some(10),
    };
    assert_eq!(
        supervise(&path, DAY, &m, Kill::None).step,
        EveningStep::Idle
    );
    assert!(
        path.is_dir(),
        "the decision path must not replace the directory"
    );

    // A FILE where the state directory should be.
    let dir2 = TempDir::new("file-in-way");
    let state_dir = dir2.0.join("data").join("state");
    std::fs::create_dir_all(state_dir.parent().expect("parent")).expect("data dir");
    std::fs::write(&state_dir, b"not a directory").expect("file in the way");
    let path2 = state_dir.join("evening-drain.state");
    assert_eq!(
        load_state(&path2),
        None,
        "ENOTDIR must not read as no restart yet"
    );
    assert!(store_state(&path2, &EveningState::default()).is_err());
    assert_eq!(
        supervise(&path2, DAY, &m, Kill::None).step,
        EveningStep::Idle
    );
}

#[test]
fn garbage_bytes_fail_closed_and_never_panic() {
    let dir = TempDir::new("garbage");
    let path = dir.state_path();
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    let fixed: [&[u8]; 12] = [
        b"",
        b"\n",
        b"\xff\xfe\x00\x01",
        b"v1 \xff 1 2 3 4 0\n",
        b"v1 1 2 3 4 5 0\x00",
        b"V1 1 2 3 4 5 0\n",
        b"v1 1 -2 3 4 5 0\n",
        b"v1 1 2 3 -4 5 0\n",
        b"v1 1 2 3 4 5 true\n",
        b"v1 1 2 3 4 5 0 0\n",
        b"v1 99999999999 2 3 4 5 0\n",
        b"v1 1 4294967296 3 4 5 0\n",
    ];
    for bytes in fixed {
        std::fs::write(&path, bytes).expect("write");
        assert_eq!(load_state(&path), None, "parsed {bytes:?}");
    }
    let mut rng = SplitMix(0xE7E1_96D7_A1A0_0001);
    for _ in 0..2_000 {
        let len = usize::try_from(rng.below(64)).unwrap_or(0);
        let bytes: Vec<u8> = (0..len)
            .map(|_| u8::try_from(rng.below(256)).unwrap_or(0))
            .collect();
        std::fs::write(&path, &bytes).expect("write");
        if !bytes.starts_with(b"v1") {
            assert_eq!(load_state(&path), None, "parsed random {bytes:?}");
        }
    }
}

/// Every kill point of one restart minute: before the decision is acted on,
/// with a torn temp file left behind, between the temp write and the rename,
/// after the rename and before the restart is asked.
#[test]
fn a_kill_at_every_point_of_the_restart_write_keeps_the_count_and_the_spacing() {
    let start = evening_start_unix(0);
    let first = start + 60 * MINUTE;
    let prior = state_for_restart(&EveningState::default(), DAY, 500, first);
    let at = first + EVENING_DRAIN_SPACING_SECS;
    let m = Minute {
        now: at,
        secs: secs_ist_of(at),
        other_done: true,
        marker: true,
        backlog: Some(300),
    };

    // Kill before anything is written: the old record stands; no count lost
    // or gained, and the next process decides the same restart.
    let d = TempDir::new("kill-before");
    let p = d.state_path();
    store_state(&p, &prior).expect("prior");
    let out = supervise(&p, DAY, &m, Kill::BeforeRestartStore);
    assert_eq!(out.step, EveningStep::Restart);
    assert_eq!(load_state(&p), Some(prior));
    assert_eq!(
        supervise(&p, DAY, &m, Kill::None).step,
        EveningStep::Restart
    );

    // Killed after the temp file was written (whole or torn), before the
    // rename: the state file still holds the old record; the stray temp file
    // never reads, and the next write replaces it.
    let next = state_for_restart(&prior, DAY, 300, at);
    let full = next.to_line().into_bytes();
    for cut in 0..=full.len() {
        let d = TempDir::new("kill-tmp");
        let p = d.state_path();
        store_state(&p, &prior).expect("prior");
        std::fs::write(p.with_extension("tmp"), &full[..cut]).expect("stray tmp");
        assert_eq!(load_state(&p), Some(prior), "stray tmp cut {cut} read");
        store_state(&p, &next).expect("store over a stray tmp");
        assert_eq!(load_state(&p), Some(next));
        assert!(!p.with_extension("tmp").exists(), "tmp left after rename");
    }

    // Killed after the rename, before the restart was asked: the record
    // counts, the spacing holds the next process, then progress is judged.
    let d = TempDir::new("kill-after");
    let p = d.state_path();
    store_state(&p, &prior).expect("prior");
    let out = supervise(&p, DAY, &m, Kill::AfterRestartStoreRequestLost);
    assert_eq!(out.recorded, Some(next));
    for wait in [MINUTE, 10 * MINUTE, EVENING_DRAIN_SPACING_SECS - 1] {
        let later = Minute {
            now: at + wait,
            secs: secs_ist_of(at + wait),
            ..m
        };
        assert_eq!(
            supervise(&p, DAY, &later, Kill::None).step,
            EveningStep::Wait
        );
    }
    // No restart came, so the backlog is still 300: no progress, give up.
    let judged = Minute {
        now: at + EVENING_DRAIN_SPACING_SECS,
        secs: secs_ist_of(at + EVENING_DRAIN_SPACING_SECS),
        ..m
    };
    assert_eq!(
        supervise(&p, DAY, &judged, Kill::None).step,
        EveningStep::GaveUp
    );
    assert!(load_state(&p).is_some_and(|s| s.gave_up && s.restarts == 2));
    // And it stays given up as the count moves.
    let shrunk = Minute {
        backlog: Some(1),
        now: judged.now + 60 * MINUTE,
        secs: secs_ist_of(judged.now + 60 * MINUTE),
        ..m
    };
    assert_eq!(
        supervise(&p, DAY, &shrunk, Kill::None).step,
        EveningStep::GaveUp
    );
}

/// The pending flag survives a kill between `budget_or`'s read and its
/// clearing write, but only for the 15 minutes it stays fresh.
#[test]
fn a_kill_before_the_pending_flag_is_cleared_ages_out_in_fifteen_minutes() {
    let at = evening_start_unix(0) + 120 * MINUTE;
    let s = state_for_restart(&EveningState::default(), DAY, 40, at);
    let secs = secs_ist_of(at);
    let budget = EVENING_DRAIN_CATCHUP_BUDGET_SECS;
    assert_eq!(catchup_budget_for(&s, 300, at + 5, secs), budget);
    let edge = at + EVENING_DRAIN_PENDING_MAX_AGE_SECS;
    assert_eq!(catchup_budget_for(&s, 300, edge, secs_ist_of(edge)), budget);
    let stale = edge + 1;
    assert_eq!(catchup_budget_for(&s, 300, stale, secs_ist_of(stale)), 300);
    // The standard budget never shrinks.
    assert_eq!(catchup_budget_for(&s, 5_000, at + 5, secs), 5_000);
    // A morning boot never takes the evening clock, even with a fresh flag.
    let morning = 9 * 3_600;
    assert_eq!(catchup_budget_for(&s, 300, at + 5, morning), 300);
}

/// `budget_or` on the real cwd-relative path: one boot only, and a torn,
/// missing or blocked file gives the standard budget without a write.
#[test]
fn budget_or_applies_to_one_boot_and_fails_closed_on_a_bad_file() {
    let _guard = CWD_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = TempDir::new("budget-or");
    let old_cwd = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&dir.0).expect("chdir");
    let result = std::panic::catch_unwind(|| {
        let path = Path::new(EVENING_DRAIN_STATE_PATH);
        // Missing: standard, and nothing is created.
        assert_eq!(budget_or(300), 300);
        assert!(!path.exists());
        // Fresh pending flag: the budget the pure fn gives at this instant,
        // and the flag is cleared so a second boot takes the standard one.
        let now = chrono::Utc::now().timestamp();
        let s = state_for_restart(&EveningState::default(), DAY, 12, now);
        store_state(path, &s).expect("store");
        let secs = tickvault_app::dhan_feed_stack::now_ist_secs_of_day();
        let want = catchup_budget_for(&s, 300, now, secs);
        assert_eq!(budget_or(300), want);
        let cleared = load_state(path).expect("cleared");
        assert_eq!(
            cleared,
            EveningState {
                pending_unix: 0,
                ..s
            }
        );
        assert_eq!(budget_or(300), 300, "the flag applies to one boot only");
        // Torn: standard, file untouched.
        let torn = &s.to_line().into_bytes()[..9];
        std::fs::write(path, torn).expect("tear");
        assert_eq!(budget_or(300), 300);
        assert_eq!(std::fs::read(path).expect("read"), torn);
        // A directory in the way: standard.
        std::fs::remove_file(path).expect("rm");
        std::fs::create_dir(path).expect("dir in the way");
        assert_eq!(budget_or(300), 300);
        assert!(path.is_dir());
    });
    std::env::set_current_dir(old_cwd).expect("restore cwd");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Two evenings in a row on one file, 64 seeds per backlog model, kills
/// injected at random minutes. All bounds are asserted in `drive_evening`.
#[test]
fn whole_evenings_with_kills_keep_every_bound() {
    let mut totals = [0u32; 4];
    let mut give_ups = 0u32;
    for (mi, model) in [Model::Shrinks, Model::Flat, Model::Grows, Model::Mixed]
        .into_iter()
        .enumerate()
    {
        for seed in 0..64u64 {
            let dir = TempDir::new("evening");
            let path = dir.state_path();
            let mut rng = SplitMix(0xC4A0_5EED ^ (seed << 8) ^ u64::try_from(mi).unwrap_or(0));
            for evening in 0..2 {
                let day = DAY + i32::try_from(evening).unwrap_or(0);
                let r = drive_evening(
                    &path,
                    day,
                    evening_start_unix(evening),
                    model,
                    &mut rng,
                    None,
                );
                totals[mi] += u32::try_from(r.recorded.len()).unwrap_or(0);
                assert!(r.restart_decisions >= u32::try_from(r.recorded.len()).unwrap_or(0));
                give_ups += u32::from(r.gave_up_persisted);
            }
        }
    }
    // Non-vacuous: the shrinking backlog really restarts, flat/grow rarely.
    assert!(
        totals[0] > 128,
        "shrinking model restarted only {} times",
        totals[0]
    );
    assert!(totals[1] <= 128 && totals[2] <= 128, "{totals:?}");
    // Flat and growing evenings always end in a persisted give-up.
    assert!(
        give_ups >= 256,
        "only {give_ups} persisted give-ups of 512 evenings"
    );
}

/// A torn file at a random minute mid-evening stops every later restart
/// (asserted per minute in `drive_evening`), and the file is never rewritten.
#[test]
fn a_file_torn_mid_evening_stops_the_rest_of_the_evening() {
    for seed in 0..32u64 {
        let dir = TempDir::new("tear-mid");
        let path = dir.state_path();
        let mut rng = SplitMix(0x7EA2_0000 + seed);
        // Make sure a file exists to tear: one recorded restart first.
        store_state(
            &path,
            &state_for_restart(&EveningState::default(), DAY, 1_000, evening_start_unix(0)),
        )
        .expect("seed");
        let tear = i64::try_from(rng.below(600)).unwrap_or(0);
        let r = drive_evening(
            &path,
            DAY,
            evening_start_unix(0) + EVENING_DRAIN_SPACING_SECS,
            Model::Shrinks,
            &mut rng,
            Some(tear),
        );
        assert!(r.recorded.len() < usize::try_from(EVENING_DRAIN_RESTARTS_MAX).unwrap_or(3));
    }
}

/// Limitation, pinned: a give-up decided but not written (killed between
/// the decision and the latch) is not a verdict yet. If the crash restart's
/// own catch-up then shrinks the backlog, the next minute restarts. The cap
/// and the spacing still hold.
#[test]
fn an_unwritten_give_up_can_be_followed_by_a_restart_but_the_cap_holds() {
    let d = TempDir::new("unlatched");
    let p = d.state_path();
    let start = evening_start_unix(0);
    let first = start + 60 * MINUTE;
    store_state(
        &p,
        &state_for_restart(&EveningState::default(), DAY, 500, first),
    )
    .expect("seed");
    let at = first + EVENING_DRAIN_SPACING_SECS;
    let m = Minute {
        now: at,
        secs: secs_ist_of(at),
        other_done: true,
        marker: true,
        backlog: Some(500),
    };
    assert_eq!(
        supervise(&p, DAY, &m, Kill::BeforeGiveUpStore).step,
        EveningStep::GaveUp
    );
    assert!(
        load_state(&p).is_some_and(|s| !s.gave_up),
        "latch was not written"
    );
    let after = Minute {
        now: at + 20 * MINUTE,
        secs: secs_ist_of(at + 20 * MINUTE),
        backlog: Some(450),
        ..m
    };
    let out = supervise(&p, DAY, &after, Kill::None);
    assert_eq!(out.step, EveningStep::Restart);
    assert_eq!(out.recorded.map(|s| s.restarts), Some(2));
    // Even so, the third restart is the last.
    let s3 = state_for_restart(&load_state(&p).expect("s2"), DAY, 100, at + 80 * MINUTE);
    store_state(&p, &s3).expect("s3");
    let judged = at + 80 * MINUTE + EVENING_DRAIN_SPACING_SECS;
    let fin = Minute {
        now: judged,
        secs: secs_ist_of(judged),
        backlog: Some(10),
        ..m
    };
    assert_eq!(
        supervise(&p, DAY, &fin, Kill::None).step,
        EveningStep::GaveUp
    );
}

/// Limitation, pinned: the contract says a MISSING file reads as "no restart
/// yet". A file removed mid-evening (by hand, or a wiped `data/state`) loses
/// the count AND the spacing, so a restart can follow a recorded one inside
/// the spacing. A kill alone cannot cause this (the write is a rename).
#[test]
fn a_file_deleted_mid_evening_resets_the_count_as_documented() {
    let d = TempDir::new("deleted");
    let p = d.state_path();
    let at = evening_start_unix(0) + 60 * MINUTE;
    store_state(
        &p,
        &state_for_restart(&EveningState::default(), DAY, 500, at),
    )
    .expect("seed");
    std::fs::remove_file(&p).expect("rm");
    let m = Minute {
        now: at + 2 * MINUTE,
        secs: secs_ist_of(at + 2 * MINUTE),
        other_done: true,
        marker: true,
        backlog: Some(480),
    };
    let out = supervise(&p, DAY, &m, Kill::None);
    assert_eq!(out.step, EveningStep::Restart);
    assert_eq!(out.recorded.map(|s| s.restarts), Some(1));
}

#[test]
fn nothing_restarts_before_the_cross_verification_settles_or_after_five() {
    let d = TempDir::new("clock");
    let p = d.state_path();
    let start = evening_start_unix(0);
    for k in 0..evening_minutes() {
        let now = start + k * MINUTE;
        let secs = secs_ist_of(now);
        // Fresh state every minute so only the clock and the marker decide.
        drop(std::fs::remove_file(&p));
        let m = Minute {
            now,
            secs,
            other_done: true,
            marker: false,
            backlog: Some(100),
        };
        let step = supervise(&p, DAY, &m, Kill::None).step;
        let expect_restart = !(EVENING_DRAIN_LAST_RESTART_SECS_IST
            ..EVENING_DRAIN_XVERIFY_CLEAR_SECS_IST)
            .contains(&secs);
        if expect_restart {
            assert_eq!(step, EveningStep::Restart, "{secs} s IST");
        } else if (12 * 3_600..EVENING_DRAIN_FIRST_SECS_IST).contains(&secs) {
            assert_eq!(step, EveningStep::Wait, "{secs} s IST, before the window");
        } else if in_no_restart_band(secs) {
            assert_eq!(step, EveningStep::GaveUp, "{secs} s IST, after 05:00");
        } else {
            assert_eq!(step, EveningStep::Wait, "{secs} s IST, no marker yet");
        }
        // The marker lets a restart in from 15:50 instead of 16:25.
        drop(std::fs::remove_file(&p));
        let with_marker = supervise(&p, DAY, &Minute { marker: true, ..m }, Kill::None).step;
        assert_eq!(
            with_marker == EveningStep::Restart,
            !in_no_restart_band(secs),
            "{secs} s IST with the marker"
        );
    }
}

/// DEFECT (see the ignore reason). The module header says "a damaged ... one
/// stops the restarts (it could not be counted)". A single flipped bit in
/// an ASCII digit still parses, so a damaged file can lower the count or
/// clear the give-up latch instead of stopping the restarts.
#[test]
#[ignore = "DEFECT: the state line has no checksum. '1' (0x31) and '0' (0x30) differ by one bit, \
            as do '3'/'2', so one flipped bit in gave_up or restarts still parses and lifts the \
            latch or the cap instead of failing closed. Fix: a v2 line with a CRC-32 of the \
            fields (read v1 during rollout), refused whole on mismatch; or keep the decision \
            fields in a fixed binary record with a CRC, like the WAL control files."]
fn a_single_flipped_bit_never_lets_more_restarts_through() {
    let s = EveningState {
        day: DAY,
        restarts: EVENING_DRAIN_RESTARTS_MAX,
        last_unix: 1_792_000_000,
        backlog_before: 1_696,
        pending_unix: 0,
        gave_up: true,
    };
    let line = s.to_line().into_bytes();
    let mut lifted = Vec::new();
    for i in 0..line.len() {
        for bit in 0..8 {
            let mut b = line.clone();
            b[i] ^= 1 << bit;
            let Ok(text) = std::str::from_utf8(&b) else {
                continue;
            };
            if let Some(f) = EveningState::from_line(text)
                && (f.restarts < s.restarts || !f.gave_up || f.day != s.day)
            {
                lifted.push((i, bit, f));
            }
        }
    }
    assert!(
        lifted.is_empty(),
        "{} single-bit flips lift the bound: {lifted:?}",
        lifted.len()
    );
}

/// DEFECT (see the ignore reason). A torn file stops tonight's restarts,
/// which is the contract. Nothing ever rewrites it (the decision path only
/// writes from a state it could read, and `budget_or` returns before its
/// write), so it also stops every LATER evening until someone deletes it.
/// `drain_hold.rs` maps an unreadable state to Idle and writes nothing; the
/// error is logged once per process, log-sink only. Proposed fix: when the
/// content does not parse and the file was last written before this
/// evening's 15:40 IST start, rename it aside (`evening-drain.state.bad-<unix>`)
/// and read the default; a file damaged tonight stays fail-closed.
#[test]
#[ignore = "DEFECT: an unparseable evening-drain.state is never rewritten, so one torn write \
            disables the evening drain on every later evening too. Root cause and fix in the \
            doc comment above."]
fn a_torn_file_from_last_evening_does_not_block_tonight() {
    let d = TempDir::new("torn-yesterday");
    let p = d.state_path();
    let y = state_for_restart(
        &EveningState::default(),
        DAY - 1,
        900,
        evening_start_unix(-1),
    );
    std::fs::create_dir_all(p.parent().expect("parent")).expect("dir");
    std::fs::write(&p, &y.to_line().into_bytes()[..12]).expect("torn yesterday");
    let mut restarted = false;
    let start = evening_start_unix(0);
    for k in 0..evening_minutes() {
        let now = start + k * MINUTE;
        let m = Minute {
            now,
            secs: secs_ist_of(now),
            other_done: true,
            marker: true,
            backlog: Some(1_000),
        };
        restarted |= supervise(&p, DAY, &m, Kill::None).step == EveningStep::Restart;
    }
    assert!(
        restarted,
        "tonight never restarted because of last evening's torn file"
    );
}
