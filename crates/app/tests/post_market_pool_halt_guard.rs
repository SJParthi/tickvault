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
    // bound is just to take the next ~8,500 bytes which comfortably
    // covers the four match arms (incl. the H7 lane-scoped Halt branch +
    // the boot-ON `process::exit(2);` that follows it at ~rel 6.1KB, and
    // the off-hours `reset_watchdog` at ~rel 8.3KB) without overshooting
    // into the next helper (`spawn_pool_watchdog_task` closes at ~rel 8.5KB).
    // We then use literal-substring checks on this slice. (Window was 6,000
    // before the H7 watchdog-arg change added ~870 bytes to the Halt arm.)
    let end = (start + 8_500).min(src.len());
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
    // Wave-Holiday-Gate (2026-05-09) upgraded the call to
    // `is_within_trading_session_ist()` which folds in the weekend
    // check. Either gate satisfies the ratchet — what matters is that
    // SOME market-hours/trading-session check sits in front of the
    // CRITICAL `error!` so off-hours boots don't page.
    assert!(
        window.contains("is_within_market_hours_ist()")
            || window.contains("is_within_trading_session_ist()"),
        "boot deadline alert must be market-hours gated. The 120s budget \
         is the wrong yardstick for a post-market operator-test boot \
         (index LTPs never arrive after 15:30 IST). Expected one of \
         `is_within_market_hours_ist()` or `is_within_trading_session_ist()` \
         in the window. Window scanned:\n{window}"
    );
    assert!(
        window.contains("in_market_hours"),
        "boot deadline alert must use a `let in_market_hours = ...` \
         binding so the gate is auditable. Window scanned:\n{window}"
    );
}

#[test]
fn pool_watchdog_is_reset_outside_market_hours() {
    // Pre-market deferral fix (2026-06-03): the main-feed pool is
    // intentionally DEFERRED until 09:00 IST (zero TCP sockets opened
    // pre-market because Dhan idle-resets pre-market connections). Every
    // connection therefore reads "down" during 08:42→09:00, and the
    // watchdog stamps an `AllDown { since }` at ~08:42 boot. Without
    // resetting the watchdog on each off-hours poll, that stale `since`
    // accumulates and trips the 300s Halt the instant
    // is_within_market_hours_ist() flips true at 09:00:00 — a forced
    // std::process::exit(2) + supervisor restart that paged
    // [HIGH] WS POOL HALT + [HIGH] FAST BOOT every single market open.
    //
    // This guards against a future refactor silently dropping that reset.
    let src = read_main_rs();
    // Scope to the watchdog task body. Use a generous window from the
    // poll_watchdog call so the post-match reset (which sits just after the
    // verdict match) is comfortably inside it. Window bumped 8,000 → 8,500:
    // the H7 lane-scoped Halt branch added ~870 bytes to the Halt arm,
    // pushing the off-hours `reset_watchdog` site (~rel 8.3KB) just past
    // the old 8,000 window.
    let needle = "let verdict = pool.poll_watchdog();";
    let start = src
        .find(needle)
        .expect("spawn_pool_watchdog_task must contain `let verdict = pool.poll_watchdog();`");
    let end = (start + 8_500).min(src.len());
    let block = &src[start..end];

    let reset_idx = block.find("reset_watchdog").expect(
        "spawn_pool_watchdog_task must call pool.reset_watchdog() so the pre-market \
         DEFERRED window never accumulates a stale AllDown { since } that trips the \
         300s Halt at 09:00:00. Removing it re-introduces the daily forced restart \
         + [HIGH] WS POOL HALT + [HIGH] FAST BOOT pages observed 2026-06-03 09:00 IST.",
    );
    // The reset MUST be gated on `!in_market_hours` — resetting in-hours
    // would defeat the genuine 300s all-down Halt that protects against a
    // real mid-session feed outage.
    let gate_idx = block.find("if !in_market_hours").expect(
        "reset_watchdog must be gated by `if !in_market_hours` — it may run ONLY \
         off-hours. Resetting during market hours would silently disable the \
         genuine 300s all-down → Halt safety property.",
    );
    assert!(
        gate_idx < reset_idx,
        "the `if !in_market_hours` gate must lexically precede the reset_watchdog \
         call (the reset lives inside the off-hours branch). Found gate at {gate_idx}, \
         reset at {reset_idx}."
    );
    // The gate must IMMEDIATELY precede the reset (same block, `if
    // !in_market_hours { pool.reset_watchdog(); }`) — not merely appear
    // somewhere earlier. A loose distance would let a refactor move the
    // reset out of the off-hours branch while the test still passed.
    assert!(
        reset_idx - gate_idx < 80,
        "the reset_watchdog call must sit directly inside the `if !in_market_hours` \
         block (within 80 bytes of the gate). Found a gap of {} bytes — the reset \
         may have drifted out of the off-hours branch.",
        reset_idx - gate_idx
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

/// H7 (D2b lane-scoped runtime watchdog) source-scan ratchet.
///
/// `spawn_pool_watchdog_task` takes a `lane_halt: Option<Arc<Notify>>`
/// argument (H7). On a 300s-all-down `Halt` verdict during market hours the
/// watchdog now has TWO mutually-exclusive blast-radius modes inside the
/// `if in_market_hours` branch of the `WatchdogVerdict::Halt` arm:
///
/// - **RUNTIME lane** (`lane_halt.is_some()`): the `if let Some(ref halt) =
///   lane_halt { ... }` branch fires `halt.notify_waiters()` (so the parked
///   task drives the FSM `Running → Stopping → Off` via `handle_lane_watchdog_halt`)
///   and `return`s. It MUST NOT call `std::process::exit` — a lane-local Dhan
///   fault must never kill the whole process / the independent Groww feed / the
///   shared seal-writer + aggregator + API server.
/// - **BOOT-ON** (`lane_halt.is_none()`): the pre-existing single-feed contract
///   keeps `std::process::exit(2);` so systemd/the supervisor restarts the
///   WHOLE process. This is preserved exactly.
///
/// This ratchet pins BOTH halves so a future refactor cannot:
///   (a) drop the lane-scoped no-exit path, OR
///   (b) leak `std::process::exit` into the runtime-lane branch, OR
///   (c) remove the boot-ON `std::process::exit(2);` pin.
///
/// The production code at `crates/app/src/main.rs` carries a
/// `TEST-EXEMPT: ... runtime_lane_watchdog_does_not_process_exit` reference on
/// `handle_lane_watchdog_halt`; this is the test that reference points at.
///
/// Source-scan is the right tool: the Halt action is async + clock-gated +
/// FSM-driven, so a behavioural test would need to mock tokio::time +
/// std::process::exit + the FeedRuntimeState FSM. The invariants we care about
/// (the `Some(ref halt)` branch fires `notify_waiters()` + `return`s without
/// exiting; the single `process::exit(2);` lives only AFTER that branch closes,
/// i.e. on the `None`/boot-ON path) are mechanical and visible in the source.
#[test]
fn runtime_lane_watchdog_does_not_process_exit() {
    let src = read_main_rs();

    // 1. The watchdog must accept the H7 lane-scoped Halt signal argument.
    assert!(
        src.contains("lane_halt: Option<std::sync::Arc<tokio::sync::Notify>>"),
        "spawn_pool_watchdog_task must take a `lane_halt: Option<Arc<Notify>>` \
         argument (H7). `None` = BOOT-ON (keep process::exit(2)); `Some` = \
         RUNTIME lane (signal the parked task, NEVER process::exit). Removing \
         this argument collapses the lane-scoped blast-radius fix and would let \
         a lane-local Dhan fault kill the whole process + the Groww feed."
    );

    // Scope to the watchdog task's Halt arm. The window from
    // `let verdict = pool.poll_watchdog();` comfortably covers the Halt arm's
    // `if in_market_hours { ... lane_halt ... process::exit(2); }` block (the
    // `std::process::exit(2);` site sits ~6.1KB in; use an 8KB window).
    let needle = "let verdict = pool.poll_watchdog();";
    let start = src
        .find(needle)
        .expect("spawn_pool_watchdog_task must contain `let verdict = pool.poll_watchdog();`");
    let end = (start + 8_000).min(src.len());
    let block = &src[start..end];

    let halt_arm_idx = block
        .find("WatchdogVerdict::Halt")
        .expect("watchdog match block must include the Halt arm");

    // 2. The runtime-lane branch must exist and fire the lane Halt signal.
    let some_branch_idx = block.find("if let Some(ref halt) = lane_halt").expect(
        "Halt arm must have an `if let Some(ref halt) = lane_halt { ... }` \
             runtime-lane branch (H7) so a runtime lane tears itself down via \
             the FSM instead of exiting the process.",
    );
    assert!(
        halt_arm_idx < some_branch_idx,
        "the `if let Some(ref halt) = lane_halt` branch must live inside the \
         Halt arm (found Halt arm at {halt_arm_idx}, lane branch at {some_branch_idx})."
    );

    let notify_idx = block.find("halt.notify_waiters()").expect(
        "the runtime-lane Halt branch must fire `halt.notify_waiters()` so the \
         parked task (`park_running_dhan_lane`) observes it and drives the FSM \
         `Running → Stopping → Off` via `handle_lane_watchdog_halt` — NOT \
         `process::exit`.",
    );
    assert!(
        some_branch_idx < notify_idx,
        "`halt.notify_waiters()` must appear inside the `Some(ref halt)` branch \
         (found branch at {some_branch_idx}, notify at {notify_idx})."
    );

    // 3. The single boot-ON process::exit(2); must appear ONLY AFTER the
    //    runtime-lane branch's `return;` closes it. The lexical proof: the
    //    `Some(ref halt)` branch contains `notify_waiters()` then `return;`, and
    //    the only `std::process::exit(2);` site appears strictly AFTER that
    //    `return;` (the `None`/boot-ON fall-through). If a future edit moved
    //    `process::exit` into the runtime-lane branch, it would appear BEFORE
    //    the branch-closing `return;`.
    let return_idx = block[notify_idx..]
        .find("return;")
        .map(|rel| notify_idx + rel)
        .expect(
            "the runtime-lane Halt branch must `return;` after `notify_waiters()` \
             so polling stops (the parked task owns teardown) and the boot-ON \
             `process::exit(2);` below is NOT reached for a runtime lane.",
        );

    let exit_idx = block.find("std::process::exit(2);").expect(
        "the boot-ON Halt path must still call `std::process::exit(2);` (the \
         pre-existing single-feed contract — systemd/the supervisor restarts the \
         WHOLE process). Removing this pin would silently break the boot-ON \
         crash-recovery behaviour.",
    );

    // The runtime-lane branch's `notify_waiters()` + `return;` must BOTH come
    // before the boot-ON `process::exit(2);`. This proves `process::exit` is on
    // the `None` (boot-ON) fall-through, NOT inside the `Some(ref halt)` branch.
    assert!(
        notify_idx < return_idx && return_idx < exit_idx,
        "the runtime-lane branch (`halt.notify_waiters()` @ {notify_idx} then \
         `return;` @ {return_idx}) must lexically precede the boot-ON \
         `std::process::exit(2);` @ {exit_idx}. If process::exit moved into the \
         runtime-lane branch, a lane-local Dhan fault would kill the process + \
         the Groww feed + shared infra. Ordering violated."
    );

    // 4. There must be EXACTLY ONE `std::process::exit(2);` in the watchdog
    //    block — the boot-ON one. A second one anywhere in the block would be a
    //    process::exit leaking into (or duplicated within) the runtime-lane path.
    let exit_count = block.matches("std::process::exit(2);").count();
    assert_eq!(
        exit_count, 1,
        "the watchdog Halt arm must contain EXACTLY ONE `std::process::exit(2);` \
         (the boot-ON path). Found {exit_count}. A second occurrence means \
         process::exit leaked into the runtime-lane (`Some(ref halt)`) branch, \
         which would kill the process on a lane-only fault."
    );

    // 5. Belt-and-braces: the slice of the `Some(ref halt)` branch up to its
    //    closing `return;` must NOT contain `std::process::exit` at all.
    let some_branch = &block[some_branch_idx..return_idx];
    assert!(
        !some_branch.contains("std::process::exit"),
        "the runtime-lane Halt branch (the `if let Some(ref halt) = lane_halt` \
         block ending in `return;`) must NEVER call `std::process::exit` — it \
         tears the Dhan lane down via the FSM and returns, leaving Groww + \
         shared infra alive."
    );
}
