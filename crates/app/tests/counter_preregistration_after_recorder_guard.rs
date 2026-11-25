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

/// Every byte offset at which `needle` occurs.
fn offsets_of(haystack: &str, needle: &str) -> Vec<usize> {
    haystack.match_indices(needle).map(|(at, _)| at).collect()
}

/// `src` with every line comment blanked to spaces, so byte offsets are
/// unchanged and only real code is searched.
///
/// Needed because this file explains the hazard in PROSE, and that prose
/// names the constructor. Without this, a comment that merely mentions
/// `WsFrameSpill::new` above the recorder install fails the guard — a false
/// positive whose cheapest fix is deleting the explanation, which is exactly
/// the note the sibling test below exists to keep alive.
fn code_only(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.split_inclusive('\n') {
        match line.find("//") {
            Some(at) => {
                out.push_str(&line[..at]);
                for ch in line[at..].chars() {
                    if ch == '\n' {
                        out.push(ch);
                    } else {
                        // One space per BYTE, not per char, so a multi-byte
                        // character inside a comment cannot shift the offsets
                        // this function exists to preserve.
                        for _ in 0..ch.len_utf8() {
                            out.push(' ');
                        }
                    }
                }
            }
            None => out.push_str(line),
        }
    }
    out
}

#[test]
fn wal_spill_is_constructed_after_the_metrics_recorder_is_installed() {
    let src = main_rs();

    let init_metrics = offset_of(&src, "observability::init_metrics(")
        .expect("main.rs must still install the Prometheus recorder");

    // Matched on the TYPE + `::new` prefix rather than one exact spelling.
    // The first version of this guard looked for the literal
    // `WsFrameSpill::new(` and went GREEN-then-RED when the constructor was
    // renamed to `new_with_guard` for the WAL directory claim: the ordering
    // was still correct, but the needle no longer existed, so the guard was
    // failing on a spelling instead of on the hazard. A prefix match covers
    // every present and future constructor, and the count assertion below
    // stops the whole test passing vacuously if the type is renamed outright.
    let constructions = offsets_of(&code_only(&src), "WsFrameSpill::new");
    assert!(
        !constructions.is_empty(),
        "main.rs no longer constructs a WsFrameSpill at all. If the WAL writer \
         was renamed or removed, this guard must be re-pointed deliberately — \
         it protects the only counter that reports UNRECOVERABLE tick loss."
    );

    for at in constructions {
        assert!(
            at > init_metrics,
            "a WsFrameSpill constructor at byte {at} runs BEFORE \
             observability::init_metrics at byte {init_metrics}.\n\
             Its SpillDropCounters handles will resolve to the no-op recorder and stay \
             no-ops for the process lifetime, so tv_ticks_lost_total and \
             tv_ws_frame_spill_drop_critical can never be published and the \
             tv-<env>-ticks-lost-spill alarm can never fire.\n\
             Move the construction below init_metrics (see the dated note there)."
        );
    }
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

/// The comment-blanker is load-bearing — a bug in it either hides a real
/// pre-install construction or fails the build on prose. Both directions are
/// pinned here rather than assumed.
#[test]
fn the_comment_blanker_keeps_code_and_drops_prose_without_moving_offsets() {
    let src =
        "let a = WsFrameSpill::new_with_guard(x);\n// WsFrameSpill::new in a comment\nlet b = 1;\n";
    let code = code_only(src);

    assert_eq!(
        code.len(),
        src.len(),
        "offsets must not move — the whole test compares byte positions"
    );
    assert_eq!(
        offsets_of(&code, "WsFrameSpill::new").len(),
        1,
        "the commented mention must be blanked and the real call kept"
    );
    assert_eq!(
        offsets_of(&code, "WsFrameSpill::new")[0],
        offsets_of(src, "WsFrameSpill::new")[0],
        "the surviving call must sit at its original offset"
    );

    // A multi-byte character inside a comment is the case a naive
    // one-space-per-char blanker gets wrong, shifting every later offset.
    let wide = "let a = 1; // — an em dash\nlet b = WsFrameSpill::new(y);\n";
    let wide_code = code_only(wide);
    assert_eq!(
        wide_code.len(),
        wide.len(),
        "multi-byte comment shifted offsets"
    );
    assert_eq!(
        offsets_of(&wide_code, "WsFrameSpill::new")[0],
        offsets_of(wide, "WsFrameSpill::new")[0]
    );
}
