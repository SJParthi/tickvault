//! Source-scan ratchet: the live lane must keep reporting its socket count to
//! `/health`.
//!
//! # The defect this closes
//!
//! `SystemHealthStatus` gates its websocket row on whether ANY producer has
//! ever pushed a count. The gate was added 2026-08-09 for a real reason: with
//! the lane deleted the count sat at 0 forever and `/health` returned
//! `degraded` on every request, a verdict carrying no information. Its own doc
//! calls the flag "arm-on-arrival" — the first pushed count arms it, with no
//! edit needed on the API side.
//!
//! The lane was revived (operator quotes 2026-08-09; default ON 2026-08-11)
//! and **nobody wrote the producer**. So for eleven days a box with sixteen
//! sockets dialing answered:
//!
//! ```json
//! "websocket": { "status": "retired", "detail": "live feeds retired 2026-07-13/15" }
//! ```
//!
//! A dead lane and a healthy lane rendered identically, on the endpoint whose
//! entire job is telling them apart. Nothing failed; `/health` simply stopped
//! being able to answer its own question.
//!
//! # What this pins, and what it deliberately does not
//!
//! It pins the two halves that a future edit could drop independently: the
//! installer is CALLED from boot, and the publish path PUSHES. It does not try
//! to prove the count is correct at runtime — that needs a live socket, and a
//! source scan that pretended otherwise would be the false-OK this file exists
//! to prevent.

use std::fs;
use std::path::{Path, PathBuf};

fn app_src(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(name)
}

fn read(p: &Path) -> String {
    fs::read_to_string(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

#[test]
fn boot_installs_the_health_websocket_reporter() {
    let main = read(&app_src("main.rs"));
    assert!(
        main.contains("dhan_feed_stack::install_health_reporter("),
        "main.rs must install the /health websocket reporter; without it the \
         endpoint reports `retired` on a box whose lane is dialing sockets"
    );
}

#[test]
fn the_reporter_is_installed_before_the_lane_is_spawned() {
    let main = read(&app_src("main.rs"));
    let install = main
        .find("dhan_feed_stack::install_health_reporter(")
        .expect("installer call site");
    let spawn = main
        .find("dhan_feed_stack::spawn_dhan_feed_stack(")
        .expect("lane spawn site");
    assert!(
        install < spawn,
        "install the reporter BEFORE the lane dials, or the first sockets come \
         up unreported and /health lies for as long as they stay up"
    );
}

#[test]
fn the_alive_count_publisher_pushes_into_health() {
    let stack = read(&app_src("dhan_feed_stack.rs"));
    let at = stack
        .find("fn publish_alive_connections(")
        .expect("publish_alive_connections must exist");
    let body_end = stack[at..]
        .find("\n}\n")
        .map(|e| at + e)
        .expect("function body must terminate");
    let body = &stack[at..body_end];
    assert!(
        body.contains("HEALTH_REPORTER.get()"),
        "publish_alive_connections must push the count into /health — it is \
         the one function both edges of AliveConnectionGuard pass through, so \
         wiring it anywhere else would drift"
    );
    assert!(
        body.contains("set_websocket_connections("),
        "the push must call set_websocket_connections, which is what arms the \
         arm-on-arrival flag on the API side"
    );
}

#[test]
fn health_no_longer_claims_a_retirement_that_was_reversed() {
    let health =
        read(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../api/src/handlers/health.rs"));
    // PRODUCTION half only. The test module quotes the old string verbatim in
    // the comment recording the correction, and that provenance is worth more
    // than a scan that is simple.
    let production = health.split("#[cfg(test)]").next().unwrap_or(&health);
    assert!(
        !production.contains("live feeds retired 2026-07-13/15"),
        "the websocket detail must not assert a retirement the operator \
         reversed on 2026-08-09; say why there is no count instead"
    );
}

/// The permutation that used to read as healthy-by-omission.
///
/// Three boots, three different truths, and before this wiring two of them
/// rendered identically:
///
/// | boot | sockets | `/health` before | `/health` now |
/// |---|---|---|---|
/// | feature off | — | `retired` | `retired` |
/// | on, none dialed | 0 | **`retired`** | `disconnected`, 0, degraded |
/// | on, N dialed | N | `retired` | `connected`, N |
///
/// The middle row is the dangerous one: a lane that is enabled and dials
/// NOTHING — empty universe, every endpoint refusing, credentials rejected —
/// gave the same answer as a box with the feature switched off. It is the one
/// case where something is actually wrong.
#[test]
fn the_row_arms_when_the_lane_commits_to_spawning_not_on_the_first_dial() {
    let stack = read(&app_src("dhan_feed_stack.rs"));
    let at = stack
        .find("pub fn spawn_dhan_feed_stack(")
        .expect("spawn entry must exist");
    let body = &stack[at..];
    let arm = body
        .find("publish_alive_connections(ALIVE_CONNECTIONS.load(")
        .expect(
            "spawn_dhan_feed_stack must arm the /health row with the current \
             count, or an enabled lane that dials nothing reads `retired` — \
             the same answer as the feature being off",
        );
    let spawn = body
        .find("tokio::spawn(run_dhan_feed_stack(")
        .expect("bring-up spawn");
    assert!(
        arm < spawn,
        "arm the row BEFORE the bring-up task, or the window between them \
         reports `retired` on a lane that is already starting"
    );
}

/// The arming must sit AFTER the gate, or a feature-off boot reports a live
/// subsystem with zero connections instead of the truth.
#[test]
fn a_disabled_lane_never_arms_the_row() {
    let stack = read(&app_src("dhan_feed_stack.rs"));
    let at = stack
        .find("pub fn spawn_dhan_feed_stack(")
        .expect("spawn entry must exist");
    let body = &stack[at..];
    let gate_return = body
        .find("return None;")
        .expect("the disabled gate must return early");
    let arm = body
        .find("publish_alive_connections(ALIVE_CONNECTIONS.load(")
        .expect("arming call");
    assert!(
        gate_return < arm,
        "the disabled-gate early return must come BEFORE the arming, or a box \
         with the feature off reports `disconnected` and degrades — claiming a \
         fault where there is only a switch in the off position"
    );
}

/// The sibling row, same defect, found by walking the other three gates.
///
/// `SystemHealthStatus` has four "arm-on-arrival" gates — websocket, pipeline,
/// tick_persistence, order_update. Two were correct (`pipeline` really has no
/// caller; `order_update` was wired 2026-08-10). Two were stale claims left by
/// the revival:
///
/// | row | claimed | true |
/// |---|---|---|
/// | `websocket` | retired 2026-07-13/15 | revived 2026-08-09, ON 2026-08-11 |
/// | `tick_persistence` | writer deleted 2026-07-17 | file exists; lane writes every tick |
///
/// Checking one gate and not its siblings is how the second one survives.
#[test]
fn the_lane_reports_tick_persistence_flush_outcomes() {
    let stack = read(&app_src("dhan_feed_stack.rs"));
    assert!(
        stack.contains("report_tick_persistence(true)"),
        "a successful flush must report tick persistence as connected"
    );
    assert!(
        stack.contains("report_tick_persistence(false)"),
        "a FAILED flush must report it as not connected — reporting only the \
         success side is how a broken writer keeps rendering healthy"
    );
    assert!(
        stack.contains("set_tick_persistence_connected("),
        "the reporter must call the setter that arms the row"
    );
}

#[test]
fn health_no_longer_claims_the_tick_writer_was_deleted() {
    let health =
        read(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../api/src/handlers/health.rs"));
    let production = health.split("#[cfg(test)]").next().unwrap_or(&health);
    assert!(
        !production.contains("tick writer deleted 2026-07-17"),
        "crates/storage/src/tick_persistence.rs exists and the lane writes \
         through it — the detail must not assert a deletion that was reversed"
    );
}
