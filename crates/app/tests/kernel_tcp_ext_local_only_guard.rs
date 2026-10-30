//! The kernel TCP drop counters and receive-buffer ceilings stay on the local
//! `/metrics` page only (plan item 52j phase A, 2026-10-30).
//!
//! Shipping a name to CloudWatch costs money every month and needs a dated
//! row in the noise lock first. This guard fails the build if any of the new
//! names appears in the CloudWatch agent's metric selector or in the
//! terraform, so they cannot be shipped or alarmed by accident.

use std::path::{Path, PathBuf};

use tickvault_app::kernel_rx_queue_sampler::{
    KERNEL_RMEM_MAX_GAUGE, KERNEL_TCP_EXT_COUNTER, KERNEL_TCP_RMEM_MAX_GAUGE,
};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves") // APPROVED: test
}

const NEW_NAMES: [&str; 3] = [
    KERNEL_TCP_EXT_COUNTER,
    KERNEL_RMEM_MAX_GAUGE,
    KERNEL_TCP_RMEM_MAX_GAUGE,
];

#[test]
fn kernel_tcp_ext_names_are_not_in_the_cloudwatch_agent_config() {
    let path = repo_root().join("deploy/aws/cloudwatch-agent.json");
    let text = std::fs::read_to_string(&path).expect("cloudwatch-agent.json is readable"); // APPROVED: test
    for name in NEW_NAMES {
        assert!(
            !text.contains(name),
            "{name} is in {}: it is local /metrics only (plan item 52j). \
             Shipping it needs a dated noise-lock row first.",
            path.display()
        );
    }
    // The shared prefix too, so a regex selector cannot pull them in.
    assert!(
        !text.contains("tv_kernel_"),
        "a tv_kernel_ selector would ship the local-only kernel names"
    );
}

#[test]
fn kernel_tcp_ext_names_are_not_in_terraform() {
    let dir = repo_root().join("deploy/aws/terraform");
    let entries = std::fs::read_dir(&dir).expect("terraform dir is readable"); // APPROVED: test
    let mut scanned = 0_usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_tf = path
            .extension()
            .is_some_and(|ext| ext == "tf" || ext == "tftpl");
        if !is_tf {
            continue;
        }
        scanned += 1;
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        for name in NEW_NAMES {
            assert!(
                !text.contains(name),
                "{name} is in {}: no alarm or filter may watch it without a dated noise-lock row",
                path.display()
            );
        }
    }
    assert!(scanned > 0, "the terraform scan must not pass vacuously");
}
