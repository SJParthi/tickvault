//! Source-scan ratchet: the BootHealthCheck safety ping must stay wired into
//! boot. It was defined in events.rs but never fired (audit-findings Rule 13:
//! defined-but-never-called = a bug). This guard fails the build if the emit is
//! removed, so the positive boot signal can't silently regress to dead code.

use std::fs;
use std::path::PathBuf;

fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn test_boot_health_check_is_emitted_after_infra_check() {
    let main_src = read("src/main.rs");
    assert!(
        main_src.contains("NotificationEvent::BootHealthCheck"),
        "main.rs must emit BootHealthCheck at boot (it was defined but never fired)."
    );
    assert!(
        main_src.contains("infra::container_health_counts()"),
        "the BootHealthCheck counts must come from infra::container_health_counts()."
    );
}

#[test]
fn test_container_health_counts_helper_exists() {
    let infra_src = read("src/infra.rs");
    assert!(
        infra_src.contains("pub async fn container_health_counts(")
            && infra_src.contains("pub fn parse_container_health("),
        "infra.rs must expose container_health_counts() + the pure parse_container_health()."
    );
}
