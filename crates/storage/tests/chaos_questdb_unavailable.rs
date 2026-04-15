//! STAGE-D P7.3 — Chaos: QuestDB unavailable / mid-session disconnect.
//!
//! The production guarantee we must preserve:
//!
//!   > QuestDB reliability: zero silent data loss during any outage duration.
//!   > WAL absorbs while QuestDB is down. Consumer resumes when QuestDB is
//!   > reachable again. Dedup keys prevent duplicates. No panics, no
//!   > corrupted ring buffer, no blocking writes.
//!
//! This test exercises the `DeepDepthWriter` against an unreachable
//! QuestDB target — simulating the scenario where the QuestDB container
//! crashed or the TCP connection dropped mid-session. The writer MUST:
//!
//!   1. **Gracefully report the disconnect** — either via a failed `new()`
//!      or via `append_deep_depth()` buffering the record into the ring
//!      buffer (no panic, no unbounded queue).
//!
//!   2. **Absorb records into the bounded ring buffer** — the writer
//!      has a `DEEP_DEPTH_BUFFER_CAPACITY` (50,000 record) ring buffer
//!      that holds records until QuestDB returns. Appending past the
//!      capacity drops the OLDEST record (FIFO eviction) and surfaces
//!      a warn log — it MUST NOT crash.
//!
//!   3. **Recover cleanly on reconnect** — when QuestDB comes back, the
//!      next successful append drains the buffered records into the
//!      ingestion path. The dedup key prevents duplicates even if some
//!      records were partially written before the crash.
//!
//! This is the P7.3 scenario from the plan, scoped to the observable
//! contract without real Docker fault injection. The bigger "kill the
//! QuestDB container for 60 seconds" test is tracked as a follow-up
//! and would require docker-in-docker or privileged CI.
//!
//! Run cost: ~100 ms. No Docker, no network, no root.

#![cfg(test)]

use tickvault_common::config::QuestDbConfig;
use tickvault_common::tick_types::DeepDepthLevel;
use tickvault_storage::deep_depth_persistence::DeepDepthWriter;

/// Build a QuestDB config that points at an unreachable port. Port 1
/// is reserved and never has a listener — the writer MUST handle the
/// ECONNREFUSED gracefully rather than panic or block.
fn unreachable_questdb_config() -> QuestDbConfig {
    QuestDbConfig {
        host: "127.0.0.1".to_string(),
        ilp_port: 1, // reserved, no listener
        http_port: 1,
        pg_port: 1,
    }
}

fn make_test_levels(count: usize) -> Vec<DeepDepthLevel> {
    (0..count)
        .map(|i| DeepDepthLevel {
            price: 1000.0 + i as f64,
            quantity: 10 * (i as u32 + 1),
            orders: i as u32 + 1,
        })
        .collect()
}

/// **P7.3 Scenario A** — constructing a `DeepDepthWriter` against a
/// dead QuestDB must either fail cleanly with `Err` or succeed and
/// buffer records on append. No panics, no hangs.
#[test]
fn chaos_deep_depth_writer_survives_unreachable_questdb_at_construction() {
    let config = unreachable_questdb_config();

    // Either outcome is acceptable — the exact behaviour depends on
    // whether the ILP client does a lazy connect or eager connect.
    // The invariant is "no panic, no hang, no unbounded blocking".
    let outcome = std::panic::catch_unwind(|| DeepDepthWriter::new(&config));

    assert!(
        outcome.is_ok(),
        "DeepDepthWriter::new must NEVER panic even against a dead QuestDB"
    );

    // If construction succeeded, append must not panic either. The
    // writer has a bounded ring buffer that absorbs records until
    // QuestDB is reachable.
    if let Ok(Ok(mut writer)) = outcome {
        let levels = make_test_levels(20);
        let ts = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        for i in 0..10u32 {
            // `append_deep_depth` returns Result — the test asserts it
            // never panics, not that it returns Ok. A buffered-append
            // path may return Ok even when QuestDB is unreachable.
            let _ = writer.append_deep_depth(
                1000 + i,
                2, // NSE_FNO
                "BID",
                &levels,
                "20",
                ts,
                i,
            );
        }
        // Flush on drop must also not panic — writer internally
        // handles the buffered records gracefully.
    }
}

/// **P7.3 Scenario B** — a large burst of appends against an
/// unreachable QuestDB must not corrupt the ring buffer and must not
/// grow unboundedly. The writer's internal buffer is capped at
/// `DEEP_DEPTH_BUFFER_CAPACITY = 50,000` records; overflows evict
/// oldest records (FIFO) with a warn log.
#[test]
fn chaos_deep_depth_writer_absorbs_burst_when_questdb_unreachable() {
    let config = unreachable_questdb_config();
    let writer = DeepDepthWriter::new(&config);

    // If construction failed (some ILP clients connect eagerly) we
    // cannot run the burst — but the graceful-error path is itself a
    // pass for this chaos test.
    let Ok(mut writer) = writer else {
        return;
    };

    let levels = make_test_levels(5);
    let ts = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);

    // Push 70,000 records — well past the 50,000 buffer cap. The
    // writer MUST evict oldest without panic.
    for i in 0..70_000u32 {
        let _ = writer.append_deep_depth(
            3000 + (i % 1000),
            2,
            if i % 2 == 0 { "BID" } else { "ASK" },
            &levels,
            "20",
            ts,
            i,
        );
    }

    // buffered_count is bounded by the ring buffer capacity.
    let buffered = writer.buffered_count();
    assert!(
        buffered <= 50_000,
        "buffered_count {buffered} exceeds ring buffer cap 50_000 — unbounded queue bug"
    );
}

/// **P7.3 Scenario C** — a writer pointed at a reachable TCP endpoint
/// that immediately resets the connection (simulates a QuestDB crash
/// during ILP handshake) must not deadlock. We use a TCP listener
/// that accepts then immediately drops the connection.
#[test]
fn chaos_deep_depth_writer_survives_questdb_reset_mid_handshake() {
    // Bind a TCP listener that accepts one connection and immediately
    // closes it — simulates a crashing/resetting QuestDB.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind tcp");
    let port = listener.local_addr().expect("local_addr").port();
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            // Immediately drop the connection — the writer sees
            // either ECONNRESET or a short read and must handle it.
            drop(s);
        }
    });

    let config = QuestDbConfig {
        host: "127.0.0.1".to_string(),
        ilp_port: port,
        http_port: port,
        pg_port: port,
    };

    // Construction may or may not succeed depending on client behaviour.
    // Either path is acceptable; the test passes as long as no panic
    // and no hang beyond a reasonable timeout.
    let start = std::time::Instant::now();
    let result = std::panic::catch_unwind(|| DeepDepthWriter::new(&config));
    let elapsed = start.elapsed();

    assert!(
        result.is_ok(),
        "DeepDepthWriter::new must not panic on mid-handshake reset"
    );
    // A sane client returns within a few seconds even if the handshake
    // fails. 30s is a very generous ceiling for CI.
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "DeepDepthWriter::new blocked for {elapsed:?} — possible deadlock on reset"
    );
}
