//! Ratchet guards for the QuestDB partition archive→verify→drop leg
//! (2026-07-13 disk-pressure remediation).
//!
//! Pins the five load-bearing invariants:
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
//! 5. (Review round 1, F4) the verify decision's INPUTS come from the REAL
//!    post-export recount + post-upload S3 HeadObject calls — a stubbed
//!    input would make the fail-closed verify vacuously true.

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
    // 2026-07-16: default raised 14 → 35 (operator one-month spot window).
    assert_eq!(cfg.market_data_hot_days, 35);
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
        Some(35),
        "base.toml must pin the market-data hot window (ticks + candles_* + \
         the per-minute chain tables — 35d since the 2026-07-16 operator \
         one-month directive)"
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
fn verify_inputs_come_from_real_recount_and_head_calls() {
    // F4 ratchet (review round 1): the fail-closed verify decision is only
    // as strong as its INPUTS. Pin that `process_one` feeds `verify_archive`
    // from a REAL post-export recount + a REAL post-upload S3 HeadObject —
    // a refactor stubbing either input (e.g. `Some(exported.rows)`) would
    // make the verify vacuously true and let a corrupt archive gate a drop.
    let archive_src = read_workspace_file("crates/storage/src/partition_archive.rs");
    let process_one = extract_fn_body(&archive_src, "async fn process_one");
    assert!(
        process_one.contains("self.recount_rows("),
        "process_one must obtain the recount from the real recount_rows() call"
    );
    assert!(
        process_one.contains("self.head_object_meta("),
        "process_one must obtain the S3 length + checksum from the real \
         head_object_meta() call"
    );
    // The two fetched values must be the ones handed to verify_archive —
    // pin the binding names appearing in the verify_archive call.
    let verify_call_idx = process_one
        .find("verify_archive(")
        .expect("process_one must call verify_archive");
    let verify_call = &process_one[verify_call_idx..];
    for needle in ["recount", "s3_len"] {
        assert!(
            verify_call.contains(needle),
            "verify_archive must be fed the fetched `{needle}` value, not a stub"
        );
    }
    // Round 2: the reuse decision must compare the STORED checksum against
    // the freshly exported digest (content identity, not just length).
    assert!(
        process_one.contains("checksum_sha256_b64"),
        "the reuse arm must consult the stored sha256 checksum"
    );
    assert!(
        process_one.contains("gzip_sha256_b64"),
        "the reuse arm must compare against the freshly exported gzip sha256"
    );
    // Vacuous-pass tripwires: verify inputs must never be self-satisfied.
    for forbidden in ["Some(exported.rows)", "Some(exported.gzip_bytes)"] {
        assert!(
            !process_one.contains(forbidden),
            "process_one must never feed verify_archive a self-satisfied \
             input ({forbidden}) — that makes the verify vacuous"
        );
    }
}

/// Extracts a fn body by brace matching from its signature marker (test-only
/// source scan; panics loudly on a missing fn so the ratchet never goes
/// silently vacuous).
fn extract_fn_body(src: &str, marker: &str) -> String {
    let start = src
        .find(marker)
        .unwrap_or_else(|| panic!("source must contain `{marker}`"));
    let open = src[start..]
        .find('{')
        .map(|i| start + i)
        .unwrap_or_else(|| panic!("`{marker}` must open a body"));
    let mut depth = 0usize;
    for (i, c) in src[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return src[start..=open + i].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("`{marker}` body never closed");
}

#[test]
fn extract_fn_body_self_test_is_not_vacuous() {
    // The F4 ratchet's own scanner must fail loudly on a missing fn and
    // must capture nested braces correctly — otherwise the ratchet could
    // silently scan the wrong region and pass vacuously.
    let sample = "fn a() { if x { y(); } }\nfn b() { z(); }";
    let body = extract_fn_body(sample, "fn a()");
    assert!(body.contains("y();"));
    assert!(
        !body.contains("z();"),
        "must stop at fn a()'s closing brace"
    );
    let result = std::panic::catch_unwind(|| extract_fn_body(sample, "fn missing()"));
    assert!(result.is_err(), "a missing marker must panic, never pass");
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
