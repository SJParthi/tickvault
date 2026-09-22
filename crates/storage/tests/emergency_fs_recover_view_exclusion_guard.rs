//! The emergency filesystem-recovery workflow must never try to `DROP TABLE`
//! a console VIEW.
//!
//! `.github/workflows/emergency-fs-recover.yml` step 4 first drops the four
//! console views with `DROP VIEW IF EXISTS`, then builds its table-drop list
//! (`TARGETS`) from `tables()` with an awk filter that takes every name
//! starting `candles_`. Two views also start `candles_` — `candles_named` and
//! the derived `candles_10m` — so the filter carries explicit
//! `$0!="candles_10m" && $0!="candles_named"` exclusions.
//!
//! If those exclusions are removed, a view that survived the `DROP VIEW` pass
//! (or reappears in `tables()` output) lands in `TARGETS`, `DROP TABLE` on a
//! view is refused, the retry is refused again, and the step sets `failed=1`
//! and exits non-zero — a recovery run on a full disk reported as FAILED for
//! a reason that has nothing to do with the disk. Nothing pinned the
//! exclusion until this guard.
//!
//! The expectation is DERIVED from the view-name constants in
//! `console_views`, not hand-copied: a new `candles_*` view added there fails
//! this test until the workflow excludes it too.

use std::path::PathBuf;

use tickvault_storage::console_views::{
    VIEW_CANDLES_10M, VIEW_CANDLES_NAMED, VIEW_DEPTH_NAMED, VIEW_TICKS_NAMED,
};

/// Every console view the recovery step must drop as a VIEW.
const CONSOLE_VIEWS: [&str; 4] = [
    VIEW_TICKS_NAMED,
    VIEW_CANDLES_NAMED,
    VIEW_CANDLES_10M,
    VIEW_DEPTH_NAMED,
];

/// The prefix the awk filter uses to pick candle tables.
const CANDLE_PREFIX: &str = "candles_";

fn workflow_text() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.github/workflows/emergency-fs-recover.yml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Returns the single `TARGETS=` line of the recovery script.
fn targets_line(text: &str) -> &str {
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.trim_start().starts_with("TARGETS="))
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "expected exactly ONE `TARGETS=` line in the recovery script, found {}",
        lines.len()
    );
    lines[0]
}

/// Returns the `for v in ...; do ... DROP VIEW` line.
fn drop_view_line(text: &str) -> &str {
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.trim_start().starts_with("for v in ") && l.contains("DROP VIEW"))
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "expected exactly ONE `for v in ... DROP VIEW` loop, found {}",
        lines.len()
    );
    lines[0]
}

/// The views a `TARGETS` line fails to exclude, given the views it must.
fn unexcluded_candle_views<'a>(targets: &str, views: &[&'a str]) -> Vec<&'a str> {
    views
        .iter()
        .copied()
        .filter(|v| v.starts_with(CANDLE_PREFIX))
        .filter(|v| !targets.contains(&format!("$0!=\"{v}\"")))
        .collect()
}

#[test]
fn the_table_drop_list_excludes_every_candles_prefixed_view() {
    let text = workflow_text();
    let targets = targets_line(&text);

    // The inclusion rule this guard exists for must still be there — if the
    // prefix match is gone the exclusion question changes shape, and this
    // test must be re-derived rather than pass on a different filter.
    assert!(
        targets.contains("index($0,\"candles_\")==1"),
        "the TARGETS filter no longer selects tables by the `candles_` prefix; \
         re-derive this guard. Line: {targets}"
    );

    let missing = unexcluded_candle_views(targets, &CONSOLE_VIEWS);
    assert!(
        missing.is_empty(),
        "emergency-fs-recover.yml would try to DROP TABLE these console VIEWS \
         (they match the `candles_` prefix but are not excluded): {missing:?}. \
         A refused drop sets failed=1 and fails the recovery run. Line: {targets}"
    );
}

#[test]
fn every_console_view_is_dropped_as_a_view_first() {
    let text = workflow_text();
    let line = drop_view_line(&text);
    for view in CONSOLE_VIEWS {
        assert!(
            line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .any(|w| w == view),
            "`{view}` is not in the recovery step's DROP VIEW loop — a view left \
             in place blocks dropping the table it reads. Line: {line}"
        );
    }
}

/// Bite-proof of the detector itself: a `TARGETS` line with the exclusions
/// stripped must be reported, and the real shape must not be.
#[test]
fn guard_self_test() {
    let stripped = r#"TARGETS=$(printf '%s\n' "$ALL" | awk '$0=="ticks" || (index($0,"candles_")==1)' | sort)"#;
    assert_eq!(
        unexcluded_candle_views(stripped, &CONSOLE_VIEWS),
        vec![VIEW_CANDLES_NAMED, VIEW_CANDLES_10M],
        "the detector must flag both candles_* views when no exclusion exists"
    );

    let good =
        r#"TARGETS=$(awk '(index($0,"candles_")==1 && $0!="candles_10m" && $0!="candles_named")')"#;
    assert!(
        unexcluded_candle_views(good, &CONSOLE_VIEWS).is_empty(),
        "the detector must accept the correct exclusion shape"
    );

    // Non-candle views never need an awk exclusion.
    assert!(!VIEW_TICKS_NAMED.starts_with(CANDLE_PREFIX));
    assert!(!VIEW_DEPTH_NAMED.starts_with(CANDLE_PREFIX));
}
