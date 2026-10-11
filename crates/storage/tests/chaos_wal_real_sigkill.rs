//! Chaos test with a REAL `SIGKILL` for the two on-disk recovery floors:
//! the capture-at-receipt frame WAL (`WsFrameSpill`) and the sealed-candle
//! spill (`SealSpillWriter`). Audit TEST-1, 2026-10-11.
//!
//! ## Why this file exists
//!
//! The older "SIGKILL" tests never kill anything.
//! `chaos_ws_frame_wal_replay.rs` drops the `WsFrameSpill`, which joins the
//! writer thread and flushes the segment: a CLEAN shutdown.
//! `chaos_seal_sigkill_spill_replay.rs` drops the writer in-process. Neither
//! can show what a torn tail looks like when the process dies between two
//! `write(2)` calls, which is the case the recovery code exists for.
//!
//! Here the test binary re-executes itself (`current_exe()` with
//! `--exact <child> --nocapture`) with an environment variable naming a temp
//! directory. The child writes a known prefix, waits until it is written,
//! drops a "ready" file, then keeps appending as fast as it can. The parent
//! sees the ready file, waits a few milliseconds, and sends `SIGKILL`
//! (`Child::kill`). Drop impls do not run. Then the parent runs the real
//! recovery reader and checks:
//!
//! * at least the known prefix came back;
//! * what came back is an EXACT prefix of what the child wrote, in order,
//!   byte for byte (a torn record at the tail is skipped, never returned
//!   half-written, and nothing after a gap is returned).
//!
//! Three rounds per floor, each killed at a different delay, each in its own
//! directory.
//!
//! ## Honest limits
//!
//! * A process kill leaves the page cache intact, so this proves crash
//!   safety against a PROCESS death, not a power loss. Power-loss safety
//!   rests on the fsync paths, which this does not exercise.
//! * The child test functions run as ordinary tests too; without the
//!   environment variable they return at once and do nothing.
//! * Unix only: `Child::kill` is `SIGKILL` there.

#![cfg(unix)]

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tickvault_common::feed::Feed;
use tickvault_storage::seal_spill::{SealSpillWriter, SerializedSeal};
use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType, replay_all};
use tickvault_trading::candles::{BufferedSeal, LiveCandleState, TfIndex};

/// Set only on the re-executed child; names the directory it writes into.
const CHILD_DIR_ENV: &str = "TV_CHAOS_KILL_CHILD_DIR";
const WAL_CHILD: &str = "chaos_kill_child_ws_frame_wal_writer";
const SEAL_CHILD: &str = "chaos_kill_child_seal_spill_writer";
const READY_FILE: &str = "child.ready";

/// Records the child writes and confirms on disk before it signals ready.
const KNOWN_PREFIX: u32 = 2_000;
/// The child stops on its own after this long or this many records, so a
/// parent that failed to kill it cannot fill the disk. The parent then fails.
const CHILD_SAFETY_SECS: u64 = 30;
const CHILD_SAFETY_RECORDS: u32 = 2_000_000;
/// Exit code of a child that hit its safety cap instead of being killed.
const CHILD_CAP_EXIT: i32 = 3;
/// Delay between "ready" and `SIGKILL`, one round each.
const KILL_DELAYS_MS: [u64; 3] = [5, 15, 25];

/// Fixed clock for the seal spill, so the day file name never changes.
const SEAL_NOW_UNIX_SECS: i64 = 1_767_268_800;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!(
        "tv-real-sigkill-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create temp dir"); // APPROVED: test
    p
}

/// 16-byte ticker packet with the marker in the security-id field.
fn live_feed_frame(marker: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 16];
    buf[0] = 2; // ticker
    buf[1..3].copy_from_slice(&16u16.to_le_bytes());
    buf[3] = 2; // NSE_FNO
    buf[4..8].copy_from_slice(&marker.to_le_bytes());
    buf[8..12].copy_from_slice(&100.25_f32.to_le_bytes());
    buf[12..16].copy_from_slice(&(1_700_000_000u32.wrapping_add(marker)).to_le_bytes());
    buf
}

/// Deterministic sealed candle for index `i`, rebuilt by the parent.
fn seal(i: u32) -> SerializedSeal {
    let mut state = LiveCandleState::empty();
    state.bucket_start_ist_secs = 1_716_000_900u32.wrapping_add(i);
    state.open = 100.0 + f64::from(i % 7);
    state.high = 110.0 + f64::from(i % 13);
    state.low = 90.0 - f64::from(i % 5);
    state.close = 100.0 + f64::from(i % 100);
    state.volume = 1_000 + u64::from(i);
    state.bucket_start_cumulative = 1_000;
    state.oi = 50_000 + i64::from(i);
    state.tick_count = 5;
    let buffered = BufferedSeal::new(u64::from(i % 11_000) + 1, 0, TfIndex::M1, state, Feed::Dhan);
    SerializedSeal::from(&buffered)
}

fn write_ready(dir: &Path) {
    std::fs::write(dir.join(READY_FILE), b"ready").expect("write ready file"); // APPROVED: test
}

/// Re-runs this test binary as `child`, waits for its ready file, then kills
/// it after `delay_ms`. Asserts the child really died of `SIGKILL`.
fn run_and_kill(child: &str, dir: &Path, delay_ms: u64) {
    let exe = std::env::current_exe().expect("current test binary"); // APPROVED: test
    let mut proc = Command::new(exe)
        .args(["--exact", child, "--nocapture", "--test-threads=1"])
        .env(CHILD_DIR_ENV, dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child test process"); // APPROVED: test

    let ready = dir.join(READY_FILE);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready.exists() {
        if let Ok(Some(status)) = proc.try_wait() {
            panic!("child {child} exited before it was ready: {status:?}");
        }
        assert!(
            Instant::now() < deadline,
            "child {child} never wrote its ready file"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    std::thread::sleep(Duration::from_millis(delay_ms));
    proc.kill().expect("send SIGKILL"); // APPROVED: test
    let status = proc.wait().expect("reap child"); // APPROVED: test
    assert_eq!(
        status.signal(),
        Some(9),
        "child {child} must die of SIGKILL mid-write, not exit on its own \
         (exit code {CHILD_CAP_EXIT} means it hit its safety cap first): {status:?}"
    );
}

// ---------------------------------------------------------------- frame WAL

/// Child half of the frame-WAL test. Does nothing unless re-executed by
/// [`chaos_real_sigkill_ws_frame_wal_replays_an_exact_prefix`].
#[test]
fn chaos_kill_child_ws_frame_wal_writer() {
    let Some(dir) = std::env::var_os(CHILD_DIR_ENV) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let spill = WsFrameSpill::new(&dir).expect("WsFrameSpill::new"); // APPROVED: test
    let started = Instant::now();
    let mut marker: u32 = 0;
    let append = |marker: u32| {
        // A full channel refuses the frame; retry it so the written sequence
        // has no holes (the parent checks for an exact prefix).
        while spill.append(WsType::LiveFeed, live_feed_frame(marker)) == AppendOutcome::Dropped {
            std::thread::yield_now();
        }
    };
    while marker < KNOWN_PREFIX {
        append(marker);
        marker += 1;
    }
    // Wait until the writer thread has taken and written every prefix frame;
    // it flushes each batch right after writing it, so a short pause after
    // that puts the whole prefix in the file.
    while spill.queued_records() != 0 || spill.persisted_count() < u64::from(KNOWN_PREFIX) {
        assert!(
            started.elapsed() < Duration::from_secs(CHILD_SAFETY_SECS),
            "prefix never persisted"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(100));
    write_ready(&dir);
    // Burst until killed.
    while marker < CHILD_SAFETY_RECORDS
        && started.elapsed() < Duration::from_secs(CHILD_SAFETY_SECS)
    {
        append(marker);
        marker += 1;
    }
    std::process::exit(CHILD_CAP_EXIT);
}

#[test]
fn chaos_real_sigkill_ws_frame_wal_replays_an_exact_prefix() {
    for delay_ms in KILL_DELAYS_MS {
        let dir = temp_dir("wal");
        run_and_kill(WAL_CHILD, &dir, delay_ms);

        let recovered = replay_all(&dir).expect("replay_all after SIGKILL"); // APPROVED: test
        assert!(
            recovered.len() >= KNOWN_PREFIX as usize,
            "round {delay_ms} ms: only {} frames survived SIGKILL; the {KNOWN_PREFIX}-frame \
             prefix was on disk before the kill",
            recovered.len()
        );
        for (i, frame) in recovered.iter().enumerate() {
            let expected = live_feed_frame(u32::try_from(i).expect("index fits u32")); // APPROVED: test
            assert_eq!(
                frame.ws_type,
                WsType::LiveFeed,
                "round {delay_ms} ms: frame {i} came back with the wrong type"
            );
            assert_eq!(
                frame.frame,
                expected,
                "round {delay_ms} ms: frame {i} of {} is not the frame written at that \
                 position (a torn, reordered or skipped record was returned)",
                recovered.len()
            );
        }
        eprintln!(
            "chaos_real_sigkill (frame WAL): round {delay_ms} ms recovered an exact prefix of \
             {} frames",
            recovered.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// --------------------------------------------------------------- seal spill

/// Child half of the seal-spill test. Does nothing unless re-executed by
/// [`chaos_real_sigkill_seal_spill_replays_an_exact_prefix`].
#[test]
fn chaos_kill_child_seal_spill_writer() {
    let Some(dir) = std::env::var_os(CHILD_DIR_ENV) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let writer = SealSpillWriter::with_spill_dir_for_test(dir.join("spill"));
    let started = Instant::now();
    let mut i: u32 = 0;
    while i < KNOWN_PREFIX {
        writer
            .append_seal(&seal(i), SEAL_NOW_UNIX_SECS)
            .expect("append_seal"); // APPROVED: test
        i += 1;
    }
    // Each append is one write(2); the prefix is already in the file.
    write_ready(&dir);
    while i < CHILD_SAFETY_RECORDS && started.elapsed() < Duration::from_secs(CHILD_SAFETY_SECS) {
        writer
            .append_seal(&seal(i), SEAL_NOW_UNIX_SECS)
            .expect("append_seal"); // APPROVED: test
        i += 1;
    }
    std::process::exit(CHILD_CAP_EXIT);
}

#[test]
fn chaos_real_sigkill_seal_spill_replays_an_exact_prefix() {
    for delay_ms in KILL_DELAYS_MS {
        let dir = temp_dir("seal");
        run_and_kill(SEAL_CHILD, &dir, delay_ms);

        let reader = SealSpillWriter::with_spill_dir_for_test(dir.join("spill"));
        let recovered = reader
            .read_all(SEAL_NOW_UNIX_SECS)
            .expect("read_all after SIGKILL"); // APPROVED: test
        assert!(
            recovered.len() >= KNOWN_PREFIX as usize,
            "round {delay_ms} ms: only {} seals survived SIGKILL; the {KNOWN_PREFIX}-seal \
             prefix was on disk before the kill",
            recovered.len()
        );
        for (i, got) in recovered.iter().enumerate() {
            let expected = seal(u32::try_from(i).expect("index fits u32")); // APPROVED: test
            assert_eq!(
                *got,
                expected,
                "round {delay_ms} ms: seal {i} of {} is not the seal written at that position",
                recovered.len()
            );
        }
        // Recovery is non-destructive: a second read returns the same prefix.
        let again = reader
            .read_all(SEAL_NOW_UNIX_SECS)
            .expect("second read_all"); // APPROVED: test
        assert_eq!(
            again, recovered,
            "round {delay_ms} ms: re-read must be identical"
        );
        eprintln!(
            "chaos_real_sigkill (seal spill): round {delay_ms} ms recovered an exact prefix of \
             {} seals",
            recovered.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
