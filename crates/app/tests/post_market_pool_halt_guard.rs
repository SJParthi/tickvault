//! 2026-04-24 ratchet — post-market pool watchdog + boot deadline must
//! be market-hours gated.
//!
//! On 2026-04-24 19:02 IST an operator booted the app post-market and
//! got a CRITICAL Telegram cascade plus a `std::process::exit(2)` after
//! 300s of post-market silence:
//!
//! ```text
//! [HIGH] WS POOL DEGRADED — All connections down for 60s
//! [HIGH] WS POOL HALT     — All connections down for 300s. Exiting...
//! [CRITICAL] BOOT DEADLINE MISSED — boot completed in 247s (over 120s)
//! [HIGH] Historical fetch — Fetched: 0 / Failed: 0 / Candles: 0
//! ```
//!
//! Root cause: `crates/app/src/main.rs` had three separate side-effect
//! sites that fired ERROR + Telegram (and one of them also exited the
//! process) regardless of market hours. The inner gate inside
//! `crates/core/src/websocket/connection_pool.rs::poll_watchdog`
//! existed (added 2026-04-20) but only suppressed the *inner* log;
//! the outer notifier + process exit in main.rs were ungated. The
//! comment at `connection_pool.rs:524` even pointed to `main.rs:3789`
//! claiming a matching gate existed there — line numbers had drifted
//! and the gate was never carried forward through the refactors.
//!
//! This test source-scans `crates/app/src/main.rs` for the three
//! gating pairs:
//!
//! 1. `WatchdogVerdict::Degraded` arm — must call
//!    `is_within_market_hours_ist()` before notifying
//!    `WebSocketPoolDegraded`.
//! 2. `WatchdogVerdict::Halt` arm — must call
//!    `is_within_market_hours_ist()` before notifying
//!    `WebSocketPoolHalt` AND before `std::process::exit(2)`.
//! 3. Boot deadline alert — must call
//!    `is_within_market_hours_ist()` before notifying
//!    `BootDeadlineMissed`.
//!
//! Source-scan is the right tool: the side effects are async + tied
//! to a system clock, so a behavioural test would need to mock
//! tokio::time + std::process::exit + the notification service. The
//! invariants we care about (market-hours gate present, on the same
//! arm as the side effect) are mechanical and visible in the source.

use std::path::PathBuf;

fn read_main_rs() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("src");
    path.push("main.rs");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("must be able to read main.rs: {err}"))
}

/// Returns the byte index of the start of the `match verdict {` block
/// inside `spawn_pool_watchdog_task`, or panics if not found. Used so
/// later assertions can scope their searches to that block (which is
/// the one we care about — there are other `match verdict {` patterns
/// in the file we don't want to false-positive on).
fn pool_watchdog_match_block(src: &str) -> &str {
    let needle = "let verdict = pool.poll_watchdog();";
    let start = src
        .find(needle)
        .expect("spawn_pool_watchdog_task must contain `let verdict = pool.poll_watchdog();`");
    // The `match verdict {` block ends at the next top-level closing
    // brace pair followed by `Healthy => { ... }` — but the cleanest
    // bound is just to take the next ~6,000 bytes which comfortably
    // covers the four match arms without overshooting into the next
    // helper. We then use literal-substring checks on this slice.
    let end = (start + 6_000).min(src.len());
    &src[start..end]
}

#[test]
fn pool_watchdog_match_block_is_locatable() {
    let src = read_main_rs();
    let block = pool_watchdog_match_block(&src);
    assert!(
        block.contains("WatchdogVerdict::Degraded"),
        "pool watchdog match block must include the Degraded arm"
    );
    assert!(
        block.contains("WatchdogVerdict::Halt"),
        "pool watchdog match block must include the Halt arm"
    );
}

#[test]
fn pool_watchdog_degraded_arm_is_market_hours_gated() {
    let src = read_main_rs();
    let block = pool_watchdog_match_block(&src);
    assert!(
        block.contains("is_within_market_hours_ist()"),
        "spawn_pool_watchdog_task must call is_within_market_hours_ist() \
         to gate Degraded/Halt side effects — outside [09:00, 15:30) IST \
         Dhan stops streaming and a 60s/300s silence is normal, not a page. \
         The 2026-04-24 19:02 IST incident fired this CRITICAL cascade."
    );
    assert!(
        block.contains("WebSocketPoolDegraded"),
        "Degraded arm must still notify WebSocketPoolDegraded (when in market hours)"
    );
}

#[test]
fn pool_watchdog_halt_arm_gates_process_exit_on_market_hours() {
    let src = read_main_rs();
    let block = pool_watchdog_match_block(&src);
    // The `std::process::exit(2)` MUST live inside the `if in_market_hours`
    // branch of the Halt arm. We assert: the `in_market_hours` check
    // appears in the block, the `WebSocketPoolHalt` notification appears,
    // and the `std::process::exit(2)` appears after both — meaning the
    // lexical structure is `if in_market_hours { error!; notify; exit; }`.
    let in_hours_idx = block
        .find("in_market_hours")
        .expect("Halt arm must reference in_market_hours (the gate variable)");
    let halt_notify_idx = block
        .find("WebSocketPoolHalt")
        .expect("Halt arm must still notify WebSocketPoolHalt (when in market hours)");
    // NOTE: search for the call statement (with trailing semicolon) so we
    // skip comment-block mentions of `std::process::exit(2)` in the gating
    // explainer that precedes the match block.
    let exit_idx = block
        .find("std::process::exit(2);")
        .expect("Halt arm must still call std::process::exit(2); (when in market hours)");
    assert!(
        in_hours_idx < halt_notify_idx,
        "in_market_hours gate must appear before WebSocketPoolHalt notify \
         (otherwise the gate is being applied AFTER the side effect). \
         Found in_market_hours at {in_hours_idx}, WebSocketPoolHalt at {halt_notify_idx}."
    );
    assert!(
        in_hours_idx < exit_idx,
        "in_market_hours gate must appear before std::process::exit(2) \
         (post-market boots must NOT exit the process — the supervisor \
         restart loop produced 4 false alarms in <5min on 2026-04-24). \
         Found in_market_hours at {in_hours_idx}, exit at {exit_idx}."
    );
}

#[test]
fn boot_deadline_alert_is_market_hours_gated() {
    let src = read_main_rs();
    // Locate the `BOOT TIMEOUT EXCEEDED` block. There is exactly one
    // such site in main.rs.
    let needle = "BOOT TIMEOUT EXCEEDED";
    let timeout_idx = src
        .find(needle)
        .expect("main.rs must contain the `BOOT TIMEOUT EXCEEDED` error log");
    // The market-hours gate must be visible within ~600 bytes BEFORE
    // the error log, because the gate is a `let in_market_hours = ...`
    // that immediately precedes the `if in_market_hours { error!(...) }`
    // block. We slice a window that ends at the error log and starts
    // 800 bytes earlier (gives plenty of room for the gate + its
    // surrounding context comment).
    let window_start = timeout_idx.saturating_sub(800);
    let window = &src[window_start..timeout_idx];
    assert!(
        window.contains("is_within_market_hours_ist()"),
        "boot deadline alert must be market-hours gated. The 120s budget \
         is the wrong yardstick for a post-market operator-test boot \
         (index LTPs never arrive after 15:30 IST). Window scanned:\n{window}"
    );
    assert!(
        window.contains("in_market_hours"),
        "boot deadline alert must use a `let in_market_hours = ...` \
         binding so the gate is auditable. Window scanned:\n{window}"
    );
}

#[test]
fn watchdog_metrics_still_increment_outside_market_hours() {
    // Even when we suppress Telegram + process exit post-market, the
    // metric counters must still increment so dashboards retain the
    // signal. This guards against a future "fix" that moves the metric
    // increment inside the `if in_market_hours` branch.
    let src = read_main_rs();
    let block = pool_watchdog_match_block(&src);
    // Find the Degraded arm slice (from `WatchdogVerdict::Degraded` to
    // the next `WatchdogVerdict::Recovered`).
    let degraded_start = block
        .find("WatchdogVerdict::Degraded")
        .expect("Degraded arm must exist");
    let degraded_end = block
        .find("WatchdogVerdict::Recovered")
        .expect("Recovered arm must follow Degraded");
    let degraded_arm = &block[degraded_start..degraded_end];
    let metric_idx = degraded_arm
        .find("tv_pool_degraded_alerts_total")
        .expect("Degraded arm must increment tv_pool_degraded_alerts_total");
    let if_idx = degraded_arm
        .find("if in_market_hours")
        .expect("Degraded arm must have an `if in_market_hours` gate");
    assert!(
        metric_idx < if_idx,
        "tv_pool_degraded_alerts_total counter must increment BEFORE the \
         market-hours gate so post-market degraded events still appear on \
         dashboards even though they don't page the operator. Found metric \
         at {metric_idx}, gate at {if_idx} within the Degraded arm."
    );
}
