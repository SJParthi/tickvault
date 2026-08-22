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
