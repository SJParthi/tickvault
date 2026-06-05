//! Meta-guard: every storage table-name constant MUST have a retention
//! decision — i.e. it must be covered by the partition manager's HOUR/DAY
//! sweep lists OR the EXEMPT list in `partition_manager.rs`.
//!
//! This prevents the 2026-06-05 stale-coverage bug from ever recurring: the
//! sweep lists had drifted to name DELETED tables while OMITTING every live
//! growing table, so nothing was swept (a storage/cost runaway). With this
//! guard, adding a new `*_audit`/data table with a `…TABLE…` constant but
//! forgetting to give it a retention decision fails the build.

use std::fs;
use std::path::Path;

/// Collect every lowercase snake-case string literal in a file. The three
/// retention lists (and the pinning unit tests) in `partition_manager.rs` name
/// each covered table, so a table is "covered" iff its name appears here.
fn string_literals(text: &str) -> Vec<String> {
    text.split('"')
        .enumerate()
        .filter(|(i, _)| i % 2 == 1) // odd segments are inside quotes
        .map(|(_, s)| s.to_string())
        .filter(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
        .collect()
}

/// Extract the VALUE of every `const …TABLE… : &str = "name"` declaration whose
/// value is a bare table name (no spaces — excludes DDL strings).
fn table_name_constants(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if !t.contains("const ") || !t.contains("TABLE") || !t.contains(": &str") {
            continue;
        }
        // value between the first `= "` and the next `"`
        if let Some(start) = t.find("= \"") {
            let rest = &t[start + 3..];
            if let Some(end) = rest.find('"') {
                let val = &rest[..end];
                if !val.is_empty()
                    && val
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                {
                    out.push(val.to_string());
                }
            }
        }
    }
    out
}

#[test]
fn every_storage_table_constant_has_a_retention_decision() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let pm =
        fs::read_to_string(src.join("partition_manager.rs")).expect("read partition_manager.rs");
    let covered = string_literals(&pm);

    let mut missing: Vec<String> = Vec::new();
    for entry in fs::read_dir(&src).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap_or_default();
        for tbl in table_name_constants(&text) {
            if !covered.contains(&tbl) {
                missing.push(format!("{tbl}  (constant in {})", path.display()));
            }
        }
    }

    assert!(
        missing.is_empty(),
        "these storage table-name constants have NO retention decision — add each \
         to HOUR_PARTITIONED_TABLES / DAY_PARTITIONED_TABLES or \
         RETENTION_EXEMPT_TABLES in partition_manager.rs:\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn guard_helpers_are_sane() {
    // self-test the extractors so a future refactor can't silently neuter them
    let sample = r#"
        pub const FOO_TABLE: &str = "foo_audit";
        const BAR_DDL: &str = "CREATE TABLE x ( a int )";
        let xs = &["foo_audit", "kept"];
    "#;
    assert_eq!(table_name_constants(sample), vec!["foo_audit".to_string()]);
    assert!(string_literals(sample).contains(&"foo_audit".to_string()));
    assert!(string_literals(sample).contains(&"kept".to_string()));
    // the DDL string (has spaces/uppercase) must NOT be treated as a table name
    assert!(!table_name_constants(sample).iter().any(|t| t.contains(' ')));
}

/// The 21 live candle tables are swept by iterating `candle_table_names()` (the
/// single source of truth) in `detach_old_partitions`. This cross-references
/// that source — closing the gap where #1022's meta-guard was blind to candle
/// tables (it only saw literals inside `partition_manager.rs`). Catches both
/// (a) deletion of the candle sweep loop and (b) a future `_shadow` rename.
#[test]
fn candle_tables_are_swept_via_single_source() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let pm =
        fs::read_to_string(src.join("partition_manager.rs")).expect("read partition_manager.rs");
    assert!(
        pm.contains("candle_table_names()"),
        "partition_manager must sweep candle tables via candle_table_names() — the candle \
         retention loop is missing (the #1022 phantom-`_shadow`-name bug would return)"
    );

    let names = tickvault_storage::shadow_persistence::candle_table_names();
    assert_eq!(names.len(), 21, "expected 21 live candle tables");
    for n in names {
        assert!(
            n.starts_with("candles_") && !n.contains("_shadow"),
            "candle table name must be plain candles_<TF> (no _shadow): {n}"
        );
    }
}
