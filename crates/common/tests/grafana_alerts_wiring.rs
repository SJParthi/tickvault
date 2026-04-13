//! E2: Sanity check for the Grafana alert provisioning file.
//!
//! We don't parse the YAML (no serde_yaml in workspace) — instead, we
//! verify that every expected alert UID introduced for the zero-tick-loss
//! work is present as a literal substring in the provisioning file. This
//! prevents silent deletion of alerts during unrelated edits.
//!
//! When you add a new alert for an E2 metric, update:
//!   1. deploy/docker/grafana/provisioning/alerting/alerts.yml (the rule)
//!   2. REQUIRED_ALERT_UIDS below (this test)
//!   3. crates/common/tests/metrics_catalog.rs (the underlying metric)

use std::path::Path;

const REQUIRED_ALERT_UIDS: &[(&str, &str)] = &[
    (
        "dlt-dlq-non-zero",
        "A2: any write to the dead-letter queue is critical",
    ),
    (
        "dlt-spill-disk-low-warn",
        "A2: spill dir free space < 500MB warning",
    ),
    (
        "dlt-spill-disk-low-critical",
        "A2: spill dir free space < 100MB critical",
    ),
    (
        "dlt-pool-degraded",
        "A4: pool watchdog fired 60s degraded alert",
    ),
    ("dlt-pool-halted", "A4: pool watchdog fired 300s halt alert"),
];

fn read_alerts_yml() -> String {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(std::path::PathBuf::from)
        .expect("workspace root must exist above crates/common"); // APPROVED: test
    let path = workspace_root
        .join("deploy")
        .join("docker")
        .join("grafana")
        .join("provisioning")
        .join("alerting")
        .join("alerts.yml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "E2: alerts.yml not found at {} — did the file get deleted? Error: {e}",
            path.display()
        )
    })
}

#[test]
fn grafana_alerts_every_required_uid_is_provisioned() {
    let yml = read_alerts_yml();
    let mut missing = Vec::new();
    for (uid, desc) in REQUIRED_ALERT_UIDS {
        if !yml.contains(uid) {
            missing.push((*uid, *desc));
        }
    }
    assert!(
        missing.is_empty(),
        "E2: {} required alert UID(s) are missing from alerts.yml:\n{}",
        missing.len(),
        missing
            .iter()
            .map(|(u, d)| format!("  - {u}: {d}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn grafana_alerts_every_rule_has_severity_label() {
    // Lightweight structural check — every uid we introduced must be
    // followed (eventually) by a severity label so the notification
    // routing can distinguish critical/warning.
    let yml = read_alerts_yml();
    for (uid, _) in REQUIRED_ALERT_UIDS {
        // Find the uid, then look forward for a `severity:` line before
        // the next uid. If severity is missing, the alert will route to
        // the default receiver silently.
        let idx = yml.find(uid).expect("uid must exist by the previous test");
        let rest = &yml[idx..];
        let next_uid_idx = rest[uid.len()..].find("uid:").map(|i| i + uid.len());
        let slice_end = next_uid_idx.unwrap_or(rest.len());
        let slice = &rest[..slice_end];
        assert!(
            slice.contains("severity:"),
            "E2: alert {uid} is missing a `severity:` label"
        );
    }
}
