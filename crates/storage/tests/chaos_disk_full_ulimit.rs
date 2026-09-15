//! WAL file-size-limit failure in an isolated Unix subprocess.
//!
//! `ulimit -f` produces EFBIG/SIGXFSZ, not ENOSPC. The child ignores SIGXFSZ
//! so Rust receives an I/O error instead of an expected process kill. A probe
//! first proves the file-size restriction actually applies. The real WAL
//! writer then receives bounded frames, shuts down, and replays the surviving
//! prefix. This checks fixture activation, process survival and recovered
//! payload integrity; it does not claim zero loss under disk exhaustion or
//! power-loss durability. Every unexpected child exit, including panic, fails.
//!
//! Unix-only: this test does not substantiate Windows behavior. The child has
//! a 15-second deadline and uses only an owned temporary directory.

#![cfg(unix)]

use std::env;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CHAOS_MODE_ENV: &str = "TICKVAULT_CHAOS_DISK_FULL_MODE";
const CHILD_FSIZE_BLOCKS: u32 = 8;
const FRAMES: u32 = 200;

fn chaos_tmp() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let dir = env::temp_dir().join(format!("tv-wal-ulimit-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&dir).expect("create isolated chaos dir");
    dir
}

fn run_child_chaos() {
    use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType, replay_all};

    let dir = chaos_tmp();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(dir.clone());

    // Shell ulimit units differ; 64 KiB exceeds eight blocks on the supported
    // Unix shells. Ignoring SIGXFSZ must leave the actual EFBIG error visible.
    let error = std::fs::write(dir.join("limit-probe"), vec![0; 65_536])
        .expect_err("file-size restriction was not applied");
    assert_eq!(error.kind(), std::io::ErrorKind::FileTooLarge);
    // libtest may print its `test name ... ` prefix without a newline.
    println!("\nCHILD_LIMIT verified");

    let wal = dir.join("wal");
    let spill = WsFrameSpill::new(&wal).expect("new WAL under file-size limit");
    let mut spilled = 0u64;
    let mut dropped = 0u64;
    for marker in 0..FRAMES {
        let mut frame = vec![0x5a; 256];
        frame[..4].copy_from_slice(&marker.to_le_bytes());
        match spill.append(WsType::LiveFeed, frame) {
            AppendOutcome::Spilled => spilled += 1,
            AppendOutcome::Dropped => dropped += 1,
        }
    }
    assert_eq!(spilled + dropped, u64::from(FRAMES));
    assert!(spilled > 0, "the actual WAL admission path must execute");
    assert_eq!(spill.drop_critical_count(), dropped);
    let queued = spill.shutdown(Duration::from_secs(5));
    assert_eq!(queued, 0, "bounded fixture must drain its accepted frames");
    drop(spill);

    let recovered = replay_all(&wal).expect("replay after restricted write");
    assert!(
        !recovered.is_empty(),
        "verify an actual surviving WAL prefix"
    );
    assert!(recovered.len() <= spilled as usize);
    let mut seen = [false; FRAMES as usize];
    for record in &recovered {
        assert_eq!(record.ws_type, WsType::LiveFeed);
        assert_eq!(record.frame.len(), 256);
        let marker = u32::from_le_bytes(record.frame[..4].try_into().expect("marker"));
        assert!(marker < FRAMES, "unwritten marker recovered");
        assert!(!seen[marker as usize], "duplicate marker recovered");
        seen[marker as usize] = true;
        assert!(record.frame[4..].iter().all(|byte| *byte == 0x5a));
    }
    println!("CHILD_STAT appended={FRAMES}");
    println!("CHILD_STAT recovered={}", recovered.len());
    println!("CHILD_RESULT completed");
}

fn assert_child_evidence(output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "child did not complete successfully: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    for marker in [
        "CHILD_LIMIT verified",
        "CHILD_STAT appended=200",
        "CHILD_RESULT completed",
    ] {
        assert!(
            stdout.lines().any(|line| line == marker),
            "child lacks {marker:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}

#[test]
fn chaos_disk_full_via_ulimit_subprocess() {
    if env::var_os(CHAOS_MODE_ENV).is_some() {
        run_child_chaos();
        return;
    }
    let current_exe = env::current_exe().expect("current test binary");
    let script = format!(
        "trap '' XFSZ || exit 125; ulimit -f {CHILD_FSIZE_BLOCKS} || exit 125; \
         exec \"$1\" --test-threads=1 --exact chaos_disk_full_via_ulimit_subprocess --nocapture"
    );
    let mut child = Command::new("/bin/sh")
        .args(["-c", &script, "tickvault-ulimit"])
        .arg(current_exe)
        .env(CHAOS_MODE_ENV, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn isolated file-size-limit child");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if child.try_wait().expect("poll child").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().expect("reap timed out child");
            panic!(
                "file-size-limit child timed out; no passing evidence\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_child_evidence(&child.wait_with_output().expect("collect child output"));
}

#[test]
#[should_panic(expected = "child did not complete successfully")]
fn child_panic_with_stdout_markers_is_rejected() {
    use std::os::unix::process::ExitStatusExt;
    assert_child_evidence(&Output {
        status: std::process::ExitStatus::from_raw(101 << 8),
        stdout: b"CHILD_LIMIT verified\nCHILD_STAT appended=200\nCHILD_RESULT completed\nthread panicked\n".to_vec(),
        stderr: Vec::new(),
    });
}

#[test]
#[should_panic(expected = "child lacks")]
fn zero_exit_without_executed_fixture_is_rejected() {
    use std::os::unix::process::ExitStatusExt;
    assert_child_evidence(&Output {
        status: std::process::ExitStatus::from_raw(0),
        stdout: b"test result: ok. 0 passed; 0 failed;\n".to_vec(),
        stderr: Vec::new(),
    });
}
