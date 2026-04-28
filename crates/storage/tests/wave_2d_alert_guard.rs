//! Wave-2-D Fix 3 — Prometheus alert guard for the 3 missing alerts.
//!
//! The Wave-2-D bug-hunt agent flagged 3 missing alerts (HIGH severity):
//! the foundation PR shipped audit / S3 / tick-gap metrics + ErrorCodes
//! but no matching alert rules in
//! `deploy/docker/grafana/provisioning/alerting/alerts.yml`. Operator
//! got ZERO Telegram on AUDIT-01..06 / STORAGE-GAP-04 / WS-GAP-06 storm.
//!
//! This guard pins all three. Future deletion of any rule fails the
//! build. Pattern matches existing
//! `crates/storage/tests/resilience_sla_alert_guard.rs`.

use std::fs;
use std::path::{Path, PathBuf};

const GRAFANA_ALERTS_YAML: &str = "deploy/docker/grafana/provisioning/alerting/alerts.yml";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read_alerts_yaml() -> String {
    let path = workspace_root().join(GRAFANA_ALERTS_YAML);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("must be able to read {}: {err}", path.display()))
}

#[test]
fn audit_write_failure_alert_is_pinned() {
    let yaml = read_alerts_yaml();
    assert!(
        yaml.contains("uid: tv-audit-write-failure"),
        "Wave-2-D Fix 3 regression: alert `tv-audit-write-failure` MUST \
         exist in alerts.yml. Without it the operator gets ZERO Telegram \
         when phase2_audit / depth_rebalance_audit / ws_reconnect_audit \
         / boot_audit / selftest_audit / order_audit cannot persist."
    );
    assert!(
        yaml.contains("rate(tv_audit_write_failures_total[5m])"),
        "Wave-2-D Fix 3 regression: tv-audit-write-failure expression \
         must be `rate(tv_audit_write_failures_total[5m])`."
    );
}

#[test]
fn s3_archive_failure_alert_is_pinned() {
    let yaml = read_alerts_yaml();
    assert!(
        yaml.contains("uid: tv-s3-archive-failure"),
        "Wave-2-D Fix 3 regression: alert `tv-s3-archive-failure` MUST \
         exist in alerts.yml. SEBI 5y retention for order_audit \
         partitions depends on S3 archive succeeding."
    );
    assert!(
        yaml.contains("increase(tv_s3_archive_errors_total[1h])"),
        "Wave-2-D Fix 3 regression: tv-s3-archive-failure expression \
         must be `increase(tv_s3_archive_errors_total[1h])`."
    );
}

#[test]
fn tick_gap_coalesce_storm_alert_is_pinned() {
    let yaml = read_alerts_yaml();
    assert!(
        yaml.contains("uid: tv-tick-gap-coalesce-storm"),
        "Wave-2-D Fix 3 regression: alert `tv-tick-gap-coalesce-storm` \
         MUST exist in alerts.yml. > 100/min coalesce summaries \
         indicates universe-wide silence (Dhan outage) or a \
         mis-spawned coalesce loop."
    );
    assert!(
        yaml.contains("rate(tv_tick_gap_summary_total[1m])"),
        "Wave-2-D Fix 3 regression: tv-tick-gap-coalesce-storm \
         expression must be `rate(tv_tick_gap_summary_total[1m])`."
    );
}

#[test]
fn all_three_wave_2d_alerts_have_severity_label() {
    let yaml = read_alerts_yaml();
    // Every Wave-2-D alert must have `wave: "2-d"` label so dashboards
    // can group/filter them. Counter-test against accidental deletion.
    let count = yaml.matches("wave: \"2-d\"").count();
    assert_eq!(
        count, 3,
        "Wave-2-D Fix 3 regression: expected exactly 3 alerts with \
         label `wave: \"2-d\"` in alerts.yml, found {count}. The 3 are: \
         tv-audit-write-failure, tv-s3-archive-failure, \
         tv-tick-gap-coalesce-storm."
    );
}
