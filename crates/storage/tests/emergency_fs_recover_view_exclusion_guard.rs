//! The emergency filesystem-recovery workflow must never try to `DROP TABLE`
//! a console VIEW — and must never SKIP a candle table that used to be a view.
//!
//! `.github/workflows/emergency-fs-recover.yml` step 4 first drops the retired
//! console views with `DROP VIEW IF EXISTS`, then builds its table-drop list
//! (`TARGETS`) from `tables()` with an awk filter that takes every name
//! starting `candles_`. One retired view also starts `candles_` —
//! `candles_named` — so the filter carries an explicit `$0!="candles_named"`
//! exclusion. If it is removed, a view that survived the `DROP VIEW` pass
//! lands in `TARGETS`, `DROP TABLE` on a view is refused, and the step sets
//! `failed=1` — a recovery run reported as FAILED for a reason that has
//! nothing to do with the disk.
//!
//! The OTHER direction arrived on 2026-09-22 (the operator's "no views"
//! directive): `candles_10m` stopped being a view and became a real folded
//! table. A leftover `$0!="candles_10m"` exclusion would then silently leave a
//! real candle table out of the wipe — the recovery reports success while one
//! table's rows survive. So names in `VIEW_NAMES_NOW_TABLES` must NOT be
//! excluded.
//!
//! Every expectation is DERIVED from `console_views`, never hand-copied: a
//! new retired `candles_*` view fails this test until the workflow excludes
//! it, and a view that becomes a table fails it until the exclusion goes.

use std::path::PathBuf;

use tickvault_storage::console_views::{RETIRED_CONSOLE_VIEWS, VIEW_NAMES_NOW_TABLES};

/// The prefix the awk filter uses to pick candle tables.
const CANDLE_PREFIX: &str = "candles_";

/// Prefixes of the tables the recovery step wipes. A retired view whose name
/// starts with one of these read one of those tables, so it must be dropped
/// as a VIEW before the table drops run.
const WIPED_TABLE_PREFIXES: [&str; 3] = ["ticks", "candles_", "market_depth"];

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

fn is_now_a_table(name: &str) -> bool {
    VIEW_NAMES_NOW_TABLES.contains(&name)
}

/// Retired `candles_*` views that are still views (never became a table).
fn candle_views_still_views() -> Vec<&'static str> {
    RETIRED_CONSOLE_VIEWS
        .iter()
        .copied()
        .filter(|v| v.starts_with(CANDLE_PREFIX) && !is_now_a_table(v))
        .collect()
}

/// Retired views that read a table this workflow wipes.
fn views_over_wiped_tables() -> Vec<&'static str> {
    RETIRED_CONSOLE_VIEWS
        .iter()
        .copied()
        .filter(|v| WIPED_TABLE_PREFIXES.iter().any(|p| v.starts_with(p)))
        .collect()
}

fn excludes(targets: &str, name: &str) -> bool {
    targets.contains(&format!("$0!=\"{name}\""))
}

/// The views a `TARGETS` line fails to exclude, given the views it must.
fn unexcluded<'a>(targets: &str, views: &[&'a str]) -> Vec<&'a str> {
    views
        .iter()
        .copied()
        .filter(|v| !excludes(targets, v))
        .collect()
}

/// Names that are real tables now but are still (wrongly) excluded.
fn wrongly_excluded<'a>(targets: &str, tables: &[&'a str]) -> Vec<&'a str> {
    tables
        .iter()
        .copied()
        .filter(|t| excludes(targets, t))
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

    let views = candle_views_still_views();
    assert!(
        !views.is_empty(),
        "anti-vacuity: at least one retired candles_* view must remain to test"
    );
    let missing = unexcluded(targets, &views);
    assert!(
        missing.is_empty(),
        "emergency-fs-recover.yml would try to DROP TABLE these console VIEWS \
         (they match the `candles_` prefix but are not excluded): {missing:?}. \
         A refused drop sets failed=1 and fails the recovery run. Line: {targets}"
    );
}

#[test]
fn a_view_name_that_became_a_table_is_wiped_not_excluded() {
    let text = workflow_text();
    let targets = targets_line(&text);
    let wrong = wrongly_excluded(targets, &VIEW_NAMES_NOW_TABLES);
    assert!(
        wrong.is_empty(),
        "emergency-fs-recover.yml still EXCLUDES {wrong:?} from the table wipe, \
         but these are real candle tables now (2026-09-22, no views). The \
         recovery would report success while their rows survive. Line: {targets}"
    );
}

#[test]
fn every_view_over_a_wiped_table_is_dropped_as_a_view_first() {
    let text = workflow_text();
    let line = drop_view_line(&text);
    let views = views_over_wiped_tables();
    assert!(
        !views.is_empty(),
        "anti-vacuity: the derived view set must not be empty"
    );
    for view in views {
        assert!(
            line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .any(|w| w == view),
            "`{view}` is not in the recovery step's DROP VIEW loop — a view left \
             in place blocks dropping the table it reads. Line: {line}"
        );
    }
}

/// Bite-proof of the detectors themselves.
#[test]
fn guard_self_test() {
    let views = candle_views_still_views();
    assert_eq!(views, vec!["candles_named"]);

    let stripped = r#"TARGETS=$(printf '%s\n' "$ALL" | awk '$0=="ticks" || (index($0,"candles_")==1)' | sort)"#;
    assert_eq!(
        unexcluded(stripped, &views),
        vec!["candles_named"],
        "the detector must flag a candles_* view when no exclusion exists"
    );
    assert!(wrongly_excluded(stripped, &VIEW_NAMES_NOW_TABLES).is_empty());

    let stale =
        r#"TARGETS=$(awk '(index($0,"candles_")==1 && $0!="candles_10m" && $0!="candles_named")')"#;
    assert!(unexcluded(stale, &views).is_empty());
    assert_eq!(
        wrongly_excluded(stale, &VIEW_NAMES_NOW_TABLES),
        vec!["candles_10m"],
        "the detector must flag the stale candles_10m exclusion"
    );

    let good = r#"TARGETS=$(awk '(index($0,"candles_")==1 && $0!="candles_named")')"#;
    assert!(unexcluded(good, &views).is_empty());
    assert!(wrongly_excluded(good, &VIEW_NAMES_NOW_TABLES).is_empty());

    // The drop-loop set covers the four table-reading views and excludes the
    // top_volume_rank_* views, which read no table this workflow wipes.
    let over = views_over_wiped_tables();
    for v in [
        "ticks_named",
        "candles_named",
        "candles_10m",
        "market_depth_named",
    ] {
        assert!(over.contains(&v), "{v} must be in the drop-loop set");
    }
    assert!(over.iter().all(|v| !v.starts_with("top_volume")));
}
