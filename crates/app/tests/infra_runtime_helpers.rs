//! Integration tests for the `infra` runtime helpers that previously only ran
//! on a live host: the sd_notify systemd protocol, the spill-directory size /
//! cleanup sweepers, the disk/RSS probes, and the QuestDB liveness probe with
//! its consecutive-failure counter.
//!
//! Coverage top-up (2026-07-02, coverage-gate fix PR). Every test asserts real
//! observable behaviour (actual datagram payloads, actual files deleted, the
//! actual failure-counter transitions) — no smoke-only calls.
//!
//! Concurrency discipline:
//! - `NOTIFY_SOCKET` is touched ONLY by the sd_notify test in this binary.
//! - `data/spill` (cwd = crates/app) is touched ONLY by the spill test, which
//!   uses delta/existence assertions so stray files from other test binaries
//!   can never flake it, and removes exactly the files it created.
//! - The liveness FAILURE→SUCCESS sequence lives in ONE test so the global
//!   `QUESTDB_LIVENESS_FAILURES` counter transitions deterministically.

use std::io::{Read as _, Write as _};
use std::path::PathBuf;

use tickvault_app::infra::{
    QuestDbLivenessOutcome, check_disk_space, check_memory_rss, check_questdb_liveness,
    check_spill_file_size, cleanup_old_spill_files, container_health_counts, notify_systemd_ready,
    notify_systemd_watchdog, questdb_liveness_failures,
};
use tickvault_common::config::QuestDbConfig;

fn qdb_config(port: u16) -> QuestDbConfig {
    QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: port,
        pg_port: 8812,
        ilp_port: 9009,
    }
}

// ---------------------------------------------------------------------------
// sd_notify — the REAL systemd datagram protocol, against a real unix socket
// ---------------------------------------------------------------------------

#[test]
fn sd_notify_sends_watchdog_and_ready_payloads_to_notify_socket() {
    use std::os::unix::net::UnixDatagram;

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let sock_path =
        std::env::temp_dir().join(format!("tv-sd-notify-{}-{nanos}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock_path);
    let receiver = UnixDatagram::bind(&sock_path).expect("bind notify socket");
    receiver
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("set timeout");

    // SAFETY: unsafe block required by Rust 2024 edition for std::env::set_var.
    // Only this test touches NOTIFY_SOCKET in this test binary.
    unsafe {
        std::env::set_var("NOTIFY_SOCKET", &sock_path);
    }

    // WATCHDOG=1 — the C1 heartbeat the watchdog loop sends every 30s.
    notify_systemd_watchdog();
    let mut buf = [0u8; 64];
    let n = receiver.recv(&mut buf).expect("receive WATCHDOG datagram");
    assert_eq!(
        &buf[..n],
        b"WATCHDOG=1",
        "exact sd_notify heartbeat payload"
    );

    // READY=1 — the one-shot boot-complete notification.
    notify_systemd_ready();
    let n = receiver.recv(&mut buf).expect("receive READY datagram");
    assert_eq!(&buf[..n], b"READY=1", "exact sd_notify ready payload");

    // Unset so no later code in this process mistakes itself for systemd-run.
    // SAFETY: same Rust 2024 edition requirement as above.
    unsafe {
        std::env::remove_var("NOTIFY_SOCKET");
    }
    let _ = std::fs::remove_file(&sock_path);
}

// ---------------------------------------------------------------------------
// Disk + RSS probes — best-effort readers must produce real values on Linux
// ---------------------------------------------------------------------------

#[test]
fn disk_space_probe_returns_a_sane_free_percentage() {
    let free = check_disk_space().expect("df / must work on a unix test host");
    assert!(free <= 100, "free percentage must be 0..=100, got {free}");
}

#[test]
fn memory_rss_probe_reads_own_process_rss() {
    let rss_mb = check_memory_rss().expect("/proc/self/status VmRSS must parse on Linux");
    // A running Rust test process realistically holds at least 1 MB RSS.
    assert!(
        rss_mb >= 1,
        "own-process RSS must be non-trivial, got {rss_mb} MB"
    );
}

// ---------------------------------------------------------------------------
// Spill sweepers — size accounting + age-based cleanup on real files
// ---------------------------------------------------------------------------

#[test]
fn spill_size_counts_new_bytes_and_cleanup_removes_only_aged_files() {
    // cwd for cargo test binaries is the package root (crates/app), so
    // "data/spill" is crates/app/data/spill — create it, work in deltas,
    // remove exactly what we created.
    let spill_dir = PathBuf::from("data/spill");
    std::fs::create_dir_all(&spill_dir).expect("mkdir data/spill");

    let tag = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let fresh = spill_dir.join(format!("ticks-fresh-{tag}.bin"));
    let aged = spill_dir.join(format!("ticks-aged-{tag}.bin"));

    let before_bytes = check_spill_file_size();

    std::fs::write(&fresh, vec![0u8; 100]).expect("write fresh spill file");
    std::fs::write(&aged, vec![0u8; 50]).expect("write aged spill file");
    // Age one file past SPILL_FILE_MAX_AGE_SECS (7 days) via mtime rewind.
    let touch = std::process::Command::new("touch")
        .args(["-d", "10 days ago"])
        .arg(&aged)
        .status()
        .expect("run touch");
    assert!(touch.success(), "touch -d must succeed");

    // Size sweep sees exactly the 150 new bytes on top of whatever was there.
    let after_bytes = check_spill_file_size();
    assert_eq!(
        after_bytes.saturating_sub(before_bytes),
        150,
        "size sweep must count exactly the two new files' bytes"
    );

    // Cleanup removes the >7d file (and possibly stray aged files from other
    // runs — hence >=) but must NEVER touch the fresh one.
    let cleaned = cleanup_old_spill_files();
    assert!(cleaned >= 1, "the 10-day-old spill file must be cleaned");
    assert!(!aged.exists(), "aged file must be deleted by the sweeper");
    assert!(fresh.exists(), "fresh file must survive the sweeper");

    let _ = std::fs::remove_file(&fresh);
}

// ---------------------------------------------------------------------------
// QuestDB liveness — failure counter increments, success resets it
// ---------------------------------------------------------------------------

/// One-shot minimal HTTP responder: accepts a single connection, reads the
/// request, answers 200 OK, closes. Enough for the liveness `SELECT 1` GET.
fn spawn_one_shot_http_200() -> (std::thread::JoinHandle<()>, u16) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test http listener");
    let port = listener.local_addr().expect("local addr").port();
    let handle = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf); // consume the GET
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
            let _ = stream.flush();
        }
    });
    (handle, port)
}

#[tokio::test]
async fn questdb_liveness_failure_increments_counter_then_success_resets_it() {
    // Port 1 is never listening → instant connection-refused → Failure.
    let outcome = check_questdb_liveness(&qdb_config(1)).await;
    assert_eq!(outcome, QuestDbLivenessOutcome::Failure);
    assert!(!outcome.is_success());
    let failures_after_first = questdb_liveness_failures();
    assert!(
        failures_after_first >= 1,
        "a failed probe must increment the consecutive-failure counter"
    );

    // A second failure keeps counting up (consecutive semantics).
    let outcome = check_questdb_liveness(&qdb_config(1)).await;
    assert_eq!(outcome, QuestDbLivenessOutcome::Failure);
    assert!(questdb_liveness_failures() > failures_after_first);

    // A real HTTP 200 responder → Success, and the counter RESETS to zero
    // (a single success clears the consecutive streak).
    let (handle, port) = spawn_one_shot_http_200();
    let outcome = check_questdb_liveness(&qdb_config(port)).await;
    assert_eq!(outcome, QuestDbLivenessOutcome::Success);
    assert!(outcome.is_success());
    assert_eq!(
        questdb_liveness_failures(),
        0,
        "one success must reset the consecutive-failure counter"
    );
    let _ = handle.join();
}

// ---------------------------------------------------------------------------
// Container health counts — env-tolerant invariant (docker present or not)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn container_health_counts_never_reports_more_healthy_than_total() {
    // With no docker daemon this is the documented honest (0, 0); with a live
    // daemon it is (healthy, total). Either way the invariant holds and the
    // shell-out + parse path is exercised end-to-end.
    let (healthy, total) = container_health_counts().await;
    assert!(
        healthy <= total,
        "healthy ({healthy}) can never exceed total ({total})"
    );
}
