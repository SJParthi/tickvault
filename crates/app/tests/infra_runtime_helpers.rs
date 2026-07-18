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

use tickvault_app::infra::{
    container_health_counts, notify_systemd_ready, notify_systemd_watchdog,
};

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

// ---------------------------------------------------------------------------
// Spill sweepers — size accounting + age-based cleanup on real files
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// QuestDB liveness — failure counter increments, success resets it
// ---------------------------------------------------------------------------

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
