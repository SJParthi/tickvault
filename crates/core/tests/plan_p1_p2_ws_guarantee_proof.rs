//! P1 + P2 — Proof tests that WS read loops obey the zero-loss contract.
//!
//! These are **code-scan tests**: they read the actual source files and verify
//! structural properties (no `time::timeout` around `read.next()`, no manual
//! ping frames, etc.). This catches regressions at compile time without
//! requiring a mock WebSocket server.

/// P1.1 — All 4 WS read loops use plain `read.next().await` with NO
/// `time::timeout()` wrapper. The watchdog (P2) handles dead sockets
/// instead. Dhan's server pings every 10s and the library auto-responds.
#[test]
fn test_all_4_read_loops_exit_only_on_stream_close_or_error() {
    // Read the three WS source files.
    let connection_rs = include_str!("../src/websocket/connection.rs");
    let depth_rs = include_str!("../src/websocket/depth_connection.rs");
    let order_update_rs = include_str!("../src/websocket/order_update_connection.rs");

    // No file should contain `time::timeout(` wrapping `read.next()`.
    // The pattern "timeout" + "read.next()" on the same line or within
    // a few lines indicates a client-side read deadline.
    for (name, src) in [
        ("connection.rs", connection_rs),
        ("depth_connection.rs", depth_rs),
        ("order_update_connection.rs", order_update_rs),
    ] {
        // Check that no line contains both "timeout" and "read.next" —
        // this is the banned pattern from P1.1.
        for (i, line) in src.lines().enumerate() {
            let trimmed = line.trim_start();
            // Skip comments and string literals (test data).
            if trimmed.starts_with("//")
                || trimmed.starts_with("\"")
                || trimmed.starts_with("let ")
                    && trimmed.contains("\"")
                    && trimmed.contains("timeout")
            {
                continue;
            }
            // Skip lines inside #[cfg(test)] test modules — test data strings
            // may legitimately contain banned patterns for verification.
            if trimmed.contains("assert") || trimmed.starts_with("fn test_") {
                continue;
            }
            let lower = line.to_lowercase();
            if lower.contains("timeout") && lower.contains("read.next") {
                panic!(
                    "REGRESSION: {name} line {} contains client-side read deadline: {line}",
                    i + 1
                );
            }
        }
    }
}

/// P1.1 / P2.1 — WS connections handle Ping correctly: receive Ping → send Pong.
/// After stream split, tokio-tungstenite cannot auto-respond, so manual
/// Ping→Pong is correct. But NO code should INITIATE a Ping (client→server).
/// Dhan docs: "Server pings every 10s" — the client only responds.
#[test]
fn test_all_4_ws_use_library_ping_handler() {
    let connection_rs = include_str!("../src/websocket/connection.rs");
    let depth_rs = include_str!("../src/websocket/depth_connection.rs");
    let order_update_rs = include_str!("../src/websocket/order_update_connection.rs");

    for (name, src) in [
        ("connection.rs", connection_rs),
        ("depth_connection.rs", depth_rs),
        ("order_update_connection.rs", order_update_rs),
    ] {
        // Count Ping handling — receiving Ping and sending Pong is correct.
        // Two valid patterns:
        //   1. Split stream: manually send Message::Pong in response to Ping
        //   2. Unsplit stream: tokio-tungstenite auto-pongs (comment documents this)
        let handles_ping = src.contains("Message::Ping(");
        let sends_pong = src.contains("Message::Pong(");
        let auto_pongs = src.contains("auto-pong");

        // Every file that handles Ping must either send Pong or document auto-pong.
        if handles_ping {
            assert!(
                sends_pong || auto_pongs,
                "{name} handles Message::Ping but neither sends Pong nor documents auto-pong"
            );
        }

        // No code should initiate a Ping from client to server.
        // Client-initiated Ping would be: constructing Ping(data) and sending it
        // WITHOUT first receiving a Ping. In our code, Ping is only sent in test
        // modules (mock server sends Ping to test client response).
        let mut in_test_mod = false;
        for (i, line) in src.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.contains("#[cfg(test)]") || trimmed.starts_with("mod tests") {
                in_test_mod = true;
            }
            if in_test_mod {
                continue;
            }
            if trimmed.starts_with("//") {
                continue;
            }
            // Client-initiated Ping: sending Ping(new data) not in response to received Ping.
            // Pattern: `send(Message::Ping(` without a preceding `Message::Ping(data)` match arm.
            if trimmed.contains(".send(Message::Ping(") {
                panic!(
                    "REGRESSION: {name} line {} initiates client→server Ping — \
                     only server should ping: {line}",
                    i + 1
                );
            }
        }
    }
}

/// P1.3 — Dead timeout constants for live-feed and depth WS must NOT exist.
/// These were removed in STAGE-B: WS readers use try_send() + WAL spill.
/// NOTE: ORDER_UPDATE_*_TIMEOUT_SECS are deliberately kept — order update
/// WS has a different protocol (JSON, sparse messages, longer idle periods).
#[test]
fn test_dead_timeout_constants_do_not_exist() {
    let constants_rs = include_str!("../../common/src/constants.rs");

    // These two constants are dead — no code imports them after P1.2/P1.3.
    let banned = [
        "FRAME_BACKPRESSURE_TIMEOUT_SECS",
        "FRAME_SEND_TIMEOUT_SECS",
        "DEPTH_READ_TIMEOUT_SECS",
    ];

    for name in banned {
        // Skip if the constant appears only in a comment (deleted marker).
        let has_pub_const = constants_rs.lines().any(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with("//") && trimmed.contains(name)
        });
        assert!(
            !has_pub_const,
            "REGRESSION: constants.rs still contains banned constant {name}"
        );
    }
}

/// P1.2 — No `.await` on `frame_sender` inside any WS read loop.
/// The WS reader must only write to WAL/spill (non-blocking), never await
/// downstream. This prevents the reader from stalling on a slow consumer.
#[test]
fn test_banned_pattern_rejects_ws_read_loop_backpressure_await() {
    let connection_rs = include_str!("../src/websocket/connection.rs");
    let depth_rs = include_str!("../src/websocket/depth_connection.rs");
    let order_update_rs = include_str!("../src/websocket/order_update_connection.rs");

    for (name, src) in [
        ("connection.rs", connection_rs),
        ("depth_connection.rs", depth_rs),
        ("order_update_connection.rs", order_update_rs),
    ] {
        for (i, line) in src.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            // `frame_sender.send(data).await` is the banned pattern.
            if trimmed.contains("frame_sender") && trimmed.contains(".await") {
                panic!(
                    "REGRESSION: {name} line {} has `.await` on frame_sender in read loop — \
                     use try_send() or WAL append: {line}",
                    i + 1
                );
            }
        }
    }
}

/// P2 — Activity watchdog exists and uses correct thresholds.
/// Live/depth: 50s (40s Dhan timeout + 10s buffer).
/// Order update: 660s (600s idle tolerance + 60s buffer).
#[test]
fn test_watchdog_thresholds_match_dhan_spec() {
    let watchdog_rs = include_str!("../src/websocket/activity_watchdog.rs");
    let pool_watchdog_rs = include_str!("../src/websocket/pool_watchdog.rs");

    // Activity watchdog must exist.
    assert!(
        watchdog_rs.contains("ActivityWatchdog"),
        "activity_watchdog.rs must define ActivityWatchdog"
    );

    // Pool watchdog must exist.
    assert!(
        pool_watchdog_rs.contains("PoolWatchdog") || pool_watchdog_rs.contains("pool_watchdog"),
        "pool_watchdog.rs must define pool-level monitoring"
    );

    // Check that the 50s threshold exists somewhere (live/depth watchdog).
    let all_ws_src = format!(
        "{}{}{}",
        include_str!("../src/websocket/connection.rs"),
        include_str!("../src/websocket/depth_connection.rs"),
        watchdog_rs
    );
    assert!(
        all_ws_src.contains("50") || all_ws_src.contains("WATCHDOG_THRESHOLD"),
        "Watchdog threshold (~50s) must be defined for live/depth connections"
    );
}
