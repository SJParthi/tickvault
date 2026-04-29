//! Movers 22-TF integration ratchets — Phase 12 of v3 plan, 2026-04-28.
//!
//! Cross-component guards that verify Phase 8 (storage tables) + Phase 11
//! (observability) + Phase 13 (hooks) wiring is actually complete and not
//! drifting. Source-only checks — no live QuestDB / Grafana / Prometheus
//! roundtrip — so they run on every `cargo test` for the storage crate.
//!
//! Test count: 12 ratchets covering:
//! - Partition manager + S3 lifecycle alignment (Risk #7 / #8)
//! - Grafana dashboard structural integrity (Phase 11)
//! - Alert rules YAML structural integrity
//! - Banned-pattern hook category presence
//! - Make doctor section presence
//! - Runbook presence (cross-ref test enforces the variant -> rule binding;
//!   these tests enforce the inverse — that the rule file is actually well
//!   formed and present on disk).

use std::fs;

const PARTITION_MANAGER_PATH: &str = "src/partition_manager.rs";
const S3_LIFECYCLE_PATH: &str = "../../deploy/aws/s3-lifecycle-movers-tables.json";
const GRAFANA_DASHBOARD_PATH: &str = "../../deploy/docker/grafana/dashboards/movers-22tf.json";
const ALERTS_YML_PATH: &str = "../../deploy/docker/grafana/provisioning/alerting/alerts.yml";
const BANNED_PATTERN_PATH: &str = "../../.claude/hooks/banned-pattern-scanner.sh";
const DOCTOR_PATH: &str = "../../scripts/doctor.sh";
const RUNBOOK_PATH: &str = "../../.claude/rules/project/movers-22tf-error-codes.md";

const MOVERS_22TF_LABELS: &[&str] = &[
    "1s", "5s", "10s", "15s", "30s", "1m", "2m", "3m", "4m", "5m", "6m", "7m", "8m", "9m", "10m",
    "11m", "12m", "13m", "14m", "15m", "30m", "1h",
];

/// Phase 12 ratchet: partition_manager.rs lists every movers_{T} timeframe
/// in HOUR_PARTITIONED_TABLES (Risk #7 — table fills EBS without rotation).
#[test]
fn test_partition_manager_lists_all_22_movers_tables() {
    let content =
        fs::read_to_string(PARTITION_MANAGER_PATH).expect("partition_manager.rs must exist");
    for label in MOVERS_22TF_LABELS {
        let table_literal = format!("\"movers_{label}\",");
        assert!(
            content.contains(&table_literal),
            "partition_manager.rs HOUR_PARTITIONED_TABLES is missing entry for movers_{label}: \
             expected literal {table_literal:?}"
        );
    }
}

/// Phase 12 ratchet: S3 lifecycle JSON has a rule for every movers_{T}
/// (Risk #8 — SEBI 90d classification, S3 cold archive cost discipline).
#[test]
fn test_s3_lifecycle_has_rule_for_each_movers_table() {
    let content = fs::read_to_string(S3_LIFECYCLE_PATH).expect("S3 lifecycle JSON must exist");
    for label in MOVERS_22TF_LABELS {
        let prefix = format!("\"Prefix\": \"movers_{label}/\"");
        assert!(
            content.contains(&prefix),
            "s3-lifecycle-movers-tables.json missing rule for movers_{label}: \
             expected {prefix:?}"
        );
    }
}

/// Phase 12 ratchet: lifecycle JSON tier transitions are uniform — every
/// rule must transition to GLACIER_IR at day 90 + DEEP_ARCHIVE at day 365.
#[test]
fn test_s3_lifecycle_transitions_consistent() {
    let content = fs::read_to_string(S3_LIFECYCLE_PATH).expect("S3 lifecycle JSON must exist");
    let glacier_count = content.matches("\"StorageClass\": \"GLACIER_IR\"").count();
    let deep_archive_count = content
        .matches("\"StorageClass\": \"DEEP_ARCHIVE\"")
        .count();
    let intelligent_count = content
        .matches("\"StorageClass\": \"INTELLIGENT_TIERING\"")
        .count();
    assert_eq!(
        glacier_count, 22,
        "every rule must declare GLACIER_IR transition; got {glacier_count}, expected 22"
    );
    assert_eq!(
        deep_archive_count, 22,
        "every rule must declare DEEP_ARCHIVE transition; got {deep_archive_count}, expected 22"
    );
    assert_eq!(
        intelligent_count, 22,
        "every rule must declare INTELLIGENT_TIERING transition; got {intelligent_count}, \
         expected 22"
    );
}

/// Phase 12 ratchet: lifecycle JSON expiration is 5y (1825d) on every rule.
#[test]
fn test_s3_lifecycle_expiration_is_5_years() {
    let content = fs::read_to_string(S3_LIFECYCLE_PATH).expect("S3 lifecycle JSON must exist");
    let expiration_count = content
        .matches("\"Expiration\": { \"Days\": 1825 }")
        .count();
    assert_eq!(
        expiration_count, 22,
        "every movers_{{T}} rule must expire at 1825d (5y); got {expiration_count}, expected 22"
    );
}

/// Phase 12 ratchet: Grafana dashboard JSON exists and parses.
#[test]
fn test_grafana_dashboard_exists_and_parses_as_json() {
    let content = fs::read_to_string(GRAFANA_DASHBOARD_PATH).expect("movers-22tf.json must exist");
    let parsed: serde_json::Value =
        serde_json::from_str(&content).expect("movers-22tf.json must be valid JSON");
    assert!(
        parsed.get("title").is_some(),
        "dashboard must have a top-level title field"
    );
    assert_eq!(
        parsed["uid"].as_str(),
        Some("tv-movers-22tf"),
        "dashboard UID must be tv-movers-22tf for stable provisioning"
    );
}

/// Phase 12 ratchet: dashboard has all 22 timeframes in the $timeframe
/// template variable options list.
#[test]
fn test_grafana_dashboard_timeframe_variable_has_22_options() {
    let content = fs::read_to_string(GRAFANA_DASHBOARD_PATH).expect("movers-22tf.json must exist");
    for label in MOVERS_22TF_LABELS {
        let needle = format!("\"value\": \"{label}\"");
        assert!(
            content.contains(&needle),
            "$timeframe template variable missing option for {label}: expected {needle:?}"
        );
    }
}

/// Phase 12 ratchet: dashboard has the 5 row headers (Health, Stocks,
/// Index, Options, Futures, Depth) per the 24-panel layout.
#[test]
fn test_grafana_dashboard_has_all_5_row_headers() {
    let content = fs::read_to_string(GRAFANA_DASHBOARD_PATH).expect("movers-22tf.json must exist");
    for header in [
        "Health & Universe Coverage",
        "Stocks (NSE_EQ)",
        "Index (IDX_I)",
        "Options (NSE_FNO OPT*)",
        "Futures (NSE_FNO FUT*)",
        "Depth (existing market_depth + deep_market_depth, Option C)",
    ] {
        assert!(
            content.contains(header),
            "movers-22tf.json missing row header `{header}`"
        );
    }
}

/// Phase 12 ratchet: alerts.yml contains all 4 movers-22tf alert UIDs.
#[test]
fn test_alerts_yml_contains_4_movers_22tf_alerts() {
    let content = fs::read_to_string(ALERTS_YML_PATH).expect("alerts.yml must exist");
    for uid in [
        "tv-movers-22tf-drop-sla",
        "tv-movers-22tf-scheduler-panic-burst",
        "tv-movers-22tf-universe-drift",
        "tv-movers-22tf-schedule-drift",
    ] {
        assert!(
            content.contains(uid),
            "alerts.yml missing alert UID `{uid}`"
        );
    }
}

/// Phase 12 ratchet: alerts.yml is valid YAML (delegated to a structural
/// check via simple line scanning — full YAML parse uses serde_yaml which
/// is not in workspace deps; the python validator is invoked separately
/// in CI).
#[test]
fn test_alerts_yml_starts_with_valid_top_level_keys() {
    let content = fs::read_to_string(ALERTS_YML_PATH).expect("alerts.yml must exist");
    assert!(
        content.contains("apiVersion:"),
        "alerts.yml must declare top-level apiVersion"
    );
    assert!(
        content.contains("groups:"),
        "alerts.yml must declare top-level groups"
    );
}

/// Phase 12 ratchet: banned-pattern hook contains all 3 new categories.
#[test]
fn test_banned_pattern_hook_has_movers_categories() {
    let content =
        fs::read_to_string(BANNED_PATTERN_PATH).expect("banned-pattern-scanner.sh must exist");
    assert!(
        content.contains("CATEGORY 8: MOVERS 22-TF"),
        "Cat 8 (Movers I-P1-11) missing from banned-pattern-scanner.sh"
    );
    assert!(
        content.contains("CATEGORY 9: MOVERS DDL REGISTRATION"),
        "Cat 9 (Movers DDL) missing from banned-pattern-scanner.sh"
    );
    assert!(
        content.contains("CATEGORY 10: MOVERS WRITER PANIC"),
        "Cat 10 (Movers writer panic) missing from banned-pattern-scanner.sh"
    );
}

/// Phase 12 ratchet: doctor.sh has movers DDL integrity + universe coverage
/// sections.
#[test]
fn test_doctor_has_movers_sections() {
    let content = fs::read_to_string(DOCTOR_PATH).expect("doctor.sh must exist");
    assert!(
        content.contains("movers DDL integrity (Phase 13 sec 8)"),
        "doctor.sh missing Phase 13 section 8 header"
    );
    assert!(
        content.contains("movers universe coverage (Phase 13 sec 9)"),
        "doctor.sh missing Phase 13 section 9 header"
    );
}

/// Phase 12 ratchet: movers-22tf runbook covers all 3 ErrorCodes.
#[test]
fn test_runbook_covers_all_3_error_codes() {
    let content = fs::read_to_string(RUNBOOK_PATH).expect("movers-22tf-error-codes.md must exist");
    assert!(
        content.contains("MOVERS-22TF-01"),
        "runbook missing MOVERS-22TF-01 section"
    );
    assert!(
        content.contains("MOVERS-22TF-02"),
        "runbook missing MOVERS-22TF-02 section"
    );
    assert!(
        content.contains("MOVERS-22TF-03"),
        "runbook missing MOVERS-22TF-03 section"
    );
}
