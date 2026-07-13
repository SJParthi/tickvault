//! Ratchet guards for the QuestDB partition archive→verify→drop leg
//! (2026-07-13 disk-pressure remediation).
//!
//! Pins the four load-bearing invariants:
//! 1. `archive_enabled` defaults FALSE in code (the destructive leg must be
//!    explicitly configured on; deleting the key = instant rollback).
//! 2. `config/base.toml` explicitly enables it for the deployed box
//!    (retention that actually frees disk is the prod remediation).
//! 3. `main.rs` wires the archive cycle BEFORE the legacy detach cycle
//!    (a >90d partition must be archived+dropped, never detached-unarchived
//!    first) and KEEPS the detach cycle (its `total_detached=…` log lines
//!    are the CloudWatch evidence trail).
//! 4. The DESTRUCTIVE `DROP PARTITION` emit site exists ONLY behind the
//!    `VerifiedArchive` type-state proof (private-field struct — the
//!    compiler enforces archive-verify-before-drop; this scan keeps a
//!    refactor from adding a second, proof-free drop site).

use std::fs;
use std::path::{Path, PathBuf};

use tickvault_common::config::PartitionRetentionConfig;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read_workspace_file(rel: &str) -> String {
    let path = workspace_root().join(rel);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("must be able to read {}: {err}", path.display()))
}

#[test]
fn archive_enabled_default_is_false_in_code() {
    // The destructive leg is opt-in: a config with the key ABSENT must
    // behave exactly like today (detach-only). Kills `default -> true`
    // regressions and pins the instant-rollback contract.
    let cfg = PartitionRetentionConfig::default();
    assert!(
        !cfg.archive_enabled,
        "archive_enabled MUST default false — the archive→verify→drop leg is \
         explicitly configured on in config, never on by code default"
    );
    // The class windows keep their documented defaults.
    assert_eq!(cfg.retention_days, 90);
    assert_eq!(cfg.market_data_hot_days, 14);
}

#[test]
fn base_toml_enables_archive_with_market_data_window() {
    // The prod remediation: base.toml flips the gate ON and sets the
    // market-data hot window. (Deleting these keys is the documented
    // one-line rollback — this guard makes that deletion a CONSCIOUS act,
    // not silent drift.)
    let toml_text = read_workspace_file("config/base.toml");
    let parsed: toml::Table = toml::from_str(&toml_text).expect("base.toml must parse");
    let section = parsed
        .get("partition_retention")
        .expect("base.toml must carry [partition_retention]");
    assert_eq!(
        section
            .get("archive_enabled")
            .and_then(toml::Value::as_bool),
        Some(true),
        "base.toml must enable the archive→verify→drop leg (2026-07-13 \
         disk-pressure remediation) — flipping this to false is the \
         documented rollback and must be deliberate"
    );
    assert_eq!(
        section
            .get("market_data_hot_days")
            .and_then(toml::Value::as_integer),
        Some(14),
        "base.toml must pin the market-data hot window (ticks + candles_*)"
    );
    assert_eq!(
        section
            .get("retention_days")
            .and_then(toml::Value::as_integer),
        Some(90),
        "the standard-class 90d window must stay explicit in base.toml"
    );
}

#[test]
fn main_rs_wires_archive_before_detach_and_keeps_detach() {
    let main_rs = read_workspace_file("crates/app/src/main.rs");

    let archive_idx = main_rs
        .find("archive_and_drop_old_partitions")
        .expect("main.rs must run the partition archive cycle");
    let detach_idx = main_rs.find("detach_old_partitions").expect(
        "main.rs must KEEP the legacy detach cycle (its total_detached \
                 log lines are the CloudWatch evidence trail)",
    );
    assert!(
        archive_idx < detach_idx,
        "the archive cycle MUST run BEFORE the detach cycle — otherwise a \
         >retention_days partition gets DETACHED (renamed, unarchived, zero \
         bytes freed) before the archiver can export it to S3"
    );
    assert!(
        main_rs.contains("config.partition_retention.archive_enabled"),
        "the archive cycle must be gated on the archive_enabled config flag \
         (the instant-rollback contract)"
    );
}

#[test]
fn drop_partition_requires_verified_archive_proof() {
    // Type-state ratchet: `VerifiedArchive` carries a PRIVATE proof field
    // (constructible only by `verify_archive`), and the ONLY `DROP
    // PARTITION` DDL emit site in the storage crate is the fn that takes
    // that proof. A refactor adding a proof-free drop site fails here.
    let archive_src = read_workspace_file("crates/storage/src/partition_archive.rs");
    assert!(
        archive_src.contains("_proof: VerifyProof"),
        "VerifiedArchive must keep its private VerifyProof field — the \
         compile-time archive-verify-before-drop guarantee"
    );
    assert!(
        !archive_src.contains("pub struct VerifyProof"),
        "VerifyProof must stay module-private (a pub proof token would let \
         callers forge a VerifiedArchive)"
    );
    assert!(
        archive_src.contains("proof: &VerifiedArchive"),
        "the drop fn must take the VerifiedArchive proof"
    );

    // "DROP PARTITION" appears in partition_archive.rs ONLY inside
    // build_drop_sql (+ docs/tests) and NOWHERE else in storage src.
    let src_dir = workspace_root().join("crates/storage/src");
    for entry in fs::read_dir(&src_dir).expect("read storage src") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap_or_default();
        let is_archive_module = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "partition_archive.rs");
        if !is_archive_module {
            assert!(
                !text.contains("DROP PARTITION"),
                "{} contains a DROP PARTITION emit site outside \
                 partition_archive.rs — the destructive DDL must exist only \
                 behind the VerifiedArchive type-state proof",
                path.display()
            );
        }
    }
}

#[test]
fn archive_audit_table_has_a_retention_decision() {
    // The forensic table itself must be covered by the retention sweep
    // (the partition_retention_coverage_guard enforces the string literal;
    // this pins the DECISION: DAY-swept standard class, never exempt).
    let pm = read_workspace_file("crates/storage/src/partition_manager.rs");
    assert!(
        pm.contains("\"partition_archive_audit\""),
        "partition_archive_audit must appear in partition_manager.rs's \
         retention lists (DAY_PARTITIONED_TABLES)"
    );
}
