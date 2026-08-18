//! Guard: the live lane must WAIT for today's mapping artifact before it
//! resolves its subscription set.
//!
//! # The production incident this pins
//!
//! The daily `[dhan_universe]` rider WRITES `dhan-nse-mapping-<today>.json`;
//! the live lane READS it. Both start from the same boot with no ordering
//! between them, and the filename is date-stamped so yesterday's file cannot
//! stand in. Measured on the prod box:
//!
//! | Time (IST) | Event |
//! |---|---|
//! | 2026-08-18 08:11:48 | lane reads the artifact — `NotFound` — falls back to **4 instruments** |
//! | 2026-08-18 08:11:57 | rider writes it: `resolved: 4567` — **9 seconds too late** |
//! | 2026-08-18 08:31:29 | box happens to boot again; now the file exists → **4,565 subscribed** |
//!
//! On 2026-08-17 there was only ONE boot (08:31:37). It lost the race, and the
//! **entire trading session ran on 4 instruments instead of 4,565** — silently,
//! because `tv_dhan_live_universe_fallback_total` has no alarm attached.
//!
//! So the failure is not intermittent: a box that boots once — the normal case
//! — loses every single day. Today's green reading was luck, not design.
//!
//! These tests fail the build if the wait is removed, if it stops being
//! awaited, or if it drifts back below the resolve call.

const MAIN_RS: &str = include_str!("../src/main.rs");
const LIVE_UNIVERSE_RS: &str = include_str!("../src/dhan_live_universe.rs");

/// Strip `//` line comments so a prose mention of a symbol cannot satisfy a
/// scan that is meant to find real code.
fn code_only(src: &str) -> String {
    src.lines()
        .map(|line| match line.find("//") {
            Some(idx) => &line[..idx],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn boot_awaits_the_mapping_artifact_before_resolving_the_universe() {
    let code = code_only(MAIN_RS);

    let wait_at = code.find("await_mapping_artifact").unwrap_or_else(|| {
        panic!(
            "crates/app/src/main.rs no longer calls `await_mapping_artifact`. Without it the \
             lane races the daily rider and collapses to 4 instruments on every single-boot \
             day — the 2026-08-17 incident in this file's header."
        )
    });

    let resolve_at = code.find("resolve_live_universe").unwrap_or_else(|| {
        panic!("crates/app/src/main.rs no longer calls `resolve_live_universe`")
    });

    assert!(
        wait_at < resolve_at,
        "`await_mapping_artifact` must run BEFORE `resolve_live_universe` in main.rs. \
         Resolving first is precisely the race: the resolve reads a file the rider has not \
         written yet."
    );
}

#[test]
fn the_wait_is_actually_awaited() {
    let code = code_only(MAIN_RS);
    let idx = code
        .find("await_mapping_artifact")
        .expect("call site must exist — covered by the test above");
    let tail = &code[idx..];
    // The call spans several lines (formatted args), so look at a generous
    // window rather than the single line.
    let window: String = tail.chars().take(400).collect();
    assert!(
        window.contains(".await"),
        "`await_mapping_artifact` is called but never awaited — the future would be dropped \
         and the wait would silently do nothing, which reads exactly like the bug it fixes. \
         Window inspected:\n{window}"
    );
}

#[test]
fn the_wait_is_bounded_and_fail_soft() {
    let src = LIVE_UNIVERSE_RS;

    assert!(
        src.contains("MAPPING_WAIT_DEADLINE_SECS"),
        "the wait must carry an explicit deadline constant — an unbounded wait would hang \
         boot behind a vendor outage, which is strictly worse than the fallback it avoids"
    );

    let code = code_only(src);
    assert!(
        code.contains("started.elapsed() < deadline"),
        "the poll loop must be deadline-bounded"
    );

    // Fail-soft: the function returns rather than panicking or looping forever.
    assert!(
        !code.contains("panic!") && !code.contains("unwrap()"),
        "the boot wait must never panic — a failed wait has to degrade to the index universe, \
         not abort the process"
    );
}

#[test]
fn every_wait_outcome_is_counted() {
    let src = LIVE_UNIVERSE_RS;
    assert!(
        src.contains("MAPPING_WAIT_COUNTER"),
        "the wait must publish a counter; without it a boot that timed out is \
         indistinguishable from one that never needed to wait"
    );
    for outcome in [
        "not_requested",
        "rider_disabled",
        "already_present",
        "became_ready",
        "timed_out",
    ] {
        assert!(
            src.contains(outcome),
            "wait outcome `{outcome}` is not emitted — an unlabelled exit is an unobservable one"
        );
    }
}

#[test]
fn a_disabled_rider_does_not_burn_the_whole_deadline() {
    let code = code_only(LIVE_UNIVERSE_RS);
    let disabled_at = code
        .find("if !cfg.enabled")
        .expect("the wait must short-circuit when the rider that writes the artifact is off");
    let loop_at = code
        .find("while started.elapsed()")
        .expect("the poll loop must exist");
    assert!(
        disabled_at < loop_at,
        "the `[dhan_universe] enabled = false` check must come BEFORE the poll loop. With no \
         writer, waiting the full deadline reaches the same fallback having wasted the time."
    );
}
