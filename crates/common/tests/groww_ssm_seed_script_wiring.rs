//! Ratchet: `scripts/aws-seed-ssm-parameters.sh` MUST seed the two Groww
//! feed-#2 SSM parameters (`groww/api-key` + `groww/totp-secret`).
//!
//! Why this guard exists (closes the gap PR #1299 fixed): prod runs Groww
//! (`config/production.toml` `groww_enabled = true`) and the Groww sidecar
//! reads `/tickvault/<env>/groww/api-key` + `/totp-secret` from AWS SSM at
//! boot (`crates/core/src/auth/secret_manager.rs`). If the seed script ever
//! stops provisioning those two paths, the sidecar's SSM fetch backs off
//! FOREVER and Groww silently produces ZERO ticks while Dhan runs fine — the
//! AWS-side "subscribed but no ticks" trap. This ratchet makes that
//! regression a BUILD FAILURE instead of a silent go-live surprise, so the
//! "both feeds run unattended" guarantee cannot quietly rot.
//!
//! Mirrors `dhan_ip_registration_script_wiring.rs`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

#[test]
fn test_seed_script_exists_and_is_executable() {
    let path = workspace_root().join("scripts/aws-seed-ssm-parameters.sh");
    assert!(
        path.exists(),
        "Z+ L1 DETECT ratchet: scripts/aws-seed-ssm-parameters.sh is missing — \
         the operator cannot seed any SSM secrets (Dhan OR Groww)."
    );
    let meta = fs::metadata(&path).unwrap_or_else(|e| panic!("stat: {e}")); // APPROVED: test
    assert!(
        meta.permissions().mode() & 0o111 != 0,
        "Z+ L1 DETECT ratchet: aws-seed-ssm-parameters.sh is not executable."
    );
}

#[test]
fn test_seed_script_actually_seeds_both_groww_ssm_params() {
    // Assert the REAL put_param seeding calls (not just a doc comment) for the
    // two paths the Groww sidecar reads. A future edit that drops the seeding
    // — even if it leaves the docstring — fails here.
    let body = read("scripts/aws-seed-ssm-parameters.sh");
    for path in &[
        "/tickvault/${ENVIRONMENT}/groww/api-key",
        "/tickvault/${ENVIRONMENT}/groww/totp-secret",
    ] {
        assert!(
            body.contains(&format!("put_param \"{path}\"")),
            "Z+ L2 VERIFY ratchet: aws-seed-ssm-parameters.sh no longer SEEDS \
             '{path}' (missing its put_param call). The Groww sidecar reads this \
             exact path from SSM at boot — without it, Groww's SSM fetch backs off \
             forever and produces ZERO ticks. Re-add the put_param seeding."
        );
    }
}

#[test]
fn test_seed_script_groww_paths_match_the_rust_reader() {
    // The seed script's SSM path SUFFIXES must match what
    // secret_manager.rs reads, so a rename on either side is caught.
    let script = read("scripts/aws-seed-ssm-parameters.sh");
    let reader = read("crates/core/src/auth/secret_manager.rs");
    for suffix in &["groww/api-key", "groww/totp-secret"] {
        assert!(
            script.contains(suffix),
            "Z+ L3 RECONCILE ratchet: seed script missing SSM suffix '{suffix}'."
        );
        assert!(
            reader.contains(suffix),
            "Z+ L3 RECONCILE ratchet: secret_manager.rs no longer reads SSM suffix \
             '{suffix}' — the seed script + the app would target different paths."
        );
    }
}

#[test]
fn test_seed_script_exposes_groww_from_env_var_names() {
    // The --from-env (CI / non-interactive automation) mode needs the two env
    // var names, or automated seeding can never provide Groww creds unattended.
    let body = read("scripts/aws-seed-ssm-parameters.sh");
    for var in &["TV_GROWW_API_KEY", "TV_GROWW_TOTP_SECRET"] {
        assert!(
            body.contains(var),
            "Z+ L2 VERIFY ratchet: aws-seed-ssm-parameters.sh missing --from-env \
             var '{var}' — non-interactive/CI seeding of Groww creds would be \
             impossible (breaks the zero-manual-intervention path)."
        );
    }
}
