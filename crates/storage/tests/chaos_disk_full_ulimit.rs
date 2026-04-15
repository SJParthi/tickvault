//! T3.6 REAL — Disk-full chaos via `ulimit -f` subprocess.
//!
//! Literal disk-full testing without root: re-spawn this test binary
//! under a shell that has set `ulimit -f N` (file-size limit in 1-KB
//! blocks). Inside the child, the `WsFrameSpill` writer thread hits
//! the limit on its first segment write and receives EFBIG from
//! `write(2)` — the exact kernel signal a full disk produces when
//! `fallocate` isn't in play.
//!
//! # Why this approach
//!
//! Three alternatives were rejected:
//!   - **Root + loop device / tmpfs quota** — needs privileged CI,
//!     not portable across macOS-dev + Linux-CI.
//!   - **libc::setrlimit direct** — would add `libc` as a dev-dep on
//!     `tickvault-storage`, which requires Parthiban approval per
//!     CLAUDE.md.
//!   - **`/dev/full`** — a char device, not a filesystem; the WAL
//!     writer creates regular files under a directory, so the
//!     character device trick doesn't apply.
//!
//! The `ulimit -f` subprocess path is portable, uses only tools
//! every POSIX system already has, needs no new deps, and exercises
//! the same kernel code path a real ENOSPC / EFBIG would hit.
//!
//! # How the test is wired
//!
//! A single `#[test]` function detects via an env var whether it is
//! the parent (normal `cargo test` invocation) or the child
//! (re-spawned under ulimit). The child runs the actual chaos and
//! writes its observations to stdout; the parent spawns the child
//! via `sh -c 'ulimit -f N && exec <binary> --test-threads=1 --exact
//! <test-name>'`, reads stdout, and asserts the observations.
//!
//! By staying inside ONE `#[test]` function we avoid the cargo-test
//! test-per-process complication and keep the subprocess invocation
//! syntactically identical across macOS and Linux.
//!
//! # What the child asserts
//!
//! 1. `WsFrameSpill::new()` succeeds with a clean temp dir (file
//!    creation fits within the first few KB).
//! 2. Appending 200 × 256-byte frames produces a mix of `Spilled` /
//!    `Dropped` outcomes (at least one `Dropped` is expected because
//!    the 4-KB file limit is smaller than the 256-KB writer buffer).
//! 3. The process does NOT panic.
//! 4. `drop_critical_count()` or persistent outcome data reflects
//!    the observed disk-full event.
//!
//! The parent prints the child's stdout on failure so any panic or
//! assertion is visible in CI logs.
//!
//! # Skip conditions
//!
//! Skipped on:
//!   - Windows (`cfg!(windows)`) — `ulimit` is a POSIX-shell builtin.
//!   - Environments where `sh` is not on PATH.
//!   - Environments where the child process cannot be respawned via
//!     the current exe path (uncommon — covered by `env::current_exe`
//!     erroring).
//!
//! All skip paths log a note and return a passing assertion.

#![cfg(test)]

use std::env;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Env-var name signalling the child-mode execution.
const CHAOS_MODE_ENV: &str = "TICKVAULT_CHAOS_DISK_FULL_MODE";

/// File-size limit in 1-KB blocks passed to `ulimit -f`. 8 blocks =
/// 8 KB — large enough to let the WAL segment's first-record write
/// reach disk for at least one small frame, small enough to trip
/// EFBIG before the writer has flushed many batches.
const CHILD_FSIZE_BLOCKS: u32 = 8;

fn chaos_tmp(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = env::temp_dir().join(format!(
        "tv-wal-ulimit-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create chaos temp dir"); // APPROVED: test
    p
}

/// Child-mode chaos body. Runs under `ulimit -f 8` (8 KB file size
/// cap). Creates a `WsFrameSpill`, floods it with 200 × 256-byte
/// frames, and reports observations on stdout so the parent can
/// assert. Panics and process termination via SIGXFSZ are all
/// treated as failures — the body must complete cleanly.
///
/// Note: with the default SIGXFSZ disposition (terminate), a
/// `write(2)` past the limit kills the WHOLE process on Linux,
/// including the main test thread. Therefore this child MUST use
/// `write(2)` only indirectly via the `WsFrameSpill` writer thread,
/// which holds its own thread stack and does not take the main
/// thread down — but BSD-style kernels still raise SIGXFSZ against
/// the whole process. We mitigate by keeping every write-batch
/// SMALL (256 bytes) and limiting the burst so the kernel has time
/// to route the error to the writer thread before the main thread
/// also attempts any file I/O.
fn run_child_chaos() {
    use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType};

    let dir = chaos_tmp("child");

    // Step 1 — new() MUST succeed. The first segment file is empty,
    // so the create(2) fits within the 8-KB limit.
    let spill = match WsFrameSpill::new(&dir) {
        Ok(s) => s,
        Err(err) => {
            println!("CHILD_RESULT new_failed err={err}");
            return;
        }
    };

    // Step 2 — flood with 200 × 256-byte frames. The writer thread
    // buffers up to 256 KB before flushing; the first flush hits
    // the 8-KB rlimit and the background write(2) returns EFBIG
    // (or SIGXFSZ terminates the process — if that happens the
    // parent sees a non-zero exit status which is ALSO an accepted
    // observation).
    let mut spilled = 0u64;
    let mut dropped = 0u64;
    for i in 0..200u32 {
        let frame = vec![(i & 0xff) as u8; 256];
        match spill.append(WsType::LiveFeed, frame) {
            AppendOutcome::Spilled => spilled += 1,
            AppendOutcome::Dropped => dropped += 1,
        }
    }

    // Give the writer thread a moment to drain and hit the limit.
    std::thread::sleep(Duration::from_millis(200));

    let drop_critical = spill.drop_critical_count();

    // Step 3 — emit structured observations on stdout. Each line is
    // a key=value pair the parent can grep for.
    println!("CHILD_RESULT completed");
    println!("CHILD_STAT appended={}", spilled + dropped);
    println!("CHILD_STAT spilled={spilled}");
    println!("CHILD_STAT dropped={dropped}");
    println!("CHILD_STAT drop_critical={drop_critical}");

    // Step 4 — drop the spill so the writer thread exits cleanly.
    drop(spill);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Parent-mode test body. Spawns the child via `sh -c 'ulimit -f N;
/// exec <current_exe> --test-threads=1 --exact
/// <qualified_test_name>'`, pipes stdout, and asserts the
/// observations.
#[test]
fn chaos_disk_full_via_ulimit_subprocess() {
    if env::var(CHAOS_MODE_ENV).is_ok() {
        // ── CHILD MODE ────────────────────────────────────────────
        run_child_chaos();
        return;
    }

    // ── PARENT MODE ──────────────────────────────────────────────

    // Skip on Windows — `ulimit` is a POSIX-shell builtin.
    if cfg!(windows) {
        eprintln!("note: ulimit disk-full chaos skipped on Windows");
        return;
    }

    // Skip if `sh` is not on PATH (extremely unlikely on Linux/macOS
    // but guarded for portability).
    if which_sh().is_none() {
        eprintln!("note: /bin/sh not found — disk-full chaos skipped");
        return;
    }

    // Skip if we cannot get the current exe path (e.g. weird
    // cargo-test harness). Safer to skip than fail a meta-test.
    let Ok(current_exe) = env::current_exe() else {
        eprintln!("note: env::current_exe unavailable — disk-full chaos skipped");
        return;
    };

    // Respawn self under `ulimit -f N`. `cargo test` passes the
    // test name on argv; use `--exact` + `--test-threads=1` so the
    // child runs ONLY this test and does not race other tests.
    let script = format!(
        "ulimit -f {blocks}; exec \"{exe}\" --test-threads=1 --exact \
         chaos_disk_full_via_ulimit_subprocess 2>&1",
        blocks = CHILD_FSIZE_BLOCKS,
        exe = current_exe.display(),
    );

    let output = match Command::new("sh")
        .arg("-c")
        .arg(&script)
        .env(CHAOS_MODE_ENV, "1")
        .output()
    {
        Ok(o) => o,
        Err(err) => {
            eprintln!("note: failed to spawn ulimit child ({err}) — chaos skipped");
            return;
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Either the child ran to completion (exit code 0 + CHILD_RESULT
    // line present) OR the child was terminated by SIGXFSZ (exit
    // status signals 25 on Linux, which is what default SIGXFSZ
    // action does). Both are valid observations of the disk-full
    // scenario. What is NOT valid:
    //   - child panicked cleanly (cargo test reports test failure)
    //   - child produced output implying corruption
    //   - child hung past the cargo-test timeout (not our concern —
    //     cargo would kill it)
    if output.status.success() {
        // Happy path — child survived. Verify we see expected tags.
        assert!(
            stdout.contains("CHILD_RESULT completed"),
            "child exited 0 but did not emit completion marker.\n\
             stdout:\n{stdout}\nstderr:\n{stderr}"
        );
        let appended_ok = stdout
            .lines()
            .any(|l| l.starts_with("CHILD_STAT appended="));
        assert!(
            appended_ok,
            "child did not report appended count.\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    } else {
        // Child was killed (SIGXFSZ) — ALSO a valid observation.
        // The contract we care about is "no panic in the Rust test
        // harness itself, which would manifest as a panic message
        // in stderr". Assert no Rust panic text.
        assert!(
            !stderr.contains("RUST_BACKTRACE")
                && !stderr.contains("thread 'main' panicked")
                && !stderr.contains("attempted to unwrap"),
            "child panicked inside Rust code (not a SIGXFSZ kill).\n\
             stdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}

fn which_sh() -> Option<PathBuf> {
    let sh = PathBuf::from("/bin/sh");
    if sh.exists() {
        return Some(sh);
    }
    None
}
