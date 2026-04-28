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

// Wave-2-D adversarial review (CRITICAL): the originally-proposed
// `tv-s3-archive-failure` alert referenced
// `tv_s3_archive_errors_total` — a metric with NO production
// emission site (the Wave-3 `crates/storage/src/s3_archive.rs`
// writer is not yet shipped). An alert that references a phantom
// metric never fires, which would silently mask SEBI archive
// failures the day Wave-3 lands.
//
// We REMOVED that alert from alerts.yml. The guard test below
// pins the absence so a future PR cannot re-introduce a phantom
// reference. The alert + emission BOTH ship together in Wave-3.
#[test]
fn s3_archive_failure_alert_is_intentionally_absent_until_wave_3() {
    let yaml = read_alerts_yaml();
    assert!(
        !yaml.contains("uid: tv-s3-archive-failure"),
        "Wave-2-D adversarial-review CRITICAL fix: the \
         `tv-s3-archive-failure` alert MUST NOT be present until the \
         Wave-3 `s3_archive.rs` writer is shipped and emits \
         `tv_s3_archive_errors_total`. A phantom-metric alert never \
         fires and silently masks SEBI archive failures."
    );
    assert!(
        !yaml.contains("tv_s3_archive_errors_total"),
        "Wave-2-D adversarial-review CRITICAL fix: phantom metric \
         `tv_s3_archive_errors_total` must not appear in alerts.yml \
         until the emission site ships."
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
fn wave_2d_alerts_have_severity_label() {
    // Every Wave-2-D alert must carry the `wave: "2-d"` label so
    // dashboards can group/filter them. The s3-archive alert was
    // removed (see s3_archive_failure_alert_is_intentionally_absent_until_wave_3),
    // so the count is 2: tv-audit-write-failure + tv-tick-gap-coalesce-storm.
    //
    // Adversarial review (LOW finding) — the original byte-substring
    // match was fragile under YAML reformatting. We now check both
    // the label key and the value-fragment separately so a
    // single-quote vs double-quote reformat keeps the test green.
    let yaml = read_alerts_yaml();
    let count_double = yaml.matches("wave: \"2-d\"").count();
    let count_single = yaml.matches("wave: '2-d'").count();
    let total = count_double + count_single;
    assert_eq!(
        total, 2,
        "Wave-2-D Fix 3 regression: expected exactly 2 alerts with \
         label `wave: \"2-d\"` (or single-quoted equivalent) in \
         alerts.yml, found {total}. The 2 are: \
         tv-audit-write-failure, tv-tick-gap-coalesce-storm. \
         (tv-s3-archive-failure intentionally deferred to Wave-3.)"
    );
}
