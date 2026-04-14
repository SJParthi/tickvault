//! S7-Step8 / Phase 8: Lockdown test for the dlt_doctor CLI binary.
//!
//! Prevents accidental deletion of the triage CLI that the AWS
//! disaster-recovery runbook depends on for "one-command incident
//! snapshot" on CRITICAL alerts.

use std::path::Path;

fn workspace_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(std::path::PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

#[test]
fn test_dlt_doctor_binary_exists() {
    let path = workspace_root()
        .join("crates")
        .join("app")
        .join("src")
        .join("bin")
        .join("dlt_doctor.rs");
    assert!(
        path.exists(),
        "S7-Step8: dlt_doctor.rs missing — DR runbook §1-§11 depend on it"
    );
}

#[test]
fn test_dlt_doctor_collects_required_probes() {
    let content =
        std::fs::read_to_string(workspace_root().join("crates/app/src/bin/dlt_doctor.rs"))
            .expect("dlt_doctor.rs must be readable"); // APPROVED: test

    // The 5 required probes per the session 7 plan.
    for probe in &[
        "collect_systemd_state",
        "collect_docker_state",
        "collect_disk_usage",
        "collect_journal_tail",
        "collect_metrics_snapshot",
    ] {
        assert!(
            content.contains(probe),
            "dlt_doctor missing probe '{probe}' — runbook invariant"
        );
    }
}

#[test]
fn test_dlt_doctor_supports_all_output_formats() {
    let content =
        std::fs::read_to_string(workspace_root().join("crates/app/src/bin/dlt_doctor.rs"))
            .expect("dlt_doctor.rs must be readable"); // APPROVED: test

    for renderer in &["print_markdown", "print_json", "print_plain"] {
        assert!(
            content.contains(renderer),
            "dlt_doctor must support {renderer} output format"
        );
    }
}
