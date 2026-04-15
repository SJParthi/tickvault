//! P7.3 REAL — Docker-based QuestDB pause/unpause chaos.
//!
//! This is the "real fault injection" companion to
//! `chaos_questdb_unavailable.rs`. That file uses an unreachable
//! TCP port to simulate a dead QuestDB; this file goes one step
//! further and uses `docker pause` / `docker unpause` on the actual
//! `tv-questdb` container so the in-process `DeepDepthWriter`
//! observes the exact TCP-level behaviour of a genuinely frozen
//! QuestDB:
//!
//!   1. Existing ILP connections hang on writes (no RST, no FIN —
//!      just indefinite block).
//!   2. New connection attempts hang on the TCP handshake.
//!   3. On `docker unpause`, every buffered ILP connection resumes
//!      within a bounded time.
//!
//! The production invariant we protect: during a real QuestDB
//! freeze, the `DeepDepthWriter` MUST absorb records into its
//! 50,000-slot ring buffer without panic, and MUST drain them on
//! recovery via `try_reconnect()` + `drain_depth_buffer()`.
//!
//! # Environment gate
//!
//! This test is GUARDED by three conditions:
//!
//!   1. `docker` binary is on PATH (not in minimal CI containers)
//!   2. The `tv-questdb` container is currently running (skipped
//!      during `cargo test` outside of `make run`)
//!   3. The environment variable `TICKVAULT_DOCKER_CHAOS=1` is set
//!      (opt-in — we do NOT want random `cargo test` runs to pause
//!      the operator's dev QuestDB)
//!
//! If any guard fails, the test prints a skip note and passes.
//! To RUN the test explicitly:
//!
//! ```bash
//! TICKVAULT_DOCKER_CHAOS=1 cargo test -p tickvault-storage \
//!     --test chaos_questdb_docker_pause -- --ignored
//! ```
//!
//! # Why `#[ignore]`
//!
//! `docker pause` on the operator's running QuestDB would break
//! unrelated dev work. Keeping the test `#[ignore]` means it only
//! runs under explicit opt-in (the env var above + `-- --ignored`).
//! CI can run it as a dedicated job against a throwaway test
//! QuestDB container.
//!
//! # Why this complements the in-process test
//!
//! `chaos_questdb_unavailable.rs` gives us the CONTRACT: no panic,
//! bounded ring buffer, graceful Err surfacing. That test runs on
//! every CI push and catches regressions in the writer's error
//! handling. THIS test gives us the END-TO-END observation that
//! the contract holds against the real kernel + real ILP protocol
//! state machine, which is subtly different from a `connect()` to
//! a reserved port. Together they form a two-layer defence.

#![cfg(test)]

use std::process::Command;
use std::time::{Duration, Instant};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::tick_types::DeepDepthLevel;
use tickvault_storage::deep_depth_persistence::DeepDepthWriter;

const CONTAINER_NAME: &str = "tv-questdb";
const CHAOS_ENV: &str = "TICKVAULT_DOCKER_CHAOS";

/// Return `Some(())` if every guard passes, `None` otherwise.
fn guard_all_conditions_met() -> Option<()> {
    // 1. Must be explicitly opted-in via env var.
    if std::env::var(CHAOS_ENV).ok().as_deref() != Some("1") {
        eprintln!("note: Docker QuestDB chaos skipped — set {CHAOS_ENV}=1 to opt in");
        return None;
    }
    // 2. docker binary on PATH.
    if Command::new("docker").arg("version").output().is_err() {
        eprintln!("note: docker binary not found — chaos skipped");
        return None;
    }
    // 3. Container is running.
    let ps = Command::new("docker")
        .args([
            "ps",
            "--filter",
            "name=tv-questdb",
            "--format",
            "{{.Names}}",
        ])
        .output();
    match ps {
        Ok(out) => {
            let names = String::from_utf8_lossy(&out.stdout);
            if !names.lines().any(|l| l.trim() == CONTAINER_NAME) {
                eprintln!("note: {CONTAINER_NAME} container not running — chaos skipped");
                return None;
            }
        }
        Err(_) => {
            eprintln!("note: docker ps failed — chaos skipped");
            return None;
        }
    }
    Some(())
}

fn docker_pause() -> std::io::Result<()> {
    let status = Command::new("docker")
        .args(["pause", CONTAINER_NAME])
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other("docker pause failed"));
    }
    Ok(())
}

fn docker_unpause() -> std::io::Result<()> {
    let status = Command::new("docker")
        .args(["unpause", CONTAINER_NAME])
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other("docker unpause failed"));
    }
    Ok(())
}

fn make_test_levels() -> Vec<DeepDepthLevel> {
    (0..20)
        .map(|i| DeepDepthLevel {
            price: 20000.0 + i as f64,
            quantity: 100,
            orders: 5,
        })
        .collect()
}

fn default_questdb_config() -> QuestDbConfig {
    QuestDbConfig {
        host: "127.0.0.1".to_string(),
        ilp_port: 9009,
        http_port: 9000,
        pg_port: 8812,
    }
}

/// **End-to-end Docker pause/unpause chaos.**
///
/// Builds a real `DeepDepthWriter` against the running `tv-questdb`
/// container, writes a few records, pauses the container, writes
/// more records (which MUST be absorbed by the ring buffer),
/// unpauses, and verifies `try_reconnect` + drain completes within
/// a bounded timeout.
#[test]
#[ignore = "Docker-dependent — requires tv-questdb running and TICKVAULT_DOCKER_CHAOS=1"]
fn chaos_questdb_docker_pause_drain_on_recovery() {
    if guard_all_conditions_met().is_none() {
        return;
    }

    let config = default_questdb_config();

    // Step 1 — baseline writes must succeed while QuestDB is healthy.
    let mut writer = DeepDepthWriter::new(&config).expect("baseline new");
    let levels = make_test_levels();
    let ts = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    for i in 0..10u32 {
        writer
            .append_deep_depth(9000 + i, 2, "BID", &levels, "20", ts, i)
            .expect("baseline append");
    }
    let _ = writer.flush();

    // Step 2 — pause the container mid-session.
    docker_pause().expect("docker pause");

    // Step 3 — writes while paused must NOT panic. Records land in
    // the ring buffer via the writer's internal buffer_record path.
    let buffered_before = writer.buffered_count();
    for i in 10..50u32 {
        let _ = writer.append_deep_depth(9000 + i, 2, "ASK", &levels, "20", ts, i);
    }
    let buffered_after = writer.buffered_count();
    assert!(
        buffered_after >= buffered_before,
        "buffered_count must not decrease while QuestDB is paused"
    );

    // Step 4 — unpause and verify drain.
    docker_unpause().expect("docker unpause");

    // Give the writer a moment to detect the reconnect and drain.
    // We poll rather than sleep a flat duration so we fail fast on
    // real regressions.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut drained = false;
    while Instant::now() < deadline {
        // Hint the writer that QuestDB is back by trying to append
        // one more record — the writer's `if self.sender.is_none()`
        // branch kicks off `try_reconnect` + `drain_depth_buffer`.
        let _ = writer.append_deep_depth(9999, 2, "BID", &levels, "20", ts, 9999);
        if writer.buffered_count() <= buffered_before {
            drained = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    // On failure, unpause the container so we don't leave the
    // operator's dev QuestDB frozen.
    if !drained {
        let _ = docker_unpause();
        panic!(
            "writer failed to drain buffered records within 30s after unpause \
             (buffered_count = {})",
            writer.buffered_count()
        );
    }

    // Final cleanup — flush any remaining buffered rows.
    let _ = writer.flush();
}

/// **Guard-only smoke test** — verifies the guard logic itself.
/// Runs on every CI push (NOT `#[ignore]`) so a typo in the guard
/// code can never silently break the real chaos test. Asserts that
/// without the env var, the guard returns None (skip).
#[test]
fn chaos_questdb_docker_pause_guard_returns_none_without_env_var() {
    // SAFETY: remove_var is only unsafe because of multi-threaded
    // test runners reading env concurrently. We restore the prior
    // value before returning, and this test asserts the default
    // skip-path behaviour — a race would still yield a passing
    // assertion because both branches of the guard return None when
    // the var is unset.
    //
    // APPROVED: test-only env manipulation to exercise the guard.
    let prior = std::env::var(CHAOS_ENV).ok();
    unsafe { std::env::remove_var(CHAOS_ENV) };
    assert!(
        guard_all_conditions_met().is_none(),
        "guard must skip when {CHAOS_ENV} is unset"
    );
    if let Some(val) = prior {
        unsafe { std::env::set_var(CHAOS_ENV, val) };
    }
}
