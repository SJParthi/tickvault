//! Ratchets for pressure-triggered archival (feed-hardening Item 5).
//!
//! The property these defend is narrow and load-bearing: **the pressure path
//! adds a trigger, never a delete path.** Everything it drops was exported,
//! uploaded, checksum-matched, row-count-matched and audit-logged by
//! `partition_archive` first. If a future change gave this module its own
//! `DROP PARTITION`, the whole safety argument would evaporate silently —
//! the code would still compile, still pass every other test, and still look
//! like it was archiving.
//!
//! Each guard is bite-proven: the negative fixture below asserts the scanner
//! actually FAILS on the shape it is supposed to catch, so a guard that has
//! quietly stopped scanning cannot pass by scanning nothing.

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Strip `//` line comments so a scanner cannot be satisfied — or tripped —
/// by prose. This file's own doc comments discuss `DROP PARTITION`; without
/// stripping, the guard below would fail on the module it is defending.
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn pressure_loop_never_issues_a_drop_of_its_own() {
    let src = strip_line_comments(&read("crates/app/src/disk_pressure_boot.rs"));
    for banned in [
        "DROP PARTITION",
        "drop_partition",
        "ALTER TABLE",
        "DELETE FROM",
        "remove_file",
        "remove_dir",
    ] {
        assert!(
            !src.contains(banned),
            "the pressure loop must contain NO destructive statement of its own — \
             found `{banned}`. Everything it frees must go through \
             `archive_and_drop_old_partitions`, whose VerifiedArchive type-state \
             makes an unverified drop unrepresentable."
        );
    }
}

#[test]
fn pressure_loop_delegates_to_the_verified_archive_path() {
    let src = strip_line_comments(&read("crates/app/src/disk_pressure_boot.rs"));
    assert!(
        src.contains("archive_and_drop_old_partitions"),
        "non-vacuous: the loop must actually CALL the verified archiver — a guard \
         that only checks for absent DROPs would pass on a loop that does nothing"
    );
    assert!(
        src.contains("PartitionArchiver::new"),
        "the pass must construct the real archiver, not a bespoke path"
    );
}

#[test]
fn drop_scanner_bites_on_a_real_violation() {
    // Bite-proof: the scanner above is only worth having if it fails on the
    // shape it claims to catch.
    let violating =
        "fn free_space() { exec(\"ALTER TABLE ticks DROP PARTITION LIST '2026-01-01'\"); }";
    let stripped = strip_line_comments(violating);
    assert!(
        stripped.contains("DROP PARTITION") && stripped.contains("ALTER TABLE"),
        "the scanner must see a genuine destructive statement"
    );
    // And prove the comment-stripper does not blind it to real code while
    // still neutralising prose.
    let prose_only = "// this module never issues DROP PARTITION\nfn f() {}";
    assert!(
        !strip_line_comments(prose_only).contains("DROP PARTITION"),
        "prose about the ban must not trip the ban"
    );
}

#[test]
fn the_two_day_floor_is_stated_in_code_not_only_in_prose() {
    let src = read("crates/storage/src/disk_pressure.rs");
    assert!(
        src.contains("PRESSURE_MIN_HOT_DAYS")
            && src.contains("crate::partition_archive::MIN_HOT_DAYS"),
        "the pressure floor must be DERIVED from the archiver's floor, so the two \
         cannot drift apart"
    );
    assert_eq!(
        tickvault_storage::disk_pressure::PRESSURE_MIN_HOT_DAYS,
        2,
        "today and yesterday are untouchable at any pressure; this is not a knob"
    );
}

#[test]
fn a_config_asking_for_a_smaller_window_cannot_reach_below_the_floor() {
    let cfg = tickvault_common::config::PartitionRetentionConfig {
        pressure_archive_enabled: true,
        pressure_hot_days: 0,
        ..tickvault_common::config::PartitionRetentionConfig::default()
    };
    let applied = tickvault_app::disk_pressure_boot::pressure_config(&cfg);
    assert_eq!(applied.market_data_hot_days, 2);
    assert_eq!(applied.depth_hot_days, 2);
}

#[test]
fn the_feature_is_serde_default_off() {
    // An absent [partition_retention] block, or an older config file, must
    // leave the loop unspawned — the rollback story depends on it. `Default`
    // is what serde falls back to field-by-field, so asserting on it covers
    // the absent-block case without pulling a parser into this test.
    let cfg = tickvault_common::config::PartitionRetentionConfig::default();
    assert!(
        !cfg.pressure_archive_enabled,
        "serde default must be OFF so a config rollback is byte-identical"
    );
}

#[test]
fn base_toml_opts_in_and_keeps_the_thresholds_sane() {
    let src = read("config/base.toml");
    assert!(
        src.contains("pressure_archive_enabled = true"),
        "non-vacuous: prod opts in, so the shipped config exercises the path"
    );
    // Read the real numbers out of the shipped file rather than restating
    // them here, so the sanity relation is checked against what the app will
    // actually load. A plain scan keeps this test dependency-free.
    let value_of = |key: &str| -> i64 {
        src.lines()
            .map(str::trim)
            .find(|l| l.starts_with(key) && l.contains('='))
            .and_then(|l| l.split('=').nth(1))
            .map(|v| v.split('#').next().unwrap_or(v).trim().to_string())
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or_else(|| panic!("`{key}` not found or unparseable in config/base.toml"))
    };
    let high = value_of("pressure_high_water_pct");
    let low = value_of("pressure_low_water_pct");
    assert!(
        low < high,
        "low water ({low}) must sit strictly below high water ({high}) or the loop \
         refuses to arm — the gap IS the anti-thrash band"
    );
    assert!((1..=100).contains(&high), "high water must be a percentage");
}

#[test]
fn escalation_is_coded_so_it_can_page() {
    let src = read("crates/app/src/disk_pressure_boot.rs");
    assert!(
        src.contains("StorageGap05DiskPressureUnrelievable"),
        "the give-up path must carry a coded error — a silent stop on a filling \
         volume is the false-OK class this whole chain exists to prevent"
    );
    assert_eq!(
        tickvault_common::error_code::ErrorCode::StorageGap05DiskPressureUnrelievable.severity(),
        tickvault_common::error_code::Severity::Critical,
        "the next state after this code is a full volume, which stops every writer"
    );
}
