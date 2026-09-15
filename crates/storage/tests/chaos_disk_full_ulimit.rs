//! WAL file-size-limit failure in an isolated Unix subprocess.
//!
//! `ulimit -f` produces EFBIG/SIGXFSZ, not ENOSPC. The child ignores SIGXFSZ
//! so Rust receives an I/O error instead of an expected process kill. A probe
//! first proves the restriction applies. One marker is batch-flushed before a
//! frame larger than the limit makes later writes persistently fail. Unflushed
//! frames remain retry-owned and the writer keeps its directory claim even
//! after its public handle is dropped. Only child exit releases that ownership.
//! The parent then uses owned, fenced replay: torn originals must be refused
//! and retained byte-for-byte, without an ACK or an empty-success claim.
//! This does not establish zero loss or power-loss durability: accepted records
//! that remained only in the child queue are not recovered by this fixture.
//! Every unexpected child exit, including panic, fails.
//!
//! Unix-only: this test does not substantiate Windows behavior. The child has
//! a 15-second deadline and uses only a parent-owned temporary directory.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[path = "support/owned_wal_replay.rs"]
pub mod owned_wal_replay;

const CHAOS_MODE_ENV: &str = "TICKVAULT_CHAOS_DISK_FULL_MODE";
const CHAOS_DIR_ENV: &str = "TICKVAULT_CHAOS_DISK_FULL_DIR";
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

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn original_segments(wal: &Path) -> BTreeMap<OsString, Vec<u8>> {
    let mut originals = BTreeMap::new();
    for subdir in ["", "replaying", "quarantine"] {
        let directory = wal.join(subdir);
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if !subdir.is_empty() && error.kind() == std::io::ErrorKind::NotFound => {
                continue;
            }
            Err(error) => panic!("cannot inventory fixture originals in {directory:?}: {error}"),
        };
        for entry in entries {
            let entry = entry.expect("read original fixture entry");
            if entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("wal")
            {
                continue;
            }
            let bytes = std::fs::read(entry.path()).expect("read original WAL bytes");
            assert!(
                originals.insert(entry.file_name(), bytes).is_none(),
                "a fixture original must exist in exactly one recovery tier"
            );
        }
    }
    originals
}

fn frame(marker: u32) -> Vec<u8> {
    // Marker 1 cannot fit in even a fresh segment under the child restriction.
    // This keeps failure persistent regardless of background batch scheduling.
    let size = if marker == 1 { 65_536 } else { 256 };
    let mut frame = vec![0x5a; size];
    frame[..4].copy_from_slice(&marker.to_le_bytes());
    frame
}

fn run_child_chaos() {
    use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType, lock_wal_dir};

    let dir = PathBuf::from(env::var_os(CHAOS_DIR_ENV).expect("parent supplied fixture directory"));
    assert!(dir.is_dir(), "child only uses the parent-owned fixture");

    // Shell ulimit units differ; 64 KiB exceeds eight blocks on the supported
    // Unix shells. Ignoring SIGXFSZ must leave the actual EFBIG error visible.
    let error = std::fs::write(dir.join("limit-probe"), vec![0; 65_536])
        .expect_err("file-size restriction was not applied");
    assert_eq!(error.kind(), std::io::ErrorKind::FileTooLarge);
    // libtest may print its `test name ... ` prefix without a newline.
    println!("\nCHILD_LIMIT verified");

    let wal = dir.join("wal");
    let spill = WsFrameSpill::new(&wal).expect("new WAL under file-size limit");
    assert_eq!(
        spill.append(WsType::LiveFeed, frame(0)),
        AppendOutcome::Spilled
    );
    let flush_deadline = Instant::now() + Duration::from_secs(2);
    while spill.persisted_count() == 0 && Instant::now() < flush_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        spill.persisted_count(),
        1,
        "protect one batch-flushed prefix marker"
    );
    assert_eq!(spill.queued_records(), 0);
    let prefix = original_segments(&wal);
    assert_eq!(
        prefix.len(),
        1,
        "the protected prefix has one original segment"
    );
    let (prefix_name, prefix_bytes) = prefix.first_key_value().expect("protected original");
    // TVW4 stores a four-byte CRC after the payload. Preserve the complete
    // preimage, including its original capture identity and checksum.
    let protected_payload = frame(0);
    assert!(prefix_bytes.len() >= protected_payload.len() + 4);
    let crc_at = prefix_bytes.len() - 4;
    assert_eq!(
        &prefix_bytes[crc_at - protected_payload.len()..crc_at],
        protected_payload.as_slice(),
        "protected marker payload was written"
    );
    std::fs::write(dir.join("protected-prefix"), prefix_bytes)
        .expect("save protected byte preimage");
    std::fs::write(
        dir.join("protected-name"),
        prefix_name.to_str().expect("fixture segment name is UTF-8"),
    )
    .expect("save protected segment name");

    assert_eq!(
        spill.append(WsType::LiveFeed, frame(1)),
        AppendOutcome::Spilled
    );
    let mut spilled = 2u64;
    let mut dropped = 0u64;
    for marker in 2..FRAMES {
        match spill.append(WsType::LiveFeed, frame(marker)) {
            AppendOutcome::Spilled => spilled += 1,
            AppendOutcome::Dropped => dropped += 1,
        }
    }
    assert_eq!(spilled + dropped, u64::from(FRAMES));
    assert_eq!(spill.drop_critical_count(), dropped);
    let queued = spill.shutdown(Duration::from_secs(5));
    assert!(
        queued > 0,
        "persistent EFBIG must not report a complete drain"
    );
    assert_eq!(
        spill.persisted_count(),
        1,
        "only the protected prefix was batch-flushed"
    );
    assert_eq!(
        queued as u64,
        spilled - 1,
        "every other admitted record remains unflushed"
    );
    assert_eq!(
        spill.queued_records(),
        queued,
        "shutdown retains retry accounting"
    );
    drop(spill);
    let refusal = lock_wal_dir(&wal).expect_err("retry worker must retain its directory ownership");
    assert!(
        refusal.to_string().contains("already owned"),
        "expected live retry-owner contention, got {refusal:#}"
    );
    assert!(
        std::fs::read(wal.join(prefix_name))
            .expect("protected segment remains in place")
            .starts_with(prefix_bytes),
        "failed writes must preserve the preflushed original prefix"
    );
    println!("CHILD_OWNER retry_retained");
    println!("CHILD_STAT appended={FRAMES}");
    println!("CHILD_STAT admitted={spilled}");
    println!("CHILD_STAT preflushed=1");
    println!("CHILD_STAT unflushed={queued}");
    println!("CHILD_RESULT completed");
}

fn assert_originals_retained_after_child(dir: &Path) {
    let wal = dir.join("wal");
    let owner = owned_wal_replay::claim_wal(&wal);
    let original_bytes = original_segments(&wal);
    assert!(
        !original_bytes.is_empty(),
        "the failed writer must leave original evidence"
    );
    let prefix_name = OsString::from(
        std::fs::read_to_string(dir.join("protected-name")).expect("read protected original name"),
    );
    let prefix_bytes =
        std::fs::read(dir.join("protected-prefix")).expect("read protected byte preimage");
    assert!(
        original_bytes
            .get(&prefix_name)
            .expect("protected segment survives child exit")
            .starts_with(&prefix_bytes)
    );
    // A fence refusal is Ok with a stop flag, so expect_err cannot turn an
    // unexecuted replay into the expected corrupt-original refusal.
    let refusal = owner
        .replay_fenced()
        .expect_err("the restricted writer's torn original must refuse replay");
    assert!(
        refusal.to_string().contains("could not read segment"),
        "{refusal:#}"
    );
    assert_eq!(
        original_segments(&wal),
        original_bytes,
        "refusal/quarantine must retain every original byte, including the protected marker"
    );
    assert!(
        std::fs::read_dir(wal.join("quarantine"))
            .expect("incomplete original quarantined")
            .any(|entry| entry
                .expect("read quarantine entry")
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                == Some("wal")),
        "the torn original must be isolated from ordinary replay"
    );
    assert!(
        !wal.join("archive").exists(),
        "no failed replay receives an ACK or an archive"
    );
    // Deliberately no confirmation: no complete generation was returned and
    // unflushed child-memory records have not been reconstructed or persisted.
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
        "CHILD_STAT preflushed=1",
        "CHILD_OWNER retry_retained",
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
    let dir = chaos_tmp();
    let _cleanup = Cleanup(dir.clone());
    let current_exe = env::current_exe().expect("current test binary");
    let script = format!(
        "trap '' XFSZ || exit 125; ulimit -f {CHILD_FSIZE_BLOCKS} || exit 125; \
         exec \"$1\" --test-threads=1 --exact chaos_disk_full_via_ulimit_subprocess --nocapture"
    );
    let mut child = Command::new("/bin/sh")
        .args(["-c", &script, "tickvault-ulimit"])
        .arg(current_exe)
        .env(CHAOS_MODE_ENV, "1")
        .env(CHAOS_DIR_ENV, &dir)
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
    assert_originals_retained_after_child(&dir);
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
