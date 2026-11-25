//! The emergency filesystem-recovery workflow drops NO table and NO view, and
//! deletes none of the raw capture, spill or dead-letter data.
//!
//! ⚠ CHANGED 2026-10-09 (stress audit RO-1). Until that day
//! `.github/workflows/emergency-fs-recover.yml` step 4 dropped the retired
//! console views with `DROP VIEW IF EXISTS`, then built a table-drop list
//! (`TARGETS`) from `tables()` with an awk filter and dropped `ticks`,
//! `market_depth` and every `candles_<tf>`, and step 2 ran `rm -rf` on the
//! WAL, spill and DLQ folders. This file pinned the shape of that drop list
//! (that a `candles_*` VIEW was excluded from `TARGETS`, and that a view that
//! became a table was not). Quotes 21/22 that authorized the wipe are spent,
//! and Quote 28 (2026-09-29) forbids deleting captured market data without a
//! verified copy, so the drops and deletions were removed. The guard now pins
//! their ABSENCE. Retired console views need no workflow step: the app's boot
//! sweep (`console_views`, `RETIRED_CONSOLE_VIEWS`) drops them.
//!
//! The file keeps its name so its history stays in one place.

use std::path::PathBuf;

use tickvault_storage::console_views::{RETIRED_CONSOLE_VIEWS, VIEW_NAMES_NOW_TABLES};

/// The raw-data folders under `/opt/tickvault/data` the recovery must keep.
const KEPT_DATA_DIRS: [&str; 4] = ["ws_wal", "spill", "dlq", "spill-hold"];

fn workflow_text() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.github/workflows/emergency-fs-recover.yml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Non-comment lines of `text` (YAML and shell comments start with `#`).
fn code_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect()
}

/// Lines that drop or truncate a table or a view, or build a drop list.
fn drop_lines(text: &str) -> Vec<String> {
    code_lines(text)
        .into_iter()
        .filter(|l| {
            let u = l.to_ascii_uppercase();
            u.contains("DROP TABLE")
                || u.contains("DROP VIEW")
                || u.contains("TRUNCATE TABLE")
                || l.trim_start().starts_with("TARGETS=")
        })
        .map(str::to_string)
        .collect()
}

/// Lines that run `rm` with flags on one of [`KEPT_DATA_DIRS`].
fn raw_data_rm_lines(text: &str) -> Vec<String> {
    code_lines(text)
        .into_iter()
        .filter(|l| {
            let words: Vec<&str> = l.split_whitespace().collect();
            let rm = words
                .windows(2)
                .any(|w| w[0] == "rm" && w[1].starts_with('-'));
            rm && KEPT_DATA_DIRS.iter().any(|d| l.contains(d))
        })
        .map(str::to_string)
        .collect()
}

// Regression: 2026-10-09 — the recovery dropped ticks, market_depth and every
// candles_<tf> with no verified S3 copy.
#[test]
fn test_regression_the_recovery_script_drops_no_table() {
    let text = workflow_text();
    let hits: Vec<String> = drop_lines(&text)
        .into_iter()
        .filter(|l| !l.to_ascii_uppercase().contains("DROP VIEW"))
        .collect();
    assert!(
        hits.is_empty(),
        "emergency-fs-recover.yml drops market-data tables again; Quote 28 forbids \
         it without a verified copy: {hits:#?}"
    );
}

/// No view is dropped by the workflow either: the app's boot sweep drops the
/// retired console views, and a recovery that only extends the filesystem has
/// no reason to touch the database schema.
#[test]
fn the_recovery_script_drops_no_view_and_the_boot_sweep_covers_them() {
    let text = workflow_text();
    let hits = drop_lines(&text);
    assert!(
        hits.is_empty(),
        "emergency-fs-recover.yml drops a view or builds a drop list: {hits:#?}"
    );
    // The views the workflow used to drop are all still on the boot sweep's
    // list (or are real tables now), so nothing is left behind.
    for v in [
        "ticks_named",
        "candles_named",
        "market_depth_named",
        "candles_10m",
    ] {
        assert!(
            RETIRED_CONSOLE_VIEWS.contains(&v) || VIEW_NAMES_NOW_TABLES.contains(&v),
            "`{v}` is neither swept at boot nor a table now"
        );
    }
}

// Regression: 2026-10-09 — the recovery ran `rm -rf` on the WAL, spill and
// DLQ folders with no verified S3 copy.
#[test]
fn test_regression_the_recovery_script_keeps_the_wal_spill_and_dlq() {
    let text = workflow_text();
    let hits = raw_data_rm_lines(&text);
    assert!(
        hits.is_empty(),
        "emergency-fs-recover.yml deletes raw capture, spill or dead-letter data: \
         {hits:#?}"
    );
    assert!(
        code_lines(&text).iter().any(|l| l.contains("growpart")),
        "anti-vacuity: the recovery must still grow the partition"
    );
}

/// Bite-proof of the detectors themselves.
#[test]
fn guard_self_test() {
    let old = r#"
          sudo rm -rf /opt/tickvault/data/spill /opt/tickvault/data/dlq /opt/tickvault/data/ws_wal
          sudo rm -f /opt/tickvault/data/spill-hold/*.ilp
          for v in ticks_named candles_named; do echo "$(q "DROP VIEW IF EXISTS $v")"; done
          TARGETS=$(printf '%s\n' "$ALL" | sort)
            R=$(q "DROP TABLE IF EXISTS $t")
"#;
    assert_eq!(raw_data_rm_lines(old).len(), 2);
    assert_eq!(drop_lines(old).len(), 3);

    let new = r#"
          # sudo rm -rf /opt/tickvault/data/ws_wal (history, a comment)
          sudo du -sBG /opt/tickvault/data/spill /opt/tickvault/data/ws_wal
          sudo journalctl --vacuum-size=200M || true
"#;
    assert!(raw_data_rm_lines(new).is_empty());
    assert!(drop_lines(new).is_empty());
}
