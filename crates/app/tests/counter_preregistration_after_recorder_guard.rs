//! Guard: every counter pre-registration in `main.rs` must run AFTER the
//! Prometheus recorder is installed.
//!
//! WHY THIS EXISTS — a live defect, not a hypothetical. On 2026-08-26 the
//! WAL's `WsFrameSpill::new()` sat ~35 lines ABOVE
//! `observability::init_metrics`. `metrics::counter!` with no recorder
//! installed returns a NO-OP handle, and `SpillDropCounters` CACHES its
//! handles in a struct for the process lifetime. Two consequences, and the
//! second is the serious one:
//!
//!   1. the zero pre-registration was lost, so the series never appeared; and
//!   2. every later `increment(1)` on a real dropped frame went to the no-op
//!      too — so the counter could never be published AT ALL.
//!
//! Measured on the live prod box that day: `tv_ticks_lost_total` and
//! `tv_ws_frame_spill_drop_critical` had ZERO matching lines out of 756
//! exported. `tv-<env>-ticks-lost-spill` — the alarm on UNRECOVERABLE tick
//! loss, the operator's single most important guarantee — was therefore
//! structurally incapable of firing.
//!
//! The hazard was already known and already fixed TWICE within 40 lines of
//! the offending call (the STAGE-C.2b replay counters, and
//! `prewarm_dispatcher_counters`). Knowing it was not enough; this guard is
//! the mechanical version.

use std::path::PathBuf;

fn main_rs() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    std::fs::read_to_string(&path).expect("read main.rs")
}

/// Byte offset of the first occurrence, or `None`.
fn offset_of(haystack: &str, needle: &str) -> Option<usize> {
    haystack.find(needle)
}

#[test]
fn wal_spill_is_constructed_after_the_metrics_recorder_is_installed() {
    let src = main_rs();

    let init_metrics = offset_of(&src, "observability::init_metrics(")
        .expect("main.rs must still install the Prometheus recorder");
    let spill_new = offset_of(&src, "WsFrameSpill::new(")
        .expect("main.rs must still construct the WAL spill writer");

    assert!(
        spill_new > init_metrics,
        "WsFrameSpill::new() is constructed BEFORE observability::init_metrics.\n\
         Its SpillDropCounters handles will resolve to the no-op recorder and stay \
         no-ops for the process lifetime, so tv_ticks_lost_total and \
         tv_ws_frame_spill_drop_critical can never be published and the \
         tv-<env>-ticks-lost-spill alarm can never fire.\n\
         Move the construction below init_metrics (see the dated note there)."
    );
}

#[test]
fn the_dated_note_explaining_the_ordering_survives() {
    let src = main_rs();
    assert!(
        src.contains("MOVED HERE 2026-08-26"),
        "the dated note explaining WHY the WAL init sits below init_metrics is gone; \
         without it the next reader will 'tidy' the block back up to STAGE-C and \
         silently kill the tick-loss counter again"
    );
}

/// The two sibling registrations that were already fixed for this hazard.
/// If either drifts back above the recorder install, the same class returns.
#[test]
fn the_sibling_post_install_registrations_stay_post_install() {
    let src = main_rs();
    let init_metrics =
        offset_of(&src, "observability::init_metrics(").expect("recorder install site");

    for needle in ["prewarm_dispatcher_counters("] {
        if let Some(at) = offset_of(&src, needle) {
            assert!(
                at > init_metrics,
                "{needle} moved ABOVE observability::init_metrics — handles created \
                 pre-install resolve to a no-op counter"
            );
        }
    }
}
