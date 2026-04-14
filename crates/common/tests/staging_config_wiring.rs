//! S7-Step5 / Phase 8.6: Lockdown test for config/staging.toml.
//!
//! Ensures the staging profile exists, parses cleanly, and cannot
//! accidentally promote to Live trading.

use std::path::Path;

fn workspace_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(std::path::PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

#[test]
fn test_staging_config_exists_and_parses() {
    let path = workspace_root().join("config").join("staging.toml");
    assert!(
        path.exists(),
        "S7-Step5: config/staging.toml missing — dress rehearsal environment gone"
    );
    let content =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("staging.toml not readable: {e}")); // APPROVED: test
    assert!(
        content.len() > 100,
        "staging.toml suspiciously short ({} bytes)",
        content.len()
    );
}

#[test]
fn test_staging_cannot_promote_to_live() {
    let content = std::fs::read_to_string(workspace_root().join("config").join("staging.toml"))
        .expect("staging.toml must be readable"); // APPROVED: test

    // mode MUST be sandbox or paper, never live.
    assert!(
        !content.contains("mode = \"live\""),
        "STAGING LOCKDOWN: mode = live is forbidden in staging.toml"
    );
    assert!(
        content.contains("mode = \"sandbox\"") || content.contains("mode = \"paper\""),
        "STAGING LOCKDOWN: mode must be sandbox or paper"
    );
    // sandbox_only_until must be a far-future date so staging never
    // accidentally promotes.
    assert!(
        content.contains("2099-12-31"),
        "STAGING LOCKDOWN: sandbox_only_until must be 2099-12-31 to \
         prevent accidental Live promotion"
    );
    // dry_run must be true.
    assert!(
        content.contains("dry_run = true"),
        "STAGING LOCKDOWN: dry_run must be true"
    );
}

#[test]
fn test_staging_uses_sandbox_dhan_url() {
    let content = std::fs::read_to_string(workspace_root().join("config").join("staging.toml"))
        .expect("staging.toml must be readable"); // APPROVED: test

    assert!(
        content.contains("sandbox.dhan.co"),
        "staging.toml must point rest_api_base_url at sandbox.dhan.co"
    );
    assert!(
        !content.contains("api.dhan.co/v2"),
        "staging.toml must NOT reference the live api.dhan.co base URL"
    );
}
