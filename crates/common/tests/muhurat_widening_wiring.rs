//! The Muhurat widening reaches the writers ONLY through the two functions that
//! read the boot flag: `session_window::row_is_in_an_open_window` (the predicate
//! the tick, depth, shadow-candle and spill-replay writers call) and
//! `session_window::verdict`.
//!
//! # Why this is its OWN test binary
//!
//! The flag lives behind a process-wide `OnceLock` (`muhurat::init_muhurat_session`,
//! set once at boot). A test that INSTALLS it must not share a process with one
//! that asserts it is unset, so these live in an integration binary of their own
//! rather than beside the pure-form unit tests in `session_window.rs`. Those unit
//! tests deliberately route through `nanos_in_any_open_window` / `verdict_in`,
//! taking the flag as a parameter, precisely so they stay order-independent.
//!
//! # The regression this closes (dated 2026-09-06)
//!
//! Commit `a23a11fb9` named four unit tests after the wrappers while every one of
//! them called only the pure forms, so `row_is_in_an_open_window` and `verdict`
//! had ZERO test callers in the workspace. Rewriting
//! `row_is_in_an_open_window`'s body to `nanos_in_session_window(ist_nanos)`
//! would silently strip Muhurat widening from all four writers — refusing every
//! tick of a Muhurat evening session — with the whole workspace still green.
//! These tests call the wrappers for real and fail on exactly that edit.

use tickvault_common::constants::{
    MUHURAT_PERSIST_END_SECS_OF_DAY_IST, MUHURAT_PERSIST_START_SECS_OF_DAY_IST,
};
use tickvault_common::muhurat::init_muhurat_session;
use tickvault_common::session_window::{
    WindowVerdict, nanos_in_any_open_window, nanos_in_session_window, row_is_in_an_open_window,
    verdict, verdict_in,
};

/// 2026-11-01, an arbitrary IST midnight, as epoch nanoseconds.
const DAY_START_IST_NANOS: i64 = 1_761_955_200 * 1_000_000_000;

fn at(secs_of_day: u32) -> i64 {
    DAY_START_IST_NANOS + i64::from(secs_of_day) * 1_000_000_000
}

/// Install the flag ONCE for this binary. Idempotent (first call wins), which is
/// what makes it safe for every test here to call it — they all want `true`.
fn muhurat_day() {
    init_muhurat_session(true);
}

#[test]
fn row_is_in_an_open_window_widens_on_a_muhurat_day() {
    muhurat_day();

    let evening = at(MUHURAT_PERSIST_START_SECS_OF_DAY_IST + 60);

    // The narrow predicate refuses it -- so this instant can ONLY be accepted by
    // reading the flag. Sabotaging the wrapper to `nanos_in_session_window` fails
    // right here.
    assert!(
        !nanos_in_session_window(evening),
        "a Muhurat evening instant must be OUTSIDE the regular session -- \
         otherwise this test proves nothing about widening"
    );
    assert!(
        row_is_in_an_open_window(evening),
        "row_is_in_an_open_window must widen to the Muhurat window when the boot \
         flag is set -- if this fails the wrapper stopped reading muhurat::current()"
    );
}

#[test]
fn row_is_in_an_open_window_still_refuses_what_neither_window_holds() {
    muhurat_day();

    // Between the two windows (15:40 -> 18:00) and after the Muhurat close.
    for outside in [
        at(56_400),
        at(60_000),
        at(MUHURAT_PERSIST_END_SECS_OF_DAY_IST),
    ] {
        assert!(
            !row_is_in_an_open_window(outside),
            "widening must not become accept-all: {outside} is in neither window"
        );
    }

    // The regular session is unaffected by the flag.
    assert!(
        row_is_in_an_open_window(at(40_000)),
        "the regular session must stay open on a Muhurat day"
    );
}

#[test]
fn row_is_in_an_open_window_agrees_with_the_pure_form_at_the_installed_flag() {
    muhurat_day();

    // The wrapper is exactly the pure form fed the boot flag -- checked across
    // both windows, both gaps, and both boundaries, so a body that reads the
    // flag but computes something else is caught too.
    for secs in [
        0,
        32_399,
        32_400,
        40_000,
        56_399,
        56_400,
        60_000,
        MUHURAT_PERSIST_START_SECS_OF_DAY_IST - 1,
        MUHURAT_PERSIST_START_SECS_OF_DAY_IST,
        MUHURAT_PERSIST_END_SECS_OF_DAY_IST - 1,
        MUHURAT_PERSIST_END_SECS_OF_DAY_IST,
        86_399,
    ] {
        assert_eq!(
            row_is_in_an_open_window(at(secs)),
            nanos_in_any_open_window(at(secs), true),
            "wrapper and pure form disagree at {secs}s of day"
        );
    }
}

#[test]
fn verdict_widens_on_a_muhurat_day_and_still_judges_the_receipt_clock() {
    muhurat_day();

    let evening_ts = at(MUHURAT_PERSIST_START_SECS_OF_DAY_IST + 120);
    let evening_rx = at(MUHURAT_PERSIST_START_SECS_OF_DAY_IST + 121);

    // Off a Muhurat day this pair is refused outright ...
    assert_eq!(
        verdict_in(evening_ts, Some(evening_rx), false),
        WindowVerdict::TsOutOfWindow,
        "the fixture must be outside the regular session, else the test is vacuous"
    );
    // ... and the global-reading form accepts it, so it IS reading the flag.
    assert_eq!(
        verdict(evening_ts, Some(evening_rx)),
        WindowVerdict::InWindow,
        "verdict must widen to the Muhurat window when the boot flag is set"
    );

    // Widening does not blind the receipt clock: an in-window stamp with a
    // receipt in the 15:40-18:00 gap is still caught.
    assert_eq!(
        verdict(evening_ts, Some(at(60_000))),
        WindowVerdict::ReceivedAtOutOfWindow,
        "the receipt clock must still be judged against the widened window"
    );
}
