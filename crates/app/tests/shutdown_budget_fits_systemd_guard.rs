//! Source-scan ratchet — the app's shutdown budgets MUST fit inside
//! systemd's `TimeoutStopSec`.
//!
//! **The defect this pins closed (found 2026-08-21).** The shutdown path in
//! `crates/app/src/main.rs` runs two bounded waits SEQUENTIALLY:
//!
//! | constant | value | order |
//! |---|---|---|
//! | `DHAN_LANE_SHUTDOWN_FLUSH_BUDGET_SECS` | 20s | first (main.rs:3552) |
//! | `SEAL_WRITER_SHUTDOWN_BUDGET_SECS` | 75s | second (main.rs:3585) |
//! | `SEAL_ESCALATION_SHUTDOWN_BUDGET_SECS` | 5s | third (added 2026-08-28) |
//! | `WAL_SPILL_SHUTDOWN_BUDGET_SECS` | 10s | fourth (added 2026-08-28) |
//! | `DHAN_SOCKET_STOP_BUDGET_SECS` | 5s | now FIRST, before all four (added 2026-10-02, Z11d) |
//!
//! …for a worst case of **95 seconds**, while `deploy/systemd/tickvault.service`
//! carried `TimeoutStopSec=30`. systemd therefore SIGKILLed the process 65s
//! before the app was willing to stop.
//!
//! The cost was not theoretical. If the lane flush used its full 20s, the
//! seal writer received **10s of its 75s budget**; at the measured
//! ~10,200 seals/sec that drains ~102,000 of a possible 600,000 seals at the
//! 25,000-instrument ceiling (that ceiling is `AGGREGATOR_MAX_SLOTS ×
//! TF_COUNT` and was written at TF_COUNT=24; the 2026-09-19 nine-frame
//! collapse put it at 225,000 and `TF_COUNT` 10 since 2026-09-22 puts it at
//! **250,000**, so the same 10s drains ~102,000 of 250,000 — the seals LOST
//! are unchanged, only the fraction moved).
//! The remainder — **including every instrument's final bar of the session
//! on every frame** (this read "final daily bar" until 2026-09-19; the D1
//! frame it named is retired, and the widest surviving frame is M60) — was
//! destroyed by the SIGKILL, with no counter moving,
//! because a killed process cannot log its own death.
//!
//! The sharpest part: the 75s constant's OWN comment reasons about this exact
//! hazard ("overrunning systemd's stop timeout earns a SIGKILL, which loses
//! the very tail this exists to save") and was then set to 2.5x the timeout it
//! names. A correct comment is not a gate. Nothing compared the two files, so
//! they drifted — the identical shape as the `WATCHDOG_INTERVAL_SECS x 2 <=
//! WatchdogSec` pin in `systemd_boot_notify_guard.rs`, which is why this guard
//! is written as its sibling.
//!
//! This pins the RELATIONSHIP, never the literals: raise either app budget, or
//! lower the timeout, and the build fails with the arithmetic spelled out.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/app -> repo root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// Strip `//`-comments so a doc-comment mention can never vacuously satisfy a
/// pin (the house convention — see `systemd_boot_notify_guard.rs`).
fn strip_comments(src: &str) -> String {
    src.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Read `const <NAME>: u64 = <N>;` from real (non-comment) source.
fn const_secs(src: &str, name: &str) -> u64 {
    let code = strip_comments(src);
    let needle = format!("const {name}: u64 =");
    let at = code
        .find(&needle)
        .unwrap_or_else(|| panic!("{name} not found in production source — was it renamed?"));
    code[at + needle.len()..]
        .split(';')
        .next()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or_else(|| panic!("{name} is not a plain integer literal"))
}

/// Read `TimeoutStopSec=<N>` from the unit file, ignoring `#` comments.
fn timeout_stop_secs(unit: &str) -> u64 {
    unit.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .find_map(|l| l.strip_prefix("TimeoutStopSec="))
        .unwrap_or_else(|| panic!("TimeoutStopSec not found in tickvault.service"))
        .trim()
        .parse()
        .expect("TimeoutStopSec must be a plain integer (seconds)")
}

#[test]
fn app_shutdown_budgets_fit_inside_systemd_stop_timeout() {
    let main_rs = read("crates/app/src/main.rs");
    let spill_rs = read("crates/storage/src/ws_frame_spill.rs");
    let unit = read("deploy/systemd/tickvault.service");

    // Z11d (2026-10-02): the socket close now runs BEFORE everything else.
    let sockets = const_secs(&main_rs, "DHAN_SOCKET_STOP_BUDGET_SECS");
    let lane = const_secs(&main_rs, "DHAN_LANE_SHUTDOWN_FLUSH_BUDGET_SECS");
    let seal = const_secs(&main_rs, "SEAL_WRITER_SHUTDOWN_BUDGET_SECS");
    // Third wait, and it lives in ANOTHER CRATE. `main` reaches it through
    // `WsFrameSpill::shutdown`, so a guard that only ever read main.rs would
    // have been structurally blind to it — the same shape as the unit file
    // being invisible to a compile-time assert.
    let escalation = const_secs(&main_rs, "SEAL_ESCALATION_SHUTDOWN_BUDGET_SECS");
    let wal = const_secs(&spill_rs, "WAL_SPILL_SHUTDOWN_BUDGET_SECS");
    let timeout = timeout_stop_secs(&unit);

    // The waits are SEQUENTIAL on the same path, so the worst case is the
    // SUM. Summing (rather than taking the max) is the whole point: the bug
    // was that the seal writer inherited only the remainder after the lane
    // flush had already spent its budget.
    let worst_case = sockets + lane + seal + escalation + wal;

    assert!(
        worst_case < timeout,
        "SHUTDOWN BUDGET OVERRUN — systemd will SIGKILL the app mid-drain.\n\
         \n\
           DHAN_SOCKET_STOP_BUDGET_SECS         = {sockets}s (runs first)\n\
           DHAN_LANE_SHUTDOWN_FLUSH_BUDGET_SECS = {lane}s (then this)\n\
           SEAL_WRITER_SHUTDOWN_BUDGET_SECS     = {seal}s (runs second)\n\
           SEAL_ESCALATION_SHUTDOWN_BUDGET_SECS = {escalation}s (runs third)\n\
           WAL_SPILL_SHUTDOWN_BUDGET_SECS       = {wal}s (runs fourth)\n\
           worst-case app shutdown              = {worst_case}s\n\
           systemd TimeoutStopSec               = {timeout}s\n\
         \n\
         A SIGKILL here destroys the day's final bar for every instrument and\n\
         every timeframe still buffered, and the process cannot log its own\n\
         death — the loss is silent.\n\
         \n\
         Fix by raising TimeoutStopSec in deploy/systemd/tickvault.service to\n\
         at least {}s, NOT by shrinking a drain budget that was sized from a\n\
         live measurement.",
        worst_case + 25
    );
}

#[test]
fn stop_timeout_keeps_a_real_margin_not_a_hairline() {
    let main_rs = read("crates/app/src/main.rs");
    let spill_rs = read("crates/storage/src/ws_frame_spill.rs");
    let unit = read("deploy/systemd/tickvault.service");

    let worst_case = const_secs(&main_rs, "DHAN_SOCKET_STOP_BUDGET_SECS")
        + const_secs(&main_rs, "DHAN_LANE_SHUTDOWN_FLUSH_BUDGET_SECS")
        + const_secs(&main_rs, "SEAL_WRITER_SHUTDOWN_BUDGET_SECS")
        + const_secs(&main_rs, "SEAL_ESCALATION_SHUTDOWN_BUDGET_SECS")
        + const_secs(&spill_rs, "WAL_SPILL_SHUTDOWN_BUDGET_SECS");
    let timeout = timeout_stop_secs(&unit);
    let margin = timeout.saturating_sub(worst_case);

    // A one-second margin technically passes the test above while leaving no
    // room for the process teardown that follows the drain (API + otel
    // shutdown, partition detach). 20s is the floor.
    assert!(
        margin >= 20,
        "TimeoutStopSec={timeout}s leaves only {margin}s over the {worst_case}s worst-case \
         drain. The final teardown (API + otel shutdown, partition detach) runs AFTER the \
         drain and needs headroom; a hairline margin is a SIGKILL on a slow day. Require >= 20s."
    );
}

#[test]
fn the_stop_timeout_is_documented_as_derived_not_guessed() {
    let unit = read("deploy/systemd/tickvault.service");
    // The value is load-bearing arithmetic, so the unit file must NAME every
    // constant it is derived from. Renaming a constant without updating the
    // rationale leaves the next reader unable to re-derive the number.
    for needle in [
        "DHAN_SOCKET_STOP_BUDGET_SECS",
        "DHAN_LANE_SHUTDOWN_FLUSH_BUDGET_SECS",
        "SEAL_WRITER_SHUTDOWN_BUDGET_SECS",
        "SEAL_ESCALATION_SHUTDOWN_BUDGET_SECS",
        "WAL_SPILL_SHUTDOWN_BUDGET_SECS",
    ] {
        assert!(
            unit.contains(needle),
            "tickvault.service must name {needle} in the TimeoutStopSec rationale so the \
             value can be re-derived rather than guessed at."
        );
    }
}

/// Z11b (2026-10-02): under `panic = "abort"` the panic hook is the last code
/// that runs, so it must give the WAL writer its bounded drain BEFORE handing
/// over to the default hook (after which the process aborts), and after the
/// synchronous panic line (which must never wait behind anything). The
/// writer-thread skip lives inside `drain_registered_for_abort`; this pins
/// that the hook reaches it and in which order.
#[test]
fn the_panic_hook_drains_the_wal_before_it_aborts() {
    let main_rs = strip_comments(&read("crates/app/src/main.rs"));
    let hook_at = main_rs
        .find("std::panic::set_hook(")
        .expect("main.rs installs a panic hook");
    let hook = &main_rs[hook_at..];
    let end = hook
        .find("default_panic_hook(panic_info)")
        .expect("the hook chains to the default hook");
    let hook = &hook[..end];
    let sync_line = hook
        .find("append_panic_line_sync(")
        .expect("the hook writes the synchronous panic line");
    let drain = hook
        .find("drain_registered_for_abort(")
        .expect("the hook must drain the WAL spill before the default hook aborts the process");
    assert!(
        sync_line < drain,
        "the synchronous panic line must be written before the WAL drain waits"
    );
    assert!(
        hook.contains("WAL_ABORT_DRAIN_BUDGET"),
        "the hook's WAL drain must be bounded by WAL_ABORT_DRAIN_BUDGET"
    );

    let spill_rs = read("crates/storage/src/ws_frame_spill.rs");
    let body_at = spill_rs
        .find("pub fn drain_registered_for_abort(")
        .expect("drain_registered_for_abort exists");
    let body = &spill_rs[body_at..];
    let skip = body
        .find("WAL_WRITER_THREAD_NAME")
        .expect("the drain must skip the writer thread, which cannot wait for itself");
    let wait = body
        .find(".wait(")
        .expect("the drain waits on the handshake");
    assert!(
        skip < wait,
        "the writer-thread skip must come before any wait"
    );
}

/// Z11d (2026-10-02): the sockets must be CLOSED before the lane drain is
/// stopped, and the drain before the WAL writer. Until then nothing closed
/// the sockets, so they kept appending to a WAL whose writer was being shut
/// down, and records landing after its last empty poll were lost silently.
#[test]
fn shutdown_closes_the_sockets_before_the_lane_and_the_wal() {
    let main_rs = strip_comments(&read("crates/app/src/main.rs"));
    let stop = main_rs
        .find("stop_feed_sockets(DHAN_SOCKET_STOP_BUDGET)")
        .expect("shutdown must close the Dhan feed sockets");
    let lane = main_rs
        .find("dhan_feed_shutdown.notify_one()")
        .expect("shutdown signals the lane drain");
    let wal = main_rs
        .find("spill.shutdown(")
        .expect("shutdown drains the WAL writer");
    assert!(
        stop < lane,
        "the sockets must close before the lane drain is signalled"
    );
    assert!(lane < wal, "the lane must stop before the WAL writer does");

    let stack = strip_comments(&read("crates/app/src/dhan_feed_stack.rs"));
    let body_at = stack
        .find("async fn request_stop_and_wait(")
        .expect("the socket-stop wait exists");
    let body = &stack[body_at..];
    let request = body.find("stop.request()").expect("it requests the stop");
    let wait = body
        .find("alive.load(")
        .expect("it waits on the live socket count");
    assert!(
        request < wait,
        "the stop is requested before the wait begins"
    );
}
